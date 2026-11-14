//! Engine-owned inventory: carried items, equip slots and a soft carry limit.
//!
//! All state lives in the world store and changes only through engine
//! patches, so the event log stays the single source of truth:
//!
//! - A *carrier* is an entity with the engine-owned prop `_carrier: true`
//!   ([`make_carrier`]). Everything in its containment subtree is carried;
//!   a carried bag's contents are carried too.
//! - An equipped item sits directly inside its carrier with the engine-owned
//!   prop `_equipped: "<slot>"`.
//!
//! Director patches can't move things into or out of an inventory or touch
//! `_` props (see [`World::apply_patch`]); the director changes inventory
//! only through the tools in [`tool_definitions`], handled by
//! [`Inventory::handle_tool`]. Impossible moves come back as
//! [`InventoryError::Refused`] with a reason the director can narrate.
//!
//! The engine reads a few director-invented props by convention: `closed` or
//! `locked` (true) on a container blocks reach into it, `portable: false`
//! stops an item being picked up, `bulk` (number, default 1) is what an item
//! weighs against the carry limit, and `slot` (a string or array of strings)
//! says where an item can be equipped.

mod tools;

#[cfg(test)]
mod tests;

pub use tools::tool_definitions;

use serde::Serialize;
use serde_json::{Value, json};

use crate::world::{ApplyError, CARRIER_PROP, Entity, EntityId, Op, Patch, StoreError, World};

/// Engine-owned prop on an equipped item: the slot it occupies.
pub const EQUIPPED_PROP: &str = "_equipped";

/// Slots every carrier has unless the [`Inventory`] is configured otherwise.
pub const DEFAULT_SLOTS: [&str; 8] = [
    "head",
    "body",
    "hands",
    "feet",
    "neck",
    "main_hand",
    "off_hand",
    "ring",
];

/// Default soft carry limit, in `bulk` units.
pub const DEFAULT_CARRY_LIMIT: f64 = 10.0;

/// How much a carrier can hold. Pluggable so a stat rule can drive it later.
pub trait CarryLimit {
    fn limit(&self, world: &World, carrier: EntityId) -> Result<f64, StoreError>;
}

/// The same limit for every carrier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FixedLimit(pub f64);

impl CarryLimit for FixedLimit {
    fn limit(&self, _: &World, _: EntityId) -> Result<f64, StoreError> {
        Ok(self.0)
    }
}

impl<F> CarryLimit for F
where
    F: Fn(&World, EntityId) -> Result<f64, StoreError>,
{
    fn limit(&self, world: &World, carrier: EntityId) -> Result<f64, StoreError> {
        self(world, carrier)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InventoryError {
    /// The move is impossible; the reason is meant for the director.
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<ApplyError> for InventoryError {
    fn from(e: ApplyError) -> Self {
        match e {
            ApplyError::Rejected(r) => InventoryError::Refused(r.reason),
            ApplyError::Storage(e) => InventoryError::Store(StoreError::Sqlite(e)),
        }
    }
}

type Result<T, E = InventoryError> = std::result::Result<T, E>;

fn refuse<T>(reason: impl Into<String>) -> Result<T> {
    Err(InventoryError::Refused(reason.into()))
}

/// One carried entity in an [`Listing`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CarriedItem {
    pub id: EntityId,
    pub name: String,
    /// The carried container it's in, or `None` when held directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inside: Option<EntityId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub equipped: Option<String>,
    pub bulk: f64,
}

/// What a carrier holds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Listing {
    pub carrier: EntityId,
    /// Depth-first, containers before their contents.
    pub items: Vec<CarriedItem>,
    pub load: f64,
    pub limit: f64,
    /// The limit is only checked when picking things up, so a carrier can
    /// end up over it (e.g. when a stat drops).
    pub over_limit: bool,
}

/// Inventory rules: which slots carriers have and how much they can hold.
pub struct Inventory {
    slots: Vec<String>,
    limit: Box<dyn CarryLimit>,
}

impl Default for Inventory {
    fn default() -> Self {
        Inventory::new(DEFAULT_SLOTS, FixedLimit(DEFAULT_CARRY_LIMIT))
    }
}

/// Mark an entity as a carrier so it has an inventory.
pub fn make_carrier(world: &mut World, id: EntityId) -> Result<()> {
    world.apply_engine_patch(&Patch::new(vec![Op::SetProp {
        entity: id.into(),
        key: CARRIER_PROP.into(),
        value: Value::Bool(true),
    }]))?;
    Ok(())
}

/// Whether the entity is a carrier.
pub fn is_carrier(e: &Entity) -> bool {
    e.props.get(CARRIER_PROP) == Some(&Value::Bool(true))
}

/// How the carrier can reach an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Somewhere in the carrier's inventory.
    Carried,
    /// In the carrier's location, outside any closed container.
    Here,
}

impl Inventory {
    pub fn new<S: Into<String>>(
        slots: impl IntoIterator<Item = S>,
        limit: impl CarryLimit + 'static,
    ) -> Self {
        Inventory {
            slots: slots.into_iter().map(Into::into).collect(),
            limit: Box::new(limit),
        }
    }

    pub fn slots(&self) -> &[String] {
        &self.slots
    }

    /// The soft carry limit for `carrier`.
    pub fn limit(&self, world: &World, carrier: EntityId) -> Result<f64> {
        Ok(self.limit.limit(world, carrier)?)
    }

    /// Everything `carrier` holds, with its load against the limit.
    pub fn list(&self, world: &World, carrier: EntityId) -> Result<Listing> {
        let c = carrier_entity(world, carrier)?;
        let mut items = Vec::new();
        collect(world, c.id, None, &mut items)?;
        let load = items.iter().map(|i| i.bulk).sum();
        let limit = self.limit(world, carrier)?;
        Ok(Listing {
            carrier,
            items,
            load,
            limit,
            over_limit: load > limit,
        })
    }

    /// Pick up a reachable item (or pull a carried one out of a bag into
    /// the carrier's hands).
    pub fn take(&self, world: &mut World, carrier: EntityId, item: EntityId) -> Result<i64> {
        let c = carrier_entity(world, carrier)?;
        let it = entity(world, item)?;
        check_portable(&c, &it)?;
        match reach(world, &c, &it)? {
            Reach::Carried if it.location == Some(c.id) => {
                refuse(format!("{} is already carrying {}", label(&c), label(&it)))
            }
            Reach::Carried => move_item(world, &it, Some(c.id)),
            Reach::Here => {
                self.check_capacity(world, &c, &it)?;
                move_item(world, &it, Some(c.id))
            }
        }
    }

    /// Put a carried item down in the carrier's location.
    pub fn drop(&self, world: &mut World, carrier: EntityId, item: EntityId) -> Result<i64> {
        let c = carrier_entity(world, carrier)?;
        let it = entity(world, item)?;
        require_carried(world, &c, &it)?;
        let Some(here) = c.location else {
            return refuse(format!(
                "{} is nowhere, so there's nowhere to drop it",
                label(&c)
            ));
        };
        move_item(world, &it, Some(here))
    }

    /// Hand a carried item to another carrier in the same place.
    pub fn give(
        &self,
        world: &mut World,
        from: EntityId,
        to: EntityId,
        item: EntityId,
    ) -> Result<i64> {
        let giver = carrier_entity(world, from)?;
        let it = entity(world, item)?;
        let taker = entity(world, to)?;
        if !is_carrier(&taker) {
            return refuse(format!(
                "{} can't hold things; use transfer to put {} in it",
                label(&taker),
                label(&it)
            ));
        }
        if taker.id == giver.id {
            return refuse(format!("{} already has {}", label(&giver), label(&it)));
        }
        require_carried(world, &giver, &it)?;
        if reach(world, &giver, &taker)? != Reach::Here {
            return refuse(format!(
                "{} is not within reach of {}",
                label(&taker),
                label(&giver)
            ));
        }
        self.check_capacity(world, &taker, &it)?;
        move_item(world, &it, Some(taker.id))
    }

    /// Equip a carried item. With no `slot`, use the first free slot the
    /// item fits (its `slot` prop).
    pub fn equip(
        &self,
        world: &mut World,
        carrier: EntityId,
        item: EntityId,
        slot: Option<&str>,
    ) -> Result<i64> {
        let c = carrier_entity(world, carrier)?;
        let it = entity(world, item)?;
        require_carried(world, &c, &it)?;
        if let Some(cur) = equipped_slot(&it) {
            return refuse(format!("{} is already equipped ({cur})", label(&it)));
        }
        let fits = fits_slots(&it)?;
        for s in fits.iter().flatten() {
            if !self.slots.iter().any(|k| k == s) {
                return refuse(format!(
                    "{} fits slot '{s}', which carriers don't have (slots: {})",
                    label(&it),
                    self.slots.join(", ")
                ));
            }
        }
        let occupants = occupants(world, &c)?;
        let slot = match (slot, &fits) {
            (Some(s), _) if !self.slots.iter().any(|k| k == s) => {
                return refuse(format!(
                    "there is no '{s}' slot (slots: {})",
                    self.slots.join(", ")
                ));
            }
            (Some(s), Some(fits)) if !fits.iter().any(|f| f == s) => {
                return refuse(format!(
                    "{} doesn't fit the '{s}' slot (fits: {})",
                    label(&it),
                    fits.join(", ")
                ));
            }
            (Some(s), _) => s.to_owned(),
            (None, Some(fits)) => match fits
                .iter()
                .find(|f| !occupants.iter().any(|(s, _)| s == *f))
            {
                Some(f) => f.clone(),
                None => fits[0].clone(), // all full: the conflict check below explains
            },
            (None, None) => {
                return refuse(format!(
                    "say which slot to equip {} in (slots: {})",
                    label(&it),
                    self.slots.join(", ")
                ));
            }
        };
        if let Some((_, other)) = occupants.iter().find(|(s, _)| *s == slot) {
            return refuse(format!(
                "the '{slot}' slot is taken by {}; unequip it first",
                label(other)
            ));
        }
        let mut ops = Vec::new();
        if it.location != Some(c.id) {
            ops.push(Op::Move {
                entity: it.id.into(),
                to: Some(c.id.into()),
            });
        }
        ops.push(Op::SetProp {
            entity: it.id.into(),
            key: EQUIPPED_PROP.into(),
            value: json!(slot),
        });
        Ok(world.apply_engine_patch(&Patch::new(ops))?.event_seq)
    }

    /// Unequip an item, named directly or by the slot it occupies. It stays
    /// carried.
    pub fn unequip(&self, world: &mut World, carrier: EntityId, target: Unequip) -> Result<i64> {
        let c = carrier_entity(world, carrier)?;
        let it = match target {
            Unequip::Item(item) => {
                let it = entity(world, item)?;
                if it.location != Some(c.id) || equipped_slot(&it).is_none() {
                    return refuse(format!(
                        "{} doesn't have {} equipped",
                        label(&c),
                        label(&it)
                    ));
                }
                it
            }
            Unequip::Slot(slot) => {
                match occupants(world, &c)?.into_iter().find(|(s, _)| *s == slot) {
                    Some((_, it)) => it,
                    None => {
                        return refuse(format!("{} has nothing in the '{slot}' slot", label(&c)));
                    }
                }
            }
        };
        let patch = Patch::new(vec![Op::RemoveProp {
            entity: it.id.into(),
            key: EQUIPPED_PROP.into(),
        }]);
        Ok(world.apply_engine_patch(&patch)?.event_seq)
    }

    /// Move an item between places the carrier can reach: its own
    /// inventory or bags, and open containers around it. Handing something
    /// to another carrier is [`give`](Self::give).
    pub fn transfer(
        &self,
        world: &mut World,
        carrier: EntityId,
        item: EntityId,
        to: EntityId,
    ) -> Result<i64> {
        let c = carrier_entity(world, carrier)?;
        let it = entity(world, item)?;
        let dest = entity(world, to)?;
        let from = reach(world, &c, &it)?;
        if from == Reach::Here {
            check_portable(&c, &it)?;
        }
        if dest.id == it.id {
            return refuse(format!("{} can't go inside itself", label(&it)));
        }
        if it.location == Some(dest.id) {
            return refuse(format!("{} is already in {}", label(&it), label(&dest)));
        }
        let into = if dest.id == c.id {
            Reach::Carried
        } else if Some(dest.id) == c.location {
            Reach::Here
        } else {
            if is_carrier(&dest) {
                return refuse(format!(
                    "{} is a carrier; use give to hand {} over",
                    label(&dest),
                    label(&it)
                ));
            }
            let r = reach(world, &c, &dest)?;
            if is_closed(&dest) {
                return refuse(format!("{} is closed", label(&dest)));
            }
            r
        };
        if from == Reach::Here && into == Reach::Carried {
            self.check_capacity(world, &c, &it)?;
        }
        move_item(world, &it, Some(dest.id))
    }

    /// Refuse if taking `item` would put `carrier` over its limit.
    fn check_capacity(&self, world: &World, carrier: &Entity, item: &Entity) -> Result<()> {
        let limit = self.limit(world, carrier.id)?;
        let load = subtree_bulk(world, carrier.id)?;
        let extra = bulk(item) + subtree_bulk(world, item.id)?;
        if load + extra > limit {
            return refuse(format!(
                "{} can't carry {} as well: it needs {extra} and {} is carrying {load} of {limit}",
                label(carrier),
                label(item),
                label(carrier),
            ));
        }
        Ok(())
    }
}

/// What to unequip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unequip {
    Item(EntityId),
    Slot(String),
}

fn label(e: &Entity) -> String {
    format!("{} (#{})", e.name, e.id)
}

fn entity(world: &World, id: EntityId) -> Result<Entity> {
    match world.entity(id)? {
        Some(e) => Ok(e),
        None => refuse(format!("entity #{id} does not exist")),
    }
}

fn carrier_entity(world: &World, id: EntityId) -> Result<Entity> {
    let e = entity(world, id)?;
    if !is_carrier(&e) {
        return refuse(format!("{} can't carry things", label(&e)));
    }
    Ok(e)
}

fn truthy(e: &Entity, key: &str) -> bool {
    e.props.get(key) == Some(&Value::Bool(true))
}

fn is_closed(e: &Entity) -> bool {
    truthy(e, "closed") || truthy(e, "locked")
}

fn bulk(e: &Entity) -> f64 {
    e.props
        .get("bulk")
        .and_then(Value::as_f64)
        .unwrap_or(1.0)
        .max(0.0)
}

fn equipped_slot(e: &Entity) -> Option<&str> {
    e.props.get(EQUIPPED_PROP).and_then(Value::as_str)
}

/// The slots an item fits, from its `slot` prop; `None` means unrestricted.
fn fits_slots(e: &Entity) -> Result<Option<Vec<String>>> {
    match e.props.get("slot") {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(vec![s.clone()])),
        Some(Value::Array(a)) if !a.is_empty() => a
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .map_or_else(|| refuse(bad_slot(e)), Ok),
        Some(_) => refuse(bad_slot(e)),
    }
}

fn bad_slot(e: &Entity) -> String {
    format!(
        "{}'s `slot` prop must be a slot name or a list of them",
        label(e)
    )
}

/// Equipped items directly in the carrier, with their slots.
fn occupants(world: &World, c: &Entity) -> Result<Vec<(String, Entity)>> {
    let mut out = Vec::new();
    for id in world.contents(Some(c.id))? {
        let e = entity(world, id)?;
        if let Some(s) = equipped_slot(&e) {
            out.push((s.to_owned(), e));
        }
    }
    Ok(out)
}

fn check_portable(c: &Entity, it: &Entity) -> Result<()> {
    if it.id == c.id {
        return refuse(format!("{} can't pick itself up", label(c)));
    }
    if is_carrier(it) {
        return refuse(format!("{} can't be carried", label(it)));
    }
    if it.props.get("portable") == Some(&Value::Bool(false)) {
        return refuse(format!("{} can't be picked up", label(it)));
    }
    Ok(())
}

/// Walk up from `it` to see whether `c` can reach it.
fn reach(world: &World, c: &Entity, it: &Entity) -> Result<Reach> {
    if it.id == c.id {
        return refuse(format!("{} can't reach itself", label(c)));
    }
    let mut cur = it.location;
    while let Some(id) = cur {
        if id == c.id {
            return Ok(Reach::Carried);
        }
        if Some(id) == c.location {
            return Ok(Reach::Here);
        }
        let holder = entity(world, id)?;
        if is_carrier(&holder) {
            return refuse(format!("{} is held by {}", label(it), label(&holder)));
        }
        if is_closed(&holder) {
            return refuse(format!(
                "{} is inside {}, which is closed",
                label(it),
                label(&holder)
            ));
        }
        cur = holder.location;
    }
    refuse(format!("{} is not within reach of {}", label(it), label(c)))
}

fn require_carried(world: &World, c: &Entity, it: &Entity) -> Result<()> {
    match reach(world, c, it) {
        Ok(Reach::Carried) => Ok(()),
        Ok(Reach::Here) => refuse(format!("{} isn't carrying {}", label(c), label(it))),
        // A closed bag in the inventory gives a better reason than "not carrying".
        Err(e) if world_carries(world, c.id, it)? => Err(e),
        Err(_) => refuse(format!("{} isn't carrying {}", label(c), label(it))),
    }
}

/// Whether `carrier` is an ancestor of `it`, ignoring closed containers.
fn world_carries(world: &World, carrier: EntityId, it: &Entity) -> Result<bool> {
    let mut cur = it.location;
    while let Some(id) = cur {
        if id == carrier {
            return Ok(true);
        }
        cur = entity(world, id)?.location;
    }
    Ok(false)
}

/// Move an item, clearing its equipped mark (equipped means held directly).
fn move_item(world: &mut World, it: &Entity, to: Option<EntityId>) -> Result<i64> {
    let mut ops = Vec::new();
    if equipped_slot(it).is_some() {
        ops.push(Op::RemoveProp {
            entity: it.id.into(),
            key: EQUIPPED_PROP.into(),
        });
    }
    ops.push(Op::Move {
        entity: it.id.into(),
        to: to.map(Into::into),
    });
    Ok(world.apply_engine_patch(&Patch::new(ops))?.event_seq)
}

/// Total bulk of everything inside `root` (not `root` itself).
fn subtree_bulk(world: &World, root: EntityId) -> Result<f64> {
    let mut items = Vec::new();
    collect(world, root, None, &mut items)?;
    Ok(items.iter().map(|i| i.bulk).sum())
}

fn collect(
    world: &World,
    container: EntityId,
    inside: Option<EntityId>,
    out: &mut Vec<CarriedItem>,
) -> Result<()> {
    for id in world.contents(Some(container))? {
        let e = entity(world, id)?;
        out.push(CarriedItem {
            id,
            name: e.name.clone(),
            inside,
            equipped: equipped_slot(&e).map(str::to_owned),
            bulk: bulk(&e),
        });
        collect(world, id, Some(id), out)?;
    }
    Ok(())
}
