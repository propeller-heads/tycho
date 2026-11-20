use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    vec,
};

use itertools::Itertools;
use serde::Deserialize;
use substreams::{
    pb::substreams::StoreDeltas,
    scalar::BigInt,
    store::{StoreGet, StoreGetArray, StoreGetBigInt, StoreGetString},
};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::hex::Hexable;
use tycho_substreams::{
    balances::aggregate_balances_changes,
    block_storage::get_block_storage_changes,
    entrypoint::create_entrypoint,
    prelude::{entry_point_params::TraceData, *},
};

use crate::{
    abi::plugin::functions::BeforeSwap,
    modules::{
        map_store_ticks::{gross_liquidity_key, net_liquidity_key},
        store_plugin_beacons::{
            beacon_bindings, beacon_pools_keys, plugin_beacon_key, tracked_beacon_key,
        },
        store_pool_plugins::{plugin_pool_key, pool_config_key},
    },
    pb::algebra::integral::v1::{events::pool_event::Typ, Events, LiquidityChanges, TickDeltas},
    plugin::{
        beacon::{beacon_upgrades, plugin_beacon},
        changes::{implementation_attribute, named_plugin_attributes, updated_plugin_attributes},
        modules::{lists_slot, modules_named, PluginModule},
    },
};

/// Pool calls the plugin's `beforeSwap` when this `pluginConfig` flag is set.
const BEFORE_SWAP_FLAG: u32 = 1;
const BEFORE_SWAP_SIGNATURE: &str = "beforeSwap(address,address,bool,int256,uint160,bool,bytes)";
/// `TickMath.MIN_SQRT_RATIO + 1`: the loosest price limit of a zero-for-one swap.
const MIN_SQRT_RATIO_PLUS_ONE: u64 = 4_295_128_740;

/// What differs between the chains this package is deployed on.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Params {
    /// Whether the chain's indexer runs the Dynamic Contract Indexer. With it, each plugin's
    /// `beforeSwap` is registered as an entrypoint and the block's storage changes are emitted,
    /// so a plugin with no native handler can be simulated in the VM.
    dci: bool,
    /// The plugin modules the deployment's plugins are built from, separated by commas: see
    /// [`modules_named`].
    #[serde(default)]
    modules: String,
}

#[substreams::handlers::map]
#[allow(clippy::too_many_arguments)]
pub fn map_protocol_changes(
    params: String,
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
    plugin_beacons_store: StoreGetString,
    beacon_pools_store: StoreGetArray<String>,
) -> BlockChanges {
    let params: Params = serde_qs::from_str(&params).expect("invalid map_protocol_changes params");
    // A manifest naming a module the package lacks must not run: see `modules_named`.
    let modules = modules_named(&params.modules).unwrap_or_else(|error| panic!("{error}"));
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
        let change = tick_change_type(
            &gross,
            &BigInt::from_signed_bytes_be(&tick_delta.liquidity_gross_delta),
        );
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

    // Pool events. With DCI, the plugin's `beforeSwap` is also registered as an entrypoint
    // whenever a pool enables it, so the plugin's code and storage (and the pool's) are indexed
    // for VM simulation. It is registered on `PluginConfig` rather than on `Plugin` because the
    // plugin's oracle is only initialized once the pool is, and a trace before that reverts. It
    // is registered again on every swap, on purpose: a plugin that refuses swaps until a start
    // time fails its trace, and the indexer retraces a failed entrypoint each time it is emitted
    // again, outside its retry cap, so the pool is picked up once trading opens. An entrypoint
    // already traced costs the indexer a few database statements per swap.
    let mut registered_entrypoints = HashSet::new();
    // The transactions in which a pool names a plugin, whose writes are emitted as creations.
    let mut namings: HashSet<(u32, Vec<u8>)> = HashSet::new();
    let trace_of = |index: u64| {
        block
            .transaction_traces
            .iter()
            .find(|trace| u64::from(trace.index) == index)
    };

    for event in events.pool_events {
        let typ = event.typ.unwrap();
        let tx = event.transaction.unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        let pool = hex::encode(&event.pool_address);

        if params.dci {
            let plugin_config = match &typ {
                Typ::PluginConfig(config) => Some(config.plugin_config),
                Typ::Swap(_) => pool_plugins_store
                    .get_at(event.log_ordinal, pool_config_key(&event.pool_address))
                    .and_then(|config| config.parse().ok()),
                // Spelled out so a new event type must be classified here.
                Typ::Initialize(_) |
                Typ::Mint(_) |
                Typ::Collect(_) |
                Typ::Burn(_) |
                Typ::Flash(_) |
                Typ::Skim(_) |
                Typ::Fee(_) |
                Typ::Plugin(_) => None,
            };
            if plugin_config.is_some_and(|config| config & BEFORE_SWAP_FLAG != 0) {
                let plugin = decode_plugin(pool_plugins_store.get_at(event.log_ordinal, &pool));
                if let Some(plugin) = plugin {
                    if registered_entrypoints.insert((event.pool_address.clone(), plugin.clone())) {
                        let (entrypoint, params) =
                            before_swap_entrypoint(&event.pool_address, plugin);
                        builder.add_entrypoint(&entrypoint);
                        builder.add_entrypoint_params(&params);
                    }
                }
            }
        }

        if let Typ::Plugin(named) = &typ {
            let component_id = event.pool_address.to_hex();
            let previous = decode_plugin(pool_plugins_store.get_first(&pool));
            let switched = previous
                .as_ref()
                .is_some_and(|previous| previous != &named.plugin);
            let mut attributes = Vec::new();
            if switched {
                attributes.extend(detached_plugin_attributes(&modules));
            }
            let plugin_set = named
                .plugin
                .iter()
                .any(|byte| *byte != 0);
            let mut paused = false;
            if plugin_set {
                if let Some(trace) = trace_of(tx.index) {
                    // Deployment assumption: a plugin is created in the transaction that names
                    // it to its pool, as a plugin factory does when it creates both. The
                    // configuration of a plugin created earlier is not known, so its pool is
                    // paused until it names a plugin created in the naming transaction.
                    match named_plugin_attributes(trace, &named.plugin, &modules) {
                        Some(named_attributes) => attributes.extend(named_attributes),
                        None => paused = true,
                    }
                    namings.insert((trace.index, named.plugin.clone()));
                }
            }
            if !attributes.is_empty() {
                builder.add_entity_change(&EntityChanges {
                    component_id: component_id.clone(),
                    attributes,
                });
            }
            if paused {
                builder.change_component_pause_state(&component_id, true);
            } else if switched {
                // The replaced plugin may have been the reason the pool was paused.
                builder.change_component_pause_state(&component_id, false);
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

    // Module configuration a pool's plugin wrote after it was named. The slot filter comes
    // first: a swap writes only the plugin's volatility oracle, which no module lists, so it
    // costs no store read.
    for trace in block.transactions() {
        for (plugin, ordinal) in module_slot_writers(trace, &modules) {
            if namings.contains(&(trace.index, plugin.clone())) {
                continue;
            }
            let Some(pool) = current_pool_of(&pool_plugins_store, &plugin, ordinal) else {
                continue;
            };
            let attributes = updated_plugin_attributes(trace, &plugin, &modules);
            if attributes.is_empty() {
                continue;
            }
            let tx: Transaction = trace.into();
            transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx))
                .add_entity_change(&EntityChanges { component_id: pool.to_hex(), attributes });
        }
    }

    // A plugin bound to another beacon after it was named runs that beacon's implementation.
    // The proxy reads the implementation from the beacon it is bound to, in the same
    // transaction; when the trace does not show that read the implementation is unknown and
    // the attribute is removed, which stops quotes for the pool where no VM fallback exists.
    for binding in beacon_bindings(&block, &pool_plugins_store) {
        if namings.contains(&(binding.tx_index, binding.plugin.clone())) {
            continue;
        }
        let Some(trace) = trace_of(u64::from(binding.tx_index)) else {
            continue;
        };
        let attribute = match plugin_beacon(trace, &binding.plugin) {
            Some(bound) => implementation_attribute(&bound.implementation, ChangeType::Update),
            None => implementation_attribute(&[], ChangeType::Deletion),
        };
        let tx: Transaction = trace.into();
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_entity_change(&EntityChanges {
                component_id: binding.pool.to_hex(),
                attributes: vec![attribute],
            });
    }

    // A beacon's upgrade changes the implementation of every pool whose plugin is bound to it.
    // Proxies of every kind log `Upgraded`, so only beacons some pool's plugin was bound to are
    // followed, and a pool listed under a beacon counts only while its current plugin is still
    // bound to it.
    for trace in block.transactions() {
        for upgrade in beacon_upgrades(trace) {
            if plugin_beacons_store
                .get_at(upgrade.ordinal, tracked_beacon_key(&upgrade.beacon))
                .is_none()
            {
                continue;
            }
            let beacon = hex::encode(&upgrade.beacon);
            let mut pools = HashSet::new();
            for key in beacon_pools_keys(&upgrade.beacon) {
                pools.extend(
                    beacon_pools_store
                        .get_at(upgrade.ordinal, key)
                        .unwrap_or_default(),
                );
            }
            let tx: Transaction = trace.into();
            for pool in pools.into_iter().sorted() {
                let bound_to_beacon =
                    decode_plugin(pool_plugins_store.get_at(upgrade.ordinal, &pool))
                        .and_then(|plugin| {
                            plugin_beacons_store.get_at(upgrade.ordinal, plugin_beacon_key(&plugin))
                        })
                        .is_some_and(|current| current == beacon);
                if !bound_to_beacon {
                    continue;
                }
                transaction_changes
                    .entry(tx.index)
                    .or_insert_with(|| TransactionChangesBuilder::new(&tx))
                    .add_entity_change(&EntityChanges {
                        component_id: format!("0x{pool}"),
                        attributes: vec![implementation_attribute(
                            &upgrade.implementation,
                            ChangeType::Update,
                        )],
                    });
            }
        }
    }

    BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        // Only the Dynamic Contract Indexer reads the block's storage changes.
        storage_changes: if params.dci { get_block_storage_changes(&block) } else { Vec::new() },
    }
}

/// The contracts that wrote a slot one of `modules` lists in `trace`, outside reverted calls,
/// each with the ordinal of its first such write.
fn module_slot_writers(
    trace: &eth::TransactionTrace,
    modules: &[&PluginModule],
) -> Vec<(Vec<u8>, u64)> {
    let mut writers: Vec<(Vec<u8>, u64)> = Vec::new();
    for call in &trace.calls {
        if call.state_reverted {
            continue;
        }
        for change in &call.storage_changes {
            if !lists_slot(modules, &change.key) {
                continue;
            }
            match writers
                .iter_mut()
                .find(|(address, _)| *address == change.address)
            {
                Some((_, ordinal)) => *ordinal = (*ordinal).min(change.ordinal),
                None => writers.push((change.address.clone(), change.ordinal)),
            }
        }
    }
    writers
}

/// The pool whose plugin was `plugin` at `ordinal`, if any.
fn current_pool_of(
    pool_plugins_store: &StoreGetString,
    plugin: &[u8],
    ordinal: u64,
) -> Option<Vec<u8>> {
    let pool = pool_plugins_store.get_at(ordinal, plugin_pool_key(plugin))?;
    if pool_plugins_store.get_at(ordinal, &pool)? != hex::encode(plugin) {
        return None;
    }
    hex::decode(pool).ok()
}

/// Deletions of everything a pool carries about a plugin it no longer names: the implementation
/// and every attribute of the deployment's `modules`. Attributes the new plugin sets in the same
/// transaction are added after and replace these.
fn detached_plugin_attributes(modules: &[&PluginModule]) -> Vec<Attribute> {
    let mut attributes = vec![implementation_attribute(&[], ChangeType::Deletion)];
    for module in modules {
        for field in module.fields {
            attributes.push(Attribute {
                name: field.name().to_string(),
                value: Vec::new(),
                change: ChangeType::Deletion.into(),
            });
        }
    }
    attributes
}

/// Entrypoint tracing the plugin's `beforeSwap` as the pool calls it: a one-wei zero-for-one
/// swap, which touches the same oracle, fee configuration and pool state as any other swap. The
/// other direction reads its premium and override from the same fee-override layout and reaches
/// the same fee source, so one direction loads everything the other reads.
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

/// A tick is created when its gross liquidity turns positive and deleted when it returns to
/// zero, whatever its net liquidity does.
fn tick_change_type(gross: &BigInt, gross_delta: &BigInt) -> ChangeType {
    if gross.is_zero() {
        ChangeType::Deletion
    } else if (gross.clone() - gross_delta.clone()).is_zero() {
        ChangeType::Creation
    } else {
        ChangeType::Update
    }
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
        Typ::Mint(_) | Typ::Burn(_) | Typ::Collect(_) | Typ::Flash(_) | Typ::Skim(_) => vec![],
    }
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::CallType;

    use super::*;

    #[test]
    fn test_tick_is_created_on_first_gross_liquidity_and_deleted_on_last() {
        let big = |value: i64| BigInt::from(value);

        assert_eq!(tick_change_type(&big(1_000), &big(1_000)), ChangeType::Creation);
        assert_eq!(tick_change_type(&big(1_500), &big(500)), ChangeType::Update);
        assert_eq!(tick_change_type(&big(1_000), &big(-500)), ChangeType::Update);
        assert_eq!(tick_change_type(&big(0), &big(-1_000)), ChangeType::Deletion);
    }

    #[test]
    fn params_say_whether_the_chain_runs_dci_and_name_the_modules() {
        let params: Params = serde_qs::from_str("dci=true&modules=dynamic_fee,fee_override")
            .expect("the params should parse");

        assert_eq!(params, Params { dci: true, modules: "dynamic_fee,fee_override".to_string() });
        assert!(serde_qs::from_str::<Params>("dci=true&factory=0x00").is_err());
    }

    #[test]
    fn manifest_modules_resolve() {
        for names in [
            "dynamic_fee,farming,fee_override,mevx",
            "dynamic_fee,farming,fee_override,mevx,anti_sniper",
        ] {
            modules_named(names).expect("the manifests' modules should resolve");
        }
    }

    const PLUGIN: [u8; 20] = [0xb0; 20];
    const OTHER: [u8; 20] = [0xc0; 20];

    fn call(call_type: CallType, address: [u8; 20], state_reverted: bool) -> eth::Call {
        eth::Call {
            call_type: call_type.into(),
            address: address.to_vec(),
            state_reverted,
            ..Default::default()
        }
    }

    fn write(address: [u8; 20], slot: &[u8; 32], ordinal: u64) -> eth::StorageChange {
        eth::StorageChange {
            address: address.to_vec(),
            key: slot.to_vec(),
            new_value: vec![1],
            ordinal,
            ..Default::default()
        }
    }

    #[test]
    fn only_writes_to_module_slots_outside_reverted_calls_name_a_writer() {
        let modules = modules_named("dynamic_fee").expect("dynamic_fee is indexed");
        let module_slot = modules[0].fields[0].slot();
        // The volatility oracle's header slot, which no module lists.
        let oracle_slot = [0x4c; 32];
        let mut live = call(CallType::Call, PLUGIN, false);
        live.storage_changes = vec![
            write(PLUGIN, &oracle_slot, 5),
            write(PLUGIN, module_slot, 9),
            write(PLUGIN, module_slot, 7),
            write(OTHER, &oracle_slot, 8),
        ];
        let mut reverted = call(CallType::Call, OTHER, true);
        reverted.storage_changes = vec![write(OTHER, module_slot, 3)];
        let trace = eth::TransactionTrace { calls: vec![live, reverted], ..Default::default() };

        assert_eq!(module_slot_writers(&trace, &modules), vec![(PLUGIN.to_vec(), 7)]);
        assert!(module_slot_writers(&trace, &[]).is_empty());
    }

    #[test]
    fn detached_plugin_loses_its_implementation_and_every_module_attribute() {
        let modules = modules_named("dynamic_fee,fee_override").expect("both are indexed");
        let attributes = detached_plugin_attributes(&modules);

        let fields: usize = modules
            .iter()
            .map(|module| module.fields.len())
            .sum();
        assert_eq!(attributes.len(), fields + 1);
        assert_eq!(attributes[0].name, crate::plugin::changes::PLUGIN_IMPL);
        for attribute in &attributes {
            assert_eq!(attribute.change(), ChangeType::Deletion, "{}", attribute.name);
        }
        assert!(attributes
            .iter()
            .any(|attribute| attribute.name == "fee_override/source"));
    }

    #[test]
    fn pool_events_become_their_attributes() {
        use crate::pb::algebra::integral::v1::events::pool_event;

        let attributes =
            event_to_attributes_updates(Typ::Plugin(pool_event::Plugin { plugin: vec![0xAA; 20] }));
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].name, "plugin");
        assert_eq!(attributes[0].value, vec![0xAA; 20]);

        let attributes = event_to_attributes_updates(Typ::PluginConfig(pool_event::PluginConfig {
            plugin_config: 195,
        }));
        assert_eq!(attributes[0].name, "plugin_config");
        assert_eq!(attributes[0].value, vec![195]);
    }
}
