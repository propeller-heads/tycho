use std::collections::{BTreeMap, HashMap};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tycho_substreams::prelude as tycho;

use super::config::{parse_address, PoolConfig};
use crate::lunarbase::{state::attrs, Address};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapState {
    pub block_number: u64,
    pub block_hash: [u8; 32],
    attributes: BTreeMap<String, Vec<u8>>,
}

impl BootstrapState {
    pub fn entity_change(&self, component_id: &str) -> tycho::EntityChanges {
        tycho::EntityChanges {
            component_id: component_id.to_owned(),
            attributes: self
                .attributes
                .iter()
                .map(|(name, value)| tycho::Attribute {
                    name: name.clone(),
                    value: value.clone(),
                    change: tycho::ChangeType::Creation.into(),
                })
                .collect(),
        }
    }

    pub fn balance_changes(
        &self,
        component_id: &str,
        token_x: Address,
        token_y: Address,
    ) -> [tycho::BalanceChange; 2] {
        [(attrs::RESERVE_X, token_x), (attrs::RESERVE_Y, token_y)].map(|(reserve, token)| {
            tycho::BalanceChange {
                token: token.to_vec(),
                balance: self.attributes[reserve].clone(),
                component_id: component_id.as_bytes().to_vec(),
            }
        })
    }
}

// Fixed wire width, followed by the Solidity value width.
const SCHEMA: [(&str, usize, u64); 11] = [
    (attrs::ANCHOR_PRICE_X96, 20, 160),
    (attrs::FEE_ASK_X24, 4, 24),
    (attrs::FEE_BID_X24, 4, 24),
    (attrs::LATEST_UPDATE_BLOCK, 8, 48),
    (attrs::RESERVE_X, 16, 112),
    (attrs::RESERVE_Y, 16, 112),
    (attrs::MAX_PUNISHMENT_X24, 4, 24),
    (attrs::BLOCK_DELAY, 8, 48),
    (attrs::PAUSED, 1, 1),
    (attrs::BLACKLIST_FEE_MULTIPLIER, 32, 256),
    (attrs::QUOTE_CALLER_WHITELISTED, 1, 1),
];

pub fn parse_snapshots(
    value: &str,
    pools: &[PoolConfig],
    quote_caller: Address,
) -> Result<HashMap<Address, BootstrapState>> {
    let document: Value =
        serde_json::from_str(value).context("invalid LunarBase bootstrap_states JSON")?;
    let snapshots = document.as_object().ok_or_else(|| {
        anyhow!("bootstrap_states must be an object keyed by lowercase pool address")
    })?;
    let mut parsed = HashMap::new();
    for (pool_address, value) in snapshots {
        let address = parse_address(pool_address)?;
        if pool_address != &format!("0x{}", hex::encode(address)) {
            bail!("bootstrap_states pool key must use lowercase 0x-prefixed hex: {pool_address}");
        }
        let pool = pools
            .iter()
            .find(|pool| pool.pool == address)
            .ok_or_else(|| anyhow!("bootstrap_states contains unknown pool {pool_address}"))?;
        let snapshot = parse_snapshot(value, pool, quote_caller)
            .with_context(|| format!("invalid bootstrap state for {pool_address}"))?;
        parsed.insert(address, snapshot);
    }
    Ok(parsed)
}

fn parse_snapshot(
    value: &Value,
    pool: &PoolConfig,
    quote_caller: Address,
) -> Result<BootstrapState> {
    let snapshot = value
        .as_object()
        .ok_or_else(|| anyhow!("snapshot must be an object"))?;
    for key in snapshot.keys() {
        if !["block_number", "block_hash", "quote_caller", "attributes"].contains(&key.as_str()) {
            bail!("unknown bootstrap snapshot field {key}");
        }
    }
    let block_number = snapshot
        .get("block_number")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("snapshot block_number must be an unsigned JSON integer"))?;
    let expected_parent = pool
        .bootstrap_block
        .and_then(|block| block.checked_sub(1))
        .ok_or_else(|| anyhow!("snapshot requires a positive bootstrap_block for its pool"))?;
    if block_number != expected_parent {
        bail!("snapshot block_number {block_number} must be bootstrap parent {expected_parent}");
    }
    let block_hash: [u8; 32] = decode_hex(snapshot.get("block_hash"), "block_hash", 32)?
        .try_into()
        .map_err(|_| anyhow!("snapshot block_hash must be 32 bytes"))?;
    let snapshot_caller = snapshot
        .get("quote_caller")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("snapshot quote_caller must be an address"))?;
    if parse_address(snapshot_caller)? != quote_caller {
        bail!("snapshot quote_caller does not match configured quote_caller");
    }
    let encoded = snapshot
        .get("attributes")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("snapshot attributes must be a complete state object"))?;
    for name in encoded.keys() {
        if !SCHEMA
            .iter()
            .any(|(expected, _, _)| expected == name)
        {
            bail!("unknown bootstrap state attribute {name}");
        }
    }
    let mut attributes = BTreeMap::new();
    for (name, bytes, bits) in SCHEMA {
        let decoded = decode_hex(encoded.get(name), name, bytes)?;
        if num_bigint::BigUint::from_bytes_be(&decoded).bits() > bits {
            bail!("bootstrap state attribute {name} exceeds uint{bits}");
        }
        attributes.insert(name.to_owned(), decoded);
    }
    Ok(BootstrapState { block_number, block_hash, attributes })
}

fn decode_hex(value: Option<&Value>, field: &str, bytes: usize) -> Result<Vec<u8>> {
    let encoded = value
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing or non-string bootstrap field {field}"))?;
    let digits = encoded
        .strip_prefix("0x")
        .ok_or_else(|| anyhow!("bootstrap field {field} must be 0x-prefixed hex"))?;
    let decoded =
        hex::decode(digits).with_context(|| format!("invalid bootstrap field {field} hex"))?;
    if decoded.len() != bytes {
        bail!("bootstrap field {field} must contain exactly {bytes} bytes, got {}", decoded.len());
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        lunarbase::{events::LunarBaseEvent, indexed},
        modules::config::Config,
    };

    const POOL: &str = "0x0000000000000000000000000000000000000001";
    const CALLER: &str = "0x0000000000000000000000000000000000000009";
    const PARENT_HASH: [u8; 32] = [0xab; 32];

    fn snapshots() -> Value {
        let attributes: serde_json::Map<String, Value> = indexed::initial_entity_change(POOL)
            .attributes
            .into_iter()
            .map(|attribute| {
                (attribute.name, Value::String(format!("0x{}", hex::encode(attribute.value))))
            })
            .collect();
        serde_json::json!({POOL: {
            "block_number": 9,
            "block_hash": format!("0x{}", hex::encode(PARENT_HASH)),
            "quote_caller": CALLER,
            "attributes": attributes,
        }})
    }

    fn config(snapshots: &Value) -> Result<Config> {
        Config::parse(&format!(
            "pool={POOL}&bootstrap_block=10&quote_caller={CALLER}&bootstrap_states={snapshots}"
        ))
    }

    #[test]
    fn verifies_parent_number_hash_and_caller_binding() {
        let parsed = config(&snapshots()).unwrap();
        assert!(parsed
            .validate_bootstrap_parent(10, Some(&PARENT_HASH))
            .is_ok());
        assert!(parsed
            .validate_bootstrap_parent(10, Some(&[0; 32]))
            .is_err());
        assert!(parsed
            .validate_bootstrap_parent(10, None)
            .is_err());
        assert!(parsed
            .validate_bootstrap_parent(11, None)
            .is_ok());
        for (field, invalid) in [
            ("block_number", serde_json::json!(10)),
            ("quote_caller", serde_json::json!(POOL)),
            ("block_hash", serde_json::json!("0x1234")),
        ] {
            let mut snapshots = snapshots();
            snapshots[POOL][field] = invalid;
            assert!(config(&snapshots).is_err(), "accepted invalid {field}");
        }
    }

    #[test]
    fn rejects_partial_unknown_and_out_of_range_state() {
        let mut missing = snapshots();
        missing[POOL]["attributes"]
            .as_object_mut()
            .unwrap()
            .remove(attrs::PAUSED);
        assert!(config(&missing).is_err());
        let mut unknown = snapshots();
        unknown[POOL]["attributes"]["other"] = serde_json::json!("0x00");
        assert!(config(&unknown).is_err());
        for (field, invalid) in [
            (attrs::ANCHOR_PRICE_X96, format!("0x{}", "ff".repeat(21))),
            (attrs::FEE_ASK_X24, "0x01000000".to_owned()),
            (attrs::RESERVE_X, format!("0x0001{}", "00".repeat(14))),
            (attrs::PAUSED, "0x02".to_owned()),
            (attrs::BLACKLIST_FEE_MULTIPLIER, "0x01".to_owned()),
        ] {
            let mut invalid_snapshot = snapshots();
            invalid_snapshot[POOL]["attributes"][field] = Value::String(invalid);
            assert!(config(&invalid_snapshot).is_err(), "accepted invalid {field}");
        }
        let mut unknown_pool = snapshots();
        let value = unknown_pool
            .as_object_mut()
            .unwrap()
            .remove(POOL)
            .unwrap();
        unknown_pool[CALLER] = value;
        assert!(config(&unknown_pool).is_err());
    }

    #[test]
    fn seeds_parent_state_before_current_block_events() {
        let mut snapshots = snapshots();
        snapshots[POOL]["attributes"][attrs::RESERVE_X] =
            serde_json::json!(format!("0x{}", hex::encode(123u128.to_be_bytes())));
        snapshots[POOL]["attributes"][attrs::LATEST_UPDATE_BLOCK] =
            serde_json::json!(format!("0x{}", hex::encode(9u64.to_be_bytes())));
        snapshots[POOL]["attributes"][attrs::PAUSED] = serde_json::json!("0x00");
        let parsed = config(&snapshots).unwrap();
        let snapshot = parsed
            .bootstrap_states
            .get(&parse_address(POOL).unwrap())
            .unwrap();
        let mut builder = tycho::TransactionChangesBuilder::new(&tycho::Transaction::default());
        builder.add_entity_change(&snapshot.entity_change(POOL));
        for balance in snapshot.balance_changes(POOL, [0x11; 20], [0x22; 20]) {
            builder.add_balance_change(&balance);
        }
        builder.add_entity_change(&indexed::entity_change_for_event(
            POOL,
            &LunarBaseEvent::PunishmentApplied { fee_ask_x24: 10, fee_bid_x24: 20 },
            10,
        ));
        let changes = builder.build().unwrap();
        let attributes = &changes.entity_changes[0].attributes;
        let value = |name: &str| {
            &attributes
                .iter()
                .find(|attr| attr.name == name)
                .unwrap()
                .value
        };
        assert_eq!(*value(attrs::RESERVE_X), 123u128.to_be_bytes());
        assert_eq!(*value(attrs::LATEST_UPDATE_BLOCK), 9u64.to_be_bytes());
        assert_eq!(*value(attrs::FEE_BID_X24), 20u32.to_be_bytes());
        assert_eq!(*value(attrs::PAUSED), vec![0]);
        assert_eq!(changes.balance_changes.len(), 2);
        let x_balance = changes
            .balance_changes
            .iter()
            .find(|balance| balance.token == [0x11; 20])
            .unwrap();
        assert_eq!(x_balance.balance, 123u128.to_be_bytes());
    }
}
