//! Declarative rules: the director's rulings, kept so the engine can replay
//! them without asking again.
//!
//! A rule is a trigger (in words, for the interpreter), named roles the
//! interpreter binds to entities, conditions on those entities, and effects
//! written as ordinary patch ops in which a string handle naming a role
//! stands for the bound entity. The director picks the generality: a rule
//! conditioned on `kind` and props covers every matching entity; one pinned
//! with an `entity` condition covers only that one.
//!
//! Each rule is an engine-owned entity of kind [`RULE_KIND`] holding its
//! definition in the `_rule` prop, written by engine patches; every mint,
//! amendment and retirement bumps its version and logs a `rule_change`
//! event. Replay and forking need no extra code.

pub mod tools;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::glossary;
use crate::world::{
    Applied, ApplyError, ENGINE_PROP_PREFIX, Entity, EntityId, EntityRef, EventKind, Op, Patch,
    StoreError, World,
};

/// Kind of rule entities.
pub const RULE_KIND: &str = "rule";
/// Engine-owned prop holding a rule's definition and status.
pub const RULE_PROP: &str = "_rule";
/// Role always bound to the player.
pub const PLAYER_ROLE: &str = "player";
/// Role always bound to the player's current room.
pub const ROOM_ROLE: &str = "room";

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
    /// The role is this one entity: pins the rule to it.
    Entity { role: String, id: EntityId },
}

impl Condition {
    pub fn role(&self) -> &str {
        match self {
            Condition::Kind { role, .. }
            | Condition::Prop { role, .. }
            | Condition::NotProp { role, .. }
            | Condition::Carried { role }
            | Condition::Entity { role, .. } => role,
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
}

/// A stored rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(skip)]
    pub id: RuleId,
    #[serde(flatten)]
    pub def: RuleDef,
    pub class: RuleClass,
    pub version: u32,
    pub status: RuleStatus,
}

impl Rule {
    pub fn is_active(&self) -> bool {
        self.status == RuleStatus::Active
    }

    /// One line for prompts and listings.
    pub fn summary(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.def.name,
            "trigger": self.def.trigger,
            "roles": self.def.roles,
            "conditions": self.def.conditions,
            "class": self.class,
            "version": self.version,
        })
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
    }
    let mut created: HashSet<&str> = HashSet::new();
    for (i, op) in def.effects.iter().enumerate() {
        for r in op_refs(op) {
            if let EntityRef::Handle(h) = r
                && !known(h)
                && !created.contains(h.as_str())
            {
                return Err(format!(
                    "effect {i}: {r} is neither a role nor created by an earlier effect"
                ));
            }
        }
        let keys: Vec<&String> = match op {
            Op::Create { props, .. } => props.keys().collect(),
            Op::SetProp { key, .. } | Op::RemoveProp { key, .. } => vec![key],
            _ => vec![],
        };
        if let Some(k) = keys.iter().find(|k| k.starts_with(ENGINE_PROP_PREFIX)) {
            return Err(format!("effect {i}: prop {k:?} is engine-owned"));
        }
        if let Op::Create {
            handle: Some(h), ..
        } = op
        {
            if known(h) {
                return Err(format!("effect {i}: handle {h:?} clashes with a role"));
            }
            created.insert(h);
        }
    }
    Ok(())
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
            Condition::Entity { id, .. } => {
                if e.id != *id {
                    reasons.push(format!("{role} is #{}, not #{id}", e.id));
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
    let mut ops = rule.effects.clone();
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
    /// The world store rejected the effects.
    Rejected(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Binding(r) => write!(f, "bindings don't fit: {r}"),
            Refusal::Unmet(rs) => write!(f, "conditions not met: {}", rs.join("; ")),
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
    let patch = instantiate(rule, &bindings);
    let note = Map::from_iter([
        ("rule".to_owned(), tag),
        ("bindings".to_owned(), json!(bindings)),
    ]);
    let applied = match world.apply_patch_noted(&patch, &note) {
        Ok(a) => a,
        Err(ApplyError::Rejected(r)) => return Ok(Err(Refusal::Rejected(r.to_string()))),
        Err(ApplyError::Storage(e)) => return Err(e.into()),
    };
    let new_terms = glossary::register_patch(world, &patch)?;
    Ok(Ok(Application {
        bindings,
        patch,
        applied,
        new_terms,
    }))
}

/// The `rule` tag recorded on patches a stored rule makes.
pub fn tag(rule: &Rule) -> Value {
    json!({"id": rule.id, "version": rule.version})
}

/// Look up a rule.
pub fn get(world: &World, id: RuleId) -> Result<Option<Rule>, StoreError> {
    match world.entity(id)? {
        Some(e) if e.kind == RULE_KIND => parse(&e).map(Some),
        _ => Ok(None),
    }
}

/// Every rule, in id order.
pub fn all(world: &World) -> Result<Vec<Rule>, StoreError> {
    let mut rules = Vec::new();
    for id in world.entities_of_kind(RULE_KIND)? {
        if let Some(e) = world.entity(id)? {
            rules.push(parse(&e)?);
        }
    }
    Ok(rules)
}

/// Active rules, in id order.
pub fn active(world: &World) -> Result<Vec<Rule>, StoreError> {
    Ok(all(world)?.into_iter().filter(Rule::is_active).collect())
}

fn parse(e: &Entity) -> Result<Rule, StoreError> {
    let raw = e
        .props
        .get(RULE_PROP)
        .ok_or_else(|| StoreError::Corrupt(format!("rule #{} has no {RULE_PROP}", e.id)))?;
    let mut rule: Rule = serde_json::from_value(raw.clone())
        .map_err(|err| StoreError::Corrupt(format!("rule #{}: {err}", e.id)))?;
    rule.id = e.id;
    Ok(rule)
}

/// Store a new rule (version 1) and log its minting. Registers the prop
/// names it uses in the glossary. Returns the rule and the new glossary
/// entries, or why the definition is invalid.
pub fn mint(
    world: &mut World,
    def: RuleDef,
    class: RuleClass,
    reason: &str,
) -> Result<Result<(Rule, Vec<String>), String>, StoreError> {
    if let Err(r) = validate(&def) {
        return Ok(Err(r));
    }
    let mut rule = Rule {
        id: 0,
        def,
        class,
        version: 1,
        status: RuleStatus::Active,
    };
    let patch = Patch::new(vec![Op::Create {
        handle: None,
        name: rule.def.name.clone(),
        kind: RULE_KIND.into(),
        location: None,
        props: Map::from_iter([(RULE_PROP.to_owned(), to_json(&rule))]),
    }]);
    rule.id = engine_patch(world, &patch)?.created[0];
    log_change(world, "minted", &rule, reason)?;
    let new_terms = register_vocabulary(world, &rule.def)?;
    Ok(Ok((rule, new_terms)))
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
    if let Err(r) = validate(&rule.def) {
        return Ok(Err(r));
    }
    rule.version += 1;
    store(world, &rule)?;
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
    store(world, &rule)?;
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

fn store(world: &mut World, rule: &Rule) -> Result<(), StoreError> {
    let patch = Patch::new(vec![
        Op::Rename {
            entity: rule.id.into(),
            name: rule.def.name.clone(),
        },
        Op::SetProp {
            entity: rule.id.into(),
            key: RULE_PROP.into(),
            value: to_json(rule),
        },
    ]);
    engine_patch(world, &patch).map(|_| ())
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
    let mut payload = json!({
        "action": action,
        "id": rule.id,
        "version": rule.version,
        "rule": to_json(rule),
    });
    if !reason.trim().is_empty() {
        payload["reason"] = json!(reason);
    }
    world.log_event(EventKind::RuleChange, &payload)?;
    Ok(())
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
    let effects = Patch::new(def.effects.clone());
    for t in glossary::register_patch(world, &effects)? {
        if !added.contains(&t) {
            added.push(t);
        }
    }
    Ok(added)
}
