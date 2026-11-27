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
            plugin_beacon_key, trace_at, tracked_beacon_key, ListStore,
        },
        store_pool_plugins::{
            decode_plugin, plugin_pool_key, plugin_switch, PluginSwitch, StringStore,
        },
    },
    pb::algebra::integral::v1::{
        events::{pool_event::Typ, PoolEvent},
        Events, LiquidityChanges, TickDeltas,
    },
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
    /// [`modules_named`]. Required: without it no module attribute is indexed and simulation
    /// rejects every pool that has a plugin.
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
    // plugin's oracle is only initialized once the pool is, and a trace before that reverts.
    // `setPlugin` resets the config to zero, so a new plugin always gets a `PluginConfig` of its
    // own. When the trace fails, as for a plugin that refuses swaps until a start time, the
    // indexer writes `paused = 2` on the pool; the README's `paused` row says what clears it.
    add_plugin_changes(
        &mut transaction_changes,
        &block,
        &events.pool_events,
        &modules,
        &pool_plugins_store,
        &plugin_beacons_store,
        &beacon_pools_store,
    );
    let mut registered_entrypoints = HashSet::new();

    for event in events.pool_events {
        let typ = event.typ.unwrap();
        let tx = event.transaction.unwrap();
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        let pool = hex::encode(&event.pool_address);

        if params.dci {
            let calls_before_swap = matches!(
                &typ,
                Typ::PluginConfig(config) if config.plugin_config & BEFORE_SWAP_FLAG != 0
            );
            if calls_before_swap {
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

        let attributes = event_to_attributes_updates(typ);
        if attributes.is_empty() {
            continue;
        }
        builder.add_entity_change(&EntityChanges {
            component_id: event.pool_address.to_hex(),
            attributes,
        });
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

/// Adds to the block's changes what it changes on the attributes pools carry for their
/// plugins: what each `Plugin` event of `pool_events` does to its pool, then what
/// [`followed_plugin_changes`] lists, in the order the block made them. The builder of a
/// transaction keeps the last value it is given for an attribute.
fn add_plugin_changes(
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
    block: &eth::Block,
    pool_events: &[PoolEvent],
    modules: &[&PluginModule],
    pool_plugins: &impl StringStore,
    plugin_beacons: &impl StringStore,
    beacon_pools: &impl ListStore,
) {
    // The transactions in which a pool names a plugin they created, whose writes are emitted
    // as creations.
    let mut namings: HashSet<(u32, Vec<u8>)> = HashSet::new();
    for event in pool_events {
        let (Some(Typ::Plugin(named)), Some(tx)) = (&event.typ, &event.transaction) else {
            continue;
        };
        let Some(trace) = trace_at(block, tx.index) else {
            continue;
        };
        let Some(change) = naming_change(
            trace,
            &event.pool_address,
            &named.plugin,
            event.log_ordinal,
            modules,
            pool_plugins,
            plugin_beacons,
        ) else {
            continue;
        };
        if change.followed {
            namings.insert((trace.index, named.plugin.clone()));
        }
        let builder = transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(tx));
        apply_naming_change(builder, &event.pool_address.to_hex(), change);
    }

    let later_changes = followed_plugin_changes(
        block,
        modules,
        &namings,
        pool_plugins,
        plugin_beacons,
        beacon_pools,
    );
    for change in later_changes
        .into_iter()
        .sorted_by_key(|change| change.ordinal)
    {
        let tx: Transaction = change.trace.into();
        transaction_changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx))
            .add_entity_change(&EntityChanges {
                component_id: change.pool.to_hex(),
                attributes: change.attributes,
            });
    }
}

/// A change to the attributes a pool carries for its plugin, made in `trace` by something
/// other than the pool's own events.
struct PluginChange<'a> {
    trace: &'a eth::TransactionTrace,
    /// Where in the block the change was made: a transaction can make several to one attribute,
    /// and the last one stands.
    ordinal: u64,
    pool: Vec<u8>,
    attributes: Vec<Attribute>,
}

/// What the block changes on pools that follow a plugin named earlier: module configuration
/// the plugin wrote, the plugin's move to another beacon, and upgrades of the beacon it is
/// bound to. `namings` lists the transactions whose `Plugin` event already emitted the
/// plugin's writes.
fn followed_plugin_changes<'a>(
    block: &'a eth::Block,
    modules: &[&PluginModule],
    namings: &HashSet<(u32, Vec<u8>)>,
    pool_plugins: &impl StringStore,
    plugin_beacons: &impl StringStore,
    beacon_pools: &impl ListStore,
) -> Vec<PluginChange<'a>> {
    let mut changes = Vec::new();

    // Module configuration a pool's plugin wrote after it was named. The slot filter comes
    // first: a swap writes only the plugin's volatility oracle, which no module lists, so it
    // costs no store read.
    for trace in block.transactions() {
        for (plugin, ordinal) in module_slot_writers(trace, modules) {
            if namings.contains(&(trace.index, plugin.clone())) {
                continue;
            }
            let Some(pool) = current_pool_of(pool_plugins, &plugin, ordinal) else {
                continue;
            };
            if !follows_after(plugin_beacons, trace, &pool, &plugin) {
                continue;
            }
            let attributes = updated_plugin_attributes(trace, &plugin, modules);
            if attributes.is_empty() {
                continue;
            }
            changes.push(PluginChange { trace, ordinal, pool, attributes });
        }
    }

    // A plugin bound to another beacon after it was named runs that beacon's implementation.
    // The proxy reads the implementation from the beacon it is bound to, in the same
    // transaction; when the trace does not show that read the implementation is unknown and
    // the attribute is removed, which stops quotes for the pool where no VM fallback exists.
    // It stays unknown until the pool names another plugin.
    for binding in beacon_bindings(block, pool_plugins) {
        if namings.contains(&(binding.tx_index, binding.plugin.clone())) {
            continue;
        }
        let Some(trace) = trace_at(block, binding.tx_index.into()) else {
            continue;
        };
        if !follows_after(plugin_beacons, trace, &binding.pool, &binding.plugin) {
            continue;
        }
        let had_implementation = implementation_known_at(
            plugin_beacons,
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
        changes.push(PluginChange {
            trace,
            ordinal: binding.ordinal,
            pool: binding.pool,
            attributes: vec![attribute],
        });
    }

    // A beacon's upgrade changes the implementation of every pool whose plugin is bound to it.
    // Proxies of every kind log `Upgraded`, so only beacons some pool's plugin was bound to are
    // followed, and a pool listed under a beacon counts only while its current plugin is still
    // bound to it and the pool has a `plugin_impl` to update.
    for trace in block.transactions() {
        for upgrade in beacon_upgrades(trace) {
            if plugin_beacons
                .value_at(upgrade.ordinal, &tracked_beacon_key(&upgrade.beacon))
                .is_none()
            {
                continue;
            }
            let beacon = hex::encode(&upgrade.beacon);
            let mut pools = HashSet::new();
            for key in beacon_pools_keys(&upgrade.beacon) {
                pools.extend(beacon_pools.list_at(upgrade.ordinal, &key));
            }
            for pool in pools.into_iter().sorted() {
                let Some(plugin) = decode_plugin(pool_plugins.value_at(upgrade.ordinal, &pool))
                else {
                    continue;
                };
                let bound_to_beacon = plugin_beacons
                    .value_at(upgrade.ordinal, &plugin_beacon_key(&plugin))
                    .is_some_and(|current| current == beacon);
                let Ok(pool_address) = hex::decode(&pool) else {
                    continue;
                };
                if !bound_to_beacon ||
                    !follows_after(plugin_beacons, trace, &pool_address, &plugin) ||
                    !implementation_known_at(plugin_beacons, upgrade.ordinal, &plugin)
                {
                    continue;
                }
                changes.push(PluginChange {
                    trace,
                    ordinal: upgrade.ordinal,
                    pool: pool_address,
                    attributes: vec![implementation_attribute(
                        &upgrade.implementation,
                        ChangeType::Update,
                    )],
                });
            }
        }
    }

    changes
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
    pool_plugins: &impl StringStore,
    plugin: &[u8],
    ordinal: u64,
) -> Option<Vec<u8>> {
    let pool = pool_plugins.value_at(ordinal, &plugin_pool_key(plugin))?;
    if pool_plugins.value_at(ordinal, &pool)? != hex::encode(plugin) {
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
    let PluginSwitch { old: old_plugin, new: new_plugin } =
        plugin_switch(pool_plugins, ordinal, pool, named)?;

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

    // Whether the pool follows the new plugin is the store's decision, read back here.
    let followed =
        new_plugin.is_some() && followed_plugin_at(plugin_beacons, ordinal, pool) == new_plugin;
    let created = new_plugin
        .as_ref()
        .filter(|_| followed)
        .and_then(|plugin| named_plugin_attributes(trace, plugin, modules));
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
        Typ::Mint(_) | Typ::Burn(_) => vec![],
    }
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::CallType;

    use super::*;
    use crate::{
        modules::{
            store_plugin_beacons::{beacon_pool_writes, plugin_beacon_writes},
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
        assert!(
            serde_qs::from_str::<Params>("dci=true").is_err(),
            "a manifest that leaves out `modules` must not run"
        );
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

    /// A list store over a map, as [`MapStore`] is a string store.
    #[derive(Default)]
    struct ListMapStore {
        committed: HashMap<String, Vec<String>>,
        block: Vec<StoreWrite>,
        /// How many lists were read.
        reads: std::cell::Cell<usize>,
    }

    impl ListStore for ListMapStore {
        fn list_at(&self, ordinal: u64, key: &str) -> Vec<String> {
            self.reads.set(self.reads.get() + 1);
            let mut list = self
                .committed
                .get(key)
                .cloned()
                .unwrap_or_default();
            list.extend(
                self.block
                    .iter()
                    .filter(|write| write.ordinal <= ordinal && write.key == key)
                    .map(|write| write.value.clone()),
            );
            list
        }
    }

    impl ListMapStore {
        fn commit(&mut self) {
            for write in self.block.drain(..) {
                self.committed
                    .entry(write.key)
                    .or_default()
                    .push(write.value);
            }
        }
    }

    const POOL: [u8; 20] = [0x11; 20];
    const PLUGIN_X: [u8; 20] = [0xa1; 20];
    const PLUGIN_Y: [u8; 20] = [0xa2; 20];
    const NO_PLUGIN: [u8; 20] = [0; 20];
    const BEACON: [u8; 20] = [0x44; 20];
    const IMPLEMENTATION: [u8; 20] = [0x65; 20];
    const OTHER_BEACON: [u8; 20] = [0x45; 20];
    const NEW_IMPLEMENTATION: [u8; 20] = [0x66; 20];
    const BEACON_UPGRADED_TOPIC: [u8; 32] =
        hex_literal::hex!("1cf3b03a6cf19fa2baba4df148e9dcabedea7f8a5c07840e207e5c089be95d3e");
    const UPGRADED_TOPIC: [u8; 32] =
        hex_literal::hex!("bc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b");
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
            topics: vec![BEACON_UPGRADED_TOPIC.to_vec(), address_word(&BEACON)],
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

    /// A transaction in which `plugin`, named earlier, writes its fee configuration.
    fn reconfigures(plugin: [u8; 20], fee_word: [u8; 32]) -> Tx {
        let fee_slot = modules_named("dynamic_fee").expect("dynamic_fee is indexed")[0].fields[0]
            .slot()
            .to_vec();
        let mut set_fee = call(CallType::Call, plugin, false);
        set_fee.storage_changes = vec![eth::StorageChange {
            address: plugin.to_vec(),
            key: fee_slot,
            new_value: fee_word.to_vec(),
            ordinal: 2,
            ..Default::default()
        }];
        Tx { calls: vec![set_fee], names: Vec::new() }
    }

    /// A transaction in which `plugin`, named earlier, is bound to [`OTHER_BEACON`]. With
    /// `implementation`, the trace shows the plugin reading it from that beacon.
    fn rebinds(plugin: [u8; 20], implementation: Option<[u8; 20]>) -> Tx {
        let mut rebind = call(CallType::Call, plugin, false);
        rebind.logs = vec![eth::Log {
            address: plugin.to_vec(),
            topics: vec![BEACON_UPGRADED_TOPIC.to_vec(), address_word(&OTHER_BEACON)],
            ordinal: 1,
            ..Default::default()
        }];
        let mut calls = vec![rebind];
        if let Some(implementation) = implementation {
            calls.push(eth::Call {
                caller: plugin.to_vec(),
                address: OTHER_BEACON.to_vec(),
                input: hex_literal::hex!("5c60da1b").to_vec(),
                return_data: address_word(&implementation),
                ..Default::default()
            });
        }
        Tx { calls, names: Vec::new() }
    }

    /// A transaction in which `beacon` is upgraded to `implementation`.
    fn upgrades(beacon: [u8; 20], implementation: [u8; 20]) -> Tx {
        let mut upgrade = call(CallType::Call, beacon, false);
        upgrade.logs = vec![eth::Log {
            address: beacon.to_vec(),
            topics: vec![UPGRADED_TOPIC.to_vec(), address_word(&implementation)],
            ordinal: 1,
            ..Default::default()
        }];
        Tx { calls: vec![upgrade], names: Vec::new() }
    }

    /// One transaction: its calls and the plugins [`POOL`] names in it, in order.
    struct Tx {
        calls: Vec<eth::Call>,
        names: Vec<[u8; 20]>,
    }

    impl Tx {
        /// One transaction that does what `self` does, then what `later` does.
        fn then(mut self, mut later: Tx) -> Tx {
            const LATER: u64 = 3;
            for call in &mut later.calls {
                for log in &mut call.logs {
                    log.ordinal += LATER;
                }
                for change in &mut call.storage_changes {
                    change.ordinal += LATER;
                }
            }
            self.calls.extend(later.calls);
            self.names.extend(later.names);
            self
        }
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
        beacon_pools: ListMapStore,
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
                    status: eth::TransactionTraceStatus::Succeeded.into(),
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
            self.beacon_pools.block = beacon_pool_writes(&block, &self.pool_plugins);

            let mut builders = HashMap::new();
            add_plugin_changes(
                &mut builders,
                &block,
                &events.pool_events,
                &modules,
                &self.pool_plugins,
                &self.plugin_beacons,
                &self.beacon_pools,
            );

            for (_, builder) in builders
                .into_iter()
                .sorted_by_key(|(index, _)| *index)
            {
                let attributes = builder
                    .build()
                    .into_iter()
                    .flat_map(|changes| changes.entity_changes)
                    .flat_map(|changes| {
                        assert_eq!(changes.component_id, POOL.to_hex());
                        changes.attributes
                    });
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
            self.beacon_pools.commit();
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
    fn followed_plugin_that_writes_its_configuration_updates_the_pool() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![reconfigures(PLUGIN_X, BASE_FEE_800)]);

        assert_eq!(indexed.row_names(), FOLLOWED_PROXY);
        assert_eq!(indexed.base_fee(), [0x03, 0x20]);
    }

    #[test]
    fn plugin_that_is_not_followed_writes_nothing_to_the_pool() {
        let mut indexed = Indexed::default();

        // Named but created earlier, so the pool is paused and carries none of its fields.
        indexed
            .block(vec![names(PLUGIN_X)])
            .block(vec![
                reconfigures(PLUGIN_X, BASE_FEE_800),
                rebinds(PLUGIN_X, Some(NEW_IMPLEMENTATION)),
            ]);
        assert_eq!(indexed.row_names(), ["paused"]);

        // A plugin the pool no longer names.
        indexed
            .block(vec![creates_and_names(PLUGIN_Y, true, BASE_FEE_500)])
            .block(vec![reconfigures(PLUGIN_X, BASE_FEE_800)]);
        assert_eq!(indexed.base_fee(), [0x01, 0xf4]);
    }

    #[test]
    fn beacon_upgrade_updates_the_implementation_of_the_pool_bound_to_it() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![upgrades(BEACON, NEW_IMPLEMENTATION)]);
        assert_eq!(indexed.row_names(), FOLLOWED_PROXY);
        assert_eq!(indexed.rows["plugin_impl"], NEW_IMPLEMENTATION);

        let reads = indexed.beacon_pools.reads.get();
        assert!(reads > 0);

        // A beacon no plugin was bound to changes nothing, and costs no read of the pool lists.
        indexed.block(vec![upgrades(OTHER_BEACON, IMPLEMENTATION)]);
        assert_eq!(indexed.rows["plugin_impl"], NEW_IMPLEMENTATION);
        assert_eq!(indexed.beacon_pools.reads.get(), reads);
    }

    #[test]
    fn upgrade_then_rebind_in_one_transaction_leaves_the_implementation_of_the_last_beacon() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![
                upgrades(BEACON, [0x67; 20]).then(rebinds(PLUGIN_X, Some(NEW_IMPLEMENTATION)))
            ]);

        assert_eq!(indexed.rows["plugin_impl"], NEW_IMPLEMENTATION);
    }

    #[test]
    fn plugin_bound_to_another_beacon_takes_that_beacons_implementation() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![rebinds(PLUGIN_X, Some(NEW_IMPLEMENTATION))]);
        assert_eq!(indexed.row_names(), FOLLOWED_PROXY);
        assert_eq!(indexed.rows["plugin_impl"], NEW_IMPLEMENTATION);

        // The beacon it left no longer reaches the pool; the one it moved to does.
        indexed.block(vec![upgrades(BEACON, [0x67; 20])]);
        assert_eq!(indexed.rows["plugin_impl"], NEW_IMPLEMENTATION);
        indexed.block(vec![upgrades(OTHER_BEACON, [0x68; 20])]);
        assert_eq!(indexed.rows["plugin_impl"], [0x68; 20]);
    }

    #[test]
    fn rebind_whose_implementation_is_not_in_the_trace_deletes_it_until_another_plugin() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![rebinds(PLUGIN_X, None)]);
        assert_eq!(indexed.row_names(), FOLLOWED_PLAIN);

        // Nothing brings it back or deletes it twice: not an upgrade of the new beacon, not a
        // second rebind without the read.
        indexed
            .block(vec![upgrades(OTHER_BEACON, NEW_IMPLEMENTATION)])
            .block(vec![rebinds(PLUGIN_X, None)]);
        assert_eq!(indexed.row_names(), FOLLOWED_PLAIN);

        // A switch away deletes only what is left, and a new plugin has its own.
        indexed.block(vec![creates_and_names(PLUGIN_Y, true, BASE_FEE_800)]);
        assert_eq!(indexed.row_names(), FOLLOWED_PROXY);
        assert_eq!(indexed.rows["plugin_impl"], IMPLEMENTATION);
    }

    #[test]
    fn rebind_without_the_read_in_the_block_that_named_the_plugin_leaves_no_implementation() {
        let mut indexed = Indexed::default();

        // The store keeps the rebind's missing implementation over the naming's, which it wrote
        // earlier in the block, so the upgrade has no row to update.
        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500), rebinds(PLUGIN_X, None)]);
        assert_eq!(indexed.row_names(), FOLLOWED_PLAIN);
        indexed.block(vec![upgrades(OTHER_BEACON, NEW_IMPLEMENTATION)]);
        assert_eq!(indexed.row_names(), FOLLOWED_PLAIN);
    }

    #[test]
    fn rebind_that_shows_the_implementation_again_creates_it() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![rebinds(PLUGIN_X, None)])
            .block(vec![rebinds(PLUGIN_X, Some(NEW_IMPLEMENTATION))]);

        assert_eq!(indexed.row_names(), FOLLOWED_PROXY);
        assert_eq!(indexed.rows["plugin_impl"], NEW_IMPLEMENTATION);
    }

    #[test]
    fn upgrade_and_reconfiguration_in_the_block_of_a_switch_reach_only_the_plugin_followed() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![
                reconfigures(PLUGIN_X, BASE_FEE_800),
                creates_and_names(PLUGIN_Y, false, BASE_FEE_500),
                upgrades(BEACON, NEW_IMPLEMENTATION),
            ]);

        assert_eq!(indexed.row_names(), FOLLOWED_PLAIN);
        assert_eq!(indexed.base_fee(), [0x01, 0xf4]);
    }

    #[test]
    fn reconfiguration_before_a_switch_in_the_same_transaction_is_not_emitted() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![reconfigures(PLUGIN_X, BASE_FEE_800).then(creates_and_names(
                PLUGIN_Y,
                false,
                BASE_FEE_500,
            ))]);

        assert_eq!(indexed.row_names(), FOLLOWED_PLAIN);
        assert_eq!(indexed.base_fee(), [0x01, 0xf4]);
    }

    #[test]
    fn upgrade_before_a_switch_in_the_same_transaction_is_not_emitted() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![upgrades(BEACON, NEW_IMPLEMENTATION).then(creates_and_names(
                PLUGIN_Y,
                false,
                BASE_FEE_500,
            ))]);

        assert_eq!(indexed.row_names(), FOLLOWED_PLAIN);
    }

    #[test]
    fn upgrade_after_a_switch_reaches_the_new_plugin_bound_to_the_same_beacon() {
        let mut indexed = Indexed::default();

        indexed
            .block(vec![creates_and_names(PLUGIN_X, true, BASE_FEE_500)])
            .block(vec![
                creates_and_names(PLUGIN_Y, true, BASE_FEE_800),
                reconfigures(PLUGIN_X, BASE_FEE_500),
                upgrades(BEACON, NEW_IMPLEMENTATION),
            ]);

        assert_eq!(indexed.row_names(), FOLLOWED_PROXY);
        assert_eq!(indexed.base_fee(), [0x03, 0x20]);
        assert_eq!(indexed.rows["plugin_impl"], NEW_IMPLEMENTATION);
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
