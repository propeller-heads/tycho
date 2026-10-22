use crate::{
    code::{CodeSource, RpcCode},
    common::*,
    config::DeploymentConfig,
    pb::tessera::v1::BlockStorageChanges,
};
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use substreams::{
    pb::substreams::StoreDeltas,
    store::{StoreGet, StoreGetArray, StoreGetProto, StoreGetRaw},
};
use substreams_ethereum::pb::eth;
use tycho_substreams::{
    balances::aggregate_balances_changes, contract::extract_contract_changes_builder, prelude::*,
};

/// Safety signals recorded for the currently known pairs.
struct Safety {
    /// Engine last recorded in TesseraSwap slot 0.
    engine: Option<Vec<u8>>,
    /// Write helper address of each pair, keyed by pair id.
    helpers: HashMap<String, Vec<u8>>,
    /// Tag-0 fee word of each write helper, keyed by helper address.
    fees: HashMap<Vec<u8>, Vec<u8>>,
}

/// Store-derived state the block's changes are computed against.
struct PairView {
    known: HashSet<String>,
    owner: Vec<u8>,
    safety: Safety,
    /// Engine registry slot of each known pair's token pair, keyed by pair id.
    registry: HashMap<String, Vec<u8>>,
}

/// This module's upstream map and store inputs.
struct ModuleInputs {
    new_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    balance_store: StoreDeltas,
}

fn attribute(builder: &mut TransactionChangesBuilder, component: &str, name: &str, value: Vec<u8>) {
    builder.add_entity_change(&EntityChanges {
        component_id: component.into(),
        attributes: vec![Attribute { name: name.into(), value, change: ChangeType::Update.into() }],
    });
}

#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    storage: BlockStorageChanges,
    new_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    pair_store: StoreGetArray<String>,
    safety_store: StoreGetRaw,
    component_store: StoreGetProto<ProtocolComponent>,
    balance_store: StoreDeltas,
) -> Result<BlockChanges> {
    let config = DeploymentConfig::parse(&params)?;
    let known: HashSet<_> = pair_store
        .get_last("pairs")
        .unwrap_or_default()
        .into_iter()
        .collect();
    let owner = safety_store
        .get_last("treasury")
        .unwrap_or_else(|| config.treasury.clone());
    let safety = read_safety(&safety_store, &known);
    let registry = registry_slots(&config, &component_store, &known);
    let view = PairView { known, owner, safety, registry };
    protocol_changes_from_storage(
        &config,
        &block,
        &storage,
        ModuleInputs { new_components, deltas, balance_store },
        &view,
        &RpcCode,
    )
}

/// Engine registry slot of each known pair's token pair.
fn registry_slots(
    config: &DeploymentConfig,
    store: &StoreGetProto<ProtocolComponent>,
    known: &HashSet<String>,
) -> HashMap<String, Vec<u8>> {
    let mut slots = HashMap::new();
    for pair in known {
        if let Some(component) = store.get_last(pair) {
            if let [a, b] = component.tokens.as_slice() {
                slots.insert(pair.clone(), registry_slot(a, b, config.pair_map_slot));
            }
        }
    }
    slots
}

/// Closing-block engine, helper assignments and tag-0 fees for `known` pairs.
/// These get_last reads are not an ordinal-by-ordinal reconstruction of safety state.
fn read_safety(store: &StoreGetRaw, known: &HashSet<String>) -> Safety {
    let mut helpers = HashMap::new();
    let mut fees = HashMap::new();
    for pair in known {
        let Some(helper) = store.get_last(format!("helper:{pair}")) else {
            continue;
        };
        if let Some(fee) = store.get_last(format!("fee:{}", id(&helper))) {
            fees.insert(helper.clone(), fee);
        }
        helpers.insert(pair.clone(), helper);
    }
    Safety { engine: store.get_last("engine"), helpers, fees }
}

/// The block's Tycho changes: new pairs, balances, raw storage of TesseraSwap, the Engine and
/// every known pair, and the attributes derived from their writes.
///
/// A pair write to the implementation, pricing-library or write-helper slot publishes that
/// address as `stateless_contract_addr_0`, `_1` or `_2` and its closing-block code as
/// `stateless_contract_code_0`, `_1` or `_2`; a zero address is not published.
/// TesseraSwap writes publish `engine` and `balance_owner` on every pair. An Engine registry write
/// that moves a pair's token pair away from it pauses that pair, since TesseraSwap routes by token
/// pair and the old pair would otherwise quote its successor. A pair is never touched
/// by a transaction ordered before its creation. Every emitted transaction pauses a pair whose
/// helper charges a nonzero tag-0 fee, and pauses every pair once the recorded Engine differs from
/// the configured one.
fn protocol_changes_from_storage(
    config: &DeploymentConfig,
    block: &eth::v2::Block,
    storage: &BlockStorageChanges,
    inputs: ModuleInputs,
    view: &PairView,
    code: &impl CodeSource,
) -> Result<BlockChanges> {
    let known = &view.known;
    let mut changes = HashMap::new();
    let created: HashMap<_, _> = inputs
        .new_components
        .tx_components
        .iter()
        .flat_map(|g| {
            let index =
                g.tx.as_ref()
                    .expect("component transaction")
                    .index;
            g.components
                .iter()
                .map(move |c| (c.id.clone(), index))
        })
        .collect();
    for group in inputs.new_components.tx_components {
        let tx = group.tx.expect("component transaction");
        let builder = changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for c in group.components {
            builder.add_protocol_component(&c);
            attribute(builder, &c.id, "balance_owner", view.owner.clone());
        }
    }
    for (_, (tx, balances)) in aggregate_balances_changes(inputs.balance_store, inputs.deltas) {
        let builder = changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for values in balances.values() {
            for change in values.values() {
                builder.add_balance_change(change);
            }
        }
    }
    extract_contract_changes_builder(
        block,
        |addr| addr == config.tesseraswap || addr == config.engine || known.contains(&id(addr)),
        &mut changes,
    );
    record_pair_writes(config, storage, view, &created, code, &mut changes)?;
    enforce_pauses(config, storage, view, &created, &mut changes)?;
    let mut changes: Vec<_> = changes.into_iter().collect();
    changes.sort_by_key(|(index, _)| *index);
    Ok(BlockChanges {
        block: Some(block.into()),
        changes: changes
            .into_iter()
            .filter_map(|(_, b)| b.build())
            .collect(),
        storage_changes: vec![],
    })
}

/// Publishes the attributes each committed write sets and pauses a pair the registry moves away
/// from. Consumers refresh a pair on any change to its contracts or balances, so writes are not
/// marked.
fn record_pair_writes(
    config: &DeploymentConfig,
    storage: &BlockStorageChanges,
    view: &PairView,
    created: &HashMap<String, u64>,
    code: &impl CodeSource,
    changes: &mut HashMap<u64, TransactionChangesBuilder>,
) -> Result<()> {
    // These slots belong to TesseraSwap, not the Pair: Engine at 0, Treasury at 1.
    let engine_slot = slot(0);
    let treasury_slot = slot(config.treasury_slot);
    // Array order is the consumer's stateless_contract_addr_<i> attribute index.
    // The VM loads the published code when one of these addresses changes; a change alone
    // does not pause the pair.
    let dependency_slots = [
        IMPLEMENTATION_SLOT.to_vec(), // Pair EIP-1967 slot: implementation, attribute 0.
        slot(config.pair_lib_slot),   // Pair slot 51: pricing library, attribute 1.
        slot(config.pair_write_helper_slot), // Pair slot 52: write helper, attribute 2.
    ];
    // known includes end-block discoveries; never attach changes to a Pair in a transaction
    // earlier than the one that created it.
    let exists_at = |pair: &String, tx_index: u64| {
        created
            .get(pair)
            .is_none_or(|index| *index <= tx_index)
    };
    // (transaction index, pair, attribute index, dependency address), published once code is in.
    let mut dependencies = vec![];
    for group in &storage.transactions {
        let transaction = storage_transaction(group)?;
        for w in &group.writes {
            if w.address == config.engine {
                let replaced = id(&address(&w.old_value));
                if view.registry.get(&replaced) == Some(&w.key) &&
                    address(&w.new_value) != address(&w.old_value)
                {
                    changes
                        .entry(transaction.index)
                        .or_insert_with(|| TransactionChangesBuilder::new(transaction))
                        .change_component_pause_state(&replaced, true);
                }
            } else if w.address == config.tesseraswap {
                let name = if w.key == engine_slot {
                    "engine"
                } else if w.key == treasury_slot {
                    // map_relative_balances emits the matching balance bridge.
                    "balance_owner"
                } else {
                    continue;
                };
                let builder = changes
                    .entry(transaction.index)
                    .or_insert_with(|| TransactionChangesBuilder::new(transaction));
                for pair in &view.known {
                    if exists_at(pair, transaction.index) {
                        attribute(builder, pair, name, address(&w.new_value));
                    }
                }
            } else {
                let pair = id(&w.address);
                if !view.known.contains(&pair) || !exists_at(&pair, transaction.index) {
                    continue;
                }
                let Some(i) = dependency_slots
                    .iter()
                    .position(|key| *key == w.key)
                else {
                    continue;
                };
                if !zero(&w.new_value) {
                    changes
                        .entry(transaction.index)
                        .or_insert_with(|| TransactionChangesBuilder::new(transaction));
                    dependencies.push((transaction.index, pair, i, address(&w.new_value)));
                }
            }
        }
    }
    publish_dependencies(dependencies, code, changes)
}

/// Publishes each dependency's address and code, reading every distinct address's code once.
fn publish_dependencies(
    dependencies: Vec<(u64, String, usize, Vec<u8>)>,
    code: &impl CodeSource,
    changes: &mut HashMap<u64, TransactionChangesBuilder>,
) -> Result<()> {
    let mut addresses: Vec<_> = dependencies
        .iter()
        .map(|(_, _, _, address)| address.clone())
        .collect();
    addresses.sort();
    addresses.dedup();
    let codes = code.codes(&addresses)?;
    for (index, pair, i, address) in dependencies {
        let bytecode = codes
            .get(&address)
            .ok_or_else(|| anyhow!("no code read for dependency {}", id(&address)))?;
        let builder = changes
            .get_mut(&index)
            .ok_or_else(|| anyhow!("no transaction {index} for dependency {}", id(&address)))?;
        attribute(
            builder,
            &pair,
            &format!("stateless_contract_addr_{i}"),
            id(&address).into_bytes(),
        );
        attribute(builder, &pair, &format!("stateless_contract_code_{i}"), bytecode.clone());
    }
    Ok(())
}

/// Emits a sticky pause signal for unsafe pairs in every emitted transaction.
/// Safety is evaluated against closing-block stores; this module never emits an unpause.
fn enforce_pauses(
    config: &DeploymentConfig,
    storage: &BlockStorageChanges,
    view: &PairView,
    created: &HashMap<String, u64>,
    changes: &mut HashMap<u64, TransactionChangesBuilder>,
) -> Result<()> {
    // Always enforce persisted signals on emitted component updates. Fee-table writes can
    // occur without a Pair write, so include their transaction explicitly as a trigger.
    let fee_key = fee_tag_zero_slot();
    let helpers: HashSet<&[u8]> = view
        .safety
        .helpers
        .values()
        .map(Vec::as_slice)
        .collect();
    for group in &storage.transactions {
        if group
            .writes
            .iter()
            .any(|w| w.key == fee_key && helpers.contains(w.address.as_slice()))
        {
            let tx = storage_transaction(group)?;
            changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(tx));
        }
    }
    // Discovery and state indexing follow config.engine. If TesseraSwap slot 0 points at
    // another Engine, every Pair is paused. Storage updates inside config.engine are not
    // an Engine replacement.
    let epoch_changed = view
        .safety
        .engine
        .as_ref()
        .is_some_and(|e| *e != config.engine);
    for (index, builder) in changes.iter_mut() {
        for pair in &view.known {
            if created
                .get(pair)
                .is_some_and(|created_index| created_index > index)
            {
                continue;
            }
            let fee = view
                .safety
                .helpers
                .get(pair)
                .and_then(|helper| view.safety.fees.get(helper));
            // Empty swapData selects helper fee A[0] (mapping base slot 1). The VM loads
            // the helper's bytecode but not its storage, so it reads the fee as zero; a
            // nonzero on-chain fee would make simulation over-quote execution.
            if epoch_changed || fee.is_some_and(|value| !zero(value)) {
                builder.change_component_pause_state(pair, true);
            }
        }
    }
    Ok(())
}

/// Runs the block through `filter_storage_changes` first, as the manifest does.
#[cfg(test)]
fn protocol_changes(
    config: &DeploymentConfig,
    block: &eth::v2::Block,
    inputs: ModuleInputs,
    view: &PairView,
) -> Result<BlockChanges> {
    let known = view
        .known
        .iter()
        .map(|pair| hex::decode(pair.trim_start_matches("0x")).unwrap())
        .collect();
    let storage =
        crate::modules::map_storage_changes::filter_storage_changes(config, block, &known);
    protocol_changes_from_storage(config, block, &storage, inputs, view, &AddressAsCode)
}

/// Test code source: an address's code is `0x5f` followed by the address.
#[cfg(test)]
struct AddressAsCode;

#[cfg(test)]
impl CodeSource for AddressAsCode {
    fn codes(&self, addresses: &[Vec<u8>]) -> Result<HashMap<Vec<u8>, Vec<u8>>> {
        Ok(addresses
            .iter()
            .map(|a| (a.clone(), [&[0x5f], a.as_slice()].concat()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAIR: [u8; 20] = [0x0a; 20];
    const OTHER_PAIR: [u8; 20] = [0x0b; 20];
    const HELPER: [u8; 20] = [0x0c; 20];
    const TREASURY: [u8; 20] = [0x33; 20];

    fn config() -> DeploymentConfig {
        DeploymentConfig::parse(&format!(
            "tesseraswap={}&engine={}&treasury={}&treasury_slot=1&pair_map_slot=8\
             &pair_base_token_slot=48&pair_quote_token_slot=49&pair_lib_slot=51\
             &pair_write_helper_slot=52",
            hex::encode([0x11; 20]),
            hex::encode([0x22; 20]),
            hex::encode(TREASURY),
        ))
        .unwrap()
    }

    fn word(addr: &[u8]) -> Vec<u8> {
        let mut w = vec![0; 32];
        w[12..].copy_from_slice(addr);
        w
    }

    fn write(address: &[u8], key: Vec<u8>, old: Vec<u8>, new: Vec<u8>) -> eth::v2::StorageChange {
        eth::v2::StorageChange {
            address: address.to_vec(),
            key,
            old_value: old,
            new_value: new,
            ..Default::default()
        }
    }

    fn tx(index: u32, writes: Vec<eth::v2::StorageChange>) -> eth::v2::TransactionTrace {
        eth::v2::TransactionTrace {
            status: 1,
            index,
            // One call per write, made by the written contract, as on chain.
            calls: writes
                .into_iter()
                .map(|w| eth::v2::Call {
                    address: w.address.clone(),
                    storage_changes: vec![w],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn block(transactions: Vec<eth::v2::TransactionTrace>) -> eth::v2::Block {
        eth::v2::Block {
            header: Some(eth::v2::BlockHeader {
                timestamp: Some(Default::default()),
                ..Default::default()
            }),
            transaction_traces: transactions,
            ..Default::default()
        }
    }

    fn no_inputs() -> ModuleInputs {
        ModuleInputs {
            new_components: BlockTransactionProtocolComponents::default(),
            deltas: BlockBalanceDeltas::default(),
            balance_store: StoreDeltas::default(),
        }
    }

    fn safe() -> Safety {
        Safety { engine: None, helpers: HashMap::new(), fees: HashMap::new() }
    }

    fn view(pairs: &[&[u8]], safety: Safety) -> PairView {
        PairView {
            known: pairs.iter().map(|p| id(p)).collect(),
            owner: TREASURY.to_vec(),
            safety,
            registry: HashMap::new(),
        }
    }

    const PAIR_REGISTRY_SLOT: [u8; 32] = [0x99; 32];

    /// A view where PAIR's token pair sits at `PAIR_REGISTRY_SLOT` in the Engine registry.
    fn registered_view() -> PairView {
        let mut view = view(&[&PAIR, &OTHER_PAIR], safe());
        view.registry
            .insert(id(&PAIR), PAIR_REGISTRY_SLOT.to_vec());
        view
    }

    #[test]
    fn pauses_pair_moved_out_of_registry() {
        let engine = config().engine;
        let block = block(vec![tx(
            0,
            vec![write(&engine, PAIR_REGISTRY_SLOT.to_vec(), word(&PAIR), word(&OTHER_PAIR))],
        )]);
        let out = protocol_changes(&config(), &block, no_inputs(), &registered_view()).unwrap();

        assert_eq!(attrs_in(&out, 0, &PAIR)["paused"], vec![1]);
        assert!(!attrs_in(&out, 0, &OTHER_PAIR).contains_key("paused"));
    }

    #[test]
    fn pauses_pair_removed_from_registry() {
        let engine = config().engine;
        let block = block(vec![tx(
            0,
            vec![write(&engine, PAIR_REGISTRY_SLOT.to_vec(), word(&PAIR), vec![0; 32])],
        )]);
        let out = protocol_changes(&config(), &block, no_inputs(), &registered_view()).unwrap();

        assert_eq!(attrs_in(&out, 0, &PAIR)["paused"], vec![1]);
    }

    #[test]
    fn pair_address_outside_its_registry_slot_does_not_pause() {
        let engine = config().engine;
        let block =
            block(vec![tx(0, vec![write(&engine, slot(77), word(&PAIR), word(&OTHER_PAIR))])]);
        let out = protocol_changes(&config(), &block, no_inputs(), &registered_view()).unwrap();

        assert!(!attrs_in(&out, 0, &PAIR).contains_key("paused"));
    }

    /// Attributes emitted for `component` in the transaction at `tx_index`.
    fn attrs_in(out: &BlockChanges, tx_index: u64, component: &[u8]) -> HashMap<String, Vec<u8>> {
        let mut found = HashMap::new();
        for changes in &out.changes {
            if changes.tx.as_ref().map(|t| t.index) != Some(tx_index) {
                continue;
            }
            for entity in &changes.entity_changes {
                if entity.component_id == id(component) {
                    for a in &entity.attributes {
                        found.insert(a.name.clone(), a.value.clone());
                    }
                }
            }
        }
        found
    }

    #[test]
    fn publishes_dependency_addresses_by_slot() {
        let (implementation, library) = ([0x71; 20], [0x72; 20]);
        let block = block(vec![
            tx(
                0,
                vec![
                    write(
                        &PAIR,
                        IMPLEMENTATION_SLOT.to_vec(),
                        word(&[0x70; 20]),
                        word(&implementation),
                    ),
                    write(&PAIR, slot(51), vec![0; 32], word(&library)),
                    write(&PAIR, slot(52), vec![0; 32], word(&HELPER)),
                ],
            ),
            tx(1, vec![write(&PAIR, slot(52), word(&HELPER), vec![0; 32])]),
        ]);
        let out =
            protocol_changes(&config(), &block, no_inputs(), &view(&[&PAIR], safe())).unwrap();

        let first = attrs_in(&out, 0, &PAIR);
        assert_eq!(first["stateless_contract_addr_0"], id(&implementation).into_bytes());
        assert_eq!(first["stateless_contract_addr_1"], id(&library).into_bytes());
        assert_eq!(first["stateless_contract_addr_2"], id(&HELPER).into_bytes());
        assert_eq!(
            first["stateless_contract_code_0"],
            [&[0x5f], implementation.as_slice()].concat()
        );
        assert_eq!(first["stateless_contract_code_1"], [&[0x5f], library.as_slice()].concat());
        assert_eq!(first["stateless_contract_code_2"], [&[0x5f], HELPER.as_slice()].concat());
        let cleared = attrs_in(&out, 1, &PAIR);
        assert!(
            !cleared.contains_key("stateless_contract_addr_2") &&
                !cleared.contains_key("stateless_contract_code_2"),
            "zero address is not published"
        );
    }

    /// Records every batch it is asked for and serves fixed code.
    struct RecordingCode {
        batches: std::cell::RefCell<Vec<Vec<Vec<u8>>>>,
        fail: bool,
    }

    impl CodeSource for RecordingCode {
        fn codes(&self, addresses: &[Vec<u8>]) -> Result<HashMap<Vec<u8>, Vec<u8>>> {
            self.batches
                .borrow_mut()
                .push(addresses.to_vec());
            if self.fail {
                return Err(anyhow!("rpc down"));
            }
            Ok(addresses
                .iter()
                .map(|a| (a.clone(), vec![0xfe]))
                .collect())
        }
    }

    fn changes_with(
        block: &eth::v2::Block,
        pairs: &[&[u8]],
        code: &RecordingCode,
    ) -> Result<BlockChanges> {
        let known = pairs
            .iter()
            .map(|p| p.to_vec())
            .collect();
        let storage =
            crate::modules::map_storage_changes::filter_storage_changes(&config(), block, &known);
        protocol_changes_from_storage(
            &config(),
            block,
            &storage,
            no_inputs(),
            &view(pairs, safe()),
            code,
        )
    }

    #[test]
    fn reads_each_dependency_code_once_per_block() {
        let implementation = [0x71; 20];
        let block = block(vec![
            tx(
                0,
                vec![write(
                    &PAIR,
                    IMPLEMENTATION_SLOT.to_vec(),
                    word(&[0x70; 20]),
                    word(&implementation),
                )],
            ),
            tx(
                1,
                vec![write(
                    &OTHER_PAIR,
                    IMPLEMENTATION_SLOT.to_vec(),
                    word(&[0x70; 20]),
                    word(&implementation),
                )],
            ),
        ]);
        let code = RecordingCode { batches: Default::default(), fail: false };
        let out = changes_with(&block, &[&PAIR, &OTHER_PAIR], &code).unwrap();

        assert_eq!(*code.batches.borrow(), vec![vec![implementation.to_vec()]]);
        assert_eq!(attrs_in(&out, 0, &PAIR)["stateless_contract_code_0"], vec![0xfe]);
        assert_eq!(attrs_in(&out, 1, &OTHER_PAIR)["stateless_contract_code_0"], vec![0xfe]);
    }

    #[test]
    fn failed_code_read_fails_the_block() {
        let block = block(vec![tx(0, vec![write(&PAIR, slot(52), vec![0; 32], word(&HELPER))])]);
        let code = RecordingCode { batches: Default::default(), fail: true };
        let error = changes_with(&block, &[&PAIR], &code).unwrap_err();
        assert!(error.to_string().contains("rpc down"));
    }

    #[test]
    fn pair_writes_touch_only_their_own_pair() {
        let block = block(vec![tx(0, vec![write(&PAIR, slot(52), vec![0; 32], word(&HELPER))])]);
        let out =
            protocol_changes(&config(), &block, no_inputs(), &view(&[&PAIR, &OTHER_PAIR], safe()))
                .unwrap();

        assert_eq!(attrs_in(&out, 0, &PAIR)["stateless_contract_addr_2"], id(&HELPER).into_bytes());
        assert!(attrs_in(&out, 0, &OTHER_PAIR).is_empty());
    }

    #[test]
    fn tesseraswap_writes_reach_every_pair() {
        let (engine, treasury) = ([0x23; 20], [0x34; 20]);
        let tesseraswap = config().tesseraswap;
        let block = block(vec![tx(
            0,
            vec![
                write(&tesseraswap, slot(0), word(&[0x22; 20]), word(&engine)),
                write(&tesseraswap, slot(1), word(&TREASURY), word(&treasury)),
            ],
        )]);
        let out =
            protocol_changes(&config(), &block, no_inputs(), &view(&[&PAIR, &OTHER_PAIR], safe()))
                .unwrap();

        for pair in [PAIR, OTHER_PAIR] {
            let attrs = attrs_in(&out, 0, &pair);
            assert_eq!(attrs["engine"], engine.to_vec());
            assert_eq!(attrs["balance_owner"], treasury.to_vec());
        }
    }

    #[test]
    fn pauses_only_the_pair_whose_helper_charges_tag_zero() {
        let safety = Safety {
            engine: None,
            helpers: HashMap::from([(id(&PAIR), HELPER.to_vec())]),
            fees: HashMap::from([(HELPER.to_vec(), slot(10_000))]),
        };
        let block = block(vec![tx(
            0,
            vec![
                write(&PAIR, slot(0), vec![1; 32], vec![2; 32]),
                write(&OTHER_PAIR, slot(0), vec![1; 32], vec![2; 32]),
            ],
        )]);
        let out =
            protocol_changes(&config(), &block, no_inputs(), &view(&[&PAIR, &OTHER_PAIR], safety))
                .unwrap();

        assert_eq!(attrs_in(&out, 0, &PAIR)["paused"], vec![1]);
        assert!(!attrs_in(&out, 0, &OTHER_PAIR).contains_key("paused"));
    }

    #[test]
    fn zero_tag_zero_fee_does_not_pause() {
        let safety = Safety {
            engine: None,
            helpers: HashMap::from([(id(&PAIR), HELPER.to_vec())]),
            fees: HashMap::from([(HELPER.to_vec(), vec![0; 32])]),
        };
        let block = block(vec![tx(0, vec![write(&PAIR, slot(0), vec![1; 32], vec![2; 32])])]);
        let out =
            protocol_changes(&config(), &block, no_inputs(), &view(&[&PAIR], safety)).unwrap();

        assert!(!attrs_in(&out, 0, &PAIR).contains_key("paused"));
    }

    #[test]
    fn engine_replacement_pauses_every_pair() {
        let safety = Safety { engine: Some([0x23; 20].to_vec()), ..safe() };
        let block = block(vec![tx(0, vec![write(&PAIR, slot(0), vec![1; 32], vec![2; 32])])]);
        let out =
            protocol_changes(&config(), &block, no_inputs(), &view(&[&PAIR, &OTHER_PAIR], safety))
                .unwrap();

        assert_eq!(attrs_in(&out, 0, &PAIR)["paused"], vec![1]);
        assert_eq!(attrs_in(&out, 0, &OTHER_PAIR)["paused"], vec![1]);
    }

    #[test]
    fn unchanged_engine_does_not_pause() {
        let safety = Safety { engine: Some([0x22; 20].to_vec()), ..safe() };
        let block = block(vec![tx(0, vec![write(&PAIR, slot(0), vec![1; 32], vec![2; 32])])]);
        let out =
            protocol_changes(&config(), &block, no_inputs(), &view(&[&PAIR], safety)).unwrap();

        assert!(!attrs_in(&out, 0, &PAIR).contains_key("paused"));
    }

    #[test]
    fn helper_fee_write_alone_emits_the_pause() {
        let safety = Safety {
            engine: None,
            helpers: HashMap::from([(id(&PAIR), HELPER.to_vec())]),
            fees: HashMap::from([(HELPER.to_vec(), slot(10_000))]),
        };
        let block = block(vec![tx(
            3,
            vec![write(&HELPER, fee_tag_zero_slot(), vec![0; 32], slot(10_000))],
        )]);
        let out =
            protocol_changes(&config(), &block, no_inputs(), &view(&[&PAIR], safety)).unwrap();

        assert_eq!(attrs_in(&out, 3, &PAIR)["paused"], vec![1]);
    }

    #[test]
    fn fee_write_outside_assigned_helpers_emits_nothing() {
        let safety = Safety {
            engine: None,
            helpers: HashMap::from([(id(&PAIR), HELPER.to_vec())]),
            fees: HashMap::from([(HELPER.to_vec(), slot(10_000))]),
        };
        let stranger = [0x0f; 20];
        let block =
            block(vec![tx(3, vec![write(&stranger, fee_tag_zero_slot(), vec![0; 32], slot(1))])]);
        let out =
            protocol_changes(&config(), &block, no_inputs(), &view(&[&PAIR], safety)).unwrap();

        assert!(out.changes.is_empty());
    }

    #[test]
    fn writes_before_pair_creation_are_ignored() {
        let inputs = ModuleInputs {
            new_components: BlockTransactionProtocolComponents {
                tx_components: vec![TransactionProtocolComponents {
                    tx: Some(Transaction { index: 5, ..Default::default() }),
                    components: vec![ProtocolComponent { id: id(&PAIR), ..Default::default() }],
                }],
            },
            ..no_inputs()
        };
        let block = block(vec![
            tx(2, vec![write(&PAIR, slot(52), vec![0; 32], word(&HELPER))]),
            tx(5, vec![write(&PAIR, slot(0), vec![0; 32], vec![1; 32])]),
        ]);
        let out = protocol_changes(&config(), &block, inputs, &view(&[&PAIR], safe())).unwrap();

        assert!(attrs_in(&out, 2, &PAIR).is_empty(), "tx 2 precedes the pair");
        assert_eq!(attrs_in(&out, 5, &PAIR)["balance_owner"], TREASURY.to_vec());
    }

    #[test]
    fn missing_storage_transaction_returns_an_error() {
        let storage = BlockStorageChanges {
            transactions: vec![crate::pb::tessera::v1::TransactionStorageChanges {
                tx: None,
                writes: vec![crate::pb::tessera::v1::StorageChange {
                    address: PAIR.to_vec(),
                    key: IMPLEMENTATION_SLOT.to_vec(),
                    old_value: vec![],
                    new_value: word(&HELPER),
                    ordinal: 20,
                }],
            }],
        };
        let error = protocol_changes_from_storage(
            &config(),
            &block(vec![]),
            &storage,
            no_inputs(),
            &view(&[&PAIR], safe()),
            &AddressAsCode,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("writes without a transaction"));
    }
}
