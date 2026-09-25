use crate::{common::*, config::DeploymentConfig};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use substreams::{
    pb::substreams::StoreDeltas,
    store::{StoreGet, StoreGetString},
};
use substreams_ethereum::pb::eth;
use tycho_substreams::{
    balances::aggregate_balances_changes, contract::extract_contract_changes_builder, prelude::*,
};

/// Safety signals recorded for the currently known pairs.
struct Safety {
    /// Engine last recorded in TesseraSwap slot 0, hex without `0x`.
    engine: Option<String>,
    /// Write helper of each pair, keyed by pair id, hex without `0x`.
    helpers: HashMap<String, String>,
    /// Tag-0 fee word of each write helper, keyed like the `helpers` values.
    fees: HashMap<String, Vec<u8>>,
}

/// Store-derived state the block's changes are computed against.
struct PairView {
    known: HashSet<String>,
    owner: Vec<u8>,
    safety: Safety,
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
    builder.mark_component_as_updated(component);
}

#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    new_components: BlockTransactionProtocolComponents,
    deltas: BlockBalanceDeltas,
    pair_store: StoreGetString,
    treasury_store: StoreGetString,
    safety_store: StoreGetString,
    balance_store: StoreDeltas,
) -> Result<BlockChanges> {
    let config = DeploymentConfig::parse(&params)?;
    let known: HashSet<_> = pairs(&pair_store, "pairs")
        .into_iter()
        .collect();
    let owner = treasury_store
        .get_last("treasury")
        .map(hex::decode)
        .transpose()?
        .unwrap_or(config.treasury.clone());
    let safety = read_safety(&safety_store, &known);
    let view = PairView { known, owner, safety };
    protocol_changes(&config, &block, ModuleInputs { new_components, deltas, balance_store }, &view)
}

/// The engine, helper assignments and tag-0 fees recorded for `known` pairs.
fn read_safety(store: &StoreGetString, known: &HashSet<String>) -> Safety {
    let mut helpers = HashMap::new();
    let mut fees = HashMap::new();
    for pair in known {
        let Some(helper) = store.get_last(format!("helper:{pair}")) else {
            continue;
        };
        if let Some(fee) = store
            .get_last(format!("fee:0x{helper}"))
            .and_then(|value| hex::decode(value).ok())
        {
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
/// address as `stateless_contract_addr_0`, `_1` or `_2`; a zero address is not published.
/// TesseraSwap writes publish `engine` and `balance_owner` on every pair. A pair is never touched
/// by a transaction ordered before its creation. Every emitted transaction pauses a pair whose
/// helper charges a nonzero tag-0 fee, and pauses every pair once the recorded Engine differs from
/// the configured one.
fn protocol_changes(
    config: &DeploymentConfig,
    block: &eth::v2::Block,
    inputs: ModuleInputs,
    view: &PairView,
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
    record_pair_writes(config, block, known, &created, &mut changes);
    enforce_pauses(config, block, view, &created, &mut changes);
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

/// Marks the pairs each committed write touches and publishes the attributes it sets.
fn record_pair_writes(
    config: &DeploymentConfig,
    block: &eth::v2::Block,
    known: &HashSet<String>,
    created: &HashMap<String, u64>,
    changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for tx in block.transactions() {
        for w in committed_writes(tx) {
            let pair_id = id(&w.address);
            let targets: Vec<_> = if w.address == config.engine || w.address == config.tesseraswap {
                known.iter().cloned().collect()
            } else if known.contains(&pair_id) {
                vec![pair_id]
            } else {
                continue;
            };
            let transaction: Transaction = tx.into();
            let builder = changes
                .entry(transaction.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&transaction));
            for pair in targets {
                if created
                    .get(&pair)
                    .is_some_and(|index| *index > transaction.index)
                {
                    continue;
                }
                builder.mark_component_as_updated(&pair);
                if w.address == config.tesseraswap {
                    if w.key == slot(0) {
                        attribute(builder, &pair, "engine", address(&w.new_value));
                    }
                    if w.key == slot(config.treasury_slot) {
                        attribute(builder, &pair, "balance_owner", address(&w.new_value));
                    }
                } else if w.address != config.engine {
                    if let Some(i) = dependency_index(config, &w.key) {
                        if !zero(&w.new_value) {
                            attribute(
                                builder,
                                &pair,
                                &format!("stateless_contract_addr_{i}"),
                                id(&address(&w.new_value)).into_bytes(),
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Pauses unsafe pairs in every emitted transaction.
fn enforce_pauses(
    config: &DeploymentConfig,
    block: &eth::v2::Block,
    view: &PairView,
    created: &HashMap<String, u64>,
    changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    // Always enforce persisted signals on emitted component updates. Fee-table writes can
    // occur without a Pair write, so include their transaction explicitly as a trigger.
    let fee_key = fee_tag_zero_slot();
    let helpers: HashSet<&str> = view
        .safety
        .helpers
        .values()
        .map(String::as_str)
        .collect();
    for tx in block.transactions() {
        if tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
            .flat_map(|c| &c.storage_changes)
            .any(|w| {
                w.key == fee_key.as_slice() && helpers.contains(hex::encode(&w.address).as_str())
            })
        {
            let tx: Transaction = tx.into();
            changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        }
    }
    let epoch_changed = view
        .safety
        .engine
        .as_ref()
        .is_some_and(|e| *e != hex::encode(&config.engine));
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
            if epoch_changed || fee.is_some_and(|value| !zero(value)) {
                builder.change_component_pause_state(pair, true);
            }
        }
    }
}

/// The `stateless_contract_addr_<i>` index of a pair slot holding a dependency address.
fn dependency_index(config: &DeploymentConfig, key: &[u8]) -> Option<usize> {
    if key == IMPLEMENTATION_SLOT {
        Some(0)
    } else if key == slot(config.pair_lib_slot) {
        Some(1)
    } else if key == slot(config.pair_write_helper_slot) {
        Some(2)
    } else {
        None
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
            calls: vec![eth::v2::Call { storage_changes: writes, ..Default::default() }],
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
        PairView { known: pairs.iter().map(|p| id(p)).collect(), owner: TREASURY.to_vec(), safety }
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
        assert_eq!(first["update_marker"], vec![1]);
        let cleared = attrs_in(&out, 1, &PAIR);
        assert!(
            !cleared.contains_key("stateless_contract_addr_2"),
            "zero address is not published"
        );
        assert_eq!(cleared["update_marker"], vec![1]);
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
            helpers: HashMap::from([(id(&PAIR), hex::encode(HELPER))]),
            fees: HashMap::from([(hex::encode(HELPER), slot(10_000))]),
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
            helpers: HashMap::from([(id(&PAIR), hex::encode(HELPER))]),
            fees: HashMap::from([(hex::encode(HELPER), vec![0; 32])]),
        };
        let block = block(vec![tx(0, vec![write(&PAIR, slot(0), vec![1; 32], vec![2; 32])])]);
        let out =
            protocol_changes(&config(), &block, no_inputs(), &view(&[&PAIR], safety)).unwrap();

        assert!(!attrs_in(&out, 0, &PAIR).contains_key("paused"));
    }

    #[test]
    fn engine_replacement_pauses_every_pair() {
        let safety = Safety { engine: Some(hex::encode([0x23; 20])), ..safe() };
        let block = block(vec![tx(0, vec![write(&PAIR, slot(0), vec![1; 32], vec![2; 32])])]);
        let out =
            protocol_changes(&config(), &block, no_inputs(), &view(&[&PAIR, &OTHER_PAIR], safety))
                .unwrap();

        assert_eq!(attrs_in(&out, 0, &PAIR)["paused"], vec![1]);
        assert_eq!(attrs_in(&out, 0, &OTHER_PAIR)["paused"], vec![1]);
    }

    #[test]
    fn unchanged_engine_does_not_pause() {
        let safety = Safety { engine: Some(hex::encode([0x22; 20])), ..safe() };
        let block = block(vec![tx(0, vec![write(&PAIR, slot(0), vec![1; 32], vec![2; 32])])]);
        let out =
            protocol_changes(&config(), &block, no_inputs(), &view(&[&PAIR], safety)).unwrap();

        assert!(!attrs_in(&out, 0, &PAIR).contains_key("paused"));
    }

    #[test]
    fn helper_fee_write_alone_emits_the_pause() {
        let safety = Safety {
            engine: None,
            helpers: HashMap::from([(id(&PAIR), hex::encode(HELPER))]),
            fees: HashMap::from([(hex::encode(HELPER), slot(10_000))]),
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
}
