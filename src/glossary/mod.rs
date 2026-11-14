//! The glossary: every prop name (and enum-like value) and relation name
//! the director has introduced, with a one-line meaning (a term may name a
//! prop, a relation, or both), so rules keep matching the world's
//! vocabulary (`rotten` doesn't quietly become `decayed`).
//!
//! It is one engine-owned entity of kind [`GLOSSARY_KIND`] holding the terms
//! in its `_terms` component, written by engine patches, so replay and forking carry it with
//! no extra code. Terms are registered automatically (with an empty meaning)
//! whenever a director patch introduces them; the director describes them
//! with `define_terms`. The director's context holds only a compact index
//! ([`index`]); details come from `lookup_terms`.

pub mod tools;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::world::{
    self, ApplyError, Component, ENGINE_PROP_PREFIX, EntityId, Op, Patch, StoreError, World,
};

/// Kind of the single glossary entity.
pub const GLOSSARY_KIND: &str = "glossary";

/// Longest string value recorded as part of a term's vocabulary; longer
/// values are prose, not enum-like states.
const MAX_VALUE_CHARS: usize = 32;
/// Most words in a recorded string value.
const MAX_VALUE_WORDS: usize = 3;

/// Every term by name: the glossary entity's `_terms` component.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
struct Terms(BTreeMap<String, Term>);

impl Component for Terms {
    const KEY: &'static str = "_terms";
}

/// One registered prop or relation name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Term {
    /// What the prop means; empty until the director defines it.
    #[serde(default)]
    pub meaning: String,
    /// Known values (as text) with their meanings (possibly empty).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, String>,
    /// Free text (descriptions): values are never recorded.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub free_text: bool,
}

impl Term {
    fn new(meaning: &str) -> Term {
        Term {
            meaning: meaning.into(),
            ..Term::default()
        }
    }
}

/// Terms the engine itself reads or writes, known from the start.
pub fn builtin_terms() -> BTreeMap<String, Term> {
    let free = |m: &str| Term {
        free_text: true,
        ..Term::new(m)
    };
    BTreeMap::from([
        (
            "closed".into(),
            Term::new("true: a container that must be opened before reaching inside"),
        ),
        (
            "locked".into(),
            Term::new("true: closed and needs unlocking before it can be opened"),
        ),
        ("portable".into(), Term::new("false: cannot be picked up")),
        (
            "bulk".into(),
            Term::new("number: how much carry capacity an item takes (default 1)"),
        ),
        (
            "slot".into(),
            Term::new("equip slot name (or list) an item fits"),
        ),
        ("description".into(), free("lasting physical description")),
        ("region".into(), free("the bible region a room belongs to")),
        (
            "exits".into(),
            free("a room's exits: {direction: {label, to?}}"),
        ),
    ])
}

/// The glossary entity, if created.
pub fn glossary_id(world: &World) -> Result<Option<EntityId>, StoreError> {
    Ok(world.entities_of_kind(GLOSSARY_KIND)?.first().copied())
}

/// Every term, sorted by name. Before the glossary entity exists this is
/// the built-in terms.
pub fn terms(world: &World) -> Result<BTreeMap<String, Term>, StoreError> {
    let Some(id) = glossary_id(world)? else {
        return Ok(builtin_terms());
    };
    let Terms(terms) = world
        .get::<Terms>(id)?
        .ok_or_else(|| StoreError::Corrupt(format!("glossary #{id} has no terms")))?;
    Ok(terms)
}

/// The compact index for the director's context: term names, with `?`
/// marking those still without a meaning.
pub fn index(world: &World) -> Result<String, StoreError> {
    let names: Vec<String> = terms(world)?
        .into_iter()
        .map(|(name, t)| {
            if t.meaning.trim().is_empty() {
                format!("{name}?")
            } else {
                name
            }
        })
        .collect();
    Ok(names.join(", "))
}

/// Record the prop names, enum-like values and relation names that
/// `patch` introduces.
/// Returns the new entries (`name` or `name=value`) so the director can be
/// asked to define them.
pub fn register_patch(world: &mut World, patch: &Patch) -> Result<Vec<String>, StoreError> {
    let mut seen: Vec<(&str, Option<&Value>)> = Vec::new();
    for op in &patch.ops {
        match op {
            Op::Create { props, .. } => {
                seen.extend(props.iter().map(|(k, v)| (k.as_str(), Some(v))))
            }
            Op::SetProp { key, value, .. } => seen.push((key, Some(value))),
            Op::RemoveProp { key, .. } => seen.push((key, None)),
            Op::Relate { rel, .. } => seen.push((rel, None)),
            _ => {}
        }
    }
    register(world, seen)
}

/// Record prop names and (optionally) values; see [`register_patch`].
pub fn register<'a>(
    world: &mut World,
    seen: impl IntoIterator<Item = (&'a str, Option<&'a Value>)>,
) -> Result<Vec<String>, StoreError> {
    let mut all = terms(world)?;
    let mut changed = false;
    let mut added = Vec::new();
    for (name, value) in seen {
        if name.starts_with(ENGINE_PROP_PREFIX) || name.trim().is_empty() {
            continue;
        }
        let is_new = !all.contains_key(name);
        if is_new {
            added.push(name.to_owned());
        }
        let term = all.entry(name.to_owned()).or_default();
        let mut dirty = is_new;
        if !term.free_text
            && let Some(v) = value.and_then(value_key)
            && !term.values.contains_key(&v)
        {
            if !is_new {
                added.push(format!("{name}={v}"));
            }
            term.values.insert(v, String::new());
            dirty = true;
        }
        changed |= dirty;
    }
    if changed {
        write(world, all)?;
    }
    Ok(added)
}

/// A definition the director supplies.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Definition {
    pub name: String,
    pub meaning: String,
    /// Meanings of particular values.
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    #[serde(default)]
    pub free_text: Option<bool>,
}

/// Define (or redefine) terms. Unknown names are registered. A refusal
/// (`Ok(Err)`) changes nothing.
pub fn define(world: &mut World, defs: &[Definition]) -> Result<Result<(), String>, StoreError> {
    let mut all = terms(world)?;
    for d in defs {
        if d.name.starts_with(ENGINE_PROP_PREFIX) {
            return Ok(Err(format!(
                "{:?} is engine-owned, not a glossary term",
                d.name
            )));
        }
        if d.name.trim().is_empty() || d.meaning.trim().is_empty() {
            return Ok(Err("every term needs a name and a meaning".into()));
        }
        let term = all.entry(d.name.clone()).or_default();
        term.meaning = d.meaning.trim().to_owned();
        for (v, m) in &d.values {
            term.values.insert(v.clone(), m.trim().to_owned());
        }
        if let Some(f) = d.free_text {
            term.free_text = f;
        }
    }
    write(world, all)?;
    Ok(Ok(()))
}

/// The text a value is recorded under, if it is enum-like.
fn value_key(v: &Value) -> Option<String> {
    match v {
        Value::Bool(b) => Some(b.to_string()),
        Value::String(s)
            if !s.trim().is_empty()
                && s.chars().count() <= MAX_VALUE_CHARS
                && s.split_whitespace().count() <= MAX_VALUE_WORDS =>
        {
            Some(s.clone())
        }
        _ => None,
    }
}

/// Write the terms with an engine patch, creating the glossary first if
/// needed.
fn write(world: &mut World, all: BTreeMap<String, Term>) -> Result<(), StoreError> {
    let terms = Terms(all);
    let result = match glossary_id(world)? {
        Some(id) => world.set(id, &terms),
        None => world.apply_engine_patch(&Patch::new(vec![Op::Create {
            handle: None,
            name: "Glossary".into(),
            kind: GLOSSARY_KIND.into(),
            location: None,
            props: Map::from_iter([world::prop(&terms)]),
        }])),
    };
    match result {
        Ok(_) => Ok(()),
        Err(ApplyError::Storage(e)) => Err(e.into()),
        Err(ApplyError::Rejected(r)) => {
            Err(StoreError::Corrupt(format!("glossary patch rejected: {r}")))
        }
    }
}
