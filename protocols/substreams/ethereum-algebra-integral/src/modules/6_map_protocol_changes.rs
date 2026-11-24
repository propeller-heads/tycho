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
            beacon_bindings, beacon_pools_keys, followed_plugin_at, implementation_known_at,
            plugin_beacon_key, tracked_beacon_key,
        },
        store_pool_plugins::{
            decode_plugin, plugin_at, plugin_pool_key, pool_config_key, StringStore,
        },
    },
    pb::algebra::integral::v1::{events::pool_event::Typ, Events, LiquidityChanges, TickDeltas},
    plugin::{
        beacon::{beacon_upgrades, plugin_beacon},
        changes::{
            implementation_attribute, named_plugin_attributes, updated_plugin_attributes,
            PLUGIN_IMPL,
        },
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
    // The transactions in which a pool names a plugin they created, whose writes are emitted
    // as creations.
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
            let change = trace_of(tx.index).and_then(|trace| {
                naming_change(
                    trace,
                    &event.pool_address,
                    &named.plugin,
                    event.log_ordinal,
                    &modules,
                    &pool_plugins_store,
                    &plugin_beacons_store,
                )
            });
            if let Some(change) = change {
                if change.followed {
                    if let Ok(index) = u32::try_from(tx.index) {
                        namings.insert((index, named.plugin.clone()));
                    }
                }
                apply_naming_change(builder, &event.pool_address.to_hex(), change);
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
            if !follows_after(&plugin_beacons_store, trace, &pool, &plugin) {
                continue;
            }
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
    // It stays unknown until the pool names another plugin.
    for binding in beacon_bindings(&block, &pool_plugins_store) {
        if namings.contains(&(binding.tx_index, binding.plugin.clone())) {
            continue;
        }
        let Some(trace) = trace_of(u64::from(binding.tx_index)) else {
            continue;
        };
        if !follows_after(&plugin_beacons_store, trace, &binding.pool, &binding.plugin) {
            continue;
        }
        let had_implementation = implementation_known_at(
            &plugin_beacons_store,
            binding.ordinal.saturating_sub(1),
            &binding.plugin,
        );
        let attribute = match (plugin_beacon(trace, &binding.plugin), had_implementation) {
            (Some(bound), true) => {
                implementation_attribute(&bound.implementation, ChangeType::Update)
            }
            (Some(bound), false) => {
                implementation_attribute(&bound.implementation, ChangeType::Creation)
            }
            (None, true) => implementation_attribute(&[], ChangeType::Deletion),
            (None, false) => continue,
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
    // bound to it and the pool has a `plugin_impl` to update.
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
                let Some(plugin) = decode_plugin(pool_plugins_store.get_at(upgrade.ordinal, &pool))
                else {
                    continue;
                };
                let bound_to_beacon = plugin_beacons_store
                    .get_at(upgrade.ordinal, plugin_beacon_key(&plugin))
                    .is_some_and(|current| current == beacon);
                let Ok(pool_address) = hex::decode(&pool) else {
                    continue;
                };
                if !bound_to_beacon ||
                    !follows_after(&plugin_beacons_store, trace, &pool_address, &plugin) ||
                    !implementation_known_at(&plugin_beacons_store, upgrade.ordinal, &plugin)
                {
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

/// Whether `pool` carries `plugin`'s attributes when `trace` ends. A change `plugin` makes
/// earlier in a transaction that ends with the pool on another plugin, or paused, is not
/// emitted: it would bring back an attribute the switch deleted.
fn follows_after(
    plugin_beacons: &impl StringStore,
    trace: &eth::TransactionTrace,
    pool: &[u8],
    plugin: &[u8],
) -> bool {
    followed_plugin_at(plugin_beacons, trace.end_ordinal, pool).as_deref() == Some(plugin)
}

/// What a `Plugin` event that changes a pool's plugin does to the pool, besides its `plugin`
/// attribute.
#[derive(Debug, PartialEq)]
struct NamingChange {
    /// Deletions of what the pool carried for the old plugin and, when the transaction created
    /// the new plugin, its implementation and module fields.
    attributes: Vec<Attribute>,
    /// `Some(true)` when the pool becomes paused, `Some(false)` when it stops being paused.
    pause: Option<bool>,
    /// Whether the pool now carries the new plugin's attributes.
    followed: bool,
}

/// The change the `Plugin` event at `ordinal` makes to `pool`, which names `named` from then
/// on. `None` when `named` is the plugin the pool already names: nothing it carries changes.
///
/// A deletion is emitted only for an attribute the pool has, which the stores tell: storage
/// rejects the deletion of an attribute it holds no row for. A pool has module fields, and a
/// `plugin_impl` if its plugin's implementation is known, while it follows a plugin; it has
/// `paused` while it names a plugin it does not follow.
///
/// Deployment assumption: a plugin is created in the transaction that names it to its pool, as
/// a plugin factory does when it creates both. The configuration of a plugin created earlier is
/// not known, so its pool is paused until it names a plugin created in the naming transaction.
fn naming_change(
    trace: &eth::TransactionTrace,
    pool: &[u8],
    named: &[u8],
    ordinal: u64,
    modules: &[&PluginModule],
    pool_plugins: &impl StringStore,
    plugin_beacons: &impl StringStore,
) -> Option<NamingChange> {
    let before = ordinal.saturating_sub(1);
    let old_plugin = plugin_at(pool_plugins, before, pool);
    let new_plugin = decode_plugin(Some(hex::encode(named)));
    if old_plugin == new_plugin {
        return None;
    }

    let was_followed =
        old_plugin.is_some() && followed_plugin_at(plugin_beacons, before, pool) == old_plugin;
    let was_paused = old_plugin.is_some() && !was_followed;
    let mut carried: HashSet<&str> = HashSet::new();
    if was_followed {
        carried.extend(modules.iter().flat_map(|module| {
            module
                .fields
                .iter()
                .map(|field| field.name())
        }));
        if old_plugin
            .as_ref()
            .is_some_and(|plugin| implementation_known_at(plugin_beacons, before, plugin))
        {
            carried.insert(PLUGIN_IMPL);
        }
    }

    let created = new_plugin
        .as_ref()
        .and_then(|plugin| named_plugin_attributes(trace, plugin, modules));
    let followed = created.is_some();
    let paused = new_plugin.is_some() && !followed;

    // What the new plugin sets is an update of an attribute the pool had when the transaction
    // began, and a creation otherwise, whatever the transaction did to it before this event.
    let before_trace = trace.begin_ordinal.saturating_sub(1);
    let first_plugin = followed_plugin_at(plugin_beacons, before_trace, pool);
    let had_fields = first_plugin.is_some();
    let had_implementation = first_plugin
        .as_ref()
        .is_some_and(|plugin| implementation_known_at(plugin_beacons, before_trace, plugin));
    let mut attributes: Vec<Attribute> = created
        .unwrap_or_default()
        .into_iter()
        .map(|attribute| {
            carried.remove(attribute.name.as_str());
            let existed =
                if attribute.name == PLUGIN_IMPL { had_implementation } else { had_fields };
            let change = if existed { ChangeType::Update } else { ChangeType::Creation };
            Attribute { change: change.into(), ..attribute }
        })
        .collect();
    attributes.extend(
        carried
            .into_iter()
            .sorted()
            .map(|name| Attribute {
                name: name.to_string(),
                value: Vec::new(),
                change: ChangeType::Deletion.into(),
            }),
    );

    let pause = match (was_paused, paused) {
        (false, true) => Some(true),
        (true, false) => Some(false),
        (true, true) | (false, false) => None,
    };
    Some(NamingChange { attributes, pause, followed })
}

fn apply_naming_change(
    builder: &mut TransactionChangesBuilder,
    component_id: &str,
    change: NamingChange,
) {
    if !change.attributes.is_empty() {
        builder.add_entity_change(&EntityChanges {
            component_id: component_id.to_string(),
            attributes: change.attributes,
        });
    }
    if let Some(paused) = change.pause {
        builder.change_component_pause_state(component_id, paused);
    }
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
    use crate::{
        modules::{
            store_plugin_beacons::plugin_beacon_writes,
            store_pool_plugins::{pool_plugin_writes, StoreWrite},
        },
        pb::algebra::integral::v1::events::{pool_event, PoolEvent},
    };

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

    /// A string store over a map: the values before the block and the block's writes.
    #[derive(Default)]
    struct MapStore {
        committed: HashMap<String, String>,
        block: Vec<StoreWrite>,
    }

    impl StringStore for MapStore {
        fn value_at(&self, ordinal: u64, key: &str) -> Option<String> {
            self.block
                .iter()
                .rfind(|write| write.ordinal <= ordinal && write.key == key)
                .map(|write| write.value.clone())
                .or_else(|| self.committed.get(key).cloned())
        }
    }

    impl MapStore {
        fn commit(&mut self) {
            for write in self.block.drain(..) {
                self.committed
                    .insert(write.key, write.value);
            }
        }
    }

    const POOL: [u8; 20] = [0x11; 20];
    const PLUGIN_X: [u8; 20] = [0xa1; 20];
    const PLUGIN_Y: [u8; 20] = [0xa2; 20];
    const NO_PLUGIN: [u8; 20] = [0; 20];
    const BEACON: [u8; 20] = [0x44; 20];
    const IMPLEMENTATION: [u8; 20] = [0x65; 20];
    const BASE_FEE_500: [u8; 32] =
        hex_literal::hex!("000000000000000000000000000001f400010001000000000000000000000000");
    const BASE_FEE_800: [u8; 32] =
        hex_literal::hex!("0000000000000000000000000000032000010001000000000000000000000000");

    fn address_word(address: &[u8; 20]) -> Vec<u8> {
        [[0u8; 12].as_slice(), address].concat()
    }

    /// The calls of a plugin factory creating `plugin` as a proxy of [`BEACON`], or as a plain
    /// contract, and writing its fee configuration.
    fn creation_calls(plugin: [u8; 20], beacon_proxy: bool, fee_word: [u8; 32]) -> Vec<eth::Call> {
        let fee_slot = modules_named("dynamic_fee").expect("dynamic_fee is indexed")[0].fields[0]
            .slot()
            .to_vec();
        let mut create = call(CallType::Create, plugin, false);
        create.storage_changes = vec![eth::StorageChange {
            address: plugin.to_vec(),
            key: fee_slot,
            new_value: fee_word.to_vec(),
            ordinal: 2,
            ..Default::default()
        }];
        if !beacon_proxy {
            return vec![create];
        }
        create.logs = vec![eth::Log {
            address: plugin.to_vec(),
            topics: vec![
                hex_literal::hex!(
                    "1cf3b03a6cf19fa2baba4df148e9dcabedea7f8a5c07840e207e5c089be95d3e"
                )
                .to_vec(),
                address_word(&BEACON),
            ],
            ordinal: 1,
            ..Default::default()
        }];
        let read_beacon = eth::Call {
            caller: plugin.to_vec(),
            address: BEACON.to_vec(),
            input: hex_literal::hex!("5c60da1b").to_vec(),
            return_data: address_word(&IMPLEMENTATION),
            ..Default::default()
        };
        vec![create, read_beacon]
    }

    /// One transaction: its calls and the plugins [`POOL`] names in it, in order.
    struct Tx {
        calls: Vec<eth::Call>,
        names: Vec<[u8; 20]>,
    }

    fn names(plugin: [u8; 20]) -> Tx {
        Tx { calls: Vec::new(), names: vec![plugin] }
    }

    fn creates_and_names(plugin: [u8; 20], beacon_proxy: bool, fee_word: [u8; 32]) -> Tx {
        Tx { calls: creation_calls(plugin, beacon_proxy, fee_word), names: vec![plugin] }
    }

    /// The two stores, and the attribute rows storage would hold for [`POOL`].
    #[derive(Default)]
    struct Indexed {
        pool_plugins: MapStore,
        plugin_beacons: MapStore,
        rows: HashMap<String, Vec<u8>>,
    }

    impl Indexed {
        /// Indexes one block as the store and map modules do, and applies the pool's attribute
        /// changes to the rows as storage does: a creation needs the row to be absent, an
        /// update and a deletion need it to be present.
        fn block(&mut self, transactions: Vec<Tx>) -> &mut Self {
            const ORDINALS_PER_TX: u64 = 100;
            let modules = modules_named("dynamic_fee").expect("dynamic_fee is indexed");
            let mut traces = Vec::new();
            let mut pool_events = Vec::new();
            for (index, transaction) in transactions.into_iter().enumerate() {
                let index = u32::try_from(index).expect("few transactions");
                let begin = u64::from(index) * ORDINALS_PER_TX;
                let mut calls = transaction.calls;
                for call in &mut calls {
                    for log in &mut call.logs {
                        log.ordinal += begin;
                    }
                    for change in &mut call.storage_changes {
                        change.ordinal += begin;
                    }
                }
                let trace = eth::TransactionTrace {
                    index,
                    hash: vec![u8::try_from(index).expect("few transactions")],
                    calls,
                    begin_ordinal: begin,
                    end_ordinal: begin + ORDINALS_PER_TX - 1,
                    ..Default::default()
                };
                for (position, plugin) in transaction.names.iter().enumerate() {
                    pool_events.push(PoolEvent {
                        log_ordinal: begin + 10 + position as u64,
                        pool_address: POOL.to_vec(),
                        transaction: Some((&trace).into()),
                        typ: Some(Typ::Plugin(pool_event::Plugin { plugin: plugin.to_vec() })),
                    });
                }
                traces.push(trace);
            }
            let block = eth::Block { transaction_traces: traces, ..Default::default() };
            let events = Events { pool_events };
            self.pool_plugins.block = pool_plugin_writes(&events);
            self.plugin_beacons.block = plugin_beacon_writes(&block, &events, &self.pool_plugins);

            let component_id = POOL.to_vec().to_hex();
            for trace in &block.transaction_traces {
                let transaction: Transaction = trace.into();
                let mut builder = TransactionChangesBuilder::new(&transaction);
                for event in &events.pool_events {
                    let Some(Typ::Plugin(named)) = &event.typ else { continue };
                    if event.transaction.as_ref() != Some(&transaction) {
                        continue;
                    }
                    let change = naming_change(
                        trace,
                        &event.pool_address,
                        &named.plugin,
                        event.log_ordinal,
                        &modules,
                        &self.pool_plugins,
                        &self.plugin_beacons,
                    );
                    if let Some(change) = change {
                        assert_eq!(
                            change.followed,
                            followed_plugin_at(&self.plugin_beacons, event.log_ordinal, &POOL)
                                .is_some(),
                            "the store and the map disagree on whether the pool follows its plugin"
                        );
                        apply_naming_change(&mut builder, &component_id, change);
                    }
                }
                let attributes = builder
                    .build()
                    .into_iter()
                    .flat_map(|changes| changes.entity_changes)
                    .flat_map(|changes| changes.attributes);
                for attribute in attributes {
                    let name = attribute.name.clone();
                    match attribute.change() {
                        ChangeType::Creation => {
                            let previous = self
                                .rows
                                .insert(name.clone(), attribute.value);
                            assert!(previous.is_none(), "{name} is created but exists");
                        }
                        ChangeType::Update => {
                            let previous = self
                                .rows
                                .insert(name.clone(), attribute.value);
                            assert!(previous.is_some(), "{name} is updated but missing");
                        }
                        ChangeType::Deletion => {
                            let previous = self.rows.remove(&name);
                            assert!(previous.is_some(), "{name} is deleted but missing");
                        }
                        ChangeType::Unspecified => panic!("{name} has no change type"),
                    }
                }
            }
            self.pool_plugins.commit();
            self.plugin_beacons.commit();
            self
        }

        fn row_names(&self) -> Vec<&str> {
            self.rows
                .keys()
                .map(String::as_str)
                .sorted()
                .collect()
        }

        fn base_fee(&self) -> &[u8] {
            &self.rows["dynamic_fee/base_fee"]
        }
    }

    const FOLLOWED_PROXY: [&str; 4] =
        ["dynamic_fee/alpha1", "dynamic_fee/alpha2", "dynamic_fee/base_fee", "plugin_impl"];
    const FOLLOWED_PLAIN: [&str; 3] =
        ["dynamic_fee/alpha1", "dynamic_fee/alpha2", "dynamic_fee/base_fee"];

    #[test]
    fn pool_named_a_plugin_created_in_the_transaction_carries_its_attributes() {
        let mut indexed = Indexed::default();

        indexed.block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)]);

        assert_eq!(indexed.row_names(), FOLLOWED_PROXY);
        assert_eq!(indexed.base_fee(), [0x01, 0xf4]);
        assert_eq!(indexed.rows["plugin_impl"], IMPLEMENTATION);
    }

    /// Nami's batched switch: the factory creates the new plugin and the pool names it in one
    /// transaction, on a pool created in an earlier block.
    #[test]
    fn switch_on_a_live_pool_updates_what_it_carries_and_never_touches_paused() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![creates_and_names(PLUGIN_Y, true, BASE_FEE_800)]);

        assert_eq!(indexed.row_names(), FOLLOWED_PROXY);
        assert_eq!(indexed.base_fee(), [0x03, 0x20]);
    }

    #[test]
    fn switch_to_a_plugin_without_a_beacon_deletes_only_the_implementation() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![creates_and_names(PLUGIN_Y, false, BASE_FEE_800)]);
        assert_eq!(indexed.row_names(), FOLLOWED_PLAIN);

        // The old plugin had no `plugin_impl`, so none is deleted on the way back to a proxy.
        indexed.block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)]);
        assert_eq!(indexed.row_names(), FOLLOWED_PROXY);
    }

    #[test]
    fn naming_no_plugin_deletes_what_the_pool_carries() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![names(NO_PLUGIN)]);

        assert!(indexed.rows.is_empty(), "{:?}", indexed.row_names());
    }

    #[test]
    fn naming_no_plugin_on_a_pool_without_one_emits_nothing() {
        let mut indexed = Indexed::default();

        indexed.block(vec![names(NO_PLUGIN)]);

        assert!(indexed.rows.is_empty());
    }

    #[test]
    fn naming_the_current_plugin_again_changes_nothing() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![names(PLUGIN_X)]);
        assert_eq!(indexed.row_names(), FOLLOWED_PROXY);

        // The pool still follows the plugin: a later switch deletes its attributes.
        indexed.block(vec![names(NO_PLUGIN)]);
        assert!(indexed.rows.is_empty(), "{:?}", indexed.row_names());
    }

    #[test]
    fn pool_named_a_plugin_created_earlier_is_paused() {
        let mut indexed = Indexed::default();

        indexed.block(vec![names(PLUGIN_X)]);
        assert_eq!(indexed.row_names(), ["paused"]);

        // Another plugin created earlier leaves it paused, with nothing emitted.
        indexed.block(vec![names(PLUGIN_Y)]);
        assert_eq!(indexed.row_names(), ["paused"]);
    }

    #[test]
    fn paused_pool_resumes_when_it_names_a_plugin_created_in_the_transaction() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![names(PLUGIN_X)])
            .block(vec![creates_and_names(PLUGIN_Y, true, BASE_FEE_800)]);

        assert_eq!(indexed.row_names(), FOLLOWED_PROXY);
    }

    #[test]
    fn paused_pool_that_names_no_plugin_is_unpaused() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![names(PLUGIN_X)])
            .block(vec![names(NO_PLUGIN)]);

        assert!(indexed.rows.is_empty(), "{:?}", indexed.row_names());
    }

    #[test]
    fn switch_away_and_back_in_one_block_pauses_the_pool() {
        // The pool stops following X when it names Y, and X is not created when it is named
        // again, so what it wrote in between is not known.
        let mut in_two_transactions = Indexed::default();
        in_two_transactions
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![names(PLUGIN_Y), names(PLUGIN_X)]);
        assert_eq!(in_two_transactions.row_names(), ["paused"]);

        let mut in_one_transaction = Indexed::default();
        in_one_transaction
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![Tx { calls: Vec::new(), names: vec![PLUGIN_Y, PLUGIN_X] }]);
        assert_eq!(in_one_transaction.row_names(), ["paused"]);
    }

    #[test]
    fn two_switches_in_one_block_are_each_compared_with_the_plugin_before_them() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![
                creates_and_names(PLUGIN_Y, false, BASE_FEE_800),
                names(NO_PLUGIN),
                creates_and_names(PLUGIN_X, true, BASE_FEE_500),
            ]);

        assert_eq!(indexed.row_names(), FOLLOWED_PROXY);
        assert_eq!(indexed.base_fee(), [0x01, 0xf4]);
    }

    #[test]
    fn plugin_created_and_replaced_in_one_transaction_leaves_only_the_last() {
        let mut indexed = Indexed::default();
        let mut calls = creation_calls(PLUGIN_X, true, BASE_FEE_500);
        calls.extend(creation_calls(PLUGIN_Y, false, BASE_FEE_800));

        indexed.block(vec![Tx { calls, names: vec![PLUGIN_X, PLUGIN_Y] }]);

        assert_eq!(indexed.row_names(), FOLLOWED_PLAIN);
        assert_eq!(indexed.base_fee(), [0x03, 0x20]);
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
