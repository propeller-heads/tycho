//! Fee inputs that live outside the pool: the factory's swap-hook assignment and the
//! `DynamicSwapFeeHook`'s settings and per-block fee cache. They are read from storage writes
//! because some of them (the block fee, the initial `secondsAgo`) have no event.

use std::collections::HashMap;

use substreams::store::{
    Appender, StoreAppend, StoreGet, StoreGetProto, StoreGetRaw, StoreGetString, StoreNew,
    StoreSet, StoreSetRaw, StoreSetString,
};
use substreams_ethereum::pb::eth::v2::{self as eth, Call, StorageChange};
use substreams_helper::hex::Hexable;
use tycho_substreams::prelude::*;

use crate::{
    modules::utils::{
        block_fee_slot, dynamic_fee_config_slot, packed_field, plain_slot, pool_swap_hook_slot,
        slots, unsigned_attribute, Params,
    },
    pb::tycho::evm::aerodrome::Pool,
};

/// A setting of the factory or the hook that every pool reads. Copied into each pool at creation
/// and fanned out to every pool on change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobalSetting {
    DefaultSwapHook,
    DefaultScalingFactor,
    DefaultFeeCap,
    SecondsAgo,
}

impl GlobalSetting {
    pub const ALL: [GlobalSetting; 4] = [
        GlobalSetting::DefaultSwapHook,
        GlobalSetting::DefaultScalingFactor,
        GlobalSetting::DefaultFeeCap,
        GlobalSetting::SecondsAgo,
    ];

    pub fn attribute(self) -> &'static str {
        match self {
            GlobalSetting::DefaultSwapHook => "default_swap_hook",
            GlobalSetting::DefaultScalingFactor => "hook_default_scaling_factor",
            GlobalSetting::DefaultFeeCap => "hook_default_fee_cap",
            GlobalSetting::SecondsAgo => "hook_seconds_ago",
        }
    }

    /// The setting a storage write of `address` changes, if any.
    fn at(params: &Params, address: &[u8], key: &[u8]) -> Option<Self> {
        let candidates: &[(GlobalSetting, &[u8], u8)] = &[
            (GlobalSetting::DefaultSwapHook, &params.factory, slots::FACTORY_DEFAULT_SWAP_HOOK),
            (GlobalSetting::DefaultScalingFactor, &params.hook, slots::HOOK_DEFAULT_SCALING_FACTOR),
            (GlobalSetting::DefaultFeeCap, &params.hook, slots::HOOK_DEFAULT_FEE_CAP),
            (GlobalSetting::SecondsAgo, &params.hook, slots::HOOK_SECONDS_AGO),
        ];
        candidates
            .iter()
            .find(|(_, contract, slot)| *contract == address && key == plain_slot(*slot))
            .map(|(setting, _, _)| *setting)
    }

    /// The attribute value held in a storage word of this setting's slot.
    pub fn decode(self, word: &[u8]) -> Vec<u8> {
        match self {
            GlobalSetting::DefaultSwapHook => packed_field(word, 0, 20),
            GlobalSetting::DefaultScalingFactor | GlobalSetting::DefaultFeeCap => {
                unsigned_attribute(&packed_field(word, 0, 32))
            }
            GlobalSetting::SecondsAgo => unsigned_attribute(&packed_field(word, 0, 4)),
        }
    }

    /// The value of a setting no write has touched: storage starts at zero.
    pub fn zero(self) -> Vec<u8> {
        self.decode(&[])
    }

    fn store_key(self) -> &'static str {
        self.attribute()
    }
}

/// The five fields of the hook's packed `DynamicFeeConfig`, as attributes.
pub fn dynamic_fee_config_attributes(word: &[u8], change: ChangeType) -> Vec<Attribute> {
    let fields = [
        ("dfc_baseFee", packed_field(word, 0, 3)),
        ("dfc_feeCap", packed_field(word, 3, 3)),
        ("dfc_scalingFactor", packed_field(word, 6, 8)),
        ("dfc_initialFeeEnabled", packed_field(word, 14, 1)),
        ("dfc_initialFee", packed_field(word, 15, 3)),
    ];
    fields
        .into_iter()
        .map(|(name, value)| Attribute {
            name: name.to_string(),
            value: unsigned_attribute(&value),
            change: change.into(),
        })
        .collect()
}

/// Fee attributes of a pool at creation: its own settings start at zero, the shared ones at
/// their current values.
pub fn creation_attributes(lookups: &dyn FeeLookups) -> Vec<Attribute> {
    let mut attributes = dynamic_fee_config_attributes(&[], ChangeType::Creation);
    attributes.push(Attribute {
        name: "pool_swap_hook".to_string(),
        value: vec![0u8; 20],
        change: ChangeType::Creation.into(),
    });
    for setting in GlobalSetting::ALL {
        attributes.push(Attribute {
            name: setting.attribute().to_string(),
            value: lookups
                .global(setting)
                .unwrap_or_else(|| setting.zero()),
            change: ChangeType::Creation.into(),
        });
    }
    attributes
}

fn slot_key(prefix: &str, slot: &[u8]) -> String {
    format!("{prefix}:{}", hex::encode(slot))
}

const CONFIG_SLOT_PREFIX: &str = "config";
const HOOK_SLOT_PREFIX: &str = "swap_hook";
const POOL_IDS_KEY: &str = "pools";

fn new_pool_addresses(
    pools_created: &BlockChanges,
) -> impl Iterator<Item = (String, Vec<u8>)> + '_ {
    pools_created
        .changes
        .iter()
        .flat_map(|change| change.component_changes.iter())
        .map(|component| {
            let address = hex::decode(component.id.trim_start_matches("0x"))
                .unwrap_or_else(|err| panic!("pool id {} is not hex: {err}", component.id));
            (component.id.clone(), address)
        })
}

/// Maps the hook and factory slots that hold a pool's own settings back to the pool.
#[substreams::handlers::store]
pub fn store_pool_fee_slots(pools_created: BlockChanges, store: StoreSetString) {
    for (id, address) in new_pool_addresses(&pools_created) {
        store.set(0, slot_key(CONFIG_SLOT_PREFIX, &dynamic_fee_config_slot(&address)), &id);
        store.set(0, slot_key(HOOK_SLOT_PREFIX, &pool_swap_hook_slot(&address)), &id);
    }
}

/// Every pool id, so a shared setting can reach all pools.
#[substreams::handlers::store]
pub fn store_pool_ids(pools_created: BlockChanges, store: StoreAppend<String>) {
    for (id, _) in new_pool_addresses(&pools_created) {
        store.append(0, POOL_IDS_KEY, id);
    }
}

/// Current value of each shared setting, keyed by its attribute name.
#[substreams::handlers::store]
pub fn store_fee_settings(params: String, block: eth::Block, store: StoreSetRaw) {
    let params = Params::parse_from_query(&params).unwrap_or_else(|err| panic!("{err}"));
    for (_, _, change) in storage_writes(&block) {
        if let Some(setting) = GlobalSetting::at(&params, &change.address, &change.key) {
            store.set(change.ordinal, setting.store_key(), &setting.decode(&change.new_value));
        }
    }
}

/// Every storage write of the block's successful transactions, outside reverted calls.
fn storage_writes(
    block: &eth::Block,
) -> impl Iterator<Item = (&eth::TransactionTrace, &Call, &StorageChange)> {
    block.transactions().flat_map(|tx| {
        tx.calls
            .iter()
            .filter(|call| !call.state_reverted)
            .flat_map(move |call| {
                call.storage_changes
                    .iter()
                    .map(move |change| (tx, call, change))
            })
    })
}

/// Read access to the stores the fee changes depend on.
pub trait FeeLookups {
    /// Id of the pool whose `dynamicFeeConfig` lives at this hook slot.
    fn pool_by_config_slot(&self, slot: &[u8]) -> Option<String>;
    /// Id of the pool whose `poolSwapHook` lives at this factory slot.
    fn pool_by_swap_hook_slot(&self, slot: &[u8]) -> Option<String>;
    /// Whether `address` is a known pool.
    fn is_pool(&self, address: &[u8]) -> bool;
    /// Ids of every pool, as of the end of the block.
    fn pool_ids(&self) -> Vec<String>;
    /// A shared setting as of the end of the block; `None` when never written.
    fn global(&self, setting: GlobalSetting) -> Option<Vec<u8>>;
}

pub struct StoreFeeLookups<'a> {
    pub pools: &'a StoreGetProto<Pool>,
    pub pool_fee_slots: StoreGetString,
    pub pool_ids: StoreGetString,
    pub fee_settings: StoreGetRaw,
}

impl FeeLookups for StoreFeeLookups<'_> {
    fn pool_by_config_slot(&self, slot: &[u8]) -> Option<String> {
        self.pool_fee_slots
            .get_last(slot_key(CONFIG_SLOT_PREFIX, slot))
    }

    fn pool_by_swap_hook_slot(&self, slot: &[u8]) -> Option<String> {
        self.pool_fee_slots
            .get_last(slot_key(HOOK_SLOT_PREFIX, slot))
    }

    fn is_pool(&self, address: &[u8]) -> bool {
        self.pools
            .get_last(format!("Pool:{}", address.to_hex()))
            .is_some()
    }

    fn pool_ids(&self) -> Vec<String> {
        self.pool_ids
            .get_last(POOL_IDS_KEY)
            .map(|joined| {
                joined
                    .split(';')
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn global(&self, setting: GlobalSetting) -> Option<Vec<u8>> {
        self.fee_settings
            .get_last(setting.store_key())
    }
}

/// One attribute change of one pool in one transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolAttributes {
    pub tx: Transaction,
    pub pool: String,
    pub attributes: Vec<Attribute>,
}

fn update(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_string(), value, change: ChangeType::Update.into() }
}

/// Fee attribute changes the block's storage writes make, in write order. A shared setting reaches
/// every pool that exists by its transaction; pools created later in the block read the
/// end-of-block value at creation.
pub fn fee_changes(
    params: &Params,
    block: &eth::Block,
    created_in: &HashMap<String, u64>,
    lookups: &dyn FeeLookups,
) -> Vec<PoolAttributes> {
    let mut changes = Vec::new();
    for (trace, call, write) in storage_writes(block) {
        let tx: Transaction = trace.into();
        if let Some(setting) = GlobalSetting::at(params, &write.address, &write.key) {
            let value = setting.decode(&write.new_value);
            if value == setting.decode(&write.old_value) {
                continue;
            }
            for pool in lookups.pool_ids() {
                if created_in
                    .get(&pool)
                    .is_some_and(|created| *created > tx.index)
                {
                    continue;
                }
                changes.push(PoolAttributes {
                    tx: tx.clone(),
                    pool,
                    attributes: vec![update(setting.attribute(), value.clone())],
                });
            }
        } else if write.address == params.hook {
            if let Some(pool) = lookups.pool_by_config_slot(&write.key) {
                let attributes =
                    dynamic_fee_config_attributes(&write.new_value, ChangeType::Update);
                changes.push(PoolAttributes { tx, pool, attributes });
            } else if lookups.is_pool(&call.caller) &&
                write.key == block_fee_slot(&call.caller, block.number)
            {
                // Only a pool calls the hook's `beforeSwap`, which caches the fee of its block.
                changes.push(PoolAttributes {
                    tx,
                    pool: call.caller.to_hex(),
                    attributes: vec![
                        update("block_fee", unsigned_attribute(&write.new_value)),
                        update("block_fee_block", unsigned_attribute(&block.number.to_be_bytes())),
                    ],
                });
            }
        } else if write.address == params.factory {
            if let Some(pool) = lookups.pool_by_swap_hook_slot(&write.key) {
                let attributes =
                    vec![update("pool_swap_hook", packed_field(&write.new_value, 0, 20))];
                changes.push(PoolAttributes { tx, pool, attributes });
            }
        }
    }
    changes
}

#[cfg(test)]
#[path = "fee_settings_tests.rs"]
mod tests;
