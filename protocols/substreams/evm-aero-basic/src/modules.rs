use std::collections::HashMap;

use itertools::Itertools;
use substreams::{
    prelude::BigInt,
    store::{
        Appender, StoreAppend, StoreGet, StoreGetProto, StoreGetRaw, StoreGetString, StoreNew,
        StoreSet, StoreSetIfNotExists, StoreSetIfNotExistsProto, StoreSetRaw,
    },
};
use substreams_ethereum::{pb::eth::v2 as eth, Event};
use tycho_substreams::prelude::*;

use crate::{
    abi::{
        custom_fee_module::events::SetCustomFee,
        pool::events::Sync,
        pool_factory::events::{PoolCreated, SetDefaultFee, SetFeeModule, SetPauseState},
    },
    params::{Params, PoolKind},
};

pub const PROTOCOL_TYPE_NAME: &str = "aero_basic_pool";

/// Factory-wide settings that every pool of the factory reads at swap time. They are copied into
/// each pool's attributes at creation and fanned out to all pools of the factory on change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactoryField {
    DefaultFee,
    FeeModule,
    Paused,
}

impl FactoryField {
    const ALL: [FactoryField; 3] =
        [FactoryField::DefaultFee, FactoryField::FeeModule, FactoryField::Paused];

    pub fn attribute(self) -> &'static str {
        match self {
            FactoryField::DefaultFee => "default_fee",
            FactoryField::FeeModule => "fee_module",
            FactoryField::Paused => "paused",
        }
    }
}

fn factory_key(kind: PoolKind, field: FactoryField) -> String {
    format!("{}:{}", kind.key(), field.attribute())
}

fn component_id(address: &[u8]) -> String {
    format!("0x{}", hex::encode(address))
}

/// A new value for one factory setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FactoryChange {
    pub kind: PoolKind,
    pub field: FactoryField,
    pub value: Vec<u8>,
}

/// A factory setting change decoded from one factory log.
fn decode_factory_event(log: &eth::Log) -> Option<(FactoryField, Vec<u8>)> {
    if let Some(event) = SetDefaultFee::match_and_decode(log) {
        return Some((FactoryField::DefaultFee, event.fee.to_signed_bytes_be()));
    }
    if let Some(event) = SetFeeModule::match_and_decode(log) {
        return Some((FactoryField::FeeModule, event.fee_module));
    }
    if let Some(event) = SetPauseState::match_and_decode(log) {
        return Some((FactoryField::Paused, vec![u8::from(event.state)]));
    }
    None
}

fn parse_params(raw: &str) -> Result<Params, substreams::errors::Error> {
    Params::parse(raw).map_err(substreams::errors::Error::msg)
}

#[substreams::handlers::map]
pub fn map_pools_created(
    params: String,
    block: eth::Block,
) -> Result<BlockChanges, substreams::errors::Error> {
    let params = parse_params(&params)?;
    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: pools_created(&params, &block),
        storage_changes: vec![],
    })
}

fn pools_created(params: &Params, block: &eth::Block) -> Vec<TransactionChanges> {
    let mut changes = Vec::new();
    for log in block.logs() {
        let Some(kind) = params.factory_kind(log.address()) else {
            continue;
        };
        let Some(event) = PoolCreated::match_and_decode(log.log) else {
            continue;
        };
        changes.push(pool_creation(kind, &event, log.receipt.transaction));
    }
    changes
}

fn pool_creation(
    kind: PoolKind,
    event: &PoolCreated,
    tx: &eth::TransactionTrace,
) -> TransactionChanges {
    let id = component_id(&event.pool);
    let zero = BigInt::zero().to_signed_bytes_be();
    let creation = |name: &str, value: Vec<u8>| Attribute {
        name: name.to_string(),
        value,
        change: ChangeType::Creation.into(),
    };
    let component = ProtocolComponent::new(&id)
        .with_tokens(&[event.token0.clone(), event.token1.clone()])
        .with_attributes(&[
            ("pool_address", event.pool.clone()),
            ("is_stable", kind.is_stable_attribute()),
        ])
        .as_swap_type(PROTOCOL_TYPE_NAME, ImplementationType::Custom);

    TransactionChanges {
        tx: Some(tx.into()),
        entity_changes: vec![EntityChanges {
            component_id: id.clone(),
            attributes: vec![
                creation("reserve0", zero.clone()),
                creation("reserve1", zero.clone()),
                // Zero means "no custom fee": the factory default applies.
                creation("custom_fee", zero.clone()),
            ],
        }],
        component_changes: vec![component],
        balance_changes: [&event.token0, &event.token1]
            .into_iter()
            .map(|token| BalanceChange {
                token: token.clone(),
                balance: zero.clone(),
                component_id: id.as_bytes().to_vec(),
            })
            .collect(),
        ..Default::default()
    }
}

#[substreams::handlers::store]
pub fn store_pools(
    pools_created: BlockChanges,
    store: StoreSetIfNotExistsProto<ProtocolComponent>,
) {
    for change in pools_created.changes {
        for component in change.component_changes {
            store.set_if_not_exists(0, &component.id, &component);
        }
    }
}

/// Appends every new pool id under its factory kind, so factory-wide changes can reach all pools.
#[substreams::handlers::store]
pub fn store_kind_pools(pools_created: BlockChanges, store: StoreAppend<String>) {
    for change in pools_created.changes {
        for component in change.component_changes {
            let kind = pool_kind(&component).unwrap_or_else(|| {
                panic!("pool {} has no valid is_stable attribute", component.id)
            });
            store.append(0, kind.key(), component.id);
        }
    }
}

/// Tracks the current factory settings, keyed by `<kind>:<attribute>`.
#[substreams::handlers::store]
pub fn store_factory_settings(params: String, block: eth::Block, store: StoreSetRaw) {
    let params = Params::parse(&params).unwrap_or_else(|err| panic!("{err}"));
    for log in block.logs() {
        let Some(kind) = params.factory_kind(log.address()) else {
            continue;
        };
        if let Some((field, value)) = decode_factory_event(log.log) {
            store.set(log.ordinal(), factory_key(kind, field), &value);
        }
    }
}

/// Read access to the stores `map_protocol_changes` depends on.
pub trait Lookups {
    /// The component of a known pool.
    fn pool(&self, id: &str) -> Option<ProtocolComponent>;
    /// Ids of every pool created by the factory of `kind`, as of the end of the block.
    fn kind_pools(&self, kind: PoolKind) -> Vec<String>;
    /// A factory setting as of the end of the block.
    fn factory_setting(&self, kind: PoolKind, field: FactoryField) -> Option<Vec<u8>>;
}

struct StoreLookups {
    pools: StoreGetProto<ProtocolComponent>,
    kind_pools: StoreGetString,
    factory_settings: StoreGetRaw,
}

impl Lookups for StoreLookups {
    fn pool(&self, id: &str) -> Option<ProtocolComponent> {
        self.pools.get_last(id)
    }

    fn kind_pools(&self, kind: PoolKind) -> Vec<String> {
        self.kind_pools
            .get_last(kind.key())
            .map(|joined| {
                joined
                    .split(';')
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn factory_setting(&self, kind: PoolKind, field: FactoryField) -> Option<Vec<u8>> {
        self.factory_settings
            .get_last(factory_key(kind, field))
    }
}

#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::Block,
    pools_created: BlockChanges,
    pools: StoreGetProto<ProtocolComponent>,
    kind_pools: StoreGetString,
    factory_settings: StoreGetRaw,
) -> Result<BlockChanges, substreams::errors::Error> {
    let params = parse_params(&params)?;
    let lookups = StoreLookups { pools, kind_pools, factory_settings };
    let changes = protocol_changes(&params, &block, pools_created, &lookups)?;
    Ok(BlockChanges { block: Some((&block).into()), changes, storage_changes: vec![] })
}

fn protocol_changes(
    params: &Params,
    block: &eth::Block,
    pools_created: BlockChanges,
    lookups: &dyn Lookups,
) -> Result<Vec<TransactionChanges>, substreams::errors::Error> {
    let mut builders: HashMap<u64, TransactionChangesBuilder> = HashMap::new();
    let created_in = add_pool_creations(pools_created, lookups, &mut builders)?;

    for log in block.logs() {
        let tx: Transaction = log.receipt.transaction.into();
        let address = log.address();
        let pool_id = component_id(address);

        if lookups.pool(&pool_id).is_some() {
            if let Some(event) = Sync::match_and_decode(log.log) {
                let builder = builder_for(&mut builders, &tx);
                add_reserves(builder, lookups, &pool_id, &event);
            }
        } else if let Some(module_kind) = params.fee_module_kind(address) {
            if let Some(event) = SetCustomFee::match_and_decode(log.log) {
                add_custom_fee(&mut builders, &tx, lookups, module_kind, &event);
            }
        } else if let Some(kind) = params.factory_kind(address) {
            if let Some((field, value)) = decode_factory_event(log.log) {
                let change = FactoryChange { kind, field, value };
                fan_out_factory_change(&mut builders, &tx, lookups, &created_in, change);
            }
        }
    }

    Ok(builders
        .into_iter()
        .sorted_unstable_by_key(|(index, _)| *index)
        .filter_map(|(_, builder)| builder.build())
        .collect())
}

fn builder_for<'a>(
    builders: &'a mut HashMap<u64, TransactionChangesBuilder>,
    tx: &Transaction,
) -> &'a mut TransactionChangesBuilder {
    builders
        .entry(tx.index)
        .or_insert_with(|| TransactionChangesBuilder::new(tx))
}

fn pool_kind(component: &ProtocolComponent) -> Option<PoolKind> {
    component
        .static_att
        .iter()
        .find(|attribute| attribute.name == "is_stable")
        .and_then(|attribute| PoolKind::from_is_stable_attribute(&attribute.value))
}

/// Adds this block's new pools with their factory settings, returning each new pool's transaction
/// index.
fn add_pool_creations(
    pools_created: BlockChanges,
    lookups: &dyn Lookups,
    builders: &mut HashMap<u64, TransactionChangesBuilder>,
) -> Result<HashMap<String, u64>, substreams::errors::Error> {
    let mut created_in = HashMap::new();
    for change in pools_created.changes {
        let tx = change
            .tx
            .ok_or_else(|| substreams::errors::Error::msg("pool creation without transaction"))?;
        let builder = builder_for(builders, &tx);
        for component in &change.component_changes {
            let kind = pool_kind(component).ok_or_else(|| {
                substreams::errors::Error::msg(format!("pool {} has no is_stable", component.id))
            })?;
            let mut attributes = Vec::with_capacity(FactoryField::ALL.len());
            for field in FactoryField::ALL {
                let value = factory_setting_at_creation(lookups, kind, field)?;
                attributes.push(Attribute {
                    name: field.attribute().to_string(),
                    value,
                    change: ChangeType::Creation.into(),
                });
            }
            builder.add_protocol_component(component);
            builder.add_entity_change(&EntityChanges {
                component_id: component.id.clone(),
                attributes,
            });
            created_in.insert(component.id.clone(), tx.index);
        }
        for entity_change in &change.entity_changes {
            builder.add_entity_change(entity_change);
        }
        for balance_change in &change.balance_changes {
            builder.add_balance_change(balance_change);
        }
    }
    Ok(created_in)
}

fn factory_setting_at_creation(
    lookups: &dyn Lookups,
    kind: PoolKind,
    field: FactoryField,
) -> Result<Vec<u8>, substreams::errors::Error> {
    if let Some(value) = lookups.factory_setting(kind, field) {
        return Ok(value);
    }
    match field {
        // The constructor never emits a pause event; factories start unpaused.
        FactoryField::Paused => Ok(vec![0]),
        // The constructor emits these, so a missing value means the start block is after the
        // factory deployment.
        FactoryField::DefaultFee | FactoryField::FeeModule => {
            Err(substreams::errors::Error::msg(format!(
                "{} factory setting `{}` is unknown at pool creation; the manifest must start at \
                 the factory deployment block",
                kind.key(),
                field.attribute()
            )))
        }
    }
}

fn add_reserves(
    builder: &mut TransactionChangesBuilder,
    lookups: &dyn Lookups,
    pool_id: &str,
    event: &Sync,
) {
    let pool = lookups
        .pool(pool_id)
        .unwrap_or_else(|| panic!("pool {pool_id} vanished from the store"));
    let reserves = [&event.reserve0, &event.reserve1];
    builder.add_entity_change(&EntityChanges {
        component_id: pool_id.to_string(),
        attributes: reserves
            .iter()
            .enumerate()
            .map(|(index, reserve)| Attribute {
                name: format!("reserve{index}"),
                value: reserve.to_signed_bytes_be(),
                change: ChangeType::Update.into(),
            })
            .collect(),
    });
    for (token, reserve) in pool.tokens.iter().zip(reserves) {
        builder.add_balance_change(&BalanceChange {
            token: token.clone(),
            balance: reserve.to_signed_bytes_be(),
            component_id: pool_id.as_bytes().to_vec(),
        });
    }
}

/// Applies a custom fee only when it comes from the fee module of the pool's own factory: the
/// stable and volatile modules are separate contracts with separate fee tables.
fn add_custom_fee(
    builders: &mut HashMap<u64, TransactionChangesBuilder>,
    tx: &Transaction,
    lookups: &dyn Lookups,
    module_kind: PoolKind,
    event: &SetCustomFee,
) {
    let pool_id = component_id(&event.pool);
    let Some(pool) = lookups.pool(&pool_id) else {
        return;
    };
    if pool_kind(&pool) != Some(module_kind) {
        return;
    }
    builder_for(builders, tx).add_entity_change(&EntityChanges {
        component_id: pool_id,
        attributes: vec![Attribute {
            name: "custom_fee".to_string(),
            value: event.fee.to_signed_bytes_be(),
            change: ChangeType::Update.into(),
        }],
    });
}

/// Copies a factory setting change to every pool of that factory that exists by `tx`. Pools
/// created later in the same block are skipped: they read the end-of-block value at creation.
fn fan_out_factory_change(
    builders: &mut HashMap<u64, TransactionChangesBuilder>,
    tx: &Transaction,
    lookups: &dyn Lookups,
    created_in: &HashMap<String, u64>,
    change: FactoryChange,
) {
    let pool_ids: Vec<String> = lookups
        .kind_pools(change.kind)
        .into_iter()
        .filter(|id| {
            created_in
                .get(id)
                .is_none_or(|created| *created <= tx.index)
        })
        .unique()
        .collect();
    if pool_ids.is_empty() {
        return;
    }
    let builder = builder_for(builders, tx);
    for pool_id in pool_ids {
        builder.add_entity_change(&EntityChanges {
            component_id: pool_id,
            attributes: vec![Attribute {
                name: change.field.attribute().to_string(),
                value: change.value.clone(),
                change: ChangeType::Update.into(),
            }],
        });
    }
}

#[cfg(test)]
#[path = "modules_tests.rs"]
mod tests;
