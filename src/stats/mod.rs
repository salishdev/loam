//! The player's stat sheet: LLM-defined stats with fixed rules, dice
//! checks with a seeded RNG, and stat changes that fire thresholds.
//!
//! The director writes the sheet with the bible. Every stat ships with stat
//! rules, ordinary [`rules`] entities of class `stat` with a [`StatHook`]
//! saying what they govern:
//!
//! - `bounds`: the value is clamped to `min..=max`;
//! - `threshold`: crossing `at` (falling to it or rising to it) fires the
//!   rule's consequence: effects, stat changes and optionally a check;
//! - `capacity`: the stat drives an engine capacity (the carry limit);
//! - `advancement`: how the stat grows. Raising a stat must cite one; the
//!   set of them is the sheet's advancement charter.
//!
//! Threshold effects reach entities only through the `player` and `room`
//! handles: a literal entity id is refused, because stat rules are never
//! amended or retired and so could not follow that entity's destruction.
//!
//! Stat rules are never amended or retired (see [`rules::amend`]). The sheet
//! can grow mid-game ([`add_stat`], with the new stat's rules, and
//! [`mint_advancement`]) but a stat is never renamed or removed: values live
//! in the player's engine-owned `_stats` component, which only this module
//! writes, and the player can't be renamed or destroyed.
//!
//! A check rolls d20 + stat against a difficulty. The die comes from a
//! splitmix64 stream seeded by the world's `_dice` seed and the number of
//! rolls already in the log, so rolls are reproducible from the log alone
//! and a fork rolls exactly what the original would have rolled next.
//! Every roll is logged as a `roll` event.

pub mod tools;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::inventory::DEFAULT_CARRY_LIMIT;
use crate::rules::{
    self, Bindings, PLAYER_ROLE, ROOM_ROLE, Rule, RuleCheck, RuleDef, RuleId, StatDelta,
};
use crate::world::{
    self, ApplyError, Component, EntityId, EventKind, Op, Patch, StoreError, World,
};

/// The player's stat values: the `_stats` component.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
struct Values(BTreeMap<String, Value_>);

impl Component for Values {
    const KEY: &'static str = "_stats";
}

/// Whether `key` is one of the props holding the sheet (values or dice
/// seed), which listings show on their own lines or not at all.
pub(crate) fn is_sheet_key(key: &str) -> bool {
    key == Values::KEY || key == Dice::KEY
}

/// One stat's stored value and meaning.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Value_ {
    value: i64,
    #[serde(default)]
    about: String,
}

/// The world's dice seed, on the player: the `_dice` component.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(transparent)]
struct Dice(u64);

impl Component for Dice {
    const KEY: &'static str = "_dice";
}
/// Most stats a sheet may have.
pub const MAX_STATS: usize = 8;
/// A total this far below the difficulty (or less) is a partial success.
pub const PARTIAL_BAND: i64 = 5;
/// Faces on the check die.
pub const DIE: u64 = 20;
/// How deep thresholds may fire thresholds.
const MAX_CHAIN: usize = 4;

/// When a threshold fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Crossing {
    /// The value drops from above `at` to `at` or below.
    FallsTo,
    /// The value climbs from below `at` to `at` or above.
    RisesTo,
}

/// An engine capacity a stat can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capacity {
    /// The player's soft carry limit, in `bulk` units.
    CarryLimit,
}

/// What a stat rule governs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StatHook {
    Bounds {
        stat: String,
        min: i64,
        max: i64,
    },
    Threshold {
        stat: String,
        at: i64,
        when: Crossing,
    },
    Capacity {
        stat: String,
        capacity: Capacity,
        base: f64,
        per_point: f64,
    },
    /// The rule's trigger says when the stat grows; `step` is the most one
    /// application may raise it.
    Advancement {
        stat: String,
        step: i64,
    },
}

impl StatHook {
    pub fn stat(&self) -> &str {
        match self {
            StatHook::Bounds { stat, .. }
            | StatHook::Threshold { stat, .. }
            | StatHook::Capacity { stat, .. }
            | StatHook::Advancement { stat, .. } => stat,
        }
    }
}

/// One stat as the sheet holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct Stat {
    pub name: String,
    pub about: String,
    pub value: i64,
    pub min: i64,
    pub max: i64,
}

/// The result of a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckResult {
    Success,
    Partial,
    Failure,
}

impl CheckResult {
    /// Grade a total against a difficulty.
    pub fn grade(total: i64, difficulty: i64) -> CheckResult {
        if total >= difficulty {
            CheckResult::Success
        } else if total > difficulty - PARTIAL_BAND {
            CheckResult::Partial
        } else {
            CheckResult::Failure
        }
    }
}

/// A logged roll.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Roll {
    /// Sequence number of the `roll` event.
    pub seq: i64,
    pub stat: String,
    /// The stat's value when rolled.
    pub value: i64,
    pub difficulty: i64,
    /// The die (1..=20).
    pub die: i64,
    pub total: i64,
    pub result: CheckResult,
    /// The seed the die came from.
    pub seed: u64,
}

/// One stat change.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatChange {
    pub stat: String,
    pub from: i64,
    pub to: i64,
}

/// A threshold that fired.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Fired {
    pub rule: RuleId,
    pub name: String,
    /// What happened, for the narrator (the check branch's when the
    /// threshold embeds a check).
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roll: Option<Roll>,
    /// Why its consequence couldn't apply, when it couldn't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Stat changes and the thresholds they fired, in order.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Changes {
    pub changed: Vec<StatChange>,
    pub fired: Vec<Fired>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.fired.is_empty()
    }
}

fn player(world: &World) -> Result<Option<EntityId>, StoreError> {
    crate::game::player_id(world)
}

/// The player's stat values.
fn values(world: &World) -> Result<Values, StoreError> {
    let Some(p) = player(world)? else {
        return Ok(Values::default());
    };
    Ok(world.get::<Values>(p)?.unwrap_or_default())
}

/// Active stat rules, in id order.
pub fn stat_rules(world: &World) -> Result<Vec<Rule>, StoreError> {
    Ok(rules::active(world)?
        .into_iter()
        .filter(|r| r.hook.is_some())
        .collect())
}

/// The sheet: every stat by name, with its bounds.
pub fn sheet(world: &World) -> Result<BTreeMap<String, Stat>, StoreError> {
    let Values(values) = values(world)?;
    if values.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut bounds = BTreeMap::new();
    for r in stat_rules(world)? {
        if let Some(StatHook::Bounds { stat, min, max }) = r.hook {
            bounds.insert(stat, (min, max));
        }
    }
    let mut sheet = BTreeMap::new();
    for (name, v) in values {
        let (min, max) = *bounds
            .get(&name)
            .ok_or_else(|| StoreError::Corrupt(format!("stat {name:?} has no bounds rule")))?;
        sheet.insert(
            name.clone(),
            Stat {
                value: v.value,
                about: v.about,
                name,
                min,
                max,
            },
        );
    }
    Ok(sheet)
}

/// splitmix64: a small, well-mixed generator step.
fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The seed of roll number `n` (counting from 0) in a world seeded `base`.
pub fn roll_seed(base: u64, n: u64) -> u64 {
    mix(base ^ mix(n))
}

/// The die a seed gives.
pub fn die(seed: u64) -> i64 {
    (mix(seed) % DIE) as i64 + 1
}

/// Roll `check(stat, difficulty)`: d20 + the stat against the difficulty.
/// Logs a `roll` event carrying `context` (e.g. the rule or reason).
/// Returns why it can't roll (an unknown stat).
pub fn check(
    world: &World,
    stat: &str,
    difficulty: i64,
    context: Value,
) -> Result<Result<Roll, String>, StoreError> {
    let sheet = sheet(world)?;
    let Some(s) = sheet.get(stat) else {
        return Ok(Err(unknown(stat, &sheet)));
    };
    let base = match player(world)? {
        Some(p) => world.get::<Dice>(p)?.map_or(0, |Dice(d)| d),
        None => 0,
    };
    let n = world.count_events(EventKind::Roll)? as u64;
    let seed = roll_seed(base, n);
    let die = die(seed);
    let total = die + s.value;
    let result = CheckResult::grade(total, difficulty);
    let mut payload = json!({
        "stat": stat, "value": s.value, "difficulty": difficulty,
        "die": die, "total": total, "result": result, "seed": seed, "n": n,
    });
    if let Value::Object(extra) = context {
        for (k, v) in extra {
            payload.as_object_mut().unwrap().entry(k).or_insert(v);
        }
    }
    let seq = world.log_event(EventKind::Roll, &payload)?;
    Ok(Ok(Roll {
        seq,
        stat: stat.to_owned(),
        value: s.value,
        difficulty,
        die,
        total,
        result,
        seed,
    }))
}

fn unknown(stat: &str, sheet: &BTreeMap<String, Stat>) -> String {
    if sheet.is_empty() {
        format!("there is no stat {stat:?}: the sheet is empty")
    } else {
        format!(
            "there is no stat {stat:?}; stats: {}",
            sheet.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    }
}

/// The director's `adjust_stat`: change a stat by `by` with a logged
/// reason. Raising one must cite an active advancement rule of that stat
/// and stay within its step. Thresholds the change crosses fire.
pub fn adjust_stat(
    world: &mut World,
    stat: &str,
    by: i64,
    reason: &str,
    cite: Option<RuleId>,
) -> Result<Result<Changes, String>, StoreError> {
    let sheet = sheet(world)?;
    if !sheet.contains_key(stat) {
        return Ok(Err(unknown(stat, &sheet)));
    }
    if by == 0 {
        return Ok(Err("by must not be 0".into()));
    }
    if reason.trim().is_empty() {
        return Ok(Err("give a reason".into()));
    }
    if by > 0 {
        let charter: Vec<(RuleId, String, i64)> = stat_rules(world)?
            .into_iter()
            .filter_map(|r| match &r.hook {
                Some(StatHook::Advancement { stat: s, step }) if s == stat => {
                    Some((r.id, r.def.trigger.clone(), *step))
                }
                _ => None,
            })
            .collect();
        let listed = || {
            charter
                .iter()
                .map(|(id, t, step)| format!("#{id} {t:?} (up to +{step})"))
                .collect::<Vec<_>>()
                .join("; ")
        };
        let Some(id) = cite else {
            return Ok(Err(format!(
                "raising {stat} must cite one of its advancement rules: {}",
                listed()
            )));
        };
        let Some((_, _, step)) = charter.iter().find(|(r, _, _)| *r == id) else {
            return Ok(Err(format!(
                "#{id} is not an advancement rule of {stat}; its rules: {}",
                listed()
            )));
        };
        if by > *step {
            return Ok(Err(format!(
                "advancement rule #{id} raises {stat} by at most {step}"
            )));
        }
    }
    let mut changes = Changes::default();
    let mut note = json!({"reason": reason.trim()});
    if let Some(id) = cite {
        note["advancement"] = json!(id);
    }
    apply_delta(world, stat, by, note, 0, &mut changes)?;
    Ok(Ok(changes))
}

/// Change a stat by `by` (clamped to its bounds) and fire the thresholds
/// the change crosses. `note` is recorded in the patch event. An unknown
/// stat changes nothing. `depth` counts thresholds firing thresholds.
pub(crate) fn apply_delta(
    world: &mut World,
    stat: &str,
    by: i64,
    note: Value,
    depth: usize,
    out: &mut Changes,
) -> Result<(), StoreError> {
    let Some(s) = sheet(world)?.remove(stat) else {
        return Ok(());
    };
    let from = s.value;
    let to = from.saturating_add(by).clamp(s.min, s.max);
    out.changed.push(StatChange {
        stat: stat.to_owned(),
        from,
        to,
    });
    if to == from {
        return Ok(());
    }
    let mut values = values(world)?;
    values.0.insert(
        stat.to_owned(),
        Value_ {
            value: to,
            about: s.about,
        },
    );
    // The change and why, e.g. {"reason": ...} or {"rule": ...}.
    let mut change = Map::from_iter([
        ("name".to_owned(), json!(stat)),
        ("from".to_owned(), json!(from)),
        ("to".to_owned(), json!(to)),
    ]);
    if let Value::Object(extra) = note {
        change.extend(extra);
    }
    let fields = Map::from_iter([("stat".to_owned(), Value::Object(change))]);
    let player = player(world)?.expect("a sheet implies a player");
    engine_patch(world, vec![world::set_op(player, &values)], Some(&fields))?;

    for rule in stat_rules(world)? {
        let Some(StatHook::Threshold { stat: s, at, when }) = &rule.hook else {
            continue;
        };
        let crossed = match when {
            Crossing::FallsTo => from > *at && to <= *at,
            Crossing::RisesTo => from < *at && to >= *at,
        };
        if s != stat || !crossed {
            continue;
        }
        let mut fired = Fired {
            rule: rule.id,
            name: rule.def.name.clone(),
            outcome: rule.def.outcome.clone(),
            roll: None,
            error: None,
        };
        if depth >= MAX_CHAIN {
            fired.error = Some("not applied: thresholds fired too deep a chain".into());
            out.fired.push(fired);
            continue;
        }
        let mut bindings = Bindings::from([(PLAYER_ROLE.to_owned(), player)]);
        if let Some(room) = world.entity(player)?.and_then(|p| p.location) {
            bindings.insert(ROOM_ROLE.to_owned(), room);
        }
        let tag = json!({"id": rule.id, "version": rule.version, "threshold": true});
        match rules::resolve(world, &rule.def, tag, bindings, depth + 1)? {
            Ok(app) => {
                fired.outcome = app.outcome;
                fired.roll = app.roll;
                out.fired.push(fired);
                out.changed.extend(app.stats.changed);
                out.fired.extend(app.stats.fired);
            }
            Err(refusal) => {
                fired.error = Some(refusal.to_string());
                out.fired.push(fired);
            }
        }
    }
    Ok(())
}

fn engine_patch(
    world: &mut World,
    ops: Vec<Op>,
    note: Option<&Map<String, Value>>,
) -> Result<(), StoreError> {
    let patch = Patch::new(ops);
    let result = match note {
        Some(n) => world.apply_engine_patch_noted(&patch, n),
        None => world.apply_engine_patch(&patch),
    };
    match result {
        Ok(_) => Ok(()),
        Err(ApplyError::Storage(e)) => Err(e.into()),
        Err(ApplyError::Rejected(r)) => {
            Err(StoreError::Corrupt(format!("stat patch rejected: {r}")))
        }
    }
}

/// The player's carry limit: driven by a `carry_limit` capacity rule when
/// the sheet has one, else [`DEFAULT_CARRY_LIMIT`]. Other carriers get the
/// default.
pub fn carry_limit(world: &World, carrier: EntityId) -> Result<f64, StoreError> {
    if player(world)? != Some(carrier) {
        return Ok(DEFAULT_CARRY_LIMIT);
    }
    for r in stat_rules(world)? {
        if let Some(StatHook::Capacity {
            stat,
            capacity: Capacity::CarryLimit,
            base,
            per_point,
        }) = &r.hook
            && let Some(s) = sheet(world)?.get(stat)
        {
            return Ok(capacity_at(*base, *per_point, s.value));
        }
    }
    Ok(DEFAULT_CARRY_LIMIT)
}

/// A capacity a stat drives, at a given value of that stat.
pub fn capacity_at(base: f64, per_point: f64, value: i64) -> f64 {
    (base + per_point * value as f64).max(0.0)
}

// ---- writing the sheet -----------------------------------------------------

/// A stat as the director defines it, with its rules.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct StatSpec {
    pub name: String,
    /// What the stat measures, in a phrase.
    pub about: String,
    pub min: i64,
    pub max: i64,
    pub start: i64,
    #[serde(default)]
    pub thresholds: Vec<ThresholdSpec>,
    #[serde(default)]
    pub capacity: Option<CapacitySpec>,
    pub advancement: Vec<AdvancementSpec>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThresholdSpec {
    #[serde(default)]
    pub name: String,
    pub at: i64,
    pub when: Crossing,
    /// What happens, for the narrator (e.g. a setback).
    pub outcome: String,
    /// Patch ops; `player` and `room` name the player and their room.
    #[serde(default)]
    pub effects: Vec<Op>,
    #[serde(default)]
    pub adjust: Vec<StatDelta>,
    #[serde(default)]
    pub check: Option<RuleCheck>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CapacitySpec {
    pub capacity: Capacity,
    pub base: f64,
    pub per_point: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AdvancementSpec {
    #[serde(default)]
    pub name: String,
    /// When the stat grows, in words.
    pub when: String,
    /// The most one application raises it.
    pub step: i64,
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name.len() <= 24
}

fn or_default(name: &str, default: String) -> String {
    if name.trim().is_empty() {
        default
    } else {
        name.trim().to_owned()
    }
}

/// Check stat names a stat-rule consequence uses against `known`.
fn check_refs(def: &RuleDef, known: &dyn Fn(&str) -> bool) -> Result<(), String> {
    let mut names: Vec<&str> = def.adjust.iter().map(|d| d.stat.as_str()).collect();
    if let Some(c) = &def.check {
        names.push(&c.stat);
        for b in [&c.partial, &c.failure].into_iter().flatten() {
            names.extend(b.adjust.iter().map(|d| d.stat.as_str()));
        }
    }
    match names.into_iter().find(|n| !known(n)) {
        Some(n) => Err(format!("unknown stat {n:?}")),
        None => Ok(()),
    }
}

fn advancement_rule(stat: &str, a: &AdvancementSpec) -> Result<(RuleDef, StatHook), String> {
    if a.when.trim().is_empty() {
        return Err(format!("{stat}: an advancement rule needs a when"));
    }
    if a.step < 1 {
        return Err(format!("{stat}: an advancement step must be at least 1"));
    }
    let def = RuleDef {
        name: or_default(&a.name, format!("{stat} advancement")),
        trigger: a.when.trim().to_owned(),
        outcome: format!("{stat} grows by up to {}", a.step),
        ..empty_def()
    };
    let hook = StatHook::Advancement {
        stat: stat.to_owned(),
        step: a.step,
    };
    Ok((def, hook))
}

fn empty_def() -> RuleDef {
    RuleDef {
        name: String::new(),
        trigger: String::new(),
        roles: BTreeMap::new(),
        conditions: Vec::new(),
        effects: Vec::new(),
        outcome: String::new(),
        adjust: Vec::new(),
        check: None,
    }
}

/// Turn one stat's definition into its rules, checked. `known` says which
/// stat names thresholds may refer to.
fn plan(spec: &StatSpec, known: &dyn Fn(&str) -> bool) -> Result<Vec<(RuleDef, StatHook)>, String> {
    let n = &spec.name;
    if !valid_name(n) {
        return Err(format!(
            "stat name {n:?}: use lowercase letters, digits and _ (at most 24), starting with a letter"
        ));
    }
    if spec.about.trim().is_empty() {
        return Err(format!("{n}: say what it measures in about"));
    }
    if spec.min >= spec.max {
        return Err(format!("{n}: min must be below max"));
    }
    if !(spec.min..=spec.max).contains(&spec.start) {
        return Err(format!("{n}: start must be within min..=max"));
    }
    if spec.advancement.is_empty() {
        return Err(format!(
            "{n}: give at least one advancement rule (how the player can grow it)"
        ));
    }
    let mut out = vec![(
        RuleDef {
            name: format!("{n} bounds"),
            trigger: format!("{n} stays within {}..={}", spec.min, spec.max),
            outcome: spec.about.trim().to_owned(),
            ..empty_def()
        },
        StatHook::Bounds {
            stat: n.clone(),
            min: spec.min,
            max: spec.max,
        },
    )];
    for t in &spec.thresholds {
        if !(spec.min..=spec.max).contains(&t.at) {
            return Err(format!("{n}: threshold at {} is outside its bounds", t.at));
        }
        if t.outcome.trim().is_empty() {
            return Err(format!("{n}: a threshold needs an outcome"));
        }
        let (verb, dir) = match t.when {
            Crossing::FallsTo => ("falls to", "or below"),
            Crossing::RisesTo => ("rises to", "or above"),
        };
        let def = RuleDef {
            name: or_default(&t.name, format!("{n} {verb} {}", t.at)),
            trigger: format!("{n} {verb} {} {dir}", t.at),
            effects: t.effects.clone(),
            outcome: t.outcome.trim().to_owned(),
            adjust: t.adjust.clone(),
            check: t.check.clone(),
            ..empty_def()
        };
        rules::validate(&def).map_err(|e| format!("{n} threshold at {}: {e}", t.at))?;
        if let Some(id) = rules::effect_ids(&def).first() {
            return Err(format!(
                "{n} threshold at {}: an effect names #{id}; use \"player\" or \"room\" \
                 (stat rules never change, so a fixed id would outlive its entity)",
                t.at
            ));
        }
        check_refs(&def, known).map_err(|e| format!("{n} threshold at {}: {e}", t.at))?;
        out.push((
            def,
            StatHook::Threshold {
                stat: n.clone(),
                at: t.at,
                when: t.when,
            },
        ));
    }
    if let Some(c) = &spec.capacity {
        if !c.base.is_finite() || !c.per_point.is_finite() {
            return Err(format!("{n}: capacity numbers must be finite"));
        }
        out.push((
            RuleDef {
                name: format!("{n} drives carry limit"),
                trigger: format!(
                    "carry limit = {} + {} per point of {n}",
                    c.base, c.per_point
                ),
                ..empty_def()
            },
            StatHook::Capacity {
                stat: n.clone(),
                capacity: c.capacity,
                base: c.base,
                per_point: c.per_point,
            },
        ));
    }
    for a in &spec.advancement {
        out.push(advancement_rule(n, a)?);
    }
    Ok(out)
}

fn capacity_taken(world: &World, capacity: Capacity) -> Result<Option<String>, StoreError> {
    Ok(stat_rules(world)?.into_iter().find_map(|r| match r.hook {
        Some(StatHook::Capacity {
            stat, capacity: c, ..
        }) if c == capacity => Some(stat),
        _ => None,
    }))
}

/// Store planned rules and the starting values in one go.
fn write(
    world: &mut World,
    planned: Vec<(RuleDef, StatHook)>,
    specs: &[StatSpec],
    seed: Option<u64>,
    reason: &str,
) -> Result<Vec<RuleId>, StoreError> {
    let player = player(world)?.expect("checked by the caller");
    let mut ids = Vec::new();
    for (def, hook) in planned {
        ids.push(rules::mint_stat(world, def, hook, reason)?.id);
    }
    let mut values = values(world)?;
    for s in specs {
        values.0.insert(
            s.name.clone(),
            Value_ {
                value: s.start,
                about: s.about.trim().to_owned(),
            },
        );
    }
    let mut ops = vec![world::set_op(player, &values)];
    if let Some(seed) = seed {
        ops.push(world::set_op(player, &Dice(seed)));
    }
    engine_patch(world, ops, None)?;
    Ok(ids)
}

fn has_seed(world: &World, player: EntityId) -> Result<bool, StoreError> {
    Ok(world.get::<Dice>(player)?.is_some())
}

/// Write the stat sheet (once, with the bible). Every stat needs bounds, a
/// start within them and at least one advancement rule. Nothing is written
/// unless the whole sheet is valid. Returns the stat rules' ids.
pub fn create_sheet(
    world: &mut World,
    specs: &[StatSpec],
    seed: u64,
) -> Result<Result<Vec<RuleId>, String>, StoreError> {
    if player(world)?.is_none() {
        return Ok(Err("write the bible first: there is no player yet".into()));
    }
    if !values(world)?.0.is_empty() {
        return Ok(Err(
            "the stat sheet is already written; add_stat adds a stat".into(),
        ));
    }
    if specs.is_empty() || specs.len() > MAX_STATS {
        return Ok(Err(format!("give 1 to {MAX_STATS} stats")));
    }
    let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    for (i, n) in names.iter().enumerate() {
        if names[..i].contains(n) {
            return Ok(Err(format!("stat {n:?} is listed twice")));
        }
    }
    let known = |s: &str| names.contains(&s);
    let mut planned = Vec::new();
    let mut capacities = Vec::new();
    for spec in specs {
        if let Some(c) = &spec.capacity {
            if capacities.contains(&c.capacity) {
                return Ok(Err(format!(
                    "{}: only one stat may drive {:?}",
                    spec.name, c.capacity
                )));
            }
            capacities.push(c.capacity);
        }
        match plan(spec, &known) {
            Ok(p) => planned.extend(p),
            Err(e) => return Ok(Err(e)),
        }
    }
    write(world, planned, specs, Some(seed), "stat sheet").map(Ok)
}

/// Add a stat mid-game, together with its rules. Its name must be new.
/// `seed` seeds the dice if the world has no sheet yet.
pub fn add_stat(
    world: &mut World,
    spec: &StatSpec,
    seed: u64,
    reason: &str,
) -> Result<Result<Vec<RuleId>, String>, StoreError> {
    let Some(p) = player(world)? else {
        return Ok(Err("there is no player yet".into()));
    };
    if reason.trim().is_empty() {
        return Ok(Err("give a reason".into()));
    }
    let current = sheet(world)?;
    if current.contains_key(&spec.name) {
        return Ok(Err(format!(
            "there is already a stat {:?}; stats are never redefined or renamed",
            spec.name
        )));
    }
    if current.len() >= MAX_STATS {
        return Ok(Err(format!("the sheet already has {MAX_STATS} stats")));
    }
    if let Some(c) = &spec.capacity
        && let Some(by) = capacity_taken(world, c.capacity)?
    {
        return Ok(Err(format!("{by} already drives {:?}", c.capacity)));
    }
    let known = |s: &str| s == spec.name || current.contains_key(s);
    let planned = match plan(spec, &known) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    let seed = (!has_seed(world, p)?).then_some(seed);
    write(world, planned, std::slice::from_ref(spec), seed, reason).map(Ok)
}

/// Mint a new advancement rule for an existing stat. Like every stat rule
/// it never changes afterwards.
pub fn mint_advancement(
    world: &mut World,
    stat: &str,
    spec: &AdvancementSpec,
    reason: &str,
) -> Result<Result<RuleId, String>, StoreError> {
    let current = sheet(world)?;
    if !current.contains_key(stat) {
        return Ok(Err(unknown(stat, &current)));
    }
    let (def, hook) = match advancement_rule(stat, spec) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    Ok(Ok(rules::mint_stat(world, def, hook, reason)?.id))
}

/// Compact lines for the director's local slice: one per stat with its
/// value, bounds, thresholds and advancement charter. Empty without a sheet.
pub fn slice_lines(world: &World) -> Result<Vec<String>, StoreError> {
    let sheet = sheet(world)?;
    let rules = stat_rules(world)?;
    let mut lines = Vec::new();
    for s in sheet.values() {
        let mut line = format!(
            "- {} {} [{}..{}] {}",
            s.name, s.value, s.min, s.max, s.about
        );
        let mut thresholds = Vec::new();
        let mut grows = Vec::new();
        for r in &rules {
            match &r.hook {
                Some(StatHook::Threshold { stat, .. }) if stat == &s.name => {
                    thresholds.push(format!("#{} {}", r.id, r.def.trigger));
                }
                Some(StatHook::Advancement { stat, step }) if stat == &s.name => {
                    grows.push(format!("#{} {} (+{step})", r.id, r.def.trigger));
                }
                Some(StatHook::Capacity { stat, .. }) if stat == &s.name => {
                    line.push_str(&format!("; {}", r.def.trigger));
                }
                _ => {}
            }
        }
        if !thresholds.is_empty() {
            line.push_str(&format!("; thresholds: {}", thresholds.join(", ")));
        }
        if !grows.is_empty() {
            line.push_str(&format!("; grows: {}", grows.join(", ")));
        }
        lines.push(line);
    }
    Ok(lines)
}

/// A sample sheet for tests: `nerve` 3 [0..5] with a setback threshold at
/// 0 and one advancement rule, and `brawn` 2 [0..5] driving the carry limit
/// (6 + 2 per point = 10, the default).
#[cfg(test)]
pub(crate) fn sample_sheet() -> Value {
    json!([
        {"name": "nerve", "about": "steadiness before the uncanny", "min": 0, "max": 5, "start": 3,
         "thresholds": [{"at": 0, "when": "falls_to",
                         "outcome": "Your nerve breaks; you flee back the way you came, shaking.",
                         "effects": [{"op": "set_prop", "entity": "player", "key": "shaken", "value": true}]}],
         "advancement": [{"when": "facing a horror without fleeing", "step": 1}]},
        {"name": "brawn", "about": "strength and endurance", "min": 0, "max": 5, "start": 2,
         "capacity": {"capacity": "carry_limit", "base": 6, "per_point": 2},
         "advancement": [{"when": "hard labour finished", "step": 1}]}
    ])
}

/// Add a `write_stats` call with [`sample_sheet`] to a scripted response
/// (the bible call), as the director makes both together.
#[cfg(test)]
pub(crate) fn with_sample_sheet(mut r: crate::llm::Response) -> crate::llm::Response {
    r.content
        .push(crate::llm::ContentBlock::ToolUse(crate::llm::ToolUse {
            id: "stats1".into(),
            name: "write_stats".into(),
            input: json!({"stats": sample_sheet()}),
        }));
    r
}
