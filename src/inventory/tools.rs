//! The inventory tools the director calls.

use serde_json::{Value, json};

use super::{Inventory, InventoryError, Unequip};
use crate::llm::{ToolDefinition, ToolOutput, ToolUse};
use crate::world::{EntityId, World};

fn id_prop(what: &str) -> Value {
    json!({"type": "integer", "description": what})
}

/// Schemas for every inventory tool. Each acts for the player unless a
/// `carrier` (or `from`) id names another carrier.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    let carrier = id_prop("Who acts; defaults to the player.");
    let item = id_prop("The item's entity id.");
    let schema = |props: Value, required: &[&str]| json!({"type": "object", "properties": props, "required": required});
    vec![
        ToolDefinition::new(
            "inventory_list",
            "List what a carrier holds: items (with the bag each is in), equipped slots, load and soft carry limit.",
            schema(json!({"carrier": carrier}), &[]),
        ),
        ToolDefinition::new(
            "take",
            "Pick up an item within reach: in the same place, not inside a closed/locked container or another carrier, and not `fixed: true`. Refused if it would exceed the carry limit (item `bulk`, default 1).",
            schema(json!({"item": item, "carrier": carrier}), &["item"]),
        ),
        ToolDefinition::new(
            "drop",
            "Put a carried item down where the carrier is. Unequips it.",
            schema(json!({"item": item, "carrier": carrier}), &["item"]),
        ),
        ToolDefinition::new(
            "give",
            "Hand a carried item to another carrier in the same place. The recipient's carry limit applies.",
            schema(
                json!({"item": item, "to": id_prop("The receiving carrier."), "from": carrier}),
                &["item", "to"],
            ),
        ),
        ToolDefinition::new(
            "equip",
            "Equip a carried item in a slot. The item's `slot` prop (name or list) limits where it fits; without `slot`, the first free fitting slot is used. Refused if the slot is taken.",
            schema(
                json!({"item": item, "slot": {"type": "string"}, "carrier": carrier}),
                &["item"],
            ),
        ),
        ToolDefinition::new(
            "unequip",
            "Unequip an item (by item id or by slot); it stays carried.",
            schema(
                json!({"item": item, "slot": {"type": "string"}, "carrier": carrier}),
                &[],
            ),
        ),
        ToolDefinition::new(
            "transfer",
            "Move an item the carrier can reach into another reachable container, its own inventory or one of its bags (e.g. put the key in the chest, the coin from the purse into the pouch). Use give for other carriers.",
            schema(
                json!({"item": item, "to": id_prop("Destination container (or the carrier)."), "carrier": carrier}),
                &["item", "to"],
            ),
        ),
    ]
}

/// Error for a malformed tool call.
fn bad_input(msg: String) -> InventoryError {
    InventoryError::Refused(format!("invalid input: {msg}"))
}

fn opt_id(input: &Value, key: &str) -> Result<Option<EntityId>, InventoryError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .map(Some)
            .ok_or_else(|| bad_input(format!("`{key}` must be an integer entity id"))),
    }
}

fn req_id(input: &Value, key: &str) -> Result<EntityId, InventoryError> {
    opt_id(input, key)?.ok_or_else(|| bad_input(format!("`{key}` is required")))
}

fn opt_str<'a>(input: &'a Value, key: &str) -> Result<Option<&'a str>, InventoryError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(Some)
            .ok_or_else(|| bad_input(format!("`{key}` must be a string"))),
    }
}

impl Inventory {
    /// Run an inventory tool call. Returns `None` when `call` isn't one of
    /// [`tool_definitions`], so the director loop can try other handlers.
    /// Impossible moves come back as [`ToolOutput::Err`] with the reason.
    pub fn handle_tool(
        &self,
        world: &mut World,
        player: EntityId,
        call: &ToolUse,
    ) -> Option<ToolOutput> {
        let result = match call.name.as_str() {
            "inventory_list" => self.run_list(world, player, &call.input),
            "take" | "drop" | "give" | "equip" | "unequip" | "transfer" => {
                self.run_change(world, player, &call.name, &call.input)
            }
            _ => return None,
        };
        Some(match result {
            Ok(v) => ToolOutput::Ok(v.to_string()),
            Err(e) => ToolOutput::Err(e.to_string()),
        })
    }

    fn run_list(
        &self,
        world: &World,
        player: EntityId,
        input: &Value,
    ) -> Result<Value, InventoryError> {
        let carrier = opt_id(input, "carrier")?.unwrap_or(player);
        Ok(serde_json::to_value(self.list(world, carrier)?).expect("listing serializes"))
    }

    fn run_change(
        &self,
        world: &mut World,
        player: EntityId,
        name: &str,
        input: &Value,
    ) -> Result<Value, InventoryError> {
        let actor =
            opt_id(input, if name == "give" { "from" } else { "carrier" })?.unwrap_or(player);
        let event = match name {
            "take" => self.take(world, actor, req_id(input, "item")?)?,
            "drop" => self.drop(world, actor, req_id(input, "item")?)?,
            "give" => self.give(world, actor, req_id(input, "to")?, req_id(input, "item")?)?,
            "equip" => self.equip(
                world,
                actor,
                req_id(input, "item")?,
                opt_str(input, "slot")?,
            )?,
            "unequip" => {
                let target = match (opt_id(input, "item")?, opt_str(input, "slot")?) {
                    (Some(item), None) => Unequip::Item(item),
                    (None, Some(slot)) => Unequip::Slot(slot.to_owned()),
                    _ => return Err(bad_input("give exactly one of `item` or `slot`".into())),
                };
                self.unequip(world, actor, target)?
            }
            "transfer" => {
                self.transfer(world, actor, req_id(input, "item")?, req_id(input, "to")?)?
            }
            _ => unreachable!("dispatched above"),
        };
        let listing = self.list(world, actor)?;
        Ok(json!({
            "ok": true,
            "event": event,
            "carrier": actor,
            "load": listing.load,
            "limit": listing.limit,
            "over_limit": listing.over_limit,
        }))
    }
}
