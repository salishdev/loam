//! The play loop: seed prompt → world bible → director turns.
//!
//! The director (Sonnet) gets a short system prompt and a local slice of the
//! world each turn; everything else it learns and changes through tools
//! (see [`tools`]). Rooms are generated lazily the first time the player
//! enters them and persist in the world store from then on.

mod context;
mod tools;

#[cfg(test)]
mod tests;

pub use context::local_slice;
pub use tools::Phase;

use serde_json::json;

use crate::inventory::Inventory;
use crate::llm::{
    Effort, LlmClient, LlmError, Message, Model, Request, StopReason, ToolOutput, run_tool_loop,
};
use crate::world::{EntityId, EventKind, StoreError, World};

/// Kind of the single entity holding the world bible.
pub const BIBLE_KIND: &str = "bible";
/// Kind of the player entity.
pub const PLAYER_KIND: &str = "player";
/// Kind of generated rooms.
pub const ROOM_KIND: &str = "room";
/// The bible's sections, in the order the director writes them.
pub const BIBLE_SECTIONS: [&str; 5] = ["tone", "regions", "factions", "npcs", "secrets"];

/// Most model calls one director turn may make.
const MAX_TURN_CALLS: usize = 16;
/// Most model calls building the bible may make.
const MAX_BIBLE_CALLS: usize = 4;

const BIBLE_SYSTEM: &str = "You are the director of Loam, a text adventure generated from the \
player's seed prompt. Build the hidden world bible with write_bible: a tone, a handful of \
regions, factions with goals in tension, key NPCs and secrets worth uncovering. Be concrete \
and specific to the seed; keep each entry to a sentence or two. Rooms are generated later, \
as the player explores. Reply with nothing but the tool call.";

const DIRECTOR_SYSTEM: &str = "You are the director of Loam, a text adventure. The player may \
try anything. Each turn: decide what the input means in this world, make the real \
consequences happen with tools, then narrate. The engine's state is the truth: look things \
up (query_entity, read_bible, search_events) rather than inventing facts that may already \
exist, and never change the world only in prose. When the player goes through an \
unexplored exit, generate the room with create_room, furnished to fit the bible. If a tool \
refuses, adapt; an impossible action is narrated as a non-change. What anyone carries \
changes only through the inventory tools (take, drop, give, equip, unequip, transfer). Write no text until your \
tool calls are done. Your final message is shown to the player verbatim: second person, \
present tense, in the bible's tone, one to three short paragraphs.";

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
    #[error("the world has no bible yet; start it from a seed")]
    NotStarted,
}

/// What one director turn produced.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    /// The narration shown to the player.
    pub narration: String,
    /// Why the director stopped, when it wasn't a normal end of turn.
    pub stop_reason: Option<StopReason>,
    /// Model calls the turn made.
    pub calls: usize,
}

/// A game session over a world store.
pub struct Game {
    world: World,
    inventory: Inventory,
}

impl Game {
    pub fn new(world: World) -> Self {
        Game {
            world,
            inventory: Inventory::default(),
        }
    }

    pub fn world(&self) -> &World {
        &self.world
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

    /// Build the world bible from a seed prompt.
    pub fn create_bible(&mut self, client: &dyn LlmClient, seed: &str) -> Result<(), GameError> {
        self.world
            .log_event(EventKind::Input, &json!({"text": seed, "seed": true}))?;
        let request = Request::new(Model::Sonnet, vec![Message::user(seed)])
            .system(BIBLE_SYSTEM)
            .effort(Effort::Medium)
            .tools(tools::definitions(Phase::Bible));
        self.run(client, request, Phase::Bible, MAX_BIBLE_CALLS, &mut |_| {})?;
        if !self.has_bible()? {
            return Err(GameError::NoBible);
        }
        Ok(())
    }

    /// Generate the opening room and narrate the opening scene.
    pub fn begin(
        &mut self,
        client: &dyn LlmClient,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<TurnOutcome, GameError> {
        self.director_turn(client, OPENING, None, on_text)
    }

    /// Play one turn: the director interprets `input`, patches the world
    /// through tools and narrates, streaming the narration to `on_text`.
    pub fn turn(
        &mut self,
        client: &dyn LlmClient,
        input: &str,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<TurnOutcome, GameError> {
        let instruction = format!("Player input: {input}");
        self.director_turn(client, &instruction, Some(input), on_text)
    }

    fn director_turn(
        &mut self,
        client: &dyn LlmClient,
        instruction: &str,
        input: Option<&str>,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<TurnOutcome, GameError> {
        if !self.has_bible()? {
            return Err(GameError::NotStarted);
        }
        // The slice is taken before the input is logged, so "Recent" shows
        // earlier turns only.
        let slice = local_slice(&self.world)?;
        if let Some(input) = input {
            self.world
                .log_event(EventKind::Input, &json!({"text": input}))?;
        }
        let request = Request::new(
            Model::Sonnet,
            vec![Message::user(format!("{slice}\n{instruction}"))],
        )
        .system(DIRECTOR_SYSTEM)
        .effort(Effort::Medium)
        .tools(tools::definitions(Phase::Play));
        let outcome = self.run(client, request, Phase::Play, MAX_TURN_CALLS, on_text)?;
        let narration = outcome.final_response.text().trim().to_owned();
        self.world
            .log_event(EventKind::Narration, &json!({"text": narration}))?;
        let stop_reason = outcome
            .final_response
            .stop_reason
            .filter(|r| *r != StopReason::EndTurn);
        Ok(TurnOutcome {
            narration,
            stop_reason,
            calls: outcome.calls,
        })
    }

    /// Run a tool loop, logging every call. A storage failure inside a tool
    /// ends the loop's usefulness, so it is surfaced after the loop.
    fn run(
        &mut self,
        client: &dyn LlmClient,
        request: Request,
        phase: Phase,
        max_calls: usize,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<crate::llm::ToolLoopOutcome, GameError> {
        let world = &mut self.world;
        let inventory = &self.inventory;
        let mut fatal: Option<StoreError> = None;
        let outcome = run_tool_loop(client, request, max_calls, on_text, &mut |call| {
            if fatal.is_some() {
                return ToolOutput::Err("engine error; stop and narrate nothing".into());
            }
            let output = match tools::dispatch(world, inventory, phase, call) {
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
            output
        });
        if let Some(e) = fatal {
            return Err(e.into());
        }
        Ok(outcome?)
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

/// The bible entity, if written.
pub(crate) fn bible_id(world: &World) -> Result<Option<EntityId>, StoreError> {
    Ok(world.entities_of_kind(BIBLE_KIND)?.first().copied())
}

/// The player entity, if created.
pub(crate) fn player_id(world: &World) -> Result<Option<EntityId>, StoreError> {
    Ok(world.entities_of_kind(PLAYER_KIND)?.first().copied())
}
