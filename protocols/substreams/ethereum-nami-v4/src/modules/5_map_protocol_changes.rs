use crate::{
    abi::plugin::functions::BeforeSwap,
    modules::{
        map_store_ticks::{gross_liquidity_key, net_liquidity_key},
        store_pool_plugins::pool_config_key,
    },
    pb::nami::v4::{events::pool_event::Typ, Events, LiquidityChanges, TickDeltas},
};
use itertools::Itertools;
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    vec,
};
use substreams::{
    pb::substreams::StoreDeltas,
    scalar::BigInt,
    store::{StoreGet, StoreGetBigInt, StoreGetString},
};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::hex::Hexable;
use tycho_substreams::{
    balances::aggregate_balances_changes,
    block_storage::get_block_storage_changes,
    entrypoint::create_entrypoint,
    prelude::{entry_point_params::TraceData, *},
};

/// Pool calls the plugin's `beforeSwap` when this `pluginConfig` flag is set.
const BEFORE_SWAP_FLAG: u32 = 1;
const BEFORE_SWAP_SIGNATURE: &str = "beforeSwap(address,address,bool,int256,uint160,bool,bytes)";
/// `TickMath.MIN_SQRT_RATIO + 1`: the loosest price limit of a zero-for-one swap.
const MIN_SQRT_RATIO_PLUS_ONE: u64 = 4_295_128_740;

#[substreams::handlers::map]
#[allow(clippy::too_many_arguments)]
pub fn map_protocol_changes(
    block: eth::Block,
    created_pools: BlockEntityChanges,
    events: Events,
    balances_map_deltas: BlockBalanceDeltas,
    balances_store_deltas: StoreDeltas,
    ticks_map_deltas: TickDeltas,
    ticks_store: StoreGetBigInt,
    pool_liquidity_changes: LiquidityChanges,
    pool_liquidity_store_deltas: StoreDeltas,
    pool_plugins_store: StoreGetString,
    plugin_events: Events,
) -> BlockChanges {
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    // Add created pools to the tx changes map
    for change in created_pools.changes.into_iter() {
        let tx = change.tx.as_ref().unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        change
            .component_changes
            .iter()
            .for_each(|c| {
                builder.add_protocol_component(c);
            });
        change
            .entity_changes
            .iter()
            .for_each(|ec| {
                builder.add_entity_change(ec);
            });
        change
            .balance_changes
            .iter()
            .for_each(|bc| {
                builder.add_balance_change(bc);
            });
    }

    // Balance changes are gathered by the `StoreDelta` based on pool events creating
    // `BlockBalanceDeltas`. These are merged onto the existing map of tx changes, inserting a new
    // one if it doesn't exist.
    aggregate_balances_changes(balances_store_deltas, balances_map_deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            balances
                .values()
                .for_each(|token_bc_map| {
                    token_bc_map
                        .values()
                        .for_each(|bc| builder.add_balance_change(bc))
                });
        });

    // Insert tick changes. An Algebra tick stays initialized, and bounds swap steps, while its
    // gross liquidity is positive, so a tick is created and deleted on its gross liquidity and
    // carries its net liquidity as the value, which may be zero.
    for tick_delta in ticks_map_deltas.deltas {
        let read = |key: String| {
            ticks_store
                .get_at(tick_delta.ordinal, key)
                .unwrap_or_else(BigInt::zero)
        };
        let net = read(net_liquidity_key(&tick_delta.pool_address, tick_delta.tick_index));
        let gross = read(gross_liquidity_key(&tick_delta.pool_address, tick_delta.tick_index));
        let gross_before =
            gross.clone() - BigInt::from_signed_bytes_be(&tick_delta.liquidity_gross_delta);

        let change = if gross.is_zero() {
            ChangeType::Deletion
        } else if gross_before.is_zero() {
            ChangeType::Creation
        } else {
            ChangeType::Update
        };
        let tx = tick_delta.transaction.unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        builder.add_entity_change(&EntityChanges {
            component_id: tick_delta.pool_address.to_hex(),
            attributes: vec![Attribute {
                name: format!("ticks/{}", tick_delta.tick_index),
                value: net.to_signed_bytes_be(),
                change: change.into(),
            }],
        });
    }

    // Insert liquidity changes
    pool_liquidity_store_deltas
        .deltas
        .into_iter()
        .zip(pool_liquidity_changes.changes)
        .for_each(|(store_delta, change)| {
            let new_value_bigint = BigInt::from_str(
                String::from_utf8(store_delta.new_value)
                    .unwrap()
                    .split(':')
                    .nth(1)
                    .unwrap(),
            )
            .unwrap();
            let tx = change.transaction.unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));

            builder.add_entity_change(&EntityChanges {
                component_id: change.pool_address.to_hex(),
                attributes: vec![Attribute {
                    name: "liquidity".to_string(),
                    value: new_value_bigint.to_bytes_be().1,
                    change: ChangeType::Update.into(),
                }],
            });
        });

    // Insert other attributes, and register the plugin's `beforeSwap` with the Dynamic Contract
    // Indexer whenever a pool enables it, so the plugin's code and storage (and the pool's) are
    // indexed for fee simulation. It is registered on `PluginConfig` rather than on `Plugin`
    // because the plugin's oracle is only initialized once the pool is, and a trace before that
    // reverts. It is registered again on every swap: a plugin that refuses swaps until a start
    // time fails its trace too, and the indexer traces a failed entrypoint again when it is
    // emitted again, while one it already traced costs it a lookup.
    let mut registered_entrypoints = HashSet::new();
    for event in events.pool_events {
        let typ = event.typ.unwrap();
        let tx = event.transaction.unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        let pool = hex::encode(&event.pool_address);

        let plugin_config = match &typ {
            Typ::PluginConfig(config) => Some(config.plugin_config),
            Typ::Swap(_) => pool_plugins_store
                .get_at(event.log_ordinal, pool_config_key(&event.pool_address))
                .and_then(|config| config.parse().ok()),
            _ => None,
        };
        if plugin_config.is_some_and(|config| config & BEFORE_SWAP_FLAG != 0) {
            let plugin = decode_plugin(pool_plugins_store.get_at(event.log_ordinal, &pool));
            if let Some(plugin) = plugin {
                if registered_entrypoints.insert((event.pool_address.clone(), plugin.clone())) {
                    let (entrypoint, params) = before_swap_entrypoint(&event.pool_address, plugin);
                    builder.add_entrypoint(&entrypoint);
                    builder.add_entrypoint_params(&params);
                }
            }
        }

        // A pool that switches plugin drops the detached plugin's settings. The new plugin's own
        // events are applied after this loop, so whatever it emits in this block stays.
        if let Typ::Plugin(plugin) = &typ {
            let previous = decode_plugin(pool_plugins_store.get_first(&pool));
            if previous.is_some_and(|previous| previous != plugin.plugin) {
                builder.add_entity_change(&EntityChanges {
                    component_id: event.pool_address.to_hex(),
                    attributes: plugin_settings_reset(),
                });
            }
        }

        let attributes = event_to_attributes_updates(typ);
        if attributes.is_empty() {
            continue;
        }
        builder.add_entity_change(&EntityChanges {
            component_id: event.pool_address.to_hex(),
            attributes,
        });
    }

    // Settings the pool's plugin emits from its own address.
    for event in plugin_events.pool_events {
        let tx = event.transaction.unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        builder.add_entity_change(&EntityChanges {
            component_id: event.pool_address.to_hex(),
            attributes: event_to_attributes_updates(event.typ.unwrap()),
        });
    }

    BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        storage_changes: get_block_storage_changes(&block),
    }
}

/// Entrypoint tracing the plugin's `beforeSwap` as the pool calls it: a one-wei zero-for-one
/// swap, which touches the same oracle, fee configuration and pool state as any other swap.
fn before_swap_entrypoint(pool: &[u8], plugin: Vec<u8>) -> (EntryPoint, EntryPointParams) {
    let calldata = BeforeSwap {
        sender: pool.to_vec(),
        recipient: pool.to_vec(),
        zero_to_one: true,
        amount_required: BigInt::from(1),
        limit_sqrt_price: BigInt::from(MIN_SQRT_RATIO_PLUS_ONE),
        with_payment_in_advance: false,
        data: vec![],
    }
    .encode();
    create_entrypoint(
        plugin,
        BEFORE_SWAP_SIGNATURE.to_string(),
        pool.to_hex(),
        TraceData::Rpc(RpcTraceData { caller: Some(pool.to_vec()), calldata }),
    )
}

/// A stored plugin address, or `None` when the store has no entry or holds the zero address.
fn decode_plugin(stored: Option<String>) -> Option<Vec<u8>> {
    stored
        .and_then(|plugin| hex::decode(plugin).ok())
        .filter(|plugin| plugin.iter().any(|byte| *byte != 0))
}

fn update_attribute(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_string(), value, change: ChangeType::Update.into() }
}

/// The attributes a pool's plugin sets for itself, cleared when the pool switches plugin.
fn plugin_settings_reset() -> Vec<Attribute> {
    vec![
        update_attribute("mevx_router", vec![]),
        update_attribute("min_gas_left", BigInt::from(0).to_bytes_be().1),
        update_attribute("call_gas_budget", BigInt::from(0).to_bytes_be().1),
        update_attribute("incentive", vec![]),
    ]
}

fn event_to_attributes_updates(ty: Typ) -> Vec<Attribute> {
    let update = update_attribute;
    match ty {
        Typ::Initialize(init) => vec![
            update("sqrt_price_x96", init.sqrt_price),
            update("tick", BigInt::from(init.tick).to_signed_bytes_be()),
        ],
        Typ::Swap(swap) => vec![
            update("sqrt_price_x96", swap.sqrt_price),
            update("tick", BigInt::from(swap.tick).to_signed_bytes_be()),
        ],
        Typ::Fee(fee) => vec![update("fee", BigInt::from(fee.fee).to_bytes_be().1)],
        Typ::Plugin(plugin) => vec![update("plugin", plugin.plugin)],
        Typ::PluginConfig(config) => {
            vec![update(
                "plugin_config",
                BigInt::from(config.plugin_config)
                    .to_bytes_be()
                    .1,
            )]
        }
        Typ::MevxRouter(router) => vec![update("mevx_router", router.router)],
        Typ::MinGasLeft(gas) => vec![update("min_gas_left", gas.min_gas_left)],
        Typ::CallGasBudget(gas) => vec![update("call_gas_budget", gas.call_gas_budget)],
        Typ::Incentive(incentive) => vec![update("incentive", incentive.incentive)],
        Typ::Mint(_) | Typ::Burn(_) | Typ::Collect(_) | Typ::Flash(_) | Typ::Skim(_) => vec![],
    }
}
