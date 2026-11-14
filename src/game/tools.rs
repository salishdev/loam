//! The director's tools: what it can read and how it changes the world.
//!
//! Every handler returns `Ok(ToolOutput)` for anything the model should see
//! (including refusals such as a rejected patch) and `Err` only for storage
//! failures, which end the turn.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::glossary::{self, GLOSSARY_KIND};
use crate::inventory::{self, Inventory};
use crate::llm::{ToolDefinition, ToolOutput, ToolUse};
use crate::quest;
use crate::rules::{self, RULE_KIND};
use crate::stats;
use crate::world::{
    ApplyError, CARRIER_PROP, Entity, EntityId, EntityRef, EventKind, Op, Patch, StoreError, World,
};

use super::{BIBLE_KIND, BIBLE_SECTIONS, PLAYER_KIND, ROOM_KIND, bible_id, player_id, scope};

/// Which tools a director call gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Building the bible from the seed: only `write_bible`.
    Bible,
    /// Playing turns (including the opening).
    Play,
    /// A world tick: read tools and off-screen patches only.
    Tick,
}

pub fn definitions(phase: Phase) -> Vec<ToolDefinition> {
    match phase {
        Phase::Bible => vec![write_bible_def(), stats::tools::sheet_tool_definition()],
        Phase::Play => vec![
            read_bible_def(),
            query_entity_def(),
            search_events_def(),
            create_room_def(),
            move_player_def(),
            apply_patch_def(),
        ]
        .into_iter()
        .chain(inventory::tool_definitions())
        .chain(quest::tool_definitions())
        .chain(rules::tools::tool_definitions())
        .chain(glossary::tools::tool_definitions())
        .chain(stats::tools::tool_definitions())
        .collect(),
        Phase::Tick => vec![
            read_bible_def(),
            query_entity_def(),
            search_events_def(),
            tick_apply_patch_def(),
        ],
    }
}

/// Run one tool call. Unknown tools and bad input go back as tool errors.
pub fn dispatch(
    world: &mut World,
    inventory: &Inventory,
    dice_seed: u64,
    phase: Phase,
    call: &ToolUse,
) -> Result<ToolOutput, StoreError> {
    let allowed = definitions(phase).iter().any(|d| d.name == call.name);
    if !allowed {
        return Ok(ToolOutput::Err(format!(
            "no tool named {:?} here",
            call.name
        )));
    }
    if let Some(player) = player_id(world)?
        && let Some(output) = inventory.handle_tool(world, player, call)
    {
        return Ok(output);
    }
    if let Some(result) = quest::handle_tool(world, call) {
        return result;
    }
    if let Some(output) = rules::tools::handle_tool(world, scope(world)?, call) {
        return output;
    }
    if let Some(output) = glossary::tools::handle_tool(world, call) {
        return output;
    }
    if let Some(output) = stats::tools::handle_tool(world, dice_seed, call) {
        return output;
    }
    macro_rules! input {
        ($t:ty) => {
            match serde_json::from_value::<$t>(call.input.clone()) {
                Ok(v) => v,
                Err(e) => return Ok(ToolOutput::Err(format!("bad input for {}: {e}", call.name))),
            }
        };
    }
    match call.name.as_str() {
        "write_bible" => write_bible(world, input!(BibleInput)),
        "read_bible" => read_bible(world, input!(ReadBibleInput)),
        "query_entity" => query_entity(world, input!(QueryEntityInput)),
        "search_events" => search_events(world, input!(SearchEventsInput)),
        "create_room" => create_room(world, input!(CreateRoomInput)),
        "move_player" => move_player(world, input!(MovePlayerInput)),
        "apply_patch" if phase == Phase::Tick => {
            let patch = input!(Patch);
            if let Some(reason) = on_screen_refusal(world, &patch)? {
                return refuse(reason);
            }
            apply_patch(world, patch)
        }
        "apply_patch" => apply_patch(world, input!(Patch)),
        // Inventory tools land here only when there is no player yet.
        _ => refuse("there is no player yet"),
    }
}

fn ok(value: Value) -> Result<ToolOutput, StoreError> {
    Ok(ToolOutput::Ok(value.to_string()))
}

fn refuse(reason: impl Into<String>) -> Result<ToolOutput, StoreError> {
    Ok(ToolOutput::Err(reason.into()))
}

/// Who a patch is applied as: the director's own content, or an engine
/// step (bible and player setup, moving the player) that may touch
/// engine-owned state.
#[derive(Clone, Copy, PartialEq, Eq)]
enum As {
    Director,
    Engine,
}

/// Apply a patch; a rejection becomes a refusal the model sees.
fn try_apply(
    world: &mut World,
    patch: &Patch,
    by: As,
) -> Result<Result<crate::world::Applied, String>, StoreError> {
    let result = match by {
        As::Director => world.apply_patch(patch),
        As::Engine => world.apply_engine_patch(patch),
    };
    match result {
        Ok(applied) => Ok(Ok(applied)),
        Err(ApplyError::Rejected(r)) => Ok(Err(format!("patch rejected: {r}"))),
        Err(ApplyError::Storage(e)) => Err(StoreError::Sqlite(e)),
    }
}

// ---- write_bible -----------------------------------------------------------

fn write_bible_def() -> ToolDefinition {
    let list = |what: &str| {
        json!({
            "type": "array",
            "minItems": 1,
            "description": what,
            "items": {"type": "object", "properties": {
                "name": {"type": "string"},
                "description": {"type": "string"}
            }, "required": ["name", "description"]}
        })
    };
    ToolDefinition::new(
        "write_bible",
        "Store the hidden world bible. Call once. Items may carry extra fields \
         (e.g. an NPC's faction, home region, wants).",
        json!({
            "type": "object",
            "properties": {
                "tone": {"type": "string", "description": "Voice, mood and genre rules for narration."},
                "regions": list("Broad areas of the world; rooms are generated inside them."),
                "factions": list("Groups with goals and tensions."),
                "npcs": list("Key characters."),
                "secrets": list("Hidden truths the player can uncover.")
            },
            "required": BIBLE_SECTIONS
        }),
    )
}

#[derive(Deserialize)]
struct BibleInput {
    tone: String,
    regions: Vec<Value>,
    factions: Vec<Value>,
    npcs: Vec<Value>,
    secrets: Vec<Value>,
}

fn write_bible(world: &mut World, input: BibleInput) -> Result<ToolOutput, StoreError> {
    if bible_id(world)?.is_some() {
        return refuse("the bible is already written");
    }
    if input.tone.trim().is_empty() {
        return refuse("tone must not be empty");
    }
    let sections = [
        ("regions", input.regions),
        ("factions", input.factions),
        ("npcs", input.npcs),
        ("secrets", input.secrets),
    ];
    let mut props = Map::new();
    props.insert("tone".into(), Value::String(input.tone));
    for (name, items) in sections {
        if items.is_empty() {
            return refuse(format!("{name} must have at least one entry"));
        }
        props.insert(name.into(), Value::Array(items));
    }
    let patch = Patch::new(vec![
        Op::Create {
            handle: None,
            name: "World bible".into(),
            kind: BIBLE_KIND.into(),
            location: None,
            props,
        },
        Op::Create {
            handle: None,
            name: "you".into(),
            kind: PLAYER_KIND.into(),
            location: None,
            props: Map::from_iter([(CARRIER_PROP.to_owned(), Value::Bool(true))]),
        },
    ]);
    match try_apply(world, &patch, As::Engine)? {
        Ok(_) => ok(json!({"stored": BIBLE_SECTIONS})),
        Err(reason) => refuse(reason),
    }
}

// ---- read_bible ------------------------------------------------------------

fn read_bible_def() -> ToolDefinition {
    ToolDefinition::new(
        "read_bible",
        "Read one section of the hidden world bible.",
        json!({
            "type": "object",
            "properties": {"section": {"type": "string", "enum": BIBLE_SECTIONS}},
            "required": ["section"]
        }),
    )
}

#[derive(Deserialize)]
struct ReadBibleInput {
    section: String,
}

fn read_bible(world: &World, input: ReadBibleInput) -> Result<ToolOutput, StoreError> {
    let Some(bible) = bible_id(world)?
        .map(|id| world.entity(id))
        .transpose()?
        .flatten()
    else {
        return refuse("there is no bible yet");
    };
    match bible.props.get(&input.section) {
        Some(v) => ok(v.clone()),
        None => refuse(format!(
            "no section {:?}; sections: {}",
            input.section,
            bible.props.keys().cloned().collect::<Vec<_>>().join(", ")
        )),
    }
}

// ---- query_entity ----------------------------------------------------------

fn query_entity_def() -> ToolDefinition {
    ToolDefinition::new(
        "query_entity",
        "Look up an entity by id: name, kind, props, where it is and what it contains.",
        json!({
            "type": "object",
            "properties": {"id": {"type": "integer"}},
            "required": ["id"]
        }),
    )
}

#[derive(Deserialize)]
struct QueryEntityInput {
    id: EntityId,
}

fn query_entity(world: &World, input: QueryEntityInput) -> Result<ToolOutput, StoreError> {
    let Some(e) = world.entity(input.id)? else {
        return refuse(format!("entity #{} does not exist", input.id));
    };
    match e.kind.as_str() {
        BIBLE_KIND => return refuse("the bible is read with read_bible"),
        RULE_KIND => return refuse("rules are read with list_rules"),
        GLOSSARY_KIND => return refuse("the glossary is read with lookup_terms"),
        _ => {}
    }
    let location = match e.location {
        Some(id) => world.entity(id)?.map(|l| brief(&l)),
        None => None,
    };
    let mut contents = Vec::new();
    for id in world.contents(Some(e.id))? {
        if let Some(c) = world.entity(id)? {
            contents.push(brief(&c));
        }
    }
    ok(json!({
        "id": e.id,
        "name": e.name,
        "kind": e.kind,
        "location": location,
        "props": e.props,
        "contents": contents,
    }))
}

fn brief(e: &Entity) -> Value {
    json!({"id": e.id, "name": e.name, "kind": e.kind})
}

// ---- search_events ---------------------------------------------------------

fn search_events_def() -> ToolDefinition {
    ToolDefinition::new(
        "search_events",
        "Search the event log (player inputs, narration, patches, tool calls) for text, \
         newest first. Use it to recall what happened before.",
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "kinds": {"type": "array", "items": {"type": "string", "enum": EventKind::ALL.map(EventKind::as_str)}},
                "limit": {"type": "integer", "minimum": 1, "maximum": 20}
            },
            "required": ["text"]
        }),
    )
}

#[derive(Deserialize)]
struct SearchEventsInput {
    text: String,
    #[serde(default)]
    kinds: Vec<String>,
    limit: Option<usize>,
}

fn search_events(world: &World, input: SearchEventsInput) -> Result<ToolOutput, StoreError> {
    let mut kinds = Vec::new();
    for k in &input.kinds {
        match EventKind::parse(k) {
            Some(kind) => kinds.push(kind),
            None => return refuse(format!("unknown event kind {k:?}")),
        }
    }
    let limit = input.limit.unwrap_or(8).clamp(1, 20);
    let events = world.search_events(&input.text, &kinds, limit)?;
    ok(Value::Array(
        events
            .into_iter()
            .map(|e| json!({"seq": e.seq, "kind": e.kind.as_str(), "payload": e.payload}))
            .collect(),
    ))
}

// ---- create_room -----------------------------------------------------------

fn create_room_def() -> ToolDefinition {
    ToolDefinition::new(
        "create_room",
        "Generate the room the player is entering for the first time, furnish it and move \
         the player in. Use via_exit (an unexplored exit of the current room) and back_exit \
         (the new room's exit leading back). Omit both only for the opening room. Everything \
         must fit the bible's region.",
        json!({
            "type": "object",
            "properties": {
                "via_exit": {"type": "string"},
                "back_exit": {"type": "string"},
                "name": {"type": "string"},
                "region": {"type": "string", "description": "Name of a bible region."},
                "description": {"type": "string", "description": "Lasting physical description."},
                "exits": {
                    "type": "object",
                    "description": "Further unexplored exits: direction -> short hint of where it leads.",
                    "additionalProperties": {"type": "string"}
                },
                "contents": {
                    "type": "array",
                    "description": "Entities here: objects, NPCs, features.",
                    "items": {"type": "object", "properties": {
                        "handle": {"type": "string", "description": "Name to put other contents inside this one."},
                        "name": {"type": "string"},
                        "kind": {"type": "string", "description": "Free-form, e.g. item, feature, container. Use npc for any being that can move, speak or act on its own (people, animals, monsters, spirits); put its role or species in props."},
                        "inside": {"type": "string", "description": "handle of a container listed earlier"},
                        "props": {"type": "object"}
                    }, "required": ["name", "kind"]}
                }
            },
            "required": ["name", "region", "description", "exits", "contents"]
        }),
    )
}

#[derive(Deserialize)]
struct CreateRoomInput {
    via_exit: Option<String>,
    back_exit: Option<String>,
    name: String,
    region: String,
    description: String,
    #[serde(default)]
    exits: Map<String, Value>,
    #[serde(default)]
    contents: Vec<ContentInput>,
}

#[derive(Deserialize)]
struct ContentInput {
    handle: Option<String>,
    name: String,
    kind: String,
    inside: Option<String>,
    #[serde(default)]
    props: Map<String, Value>,
}

const ROOM_HANDLE: &str = "@room";

fn create_room(world: &mut World, input: CreateRoomInput) -> Result<ToolOutput, StoreError> {
    let Some(player) = player_id(world)? else {
        return refuse("there is no player yet");
    };
    let here = world.entity(player)?.and_then(|p| p.location);

    // Where the player comes from, and the exit being explored.
    let origin = match (&input.via_exit, here) {
        (None, None) => None,
        (None, Some(_)) => {
            return refuse(
                "the player is already in a room: give via_exit (an unexplored exit) and back_exit",
            );
        }
        (Some(_), None) => {
            return refuse("the player is nowhere yet: omit via_exit for the opening room");
        }
        (Some(exit), Some(here)) => {
            let room = world.entity(here)?.expect("player location exists");
            let exits = exits_of(&room);
            match exits.get(exit) {
                None => {
                    return refuse(format!(
                        "{} has no exit {exit:?}; exits: {}",
                        room.name,
                        names(&exits)
                    ));
                }
                Some(e) if e.get("to").is_some() => {
                    return refuse(format!(
                        "exit {exit:?} is already explored: use move_player"
                    ));
                }
                Some(_) => {}
            }
            let Some(back) = input.back_exit.as_deref().filter(|b| !b.trim().is_empty()) else {
                return refuse("back_exit is required with via_exit");
            };
            if input.exits.contains_key(back) {
                return refuse(format!("back_exit {back:?} is also listed in exits"));
            }
            Some((room, exit.clone(), back.to_owned()))
        }
    };

    let mut exits = Map::new();
    for (dir, hint) in input.exits {
        let label = match hint {
            Value::String(s) => s,
            other => other.to_string(),
        };
        exits.insert(dir, json!({"label": label}));
    }
    if let Some((room, _, back)) = &origin {
        exits.insert(back.clone(), json!({"label": room.name, "to": room.id}));
    }

    let mut props = Map::new();
    props.insert("description".into(), input.description.into());
    props.insert("region".into(), input.region.into());
    props.insert("exits".into(), Value::Object(exits));
    let mut ops = vec![Op::Create {
        handle: Some(ROOM_HANDLE.into()),
        name: input.name.clone(),
        kind: ROOM_KIND.into(),
        location: None,
        props,
    }];
    let mut handles = BTreeSet::new();
    for c in input.contents {
        if let Some(h) = &c.handle
            && (h == ROOM_HANDLE || !handles.insert(h.clone()))
        {
            return refuse(format!("content handle {h:?} is used twice"));
        }
        let location = match c.inside {
            Some(h) if handles.contains(&h) => EntityRef::Handle(h),
            Some(h) => return refuse(format!("inside {h:?}: no earlier content has that handle")),
            None => EntityRef::Handle(ROOM_HANDLE.into()),
        };
        ops.push(Op::Create {
            handle: c.handle,
            name: c.name,
            kind: c.kind,
            location: Some(location),
            props: c.props,
        });
    }
    let patch = Patch::new(ops);
    let applied = match try_apply(world, &patch, As::Director)? {
        Ok(a) => a,
        Err(reason) => return refuse(reason),
    };
    let new_terms = glossary::register_patch(world, &patch)?;
    let room_id = applied.handles[ROOM_HANDLE];

    // Link the explored exit and move the player in.
    let mut ops = Vec::new();
    if let Some((room, exit, _)) = &origin {
        let mut exits = exits_of(room);
        exits.insert(exit.clone(), json!({"label": input.name, "to": room_id}));
        ops.push(Op::SetProp {
            entity: room.id.into(),
            key: "exits".into(),
            value: Value::Object(exits),
        });
    }
    ops.push(Op::Move {
        entity: player.into(),
        to: Some(room_id.into()),
    });
    if let Err(reason) = try_apply(world, &Patch::new(ops), As::Engine)? {
        return refuse(reason);
    }

    let mut contents = Vec::new();
    for id in &applied.created[1..] {
        if let Some(e) = world.entity(*id)? {
            contents.push(brief(&e));
        }
    }
    ok(glossary::tools::with_new_terms(
        json!({"room": room_id, "player_moved_to": room_id, "contents": contents}),
        &new_terms,
    ))
}

pub(crate) fn exits_of(room: &Entity) -> Map<String, Value> {
    room.props
        .get("exits")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn names(exits: &Map<String, Value>) -> String {
    if exits.is_empty() {
        "none".into()
    } else {
        exits.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}

// ---- move_player -----------------------------------------------------------

fn move_player_def() -> ToolDefinition {
    ToolDefinition::new(
        "move_player",
        "Move the player through an explored exit of the current room. An unexplored exit \
         needs create_room instead.",
        json!({
            "type": "object",
            "properties": {"exit": {"type": "string"}},
            "required": ["exit"]
        }),
    )
}

#[derive(Deserialize)]
struct MovePlayerInput {
    exit: String,
}

fn move_player(world: &mut World, input: MovePlayerInput) -> Result<ToolOutput, StoreError> {
    let Some(player) = player_id(world)? else {
        return refuse("there is no player yet");
    };
    let Some(here) = world.entity(player)?.and_then(|p| p.location) else {
        return refuse("the player is nowhere yet: create the opening room");
    };
    let room = world.entity(here)?.expect("player location exists");
    let exits = exits_of(&room);
    let Some(exit) = exits.get(&input.exit) else {
        return refuse(format!(
            "{} has no exit {:?}; exits: {}",
            room.name,
            input.exit,
            names(&exits)
        ));
    };
    let Some(to) = exit.get("to").and_then(Value::as_i64) else {
        return refuse(format!(
            "exit {:?} is unexplored: call create_room with via_exit {:?}",
            input.exit, input.exit
        ));
    };
    let patch = Patch::new(vec![Op::Move {
        entity: player.into(),
        to: Some(to.into()),
    }]);
    if let Err(reason) = try_apply(world, &patch, As::Engine)? {
        return refuse(reason);
    }
    query_entity(world, QueryEntityInput { id: to })
}

// ---- apply_patch -----------------------------------------------------------

fn apply_patch_def() -> ToolDefinition {
    ToolDefinition::new(
        "apply_patch",
        "Change the world: an atomic list of ops applied in order; if one fails nothing \
         applies and you get the reason. Use it for every real consequence of an action. \
         Ops: create {handle?, name, kind, location?, props?}, move {entity, to}, \
         set_prop {entity, key, value}, remove_prop {entity, key}, rename {entity, name}, \
         destroy {entity} (must be empty).",
        patch_schema(),
    )
}

/// The tick's `apply_patch`: the same ops, off-screen only, with a note.
fn tick_apply_patch_def() -> ToolDefinition {
    let mut schema = patch_schema();
    schema["properties"]["note"] = json!({
        "type": "string",
        "description": "What happened, in a sentence, for the log. The director reads it \
                        when the player next comes here."
    });
    schema["required"] = json!(["ops", "note"]);
    ToolDefinition::new(
        "apply_patch",
        "Advance an off-screen arc: an atomic list of ops applied in order; if one fails \
         nothing applies and you get the reason. Ops: create {handle?, name, kind, location?, \
         props?}, move {entity, to}, set_prop {entity, key, value}, remove_prop {entity, key}, \
         rename {entity, name}, destroy {entity} (must be empty). The player, the player's \
         room and everything in them are off-limits.",
        schema,
    )
}

fn patch_schema() -> Value {
    let r = json!({"type": ["integer", "string"], "description": "entity id, or a handle created earlier in this patch"});
    json!({
        "type": "object",
        "properties": {"ops": {"type": "array", "minItems": 1, "items": {
            "type": "object",
            "properties": {
                "op": {"type": "string", "enum": ["create", "move", "set_prop", "remove_prop", "rename", "destroy"]},
                "entity": r, "to": r, "location": r,
                "handle": {"type": "string"}, "name": {"type": "string"},
                "kind": {"type": "string", "description": "create only. Free-form, e.g. item, feature, container. Use npc for any being that can move, speak or act on its own (people, animals, monsters, spirits); put its role or species in props."},
                "key": {"type": "string"}, "value": {}, "props": {"type": "object"}
            },
            "required": ["op"]
        }}},
        "required": ["ops"]
    })
}

fn apply_patch(world: &mut World, patch: Patch) -> Result<ToolOutput, StoreError> {
    let player = player_id(world)?;
    for (i, op) in patch.ops.iter().enumerate() {
        let refs = op_refs(op);
        for r in &refs {
            let EntityRef::Id(id) = r else { continue };
            let kind = world.entity(*id)?.map(|e| e.kind);
            let why = match kind.as_deref() {
                Some(BIBLE_KIND) => "the bible is hidden and fixed; read it with read_bible",
                Some(RULE_KIND) => {
                    "rules change only through mint_rule, amend_rule and retire_rule"
                }
                Some(GLOSSARY_KIND) => "the glossary changes only through define_terms",
                _ => continue,
            };
            return refuse(format!("op {i}: {why}"));
        }
        for r in &refs {
            if let EntityRef::Id(id) = r
                && quest::is_quest(world, *id)?
            {
                return refuse(format!(
                    "op {i}: #{id} is a quest; change quests only with spawn_quest, \
                     reveal_goal and escalate_quest"
                ));
            }
        }
        if let Op::Destroy { entity } | Op::Rename { entity, .. } = op
            && matches!(entity, EntityRef::Id(x) if Some(*x) == player)
        {
            return refuse(format!("op {i}: the player cannot be renamed or destroyed"));
        }
    }
    match try_apply(world, &patch, As::Director)? {
        Ok(applied) => {
            let new_terms = glossary::register_patch(world, &patch)?;
            ok(glossary::tools::with_new_terms(
                json!({"applied": true, "created": applied.created, "handles": applied.handles}),
                &new_terms,
            ))
        }
        Err(reason) => refuse(reason),
    }
}

/// Every entity an op touches or places something into.
pub(crate) fn op_refs(op: &Op) -> Vec<&EntityRef> {
    match op {
        Op::Create { location, .. } => location.iter().collect(),
        Op::Move { entity, to } => std::iter::once(entity).chain(to).collect(),
        Op::SetProp { entity, .. }
        | Op::RemoveProp { entity, .. }
        | Op::Rename { entity, .. }
        | Op::Destroy { entity } => vec![entity],
    }
}

/// Why a tick patch may not apply: it creates a room, or touches the
/// player, the player's room or anything inside either. An entity created
/// earlier in the same patch is checked through the op that placed it.
fn on_screen_refusal(world: &World, patch: &Patch) -> Result<Option<String>, StoreError> {
    let Some(player) = player_id(world)? else {
        return Ok(None);
    };
    let room = world.entity(player)?.and_then(|p| p.location);
    for (i, op) in patch.ops.iter().enumerate() {
        if let Op::Create { kind, .. } = op
            && kind == ROOM_KIND
        {
            return Ok(Some(format!(
                "op {i}: a world tick doesn't create rooms; they are generated when the \
                 player explores"
            )));
        }
        for r in op_refs(op) {
            if let EntityRef::Id(id) = r
                && is_on_screen(world, *id, player, room)?
            {
                return Ok(Some(format!(
                    "op {i}: #{id} is where the player can see it; a world tick changes \
                     only what happens off-screen"
                )));
            }
        }
    }
    Ok(None)
}

/// Whether `id` is the player, the player's room, or inside either.
pub(crate) fn is_on_screen(
    world: &World,
    id: EntityId,
    player: EntityId,
    room: Option<EntityId>,
) -> Result<bool, StoreError> {
    let mut at = Some(id);
    while let Some(here) = at {
        if here == player || Some(here) == room {
            return Ok(true);
        }
        at = world.entity(here)?.and_then(|e| e.location);
    }
    Ok(false)
}
