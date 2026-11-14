//! Declarative rules: the director's rulings, kept so the engine can replay
//! them without asking again.
//!
//! A rule is a trigger (in words, for the interpreter), named roles the
//! interpreter binds to entities, conditions on those entities, and effects
//! written as ordinary patch ops in which a string handle naming a role
//! stands for the bound entity. The director picks the generality: a rule
//! conditioned on `kind` and props covers every matching entity; one pinned
//! with an `entity` condition covers only that one. `related` and
//! `not_related` conditions hold while a named relation between two roles
//! does (or doesn't), and `relate`/`unrelate` effects change them.
//!
//! A pin's entity id is not kept in the rule's JSON: it lives in an
//! engine-owned [`PINS_REL`] relation (rule → entity, props `{role}`), and
//! [`get`] and [`all`] put it back into the condition. An entity an effect
//! names by literal id is pinned the same way (props `{effect: true}`; the
//! id stays in the effect). Destroying a pinned entity cascades the
//! relation away, and the world store then retires the
//! rule in the same transaction ([`retire_unpinned`]), so a pinned rule never
//! silently becomes a general one.
//!
//! Each rule is an engine-owned entity of kind [`RULE_KIND`] holding its
//! definition in the private `_rule` component, written by engine patches; every mint,
//! amendment and retirement bumps its version and logs a `rule_change`
//! event. Replay and forking need no extra code.

pub mod tools;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::glossary;
use crate::stats::{self, Changes, CheckResult, Roll, StatHook};
use crate::world;
use crate::world::{
    Applied, ApplyError, Component, ENGINE_PROP_PREFIX, Entity, EntityId, EntityRef, EventKind, Op,
    Patch, StoreError, World,
};

/// Kind of rule entities.
pub const RULE_KIND: &str = "rule";
/// Role always bound to the player.
pub const PLAYER_ROLE: &str = "player";
/// Role always bound to the player's current room.
pub const ROOM_ROLE: &str = "room";
/// The engine-owned relation from a rule to each entity it is pinned to,
/// with props `{"role": <role name>}`.
pub use crate::world::PINS_REL;
/// Why the engine retires a rule whose pinned entity was destroyed.
pub const UNPINNED_REASON: &str = "pinned entity destroyed";

/// A rule is identified by its entity id.
pub type RuleId = EntityId;

/// Role name → bound entity.
pub type Bindings = BTreeMap<String, EntityId>;

/// What a rule governs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleClass {
    /// A world mechanic the director minted; amendable and retirable.
    World,
    /// A stat rule (bounds, thresholds, capacity, advancement): fixed once
    /// created.
    Stat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleStatus {
    Active,
    Retired,
}

/// One condition on a bound role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "check", rename_all = "snake_case")]
pub enum Condition {
    /// The entity's kind is `is`.
    Kind { role: String, is: String },
    /// The prop is present (and equals `is`, when given).
    Prop {
        role: String,
        prop: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is: Option<Value>,
    },
    /// The prop is absent (or, when `is` is given, doesn't equal it).
    NotProp {
        role: String,
        prop: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is: Option<Value>,
    },
    /// The entity is carried by the player.
    Carried { role: String },
    /// The role is this one entity: pins the rule to it. The id is held in
    /// a [`PINS_REL`] relation, not in the stored rule; it is `None` only
    /// once the pinned entity is gone (and the rule retired).
    Entity {
        role: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EntityId>,
    },
    /// A relation named `rel` runs from the role's entity to `other`'s.
    Related {
        role: String,
        rel: String,
        other: String,
    },
    /// No relation named `rel` runs from the role's entity to `other`'s.
    NotRelated {
        role: String,
        rel: String,
        other: String,
    },
}

impl Condition {
    pub fn role(&self) -> &str {
        match self {
            Condition::Kind { role, .. }
            | Condition::Prop { role, .. }
            | Condition::NotProp { role, .. }
            | Condition::Carried { role }
            | Condition::Entity { role, .. }
            | Condition::Related { role, .. }
            | Condition::NotRelated { role, .. } => role,
        }
    }

    /// The other role a relation condition names.
    pub fn other(&self) -> Option<&str> {
        match self {
            Condition::Related { other, .. } | Condition::NotRelated { other, .. } => Some(other),
            _ => None,
        }
    }
}

/// What the director writes when minting a rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleDef {
    /// Short label, e.g. "Kicking a rotten door".
    pub name: String,
    /// When the rule applies, in words: what the player does to what.
    pub trigger: String,
    /// Roles the interpreter binds, name → description. `player` and `room`
    /// are always bound and need no declaring.
    #[serde(default)]
    pub roles: BTreeMap<String, String>,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    /// Patch ops applied in order. A string handle naming a role stands for
    /// its entity; other handles name entities created earlier in the list.
    #[serde(default)]
    pub effects: Vec<Op>,
    /// What happens, for the narrator.
    #[serde(default)]
    pub outcome: String,
    /// Stat changes applied after the effects. A world rule may only lower
    /// stats: they grow through their advancement rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adjust: Vec<StatDelta>,
    /// A dice check rolled once the conditions hold. On success the
    /// effects, `adjust` and `outcome` above apply; on a partial or a
    /// failure, that branch's do instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<RuleCheck>,
}

/// A change to one stat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatDelta {
    pub stat: String,
    pub by: i64,
}

/// A check a rule embeds: `check(stat, difficulty)` decides which
/// consequence applies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleCheck {
    pub stat: String,
    pub difficulty: i64,
    /// What a partial success does; nothing when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<Branch>,
    /// What a failure does; nothing when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Branch>,
}

/// The consequence of one check result other than success.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Branch {
    #[serde(default)]
    pub effects: Vec<Op>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adjust: Vec<StatDelta>,
    #[serde(default)]
    pub outcome: String,
}

/// A stored rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(skip)]
    pub id: RuleId,
    #[serde(flatten)]
    pub def: RuleDef,
    pub class: RuleClass,
    /// What a stat rule governs; `None` for world rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook: Option<StatHook>,
    pub version: u32,
    pub status: RuleStatus,
}

impl Rule {
    pub fn is_active(&self) -> bool {
        self.status == RuleStatus::Active
    }

    /// One line for prompts and listings.
    pub fn summary(&self) -> Value {
        let mut v = json!({
            "id": self.id,
            "name": self.def.name,
            "trigger": self.def.trigger,
            "roles": self.def.roles,
            "conditions": self.def.conditions,
            "class": self.class,
            "version": self.version,
        });
        if let Some(hook) = &self.hook {
            v["stat"] = json!(hook);
        }
        if let Some(check) = &self.def.check {
            v["check"] = json!({"stat": check.stat, "difficulty": check.difficulty});
        }
        v
    }
}

/// Where the player is: what the built-in roles bind to and what other
/// roles may bind to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    pub player: EntityId,
    pub room: EntityId,
}

/// Check a definition's shape. Returns the reason it's invalid.
pub fn validate(def: &RuleDef) -> Result<(), String> {
    if def.name.trim().is_empty() || def.trigger.trim().is_empty() {
        return Err("a rule needs a name and a trigger".into());
    }
    for role in def.roles.keys() {
        if role == PLAYER_ROLE || role == ROOM_ROLE {
            return Err(format!("role {role:?} is built in; don't declare it"));
        }
        if role.trim().is_empty() {
            return Err("role names must not be empty".into());
        }
    }
    let known = |r: &str| r == PLAYER_ROLE || r == ROOM_ROLE || def.roles.contains_key(r);
    for c in &def.conditions {
        if !known(c.role()) {
            return Err(format!("condition on undeclared role {:?}", c.role()));
        }
        if let Condition::Prop { prop, .. } | Condition::NotProp { prop, .. } = c
            && prop.starts_with(ENGINE_PROP_PREFIX)
        {
            return Err(format!("prop {prop:?} is engine-owned"));
        }
        if let Condition::Related { rel, .. } | Condition::NotRelated { rel, .. } = c {
            if rel.trim().is_empty() {
                return Err("a relation condition needs a rel".into());
            }
            if rel.starts_with(ENGINE_PROP_PREFIX) {
                return Err(format!("relation {rel:?} is engine-owned"));
            }
        }
        if let Some(other) = c.other()
            && !known(other)
        {
            return Err(format!("condition names undeclared role {other:?}"));
        }
    }
    pins(def)?;
    validate_effects(&def.effects, &known, "effect")?;
    validate_deltas(&def.adjust)?;
    if let Some(check) = &def.check {
        if check.stat.trim().is_empty() {
            return Err("a check needs a stat".into());
        }
        for (label, branch) in [("partial", &check.partial), ("failure", &check.failure)] {
            if let Some(b) = branch {
                validate_effects(&b.effects, &known, &format!("{label} effect"))?;
                validate_deltas(&b.adjust)?;
            }
        }
    }
    Ok(())
}

/// The rule's pins, role → entity, from its `entity` conditions. Refuses an
/// `entity` condition without an id, a role pinned to two entities and an
/// entity pinned under two roles.
fn pins(def: &RuleDef) -> Result<BTreeMap<&str, EntityId>, String> {
    let mut pins: BTreeMap<&str, EntityId> = BTreeMap::new();
    for c in &def.conditions {
        let Condition::Entity { role, id } = c else {
            continue;
        };
        let Some(id) = *id else {
            return Err(format!("the entity condition on {role:?} needs an id"));
        };
        if let Some(prev) = pins.insert(role, id)
            && prev != id
        {
            return Err(format!(
                "{role:?} can't be pinned to both #{prev} and #{id}"
            ));
        }
        if let Some((other, _)) = pins.iter().find(|(r, i)| **i == id && **r != role) {
            return Err(format!(
                "#{id} is pinned under both {other:?} and {role:?}; pin it under one role"
            ));
        }
    }
    Ok(pins)
}

/// The rule's definition as stored: pinned ids stripped (they live in
/// relations).
fn unpinned(rule: &Rule) -> Rule {
    let mut rule = rule.clone();
    for c in &mut rule.def.conditions {
        if let Condition::Entity { id, .. } = c {
            *id = None;
        }
    }
    rule
}

/// Every entity the rule's effects name by literal id (main effects and
/// check branches), in id order.
fn effect_ids(def: &RuleDef) -> BTreeSet<EntityId> {
    let branches = def.check.iter().flat_map(|c| [&c.partial, &c.failure]);
    std::iter::once(&def.effects)
        .chain(branches.flatten().map(|b| &b.effects))
        .flatten()
        .flat_map(op_refs)
        .filter_map(|r| match r {
            EntityRef::Id(id) => Some(*id),
            EntityRef::Handle(_) => None,
        })
        .collect()
}

/// Ops relating `rule` to each entity it is pinned to: by an `entity`
/// condition (props `{role}`) or by a literal id in an effect (props
/// `{effect: true}`), so destroying any of them retires the rule rather
/// than leaving an effect that names a missing entity.
fn pin_ops(rule: EntityRef, def: &RuleDef) -> Result<Vec<Op>, String> {
    let pins = pins(def)?;
    let pinned: HashSet<EntityId> = pins.values().copied().collect();
    let relate = |id: EntityId, props: Map<String, Value>| Op::Relate {
        subject: rule.clone(),
        rel: PINS_REL.into(),
        object: id.into(),
        props,
    };
    let mut ops: Vec<Op> = pins
        .into_iter()
        .map(|(role, id)| relate(id, Map::from_iter([("role".to_owned(), json!(role))])))
        .collect();
    ops.extend(
        effect_ids(def)
            .into_iter()
            .filter(|id| !pinned.contains(id))
            .map(|id| relate(id, Map::from_iter([("effect".to_owned(), json!(true))]))),
    );
    Ok(ops)
}

/// Check that every entity the rule pins, or its effects name, exists.
fn check_pinned(world: &World, def: &RuleDef) -> Result<Result<(), String>, StoreError> {
    for (role, id) in pins(def).unwrap_or_default() {
        if world.entity(id)?.is_none() {
            return Ok(Err(format!(
                "{role} is pinned to #{id}, which does not exist"
            )));
        }
    }
    for id in effect_ids(def) {
        if world.entity(id)?.is_none() {
            return Ok(Err(format!("an effect names #{id}, which does not exist")));
        }
    }
    Ok(Ok(()))
}

fn validate_deltas(deltas: &[StatDelta]) -> Result<(), String> {
    for d in deltas {
        if d.stat.trim().is_empty() {
            return Err("a stat change needs a stat".into());
        }
        if d.by == 0 {
            return Err(format!("a change to {:?} by 0 does nothing", d.stat));
        }
    }
    Ok(())
}

fn validate_effects(
    effects: &[Op],
    known: &dyn Fn(&str) -> bool,
    what: &str,
) -> Result<(), String> {
    let mut created: HashSet<&str> = HashSet::new();
    for (i, op) in effects.iter().enumerate() {
        for r in op_refs(op) {
            if let EntityRef::Handle(h) = r
                && !known(h)
                && !created.contains(h.as_str())
            {
                return Err(format!(
                    "{what} {i}: {r} is neither a role nor created by an earlier effect"
                ));
            }
        }
        let keys: Vec<&String> = match op {
            Op::Create { props, .. } => props.keys().collect(),
            Op::SetProp { key, .. } | Op::RemoveProp { key, .. } => vec![key],
            _ => vec![],
        };
        if let Op::Relate { rel, .. } | Op::Unrelate { rel, .. } = op
            && rel.starts_with(ENGINE_PROP_PREFIX)
        {
            return Err(format!("{what} {i}: relation {rel:?} is engine-owned"));
        }
        if let Some(k) = keys.iter().find(|k| k.starts_with(ENGINE_PROP_PREFIX)) {
            return Err(format!("{what} {i}: prop {k:?} is engine-owned"));
        }
        if let Op::Create {
            handle: Some(h), ..
        } = op
        {
            if known(h) {
                return Err(format!("{what} {i}: handle {h:?} clashes with a role"));
            }
            created.insert(h);
        }
    }
    Ok(())
}

/// Check what only a world rule must satisfy, against the stat sheet:
/// every stat it names exists, and it only lowers stats.
pub(crate) fn validate_world(
    world: &World,
    def: &RuleDef,
) -> Result<Result<(), String>, StoreError> {
    if let Err(r) = check_pinned(world, def)? {
        return Ok(Err(r));
    }
    let sheet = stats::sheet(world)?;
    let mut deltas: Vec<&StatDelta> = def.adjust.iter().collect();
    if let Some(check) = &def.check {
        if !sheet.contains_key(&check.stat) {
            return Ok(Err(format!(
                "the check names unknown stat {:?}",
                check.stat
            )));
        }
        for b in [&check.partial, &check.failure].into_iter().flatten() {
            deltas.extend(&b.adjust);
        }
    }
    for d in deltas {
        if !sheet.contains_key(&d.stat) {
            return Ok(Err(format!("unknown stat {:?}", d.stat)));
        }
        if d.by > 0 {
            return Ok(Err(format!(
                "a world rule may only lower stats ({:?} by {}); stats grow through their \
                 advancement rules with adjust_stat",
                d.stat, d.by
            )));
        }
    }
    Ok(Ok(()))
}

/// Every entity ref an op uses.
fn op_refs(op: &Op) -> Vec<&EntityRef> {
    match op {
        Op::Create { location, .. } => location.iter().collect(),
        Op::Move { entity, to } => std::iter::once(entity).chain(to).collect(),
        Op::SetProp { entity, .. }
        | Op::RemoveProp { entity, .. }
        | Op::Rename { entity, .. }
        | Op::Destroy { entity } => vec![entity],
        Op::Relate {
            subject, object, ..
        }
        | Op::Unrelate {
            subject, object, ..
        } => vec![subject, object],
    }
}

fn op_refs_mut(op: &mut Op) -> Vec<&mut EntityRef> {
    match op {
        Op::Create { location, .. } => location.iter_mut().collect(),
        Op::Move { entity, to } => std::iter::once(entity).chain(to).collect(),
        Op::SetProp { entity, .. }
        | Op::RemoveProp { entity, .. }
        | Op::Rename { entity, .. }
        | Op::Destroy { entity } => vec![entity],
        Op::Relate {
            subject, object, ..
        }
        | Op::Unrelate {
            subject, object, ..
        } => vec![subject, object],
    }
}

/// Complete the interpreter's bindings with the built-in roles and check
/// that every declared role is bound to an entity in scope. Returns the
/// reason when they don't fit.
pub fn bind(
    world: &World,
    rule: &RuleDef,
    scope: Scope,
    given: &Bindings,
) -> Result<Result<Bindings, String>, StoreError> {
    let mut bindings = Bindings::new();
    for (role, id) in given {
        if role == PLAYER_ROLE || role == ROOM_ROLE {
            continue;
        }
        if !rule.roles.contains_key(role) {
            return Ok(Err(format!("the rule has no role {role:?}")));
        }
        if !in_scope(world, scope, *id)? {
            return Ok(Err(format!(
                "{role} = #{id} is not here (only the room, what is in it and what the player carries)"
            )));
        }
        bindings.insert(role.clone(), *id);
    }
    if let Some(role) = rule.roles.keys().find(|r| !bindings.contains_key(*r)) {
        return Ok(Err(format!("role {role:?} is not bound")));
    }
    bindings.insert(PLAYER_ROLE.into(), scope.player);
    bindings.insert(ROOM_ROLE.into(), scope.room);
    Ok(Ok(bindings))
}

/// Whether `id` is the room, the player, or inside either.
fn in_scope(world: &World, scope: Scope, id: EntityId) -> Result<bool, StoreError> {
    let mut seen = HashSet::new();
    let mut cur = Some(id);
    while let Some(c) = cur {
        if c == scope.room || c == scope.player {
            return Ok(true);
        }
        if !seen.insert(c) {
            break;
        }
        cur = match world.entity(c)? {
            Some(e) => e.location,
            None => return Ok(false),
        };
    }
    Ok(false)
}

/// The conditions that don't hold, as reasons; empty when the rule applies.
pub fn unmet(
    world: &World,
    rule: &RuleDef,
    scope: Scope,
    bindings: &Bindings,
) -> Result<Vec<String>, StoreError> {
    let mut entities: BTreeMap<&str, Entity> = BTreeMap::new();
    for (role, id) in bindings {
        if let Some(e) = world.entity(*id)? {
            entities.insert(role, e);
        }
    }
    let mut reasons = Vec::new();
    for c in &rule.conditions {
        let role = c.role();
        let Some(e) = entities.get(role) else {
            reasons.push(format!("{role} is not bound"));
            continue;
        };
        let shown = |v: Option<&Value>| v.map_or("absent".to_owned(), Value::to_string);
        match c {
            Condition::Kind { is, .. } => {
                if &e.kind != is {
                    reasons.push(format!("{role} is a {:?}, not a {is:?}", e.kind));
                }
            }
            Condition::Prop { prop, is, .. } => {
                let got = e.props.get(prop);
                let ok = match is {
                    Some(want) => got == Some(want),
                    None => got.is_some(),
                };
                if !ok {
                    let want = is.as_ref().map_or("present".to_owned(), Value::to_string);
                    reasons.push(format!("{role}.{prop} should be {want}, is {}", shown(got)));
                }
            }
            Condition::NotProp { prop, is, .. } => {
                let got = e.props.get(prop);
                let ok = match is {
                    Some(not) => got != Some(not),
                    None => got.is_none(),
                };
                if !ok {
                    let not = is.as_ref().map_or("present".to_owned(), Value::to_string);
                    reasons.push(format!(
                        "{role}.{prop} should not be {not}, is {}",
                        shown(got)
                    ));
                }
            }
            Condition::Carried { .. } => {
                if e.id == scope.player || !carried_by(world, scope.player, e.id)? {
                    reasons.push(format!("{role} is not carried"));
                }
            }
            Condition::Entity { id: Some(id), .. } => {
                if e.id != *id {
                    reasons.push(format!("{role} is #{}, not #{id}", e.id));
                }
            }
            Condition::Entity { id: None, .. } => {
                reasons.push(format!("{role}'s pinned entity no longer exists"));
            }
            Condition::Related { rel, other, .. } | Condition::NotRelated { rel, other, .. } => {
                let Some(o) = entities.get(other.as_str()) else {
                    reasons.push(format!("{other} is not bound"));
                    continue;
                };
                let holds = world
                    .relations_from(e.id, Some(rel))?
                    .iter()
                    .any(|r| r.object == o.id);
                match (c, holds) {
                    (Condition::Related { .. }, false) => {
                        reasons.push(format!("no {rel} relation runs from {role} to {other}"))
                    }
                    (Condition::NotRelated { .. }, true) => {
                        reasons.push(format!("a {rel} relation runs from {role} to {other}"))
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(reasons)
}

fn carried_by(world: &World, player: EntityId, id: EntityId) -> Result<bool, StoreError> {
    let mut seen = HashSet::new();
    let mut cur = world.entity(id)?.and_then(|e| e.location);
    while let Some(c) = cur {
        if c == player {
            return Ok(true);
        }
        if !seen.insert(c) {
            break;
        }
        cur = world.entity(c)?.and_then(|e| e.location);
    }
    Ok(false)
}

/// The patch a rule's effects make for these bindings.
pub fn instantiate(rule: &RuleDef, bindings: &Bindings) -> Patch {
    instantiate_ops(&rule.effects, bindings)
}

fn instantiate_ops(effects: &[Op], bindings: &Bindings) -> Patch {
    let mut ops = effects.to_vec();
    for op in &mut ops {
        for r in op_refs_mut(op) {
            if let EntityRef::Handle(h) = r
                && let Some(id) = bindings.get(h.as_str())
            {
                *r = EntityRef::Id(*id);
            }
        }
    }
    Patch::new(ops)
}

/// A rule application the engine refused.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// The bindings don't fit the rule.
    Binding(String),
    /// Conditions that don't hold.
    Unmet(Vec<String>),
    /// The rule's check couldn't be rolled.
    Check(String),
    /// The world store rejected the effects.
    Rejected(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Binding(r) => write!(f, "bindings don't fit: {r}"),
            Refusal::Unmet(rs) => write!(f, "conditions not met: {}", rs.join("; ")),
            Refusal::Check(r) => write!(f, "check not rolled: {r}"),
            Refusal::Rejected(r) => write!(f, "effects rejected: {r}"),
        }
    }
}

/// What applying a rule did.
#[derive(Debug, Clone, PartialEq)]
pub struct Application {
    pub bindings: Bindings,
    pub patch: Patch,
    pub applied: Applied,
    /// Glossary entries the effects introduced.
    pub new_terms: Vec<String>,
    /// The rule's check, when it has one.
    pub roll: Option<Roll>,
    /// What happened, for the narrator: the rule's outcome, or the check
    /// branch's.
    pub outcome: String,
    /// Stat changes the rule made, and thresholds they fired.
    pub stats: Changes,
}

/// Bind, check and apply a rule (stored or about to be minted) with
/// director authority. The `patch` event names the rule.
pub fn apply(
    world: &mut World,
    rule: &RuleDef,
    tag: Value,
    scope: Scope,
    given: &Bindings,
) -> Result<Result<Application, Refusal>, StoreError> {
    let bindings = match bind(world, rule, scope, given)? {
        Ok(b) => b,
        Err(r) => return Ok(Err(Refusal::Binding(r))),
    };
    let reasons = unmet(world, rule, scope, &bindings)?;
    if !reasons.is_empty() {
        return Ok(Err(Refusal::Unmet(reasons)));
    }
    resolve(world, rule, tag, bindings, 0)
}

/// Roll the rule's check, if any, then apply the consequence it picks:
/// effects (with director authority), then stat changes. `depth` counts
/// thresholds firing thresholds.
pub(crate) fn resolve(
    world: &mut World,
    rule: &RuleDef,
    tag: Value,
    bindings: Bindings,
    depth: usize,
) -> Result<Result<Application, Refusal>, StoreError> {
    let main = Branch {
        effects: rule.effects.clone(),
        adjust: rule.adjust.clone(),
        outcome: rule.outcome.clone(),
    };
    let (roll, branch) = match &rule.check {
        None => (None, main),
        Some(c) => {
            let roll = match stats::check(world, &c.stat, c.difficulty, json!({"rule": tag}))? {
                Ok(r) => r,
                Err(why) => return Ok(Err(Refusal::Check(why))),
            };
            let branch = match roll.result {
                CheckResult::Success => main,
                CheckResult::Partial => c.partial.clone().unwrap_or_default(),
                CheckResult::Failure => c.failure.clone().unwrap_or_default(),
            };
            (Some(roll), branch)
        }
    };
    let patch = instantiate_ops(&branch.effects, &bindings);
    let mut note = Map::from_iter([
        ("rule".to_owned(), tag.clone()),
        ("bindings".to_owned(), json!(bindings)),
    ]);
    if let Some(r) = &roll {
        note.insert("roll".into(), json!(r.seq));
    }
    let applied = match world.apply_patch_noted(&patch, &note) {
        Ok(a) => a,
        Err(ApplyError::Rejected(r)) => return Ok(Err(Refusal::Rejected(r.to_string()))),
        Err(ApplyError::Storage(e)) => return Err(e.into()),
    };
    let new_terms = glossary::register_patch(world, &patch)?;
    let mut changes = Changes::default();
    for d in &branch.adjust {
        stats::apply_delta(
            world,
            &d.stat,
            d.by,
            json!({"rule": tag}),
            depth,
            &mut changes,
        )?;
    }
    Ok(Ok(Application {
        bindings,
        patch,
        applied,
        new_terms,
        roll,
        outcome: branch.outcome,
        stats: changes,
    }))
}

/// The `rule` tag recorded on patches a stored rule makes.
pub fn tag(rule: &Rule) -> Value {
    json!({"id": rule.id, "version": rule.version})
}

/// A rule's stored definition and status: the `_rule` component.
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
struct Stored(Rule);

impl Component for Stored {
    const KEY: &'static str = "_rule";
}

impl Stored {
    fn into_rule(self, id: RuleId) -> Rule {
        Rule { id, ..self.0 }
    }
}

/// Put a stored rule's pinned ids back into its `entity` conditions.
fn load(world: &World, id: RuleId, stored: Stored) -> Result<Rule, StoreError> {
    let mut rule = stored.into_rule(id);
    let pins = world.relations_from(id, Some(PINS_REL))?;
    for c in &mut rule.def.conditions {
        if let Condition::Entity { role, id } = c {
            *id = pins
                .iter()
                .find(|r| r.props.get("role").and_then(Value::as_str) == Some(role.as_str()))
                .map(|r| r.object);
        }
    }
    Ok(rule)
}

/// Look up a rule.
pub fn get(world: &World, id: RuleId) -> Result<Option<Rule>, StoreError> {
    world
        .get::<Stored>(id)?
        .map(|s| load(world, id, s))
        .transpose()
}

/// Every rule, in id order.
pub fn all(world: &World) -> Result<Vec<Rule>, StoreError> {
    world
        .query::<Stored>()?
        .into_iter()
        .map(|(id, s)| load(world, id, s))
        .collect()
}

/// The retirement of rule `id`, whose pin to `destroyed` a destroy just
/// cascaded away: the op storing it retired and the `rule_change` payload
/// to log, or `None` if it isn't an active world rule. The world store
/// calls this inside the destroy's transaction, reading through `conn`.
pub(crate) fn retire_unpinned(
    conn: &rusqlite::Connection,
    id: RuleId,
    destroyed: EntityId,
) -> Result<Option<(Op, Value)>, StoreError> {
    let Some(Stored(mut rule)) = world::get_in::<Stored>(conn, id)? else {
        return Ok(None);
    };
    rule.id = id;
    if !rule.is_active() || rule.class != RuleClass::World {
        return Ok(None);
    }
    rule.version += 1;
    rule.status = RuleStatus::Retired;
    let mut payload = change_payload("retired", &rule, UNPINNED_REASON);
    payload["entity"] = json!(destroyed);
    Ok(Some((world::set_op(id, &Stored(unpinned(&rule))), payload)))
}

/// Active rules, in id order.
pub fn active(world: &World) -> Result<Vec<Rule>, StoreError> {
    Ok(all(world)?.into_iter().filter(Rule::is_active).collect())
}

/// Active world rules (what player actions can match), in id order.
pub fn active_world(world: &World) -> Result<Vec<Rule>, StoreError> {
    Ok(active(world)?
        .into_iter()
        .filter(|r| r.class == RuleClass::World)
        .collect())
}

/// Store a new world rule (version 1) and log its minting. Registers the
/// prop names it uses in the glossary. Returns the rule and the new
/// glossary entries, or why the definition is invalid.
pub fn mint(
    world: &mut World,
    def: RuleDef,
    reason: &str,
) -> Result<Result<(Rule, Vec<String>), String>, StoreError> {
    if let Err(r) = validate(&def) {
        return Ok(Err(r));
    }
    if let Err(r) = validate_world(world, &def)? {
        return Ok(Err(r));
    }
    store_new(world, def, RuleClass::World, None, reason).map(Ok)
}

/// Store a stat rule. The caller ([`crate::stats`]) has validated it.
/// Stat rules are fixed from here on.
pub(crate) fn mint_stat(
    world: &mut World,
    def: RuleDef,
    hook: StatHook,
    reason: &str,
) -> Result<Rule, StoreError> {
    Ok(store_new(world, def, RuleClass::Stat, Some(hook), reason)?.0)
}

fn store_new(
    world: &mut World,
    def: RuleDef,
    class: RuleClass,
    hook: Option<StatHook>,
    reason: &str,
) -> Result<(Rule, Vec<String>), StoreError> {
    let mut rule = Rule {
        id: 0,
        def,
        class,
        hook,
        version: 1,
        status: RuleStatus::Active,
    };
    let handle = EntityRef::Handle("rule".into());
    let mut ops = vec![Op::Create {
        handle: Some("rule".into()),
        name: rule.def.name.clone(),
        kind: RULE_KIND.into(),
        location: None,
        props: Map::from_iter([world::prop(&Stored(unpinned(&rule)))]),
    }];
    ops.extend(pin_ops(handle, &rule.def).map_err(StoreError::Corrupt)?);
    rule.id = engine_patch(world, &Patch::new(ops))?.created[0];
    log_change(world, "minted", &rule, reason)?;
    let new_terms = register_vocabulary(world, &rule.def)?;
    Ok((rule, new_terms))
}

/// Changes to a world rule; absent fields stay as they are.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Amendment {
    pub name: Option<String>,
    pub trigger: Option<String>,
    pub roles: Option<BTreeMap<String, String>>,
    pub conditions: Option<Vec<Condition>>,
    pub effects: Option<Vec<Op>>,
    pub outcome: Option<String>,
    pub adjust: Option<Vec<StatDelta>>,
    pub check: Option<RuleCheck>,
}

/// Amend an active world rule: a new version replaces the old one, which
/// stays in the log. Stat rules can't be amended.
pub fn amend(
    world: &mut World,
    id: RuleId,
    changes: Amendment,
    reason: &str,
) -> Result<Result<(Rule, Vec<String>), String>, StoreError> {
    let mut rule = match changeable(world, id)? {
        Ok(r) => r,
        Err(why) => return Ok(Err(why)),
    };
    let d = &mut rule.def;
    if let Some(v) = changes.name {
        d.name = v;
    }
    if let Some(v) = changes.trigger {
        d.trigger = v;
    }
    if let Some(v) = changes.roles {
        d.roles = v;
    }
    if let Some(v) = changes.conditions {
        d.conditions = v;
    }
    if let Some(v) = changes.effects {
        d.effects = v;
    }
    if let Some(v) = changes.outcome {
        d.outcome = v;
    }
    if let Some(v) = changes.adjust {
        d.adjust = v;
    }
    if let Some(v) = changes.check {
        d.check = Some(v);
    }
    if let Err(r) = validate(&rule.def) {
        return Ok(Err(r));
    }
    if let Err(r) = validate_world(world, &rule.def)? {
        return Ok(Err(r));
    }
    rule.version += 1;
    store(world, &rule, true)?;
    log_change(world, "amended", &rule, reason)?;
    let new_terms = register_vocabulary(world, &rule.def)?;
    Ok(Ok((rule, new_terms)))
}

/// Retire an active world rule: it stops matching but stays listed. Stat
/// rules can't be retired.
pub fn retire(
    world: &mut World,
    id: RuleId,
    reason: &str,
) -> Result<Result<Rule, String>, StoreError> {
    let mut rule = match changeable(world, id)? {
        Ok(r) => r,
        Err(why) => return Ok(Err(why)),
    };
    rule.version += 1;
    rule.status = RuleStatus::Retired;
    store(world, &rule, false)?;
    log_change(world, "retired", &rule, reason)?;
    Ok(Ok(rule))
}

fn changeable(world: &World, id: RuleId) -> Result<Result<Rule, String>, StoreError> {
    Ok(match get(world, id)? {
        None => Err(format!("there is no rule #{id}")),
        Some(r) if r.class == RuleClass::Stat => Err(format!(
            "rule #{id} is a stat rule; stat rules never change"
        )),
        Some(r) if !r.is_active() => Err(format!("rule #{id} is retired")),
        Some(r) => Ok(r),
    })
}

fn to_json(rule: &Rule) -> Value {
    serde_json::to_value(rule).expect("rule serializes")
}

/// Write a changed rule. With `repin`, its pin relations are rewritten
/// from its `entity` conditions in the same patch.
fn store(world: &mut World, rule: &Rule, repin: bool) -> Result<(), StoreError> {
    let mut ops = vec![
        Op::Rename {
            entity: rule.id.into(),
            name: rule.def.name.clone(),
        },
        world::set_op(rule.id, &Stored(unpinned(rule))),
    ];
    if repin {
        for r in world.relations_from(rule.id, Some(PINS_REL))? {
            ops.push(Op::Unrelate {
                subject: rule.id.into(),
                rel: PINS_REL.into(),
                object: r.object.into(),
            });
        }
        ops.extend(pin_ops(rule.id.into(), &rule.def).map_err(StoreError::Corrupt)?);
    }
    engine_patch(world, &Patch::new(ops)).map(|_| ())
}

fn engine_patch(world: &mut World, patch: &Patch) -> Result<Applied, StoreError> {
    match world.apply_engine_patch(patch) {
        Ok(a) => Ok(a),
        Err(ApplyError::Storage(e)) => Err(e.into()),
        Err(ApplyError::Rejected(r)) => {
            Err(StoreError::Corrupt(format!("rule patch rejected: {r}")))
        }
    }
}

fn log_change(world: &World, action: &str, rule: &Rule, reason: &str) -> Result<(), StoreError> {
    world.log_event(EventKind::RuleChange, &change_payload(action, rule, reason))?;
    Ok(())
}

/// A `rule_change` payload. The rule is shown with its pins, as the
/// director wrote it.
fn change_payload(action: &str, rule: &Rule, reason: &str) -> Value {
    let mut payload = json!({
        "action": action,
        "id": rule.id,
        "version": rule.version,
        "rule": to_json(rule),
    });
    if !reason.trim().is_empty() {
        payload["reason"] = json!(reason);
    }
    payload
}

/// Register the prop names and values a rule mentions.
fn register_vocabulary(world: &mut World, def: &RuleDef) -> Result<Vec<String>, StoreError> {
    let mut seen: Vec<(&str, Option<&Value>)> = Vec::new();
    for c in &def.conditions {
        if let Condition::Prop { prop, is, .. } | Condition::NotProp { prop, is, .. } = c {
            seen.push((prop, is.as_ref()));
        }
    }
    let mut added = glossary::register(world, seen)?;
    let mut effects = def.effects.clone();
    if let Some(c) = &def.check {
        for b in [&c.partial, &c.failure].into_iter().flatten() {
            effects.extend(b.effects.iter().cloned());
        }
    }
    for t in glossary::register_patch(world, &Patch::new(effects))? {
        if !added.contains(&t) {
            added.push(t);
        }
    }
    Ok(added)
}
