//! Emergent quests: spawned by the director, judged by Haiku.
//!
//! The director plays a drama-manager role: it spawns a quest when play
//! suggests one ([`spawn`]), records the goal with the entities it concerns
//! once the goal reveals itself ([`reveal_goal`]), and may raise the stakes
//! later ([`escalate`]). It never marks a quest done. After each turn the
//! engine finds the entities the turn's patches changed
//! ([`World::changed_since`](crate::world::World::changed_since))
//! and asks the judge ([`judge_turn`]) about each active quest that concerns
//! one of them, and only those.
//!
//! A quest is a top-level world entity of kind [`QUEST_KIND`] whose state is
//! held in the engine-owned `_quest` component, with the entities it
//! concerns as engine-owned [`CONCERNS_REL`] relations (quest → entity), all
//! changed only by engine patches, so the event log (and therefore replay
//! and forking) covers quests like anything else. Destroying a concerned
//! entity cascades its relation away and leaves the quest valid; the judge
//! still sees that entity as destroyed on that turn. Every judgment is
//! logged as a `judgment` event.

mod judge;
mod tools;

#[cfg(test)]
mod tests;

pub use judge::{JUDGE_TOOL, Judgment, Verdict, judge_turn};
pub use tools::{handle_tool, tool_definitions};

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Map;

use crate::world::{
    self, ApplyError, Component, EntityId, EntityRef, Op, Patch, StoreError, World,
};

/// Kind of quest entities.
pub const QUEST_KIND: &str = "quest";

/// A quest's state: the `_quest` component.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    status: Status,
    hook: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    goal: Option<String>,
    #[serde(default)]
    developments: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolution: Option<String>,
}

impl Component for State {
    const KEY: &'static str = "_quest";
}

/// The engine-owned relation from a quest to each entity it concerns.
pub const CONCERNS_REL: &str = "_concerns";

/// Most entities one quest may concern.
pub const MAX_CONCERNS: usize = 8;

/// Where a quest stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Spawned; its goal hasn't revealed itself yet. Not judged.
    Open,
    /// Goal recorded; judged on turns that touch its entities.
    Active,
    /// The judge ruled the goal met.
    Complete,
    /// The judge ruled the goal can no longer be met.
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Active => "active",
            Status::Complete => "complete",
            Status::Failed => "failed",
        }
    }

    pub fn is_resolved(self) -> bool {
        matches!(self, Status::Complete | Status::Failed)
    }
}

/// A quest as read from the world store.
#[derive(Debug, Clone, PartialEq)]
pub struct Quest {
    pub id: EntityId,
    pub title: String,
    /// How the quest surfaced in play.
    pub hook: String,
    pub status: Status,
    /// The recorded goal, once revealed. Fixed from then on.
    pub goal: Option<String>,
    /// Entities the goal concerns, in id order; touching one triggers the
    /// judge.
    pub concerns: Vec<EntityId>,
    /// Escalations, oldest first.
    pub developments: Vec<String>,
    /// The judge's reason, once resolved.
    pub resolution: Option<String>,
}

impl Quest {
    fn new(id: EntityId, title: String, state: State, concerns: Vec<EntityId>) -> Quest {
        Quest {
            id,
            title,
            hook: state.hook,
            status: state.status,
            goal: state.goal,
            concerns,
            developments: state.developments,
            resolution: state.resolution,
        }
    }

    fn state(&self) -> State {
        State {
            status: self.status,
            hook: self.hook.clone(),
            goal: self.goal.clone(),
            developments: self.developments.clone(),
            resolution: self.resolution.clone(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum QuestError {
    /// The change is not allowed; the reason is meant for the director.
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

type Result<T, E = QuestError> = std::result::Result<T, E>;

fn refuse<T>(reason: impl Into<String>) -> Result<T> {
    Err(QuestError::Refused(reason.into()))
}

fn engine_patch(world: &mut World, ops: Vec<Op>) -> Result<crate::world::Applied> {
    match world.apply_engine_patch(&Patch::new(ops)) {
        Ok(applied) => Ok(applied),
        Err(ApplyError::Rejected(r)) => refuse(format!("patch rejected: {r}")),
        Err(ApplyError::Storage(e)) => Err(StoreError::Sqlite(e).into()),
    }
}

/// Every quest, in the order spawned.
pub fn quests(world: &World) -> Result<Vec<Quest>, StoreError> {
    world
        .query::<State>()?
        .into_iter()
        .map(|(id, state)| load(world, id, state))
        .collect()
}

/// One quest by id, if `id` is a quest.
pub fn quest(world: &World, id: EntityId) -> Result<Option<Quest>, StoreError> {
    world
        .get::<State>(id)?
        .map(|state| load(world, id, state))
        .transpose()
}

fn load(world: &World, id: EntityId, state: State) -> Result<Quest, StoreError> {
    let title = world
        .entity(id)?
        .ok_or_else(|| StoreError::Corrupt(format!("quest #{id} vanished")))?
        .name;
    let concerns = world
        .relations_from(id, Some(CONCERNS_REL))?
        .into_iter()
        .map(|r| r.object)
        .collect();
    Ok(Quest::new(id, title, state, concerns))
}

/// Whether `id` names a quest entity.
pub fn is_quest(world: &World, id: EntityId) -> Result<bool, StoreError> {
    Ok(world.entity(id)?.is_some_and(|e| e.kind == QUEST_KIND))
}

/// Check concerned entity ids: they must exist and be ordinary world
/// entities. Returns them deduplicated, in the order given.
fn check_concerns(world: &World, ids: &[EntityId]) -> Result<Vec<EntityId>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for &id in ids {
        let Some(e) = world.entity(id)? else {
            return refuse(format!("entity #{id} does not exist"));
        };
        if e.kind == QUEST_KIND || e.kind == crate::game::BIBLE_KIND {
            return refuse(format!(
                "#{id} is a {}; a quest concerns things in the world",
                e.kind
            ));
        }
        if seen.insert(id) {
            out.push(id);
        }
    }
    Ok(out)
}

/// Spawn a quest that has surfaced in play. Its goal is recorded later,
/// when it reveals itself; `concerns` may name entities already involved.
pub fn spawn(
    world: &mut World,
    title: &str,
    hook: &str,
    concerns: &[EntityId],
) -> Result<EntityId> {
    if title.trim().is_empty() {
        return refuse("title must not be empty");
    }
    if hook.trim().is_empty() {
        return refuse("hook must not be empty");
    }
    let concerns = check_concerns(world, concerns)?;
    if concerns.len() > MAX_CONCERNS {
        return refuse(format!(
            "a quest may concern at most {MAX_CONCERNS} entities"
        ));
    }
    let state = State {
        status: Status::Open,
        hook: hook.trim().to_owned(),
        goal: None,
        developments: Vec::new(),
        resolution: None,
    };
    let mut ops = vec![Op::Create {
        handle: Some("quest".into()),
        name: title.trim().to_owned(),
        kind: QUEST_KIND.into(),
        location: None,
        props: Map::from_iter([world::prop(&state)]),
    }];
    ops.extend(relate_ops(EntityRef::Handle("quest".into()), &concerns));
    let applied = engine_patch(world, ops)?;
    Ok(applied.created[0])
}

/// The quest `id`, or a refusal naming the problem.
fn existing(world: &World, id: EntityId) -> Result<Quest> {
    match quest(world, id)? {
        Some(q) => Ok(q),
        None => refuse(format!("#{id} is not a quest")),
    }
}

/// The entities in `more` a quest doesn't concern yet, checked against the
/// cap. Returns them with the total the quest would then concern.
fn new_concerns(world: &World, q: &Quest, more: &[EntityId]) -> Result<(Vec<EntityId>, usize)> {
    let added: Vec<EntityId> = check_concerns(world, more)?
        .into_iter()
        .filter(|id| !q.concerns.contains(id))
        .collect();
    let total = q.concerns.len() + added.len();
    if total > MAX_CONCERNS {
        return refuse(format!(
            "a quest may concern at most {MAX_CONCERNS} entities"
        ));
    }
    Ok((added, total))
}

/// Ops relating `quest` to each of `concerns`.
fn relate_ops(quest: EntityRef, concerns: &[EntityId]) -> impl Iterator<Item = Op> + '_ {
    concerns.iter().map(move |&id| Op::Relate {
        subject: quest.clone(),
        rel: CONCERNS_REL.into(),
        object: id.into(),
        props: Map::new(),
    })
}

/// Record a quest's goal once it has revealed itself, with the entities it
/// concerns. The goal is fixed from then on, and the quest becomes active.
pub fn reveal_goal(
    world: &mut World,
    id: EntityId,
    goal: &str,
    concerns: &[EntityId],
) -> Result<Quest> {
    let q = existing(world, id)?;
    if q.status != Status::Open {
        return refuse(format!(
            "quest #{id} is {} and its goal is recorded: {:?}. Goals never change; \
             use escalate_quest for new developments",
            q.status.as_str(),
            q.goal.as_deref().unwrap_or("")
        ));
    }
    if goal.trim().is_empty() {
        return refuse("goal must not be empty");
    }
    let (added, total) = new_concerns(world, &q, concerns)?;
    if total == 0 {
        return refuse("name at least one entity the goal concerns");
    }
    let mut state = q.state();
    state.goal = Some(goal.trim().to_owned());
    state.status = Status::Active;
    let mut ops = vec![world::set_op(id, &state)];
    ops.extend(relate_ops(id.into(), &added));
    engine_patch(world, ops)?;
    existing(world, id)
}

/// Raise the stakes on an unresolved quest: record a development and
/// optionally more concerned entities. The goal is never changed.
pub fn escalate(
    world: &mut World,
    id: EntityId,
    development: &str,
    concerns: &[EntityId],
) -> Result<Quest> {
    let q = existing(world, id)?;
    if q.status.is_resolved() {
        return refuse(format!("quest #{id} is already {}", q.status.as_str()));
    }
    if development.trim().is_empty() {
        return refuse("development must not be empty");
    }
    let (added, _) = new_concerns(world, &q, concerns)?;
    let mut state = q.state();
    state.developments.push(development.trim().to_owned());
    let mut ops = vec![world::set_op(id, &state)];
    ops.extend(relate_ops(id.into(), &added));
    engine_patch(world, ops)?;
    existing(world, id)
}

/// Mark a quest resolved; only the judge does this.
fn resolve(world: &mut World, id: EntityId, status: Status, reason: &str) -> Result<()> {
    let mut state = existing(world, id)?.state();
    state.status = status;
    state.resolution = Some(reason.to_owned());
    engine_patch(world, vec![world::set_op(id, &state)])?;
    Ok(())
}

/// How many resolved quests the director's slice still mentions.
const RECENT_RESOLVED: usize = 3;

/// Compact lines for the director's local slice: unresolved quests in full,
/// the last few resolved ones by title. Empty when there are no quests.
pub fn slice_lines(world: &World) -> Result<Vec<String>, StoreError> {
    let all = quests(world)?;
    let mut lines = Vec::new();
    for q in all.iter().filter(|q| !q.status.is_resolved()) {
        let concerns = q
            .concerns
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut line = format!("- quest #{} {} [{}]", q.id, q.title, q.status.as_str());
        match &q.goal {
            Some(goal) => line.push_str(&format!(" goal: {goal}")),
            None => line.push_str(&format!(" hook: {}", q.hook)),
        }
        if !concerns.is_empty() {
            line.push_str(&format!(" (concerns {concerns})"));
        }
        if let Some(last) = q.developments.last() {
            line.push_str(&format!(" latest: {last}"));
        }
        lines.push(line);
    }
    let resolved: Vec<&Quest> = all.iter().filter(|q| q.status.is_resolved()).collect();
    for q in resolved.iter().rev().take(RECENT_RESOLVED).rev() {
        lines.push(format!(
            "- quest #{} {} [{}]",
            q.id,
            q.title,
            q.status.as_str()
        ));
    }
    Ok(lines)
}
