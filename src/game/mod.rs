//! The play loop: seed prompt → world bible → director turns.
//!
//! The director (Sonnet) gets a short system prompt and a local slice of the
//! world each turn; everything else it learns and changes through tools
//! (see [`tools`]). Rooms are generated lazily the first time the player
//! enters them and persist in the world store from then on.

mod context;
mod exits;
mod fast_path;
mod npcs;
mod tick;
mod tools;

#[cfg(test)]
mod tests;

pub use context::local_slice;
pub use npcs::{NpcSighting, bible_npc_names, npcs};
pub use tick::{ROOM_CHANGE_MINUTES, TICK_MINUTES, TickChange, TickOutcome};
pub use tools::Phase;

use serde_json::json;

use crate::inventory::{DEFAULT_SLOTS, Inventory};
use crate::llm::ToolUse;
use crate::llm::{
    Effort, LlmClient, LlmError, Message, Model, Request, StopReason, ToolOutput, run_tool_loop,
};
use crate::quest::{self, Judgment, Quest};
use crate::rules::{self, RuleId, Scope};
use crate::stats;
use crate::time;
use crate::world::{EntityId, EventKind, StoreError, World};

/// Kind of the single entity holding the world bible.
pub const BIBLE_KIND: &str = "bible";
/// Kind of the player entity.
pub use crate::world::PLAYER_KIND;
/// Kind of generated rooms.
pub use crate::world::ROOM_KIND;
/// The bible's sections, in the order the director writes them.
pub const BIBLE_SECTIONS: [&str; 5] = ["tone", "regions", "factions", "npcs", "secrets"];

/// Most model calls one director turn may make.
const MAX_TURN_CALLS: usize = 16;
/// Most model calls building the bible may make.
const MAX_BIBLE_CALLS: usize = 6;

const BIBLE_SYSTEM: &str = "You are the director of Loam, a text adventure generated from the \
player's seed prompt. Build the hidden world bible with write_bible: a tone, a handful of \
regions, factions with goals in tension, key NPCs and secrets worth uncovering. Be concrete \
and specific to the seed; keep each entry to a sentence or two. Then write the player's stat \
sheet with write_stats: a few stats that fit the seed, each with thresholds that bring \
setbacks (never the end of the game) and an advancement charter giving the player reasons \
to grow. Rooms are generated later, as the player explores. Reply with nothing but the \
tool calls.";

const DIRECTOR_SYSTEM: &str = "You are the director of Loam, a text adventure. The player may \
try anything. Each turn: decide what the input means in this world, make the real \
consequences happen with tools, then narrate. The engine's state is the truth: look things \
up (query_entity, read_bible, search_events) rather than inventing facts that may already \
exist, and never change the world only in prose. When the player goes through an \
unexplored exit, generate the room with create_room, furnished to fit the bible; through an \
explored one, move_player. Exits are exit relations between rooms. An entity's kind is a \
free descriptive label (sword, heron, door); what it can do comes from marker props: \
acts: true for anything that moves, speaks or acts on its own (people, animals, spirits, a \
talking sword; these are the NPCs), container: true for anything things can be put inside, \
fixed: true for anything the player can't take. Only rooms, the player, acts beings and \
containers can hold things. If a tool \
refuses, adapt; an impossible action is narrated as a non-change. What anyone carries \
changes only through the inventory tools (take, drop, give, equip, unequip, transfer). \
You are also the drama manager: when play suggests a quest, spawn_quest; once its goal is \
clear to the player, reveal_goal with the entities it concerns; escalate_quest to raise the \
stakes. A judge decides when a quest is done. The engine remembers rulings as rules and replays them without you: when you rule on an \
action that could recur, mint_rule at the generality you judge right (conditions on kind \
and props make it general; related conditions make it hold only while a relation does) and give apply_to so the rule itself is this turn's ruling; \
one-off story events need no rule. When an action's outcome is uncertain, roll check \
against the fitting stat and honour the result; a rule can embed the check. Stats change \
only through adjust_stat with a reason; raise them by their advancement rules, and build \
quests around the charter. Use glossary terms for props (lookup_terms) and define \
new ones (define_terms). When a note says rules conflict or misfire, fix them with \
amend_rule or retire_rule and, where you can, turn the contradiction into story. Write no \
text until your tool calls are done. Your final message is shown to the player verbatim: \
second person, present tense, in the bible's tone, one to three short paragraphs.";

const OPENING: &str = "The story begins. The player is nowhere yet: read the bible, create the \
opening room with create_room (no via_exit), then narrate the opening scene.";

#[derive(Debug, thiserror::Error)]
pub enum GameError {
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("the director did not write a world bible")]
    NoBible,
    #[error("the director did not write a stat sheet")]
    NoStats,
    #[error("the world has no bible yet; start it from a seed")]
    NotStarted,
}

/// What one turn produced.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    /// The narration shown to the player.
    pub narration: String,
    /// Why the director (or narrator) stopped, when it wasn't a normal end
    /// of turn.
    pub stop_reason: Option<StopReason>,
    /// Model calls the turn made (director only, not the judge).
    pub calls: usize,
    /// The judge's rulings on quests this turn touched.
    pub judgments: Vec<Judgment>,
    /// The rule applied when the turn took the fast path (no director call).
    pub fast_path: Option<RuleId>,
}

/// Where a turn's events start, and its input (none for the opening).
#[derive(Debug, Clone, Copy)]
struct Turn<'a> {
    start: i64,
    input: Option<&'a str>,
}

/// A game session over a world store.
pub struct Game {
    world: World,
    inventory: Inventory,
    /// Seeds the dice when the stat sheet is written; random when `None`.
    dice_seed: Option<u64>,
}

impl Game {
    pub fn new(world: World) -> Self {
        Game {
            world,
            // The player's carry limit follows the sheet's capacity rule.
            inventory: Inventory::new(DEFAULT_SLOTS, stats::carry_limit),
            dice_seed: None,
        }
    }

    /// Fix the seed the dice get when the stat sheet is written, so a game
    /// rolls the same every time. Has no effect once the sheet exists: the
    /// seed is then part of the world.
    pub fn set_dice_seed(&mut self, seed: u64) {
        self.dice_seed = Some(seed);
    }

    /// The player's stats, by name (empty before the sheet is written).
    pub fn stats(&self) -> Result<Vec<stats::Stat>, GameError> {
        Ok(stats::sheet(&self.world)?.into_values().collect())
    }

    pub fn world(&self) -> &World {
        &self.world
    }

    #[cfg(test)]
    pub(crate) fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    pub fn into_world(self) -> World {
        self.world
    }

    /// Whether this world already has a bible (an existing save).
    pub fn has_bible(&self) -> Result<bool, GameError> {
        Ok(bible_id(&self.world)?.is_some())
    }

    /// Whether the player has been placed in the opening room.
    pub fn has_begun(&self) -> Result<bool, GameError> {
        Ok(match player_id(&self.world)? {
            Some(id) => self.world.entity(id)?.and_then(|p| p.location).is_some(),
            None => false,
        })
    }

    /// Build the world bible and the stat sheet from a seed prompt.
    pub fn create_bible(&mut self, client: &dyn LlmClient, seed: &str) -> Result<(), GameError> {
        self.world
            .log_event(EventKind::Input, &json!({"text": seed, "seed": true}))?;
        let request = Request::new(Model::Sonnet, vec![Message::user(seed)])
            .system(BIBLE_SYSTEM)
            .effort(Effort::Medium)
            .tools(tools::definitions(Phase::Bible));
        self.run(
            client,
            request,
            Phase::Bible,
            MAX_BIBLE_CALLS,
            &mut |_| {},
            None,
        )?;
        if !self.has_bible()? {
            return Err(GameError::NoBible);
        }
        if stats::sheet(&self.world)?.is_empty() {
            return Err(GameError::NoStats);
        }
        Ok(())
    }

    /// Generate the opening room and narrate the opening scene.
    pub fn begin(
        &mut self,
        client: &dyn LlmClient,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<TurnOutcome, GameError> {
        if !self.has_bible()? {
            return Err(GameError::NotStarted);
        }
        let slice = local_slice(&self.world)?;
        let turn = Turn {
            start: self.world.last_seq()?,
            input: None,
        };
        self.director_turn(client, &slice, OPENING, turn, on_text)
    }

    /// Play one turn, streaming the narration to `on_text`.
    ///
    /// The Haiku interpreter first matches `input` against the stored rules;
    /// if exactly one applies cleanly the engine applies it and a narrator
    /// describes the result (the fast path). Otherwise the director
    /// interprets the input, patches the world through tools and narrates,
    /// told why the rules didn't settle it.
    pub fn turn(
        &mut self,
        client: &dyn LlmClient,
        input: &str,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<TurnOutcome, GameError> {
        if !self.has_bible()? {
            return Err(GameError::NotStarted);
        }
        // The slice is taken before the input is logged, so "Recent" shows
        // earlier turns only.
        let slice = local_slice(&self.world)?;
        self.world
            .log_event(EventKind::Input, &json!({"text": input}))?;
        let turn = Turn {
            start: self.world.last_seq()?,
            input: Some(input),
        };

        let mut note = String::new();
        let mut calls = 0;
        if let Some(scope) = scope(&self.world)? {
            let (verdict, n) = fast_path::interpret(&self.world, client, scope, &slice, input)?;
            calls += n;
            match verdict {
                fast_path::Verdict::Apply { rule, bindings } => {
                    match rules::apply(
                        &mut self.world,
                        &rule.def,
                        rules::tag(&rule),
                        scope,
                        &bindings,
                    )? {
                        Ok(app) => {
                            let response = fast_path::narrate(
                                &self.world,
                                client,
                                &slice,
                                input,
                                &rule,
                                &app,
                                on_text,
                            )?;
                            let mut outcome = self.finish(client, &response, turn)?;
                            outcome.calls += calls;
                            outcome.fast_path = Some(rule.id);
                            return Ok(outcome);
                        }
                        Err(refusal) => note = fast_path::refusal_note(&rule, &refusal),
                    }
                }
                fast_path::Verdict::Escalate(why) => note = why,
            }
        }
        let instruction = if note.is_empty() {
            format!("Player input: {input}")
        } else {
            format!("Engine note: {note}\nPlayer input: {input}")
        };
        let mut outcome = self.director_turn(client, &slice, &instruction, turn, on_text)?;
        outcome.calls += calls;
        Ok(outcome)
    }

    fn director_turn(
        &mut self,
        client: &dyn LlmClient,
        slice: &str,
        instruction: &str,
        turn: Turn,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<TurnOutcome, GameError> {
        let request = Request::new(
            Model::Sonnet,
            vec![Message::user(format!("{slice}\n{instruction}"))],
        )
        .system(DIRECTOR_SYSTEM)
        .effort(Effort::Medium)
        .tools(tools::definitions(Phase::Play));
        let outcome = self.run(client, request, Phase::Play, MAX_TURN_CALLS, on_text, None)?;
        let mut result = self.finish(client, &outcome.final_response, turn)?;
        result.calls = outcome.calls;
        Ok(result)
    }

    /// Log the final response's prose as the turn's narration, then let the
    /// quest judge rule on quests the turn touched.
    fn finish(
        &mut self,
        client: &dyn LlmClient,
        response: &crate::llm::Response,
        turn: Turn,
    ) -> Result<TurnOutcome, GameError> {
        let narration = response.text().trim().to_owned();
        self.world
            .log_event(EventKind::Narration, &json!({"text": narration}))?;
        // Every turn spends world time; the opening scene doesn't.
        if turn.input.is_some() {
            time::advance(
                &mut self.world,
                time::DEFAULT_MINUTES,
                time::Source::Default,
            )?;
        }
        let judgments =
            quest::judge_turn(&mut self.world, client, turn.start, turn.input, &narration)?;
        Ok(TurnOutcome {
            narration,
            stop_reason: response
                .stop_reason
                .clone()
                .filter(|r| *r != StopReason::EndTurn),
            calls: 1,
            judgments,
            fast_path: None,
        })
    }

    /// Run a tool loop, logging every call. A storage failure inside a tool
    /// ends the loop's usefulness, so it is surfaced after the loop.
    /// `observe`, if given, sees each call before it runs (with `None`) and
    /// after (with its output).
    fn run(
        &mut self,
        client: &dyn LlmClient,
        request: Request,
        phase: Phase,
        max_calls: usize,
        on_text: &mut dyn FnMut(&str),
        mut observe: Option<Observer<'_>>,
    ) -> Result<crate::llm::ToolLoopOutcome, GameError> {
        let world = &mut self.world;
        let inventory = &self.inventory;
        let dice_seed = self.dice_seed.unwrap_or_else(entropy);
        let mut fatal: Option<StoreError> = None;
        let outcome = run_tool_loop(client, request, max_calls, on_text, &mut |call| {
            if fatal.is_some() {
                return ToolOutput::Err("engine error; stop and narrate nothing".into());
            }
            if let Some(f) = observe.as_mut() {
                f(world, call, None);
            }
            let output = match tools::dispatch(world, inventory, dice_seed, phase, call) {
                Ok(o) => o,
                Err(e) => {
                    fatal = Some(e);
                    return ToolOutput::Err("engine error; stop and narrate nothing".into());
                }
            };
            let (result, is_error) = match &output {
                ToolOutput::Ok(s) => (s, false),
                ToolOutput::Err(s) => (s, true),
            };
            let logged = world.log_event(
                EventKind::ToolCall,
                &json!({"name": call.name, "input": call.input, "result": result, "is_error": is_error}),
            );
            if let Err(e) = logged {
                fatal = Some(e);
            }
            if let Some(f) = observe.as_mut() {
                f(world, call, Some(&output));
            }
            output
        });
        if let Some(e) = fatal {
            return Err(e.into());
        }
        Ok(outcome?)
    }

    /// Every quest, in the order spawned.
    pub fn quests(&self) -> Result<Vec<Quest>, GameError> {
        Ok(quest::quests(&self.world)?)
    }

    /// A plain description of where the player is, for resuming a save.
    pub fn look(&self) -> Result<String, GameError> {
        let Some(player) = player_id(&self.world)?
            .map(|id| self.world.entity(id))
            .transpose()?
            .flatten()
        else {
            return Err(GameError::NotStarted);
        };
        let Some(room) = player
            .location
            .map(|id| self.world.entity(id))
            .transpose()?
            .flatten()
        else {
            return Ok("You are nowhere yet.".into());
        };
        let description = room
            .props
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        Ok(format!("{}\n{description}", room.name))
    }
}

/// A fresh dice seed from the clock and the process id.
fn entropy() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    nanos ^ (u64::from(std::process::id()) << 32)
}

/// Watches tool calls in [`Game::run`].
type Observer<'a> = &'a mut dyn FnMut(&World, &ToolUse, Option<&ToolOutput>);

/// The bible's section names, comma-separated (engine props such as the
/// clock left out).
pub(crate) fn bible_sections(bible: &crate::world::Entity) -> String {
    bible
        .props
        .keys()
        .filter(|k| !k.starts_with(crate::world::ENGINE_PROP_PREFIX))
        .cloned()
        .collect::<Vec<_>>()
        .join(", ")
}

/// The bible entity, if written.
pub(crate) fn bible_id(world: &World) -> Result<Option<EntityId>, StoreError> {
    Ok(world.entities_of_kind(BIBLE_KIND)?.first().copied())
}

/// Where the player is, once they are somewhere.
pub(crate) fn scope(world: &World) -> Result<Option<Scope>, StoreError> {
    let Some(player) = player_id(world)? else {
        return Ok(None);
    };
    Ok(world
        .entity(player)?
        .and_then(|p| p.location)
        .map(|room| Scope { player, room }))
}

/// The player entity, if created.
pub(crate) fn player_id(world: &World) -> Result<Option<EntityId>, StoreError> {
    Ok(world.entities_of_kind(PLAYER_KIND)?.first().copied())
}
