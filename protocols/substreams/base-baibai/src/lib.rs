#![allow(clippy::not_unsafe_ptr_arg_deref)]
mod config;

use alloy_primitives::{b256, Address, B256, U256};
use anyhow::Result;
use config::Config;
use std::collections::{BTreeMap, HashMap};
use substreams::{pb::substreams::StoreDeltas, prelude::*};
use substreams_ethereum::pb::eth;
use tycho_substreams::{
    abi::erc20::functions::BalanceOf,
    balances::{aggregate_balances_changes, extract_balance_deltas_from_tx, store_balance_changes},
    prelude::*,
};

// Layout and pricing were validated through this block. Later proxy upgrades require review.
const VALIDATED_BLOCK: u64 = 51_191_196;
const TAKER_FEE_SET: B256 =
    b256!("1f50e1aaaff835659bf08a8d3473edefb7f61e28a27572c62fa8daa40da9e268");
const TAKER_FEE_CLEARED: B256 =
    b256!("4e36b92da5e2a98be73ea9f9bd228101bb4b4e83629908148cef524aaad69b94");
const UPGRADED: B256 = b256!("bc7cd75a20ee27fd9adebab32041f755214dbc6bffa90cc0225b39da2e5c2d3b");

#[substreams::handlers::map]
pub fn map_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let config = Config::parse(&params)?;
    let mut tx_components = vec![];
    if block.number == config.start_block {
        let component = ProtocolComponent::new(&config.id())
            .with_tokens(&[config.base, config.quote])
            .as_swap_type("baibai_pool", ImplementationType::Custom)
            .with_attributes(&[("base", config.base.to_vec()), ("quote", config.quote.to_vec())]);
        tx_components.push(TransactionProtocolComponents {
            tx: Some(config.creation(&block)?.into()),
            components: vec![component],
        });
    }
    Ok(BlockTransactionProtocolComponents { tx_components })
}

#[substreams::handlers::map]
pub fn map_balances(params: String, block: eth::v2::Block) -> Result<BlockBalanceDeltas> {
    relative_balances(&Config::parse(&params)?, &block)
}

fn relative_balances(config: &Config, block: &eth::v2::Block) -> Result<BlockBalanceDeltas> {
    let mut balance_deltas = vec![];
    if block.number == config.start_block {
        let tx = config.creation(block)?;
        for token in [config.base, config.quote] {
            // Seed the post-block balance once. Transfers in this block are already included.
            let balance = BalanceOf { owner: config.custodian.to_vec() }
                .call(token.to_vec())
                .ok_or_else(|| anyhow::anyhow!("BaiBai initial balanceOf failed for {token}"))?;
            balance_deltas.push(BalanceDelta {
                ord: tx.begin_ordinal,
                tx: Some(tx.into()),
                token: token.to_vec(),
                delta: balance.to_signed_bytes_be(),
                component_id: config.id().into_bytes(),
            });
        }
    } else {
        for tx in block.transactions() {
            for mut delta in extract_balance_deltas_from_tx(tx, |token, owner| {
                owner == config.custodian.as_slice() &&
                    (token == config.base.as_slice() || token == config.quote.as_slice())
            }) {
                delta.component_id = config.id().into_bytes();
                balance_deltas.push(delta);
            }
        }
    }
    balance_deltas.sort_unstable_by_key(|delta| delta.ord);
    Ok(BlockBalanceDeltas { balance_deltas })
}

#[substreams::handlers::store]
pub fn store_balances(deltas: BlockBalanceDeltas, store: StoreAddBigInt) {
    store_balance_changes(deltas, store);
}

#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    components: BlockTransactionProtocolComponents,
    balances: StoreDeltas,
    deltas: BlockBalanceDeltas,
) -> Result<BlockChanges> {
    protocol_changes(params, block, components, balances, deltas)
}

fn protocol_changes(
    params: String,
    block: eth::v2::Block,
    components: BlockTransactionProtocolComponents,
    balances: StoreDeltas,
    deltas: BlockBalanceDeltas,
) -> Result<BlockChanges> {
    let config = Config::parse(&params)?;
    let slots = config.slots();
    let mut changes: BTreeMap<u64, TransactionChangesBuilder> = BTreeMap::new();
    for creation in components.tx_components {
        let tx = creation
            .tx
            .ok_or_else(|| anyhow::anyhow!("missing creation transaction"))?;
        let builder = changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for component in creation.components {
            builder.add_protocol_component(&component);
            // start_block is the entrypoint's deployment block, before CurveBook v3
            // and custody storage exist. Subsequent writes replace these zero words.
            let mut attributes: Vec<_> = (0..32)
                .map(|i| creation_attribute(&format!("word_{i}"), vec![0; 32]))
                .collect();
            attributes.push(creation_attribute("balance_owner", config.custodian.to_vec()));
            attributes.push(creation_attribute("fees_indexed", vec![1]));
            builder.add_entity_change(&EntityChanges { component_id: component.id, attributes });
        }
    }
    for tx in block.transactions() {
        if block.number > VALIDATED_BLOCK && has_upgrade(&config, tx) {
            changes
                .entry(tx.index.into())
                .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()))
                .change_component_pause_state(&config.id(), true);
        }
        let mut attrs = storage_attributes(&config, tx, &slots);
        attrs.extend(fee_attributes(&config, tx));
        if !attrs.is_empty() {
            changes
                .entry(tx.index.into())
                .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()))
                .add_entity_change(&EntityChanges { component_id: config.id(), attributes: attrs });
        }
    }
    for (_, (tx, balances)) in aggregate_balances_changes(balances, deltas) {
        let builder = changes
            .entry(tx.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&tx));
        for tokens in balances.into_values() {
            for balance in tokens.into_values() {
                builder.add_balance_change(&balance);
            }
        }
    }
    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: changes
            .into_values()
            .filter_map(TransactionChangesBuilder::build)
            .collect(),
        storage_changes: vec![],
    })
}

/// All three dependencies are UUPS proxies. An upgrade may change storage, pricing,
/// or immutable custody wiring; pause until the integration is reviewed and reindexed.
fn has_upgrade(config: &Config, tx: &eth::v2::TransactionTrace) -> bool {
    tx.logs_with_calls().any(|(log, _)| {
        [config.entrypoint, config.curve_book, config.custodian]
            .iter()
            .any(|address| log.address == address.as_slice()) &&
            log.topics
                .first()
                .is_some_and(|topic| topic == UPGRADED.as_slice())
    })
}

/// Calls are nested trace order; their writes must be ordered by execution ordinal.
fn storage_attributes(
    config: &Config,
    tx: &eth::v2::TransactionTrace,
    slots: &[(Address, B256)],
) -> Vec<Attribute> {
    let mut latest = HashMap::new();
    for call in tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
    {
        for write in &call.storage_changes {
            if write.address != config.curve_book.as_slice() &&
                write.address != config.custodian.as_slice()
            {
                continue;
            }
            if let Some(index) = slots
                .iter()
                .position(|(address, slot)| {
                    write.address == address.as_slice() && write.key == slot.as_slice()
                })
            {
                let entry = latest.entry(index).or_insert(write);
                if write.ordinal > entry.ordinal {
                    *entry = write;
                }
            }
        }
    }
    let mut attrs: Vec<_> = latest
        .into_iter()
        .map(|(i, write)| attribute(&format!("word_{i}"), write.new_value.clone()))
        .collect();
    attrs.sort_by(|a, b| a.name.cmp(&b.name));
    attrs
}

/// Fee events are emitted by the proxy, including during delegatecall. Only the final
/// value per transaction matters; clearing an override retains an explicit unconfigured value.
fn fee_attributes(config: &Config, tx: &eth::v2::TransactionTrace) -> Vec<Attribute> {
    if !tx
        .calls
        .iter()
        .any(|call| !call.state_reverted && call.address == config.entrypoint.as_slice())
    {
        return vec![];
    }
    let mut latest = BTreeMap::new();
    for (log, _) in tx
        .logs_with_calls()
        .filter(|(log, _)| log.address == config.entrypoint.as_slice())
    {
        let Some(topic) = log.topics.first() else { continue };
        if topic != TAKER_FEE_SET.as_slice() && topic != TAKER_FEE_CLEARED.as_slice() {
            continue;
        }
        let base = Address::from_slice(&log.topics[2][12..]);
        if base != config.base && !base.is_zero() {
            continue;
        }
        let configured = topic == TAKER_FEE_SET.as_slice();
        let bps = if configured { U256::from_be_slice(&log.data) } else { U256::ZERO };
        // A pair override takes precedence over the taker-wide (base zero) fee.
        // Configured zero is distinct from clearing an override.
        let name = format!(
            "{}_fee_{:x}",
            if base.is_zero() { "taker" } else { "pair" },
            Address::from_slice(&log.topics[1][12..])
        );
        let value = vec![u8::from(configured), (bps.to::<u16>() >> 8) as u8, bps.to::<u16>() as u8];
        let entry = latest
            .entry(name)
            .or_insert((0, vec![]));
        if log.ordinal >= entry.0 {
            *entry = (log.ordinal, value);
        }
    }
    latest
        .into_iter()
        .map(|(name, (_, value))| attribute(&name, value))
        .collect()
}

fn creation_attribute(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.into(), value, change: ChangeType::Creation.into() }
}

fn attribute(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.into(), value, change: ChangeType::Update.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, B256};
    use eth::v2::{Call, StorageChange, TransactionTrace};

    const PARAMS: &str = "entrypoint=0x98c1d9e102eb2806d902b13186bdc7892ac4ffba&curve_book=0x604d9b9eb1e1571c78661a6c1088427ec9c8c6e5&custodian=0xaac48feb93c5c97e0fb3c7c57e1633922a4acda3&base=0x4200000000000000000000000000000000000006&quote=0x833589fcd6edb6e08f4c7c32d4f71b54bda02913&start_block=50895895";

    #[test]
    fn sdk_balance_aggregation_preserves_component_and_token_identity() {
        use substreams::pb::substreams::StoreDelta;
        let config: Config = serde_qs::from_str(PARAMS).unwrap();
        let id = config.id();
        let tx = Transaction { hash: vec![1; 32], ..Default::default() };
        let deltas = BlockBalanceDeltas {
            balance_deltas: vec![BalanceDelta {
                ord: 1,
                tx: Some(tx),
                token: config.base.to_vec(),
                delta: vec![7],
                component_id: id.clone().into_bytes(),
            }],
        };
        let stores = StoreDeltas {
            deltas: vec![StoreDelta {
                ordinal: 1,
                key: format!(
                    "{}:{}",
                    id,
                    config
                        .base
                        .to_string()
                        .trim_start_matches("0x")
                        .to_lowercase()
                ),
                new_value: b"7".to_vec(),
                ..Default::default()
            }],
        };
        let aggregated = aggregate_balances_changes(stores, deltas);
        let (_, balances) = aggregated.values().next().unwrap();
        let balance = &balances[id.as_bytes()][config.base.as_slice()];
        assert_eq!(balance.balance, vec![7]);
        assert_eq!(balance.component_id, id.into_bytes());
        assert_eq!(balance.token, config.base.to_vec());
    }

    #[test]
    fn tracks_custody_transfers_in_ordinal_order_without_counting_self_transfers() {
        use alloy_primitives::{keccak256, Address, U256};
        use eth::v2::{Block, Log};
        let config = Config::parse(PARAMS).unwrap();
        let outside = Address::repeat_byte(1);
        let log = |from: Address, to: Address, amount: u64, ordinal| Log {
            address: config.base.to_vec(),
            topics: vec![
                keccak256("Transfer(address,address,uint256)").to_vec(),
                from.into_word().to_vec(),
                to.into_word().to_vec(),
            ],
            data: U256::from(amount)
                .to_be_bytes::<32>()
                .to_vec(),
            ordinal,
            ..Default::default()
        };
        let tx = TransactionTrace {
            status: 1,
            calls: vec![Call {
                logs: vec![
                    log(outside, config.custodian, 7, 2),
                    log(config.custodian, outside, 5, 1),
                    log(config.custodian, config.custodian, 100, 3),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let block = Block {
            number: config.start_block + 1,
            transaction_traces: vec![tx],
            ..Default::default()
        };
        let deltas = relative_balances(&config, &block)
            .unwrap()
            .balance_deltas;
        assert_eq!(deltas.len(), 4);
        assert_eq!(deltas[0].ord, 1);
        assert_eq!(BigInt::from_signed_bytes_be(&deltas[0].delta), BigInt::from(-5));
        assert_eq!(
            BigInt::from_signed_bytes_be(&deltas[2].delta) +
                BigInt::from_signed_bytes_be(&deltas[3].delta),
            BigInt::zero()
        );
        assert_eq!(deltas[1].ord, 2);
        assert_eq!(BigInt::from_signed_bytes_be(&deltas[1].delta), BigInt::from(7));
    }

    #[test]
    fn weth_wraps_and_unwraps_change_custody_balance() {
        use alloy_primitives::{keccak256, U256};
        use eth::v2::{Block, Log};
        let config = Config::parse(PARAMS).unwrap();
        let log = |event: &str, amount: u64, ordinal| Log {
            address: config.base.to_vec(),
            topics: vec![keccak256(event).to_vec(), config.custodian.into_word().to_vec()],
            data: U256::from(amount)
                .to_be_bytes::<32>()
                .to_vec(),
            ordinal,
            ..Default::default()
        };
        let block = Block {
            number: config.start_block + 1,
            transaction_traces: vec![TransactionTrace {
                status: 1,
                calls: vec![
                    Call {
                        logs: vec![
                            log("Deposit(address,uint256)", 9, 1),
                            log("Withdrawal(address,uint256)", 4, 2),
                        ],
                        ..Default::default()
                    },
                    Call {
                        logs: vec![log("Deposit(address,uint256)", 100, 3)],
                        state_reverted: true,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let deltas = relative_balances(&config, &block)
            .unwrap()
            .balance_deltas;
        assert_eq!(deltas.len(), 2);
        assert_eq!(BigInt::from_signed_bytes_be(&deltas[0].delta), BigInt::from(9));
        assert_eq!(BigInt::from_signed_bytes_be(&deltas[1].delta), BigInt::from(-4));
        assert!(deltas
            .iter()
            .all(|delta| delta.component_id == config.id().as_bytes()));
    }

    #[test]
    fn fee_events_keep_explicit_zero_and_clear_in_execution_order() {
        use alloy_primitives::{keccak256, Address, U256};
        use eth::v2::Log;
        let config = Config::parse(PARAMS).unwrap();
        let router = Address::repeat_byte(1);
        let log = |base: Address, bps: Option<u16>, ordinal| Log {
            address: config.entrypoint.to_vec(),
            topics: vec![
                keccak256(if bps.is_some() {
                    "TakerFeeSet(address,address,uint16)"
                } else {
                    "TakerFeeCleared(address,address)"
                })
                .to_vec(),
                router.into_word().to_vec(),
                base.into_word().to_vec(),
            ],
            data: bps.map_or(vec![], |value| {
                U256::from(value)
                    .to_be_bytes::<32>()
                    .to_vec()
            }),
            ordinal,
            ..Default::default()
        };
        let tx = TransactionTrace {
            calls: vec![
                Call {
                    address: config.entrypoint.to_vec(),
                    logs: vec![log(config.base, Some(0), 20), log(Address::ZERO, None, 30)],
                    ..Default::default()
                },
                Call {
                    address: config.entrypoint.to_vec(),
                    logs: vec![
                        log(config.base, Some(10), 10),
                        log(Address::ZERO, Some(20), 15),
                        log(Address::repeat_byte(2), Some(100), 16),
                        log(config.base, Some(2000), 5),
                    ],
                    ..Default::default()
                },
                Call {
                    address: config.entrypoint.to_vec(),
                    logs: vec![log(config.base, Some(100), 40)],
                    state_reverted: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let attrs: HashMap<_, _> = fee_attributes(&config, &tx)
            .into_iter()
            .map(|attr| (attr.name, attr.value))
            .collect();
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[&format!("pair_fee_{router:x}")], vec![1, 0, 0]);
        assert_eq!(attrs[&format!("taker_fee_{router:x}")], vec![0, 0, 0]);
        let tx = TransactionTrace {
            calls: vec![Call {
                address: config.entrypoint.to_vec(),
                logs: vec![log(config.base, Some(2000), 1)],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(fee_attributes(&config, &tx)[0].value, vec![1, 7, 208]);
    }

    #[test]
    fn dependency_upgrades_pause_routing_after_the_validated_history() {
        use eth::v2::{Block, BlockHeader, Log};
        let config = Config::parse(PARAMS).unwrap();
        for dependency in [config.entrypoint, config.curve_book, config.custodian, Address::ZERO] {
            for (number, reverted, paused) in [
                (VALIDATED_BLOCK, false, false),
                (VALIDATED_BLOCK + 1, false, dependency != Address::ZERO),
                (VALIDATED_BLOCK + 1, true, false),
            ] {
                let block = Block {
                    number,
                    header: Some(BlockHeader {
                        timestamp: Some(Default::default()),
                        ..Default::default()
                    }),
                    transaction_traces: vec![TransactionTrace {
                        status: 1,
                        calls: vec![Call {
                            state_reverted: reverted,
                            logs: vec![Log {
                                address: dependency.to_vec(),
                                topics: vec![
                                    UPGRADED.to_vec(),
                                    Address::repeat_byte(1)
                                        .into_word()
                                        .to_vec(),
                                ],
                                ..Default::default()
                            }],
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                };
                let changes = protocol_changes(
                    PARAMS.into(),
                    block,
                    Default::default(),
                    Default::default(),
                    Default::default(),
                )
                .unwrap();
                let attrs: Vec<_> = changes
                    .changes
                    .iter()
                    .flat_map(|tx| &tx.entity_changes)
                    .flat_map(|entity| &entity.attributes)
                    .collect();
                assert_eq!(attrs.len(), usize::from(paused));
                if paused {
                    assert_eq!(attrs[0].name, "paused");
                    assert_eq!(attrs[0].value, vec![1]);
                    assert_eq!(attrs[0].change, ChangeType::Creation as i32);
                }
            }
        }
    }

    #[test]
    fn chooses_last_execution_write_and_ignores_reverted_calls() {
        let book = address!("604d9b9eb1e1571c78661a6c1088427ec9c8c6e5");
        let slot = B256::repeat_byte(1);
        let write = |ordinal, value| StorageChange {
            address: book.to_vec(),
            key: slot.to_vec(),
            new_value: vec![value],
            ordinal,
            ..Default::default()
        };
        let tx = TransactionTrace {
            calls: vec![
                Call { storage_changes: vec![write(30, 3)], ..Default::default() },
                Call { storage_changes: vec![write(20, 2)], ..Default::default() },
                Call {
                    storage_changes: vec![write(40, 4)],
                    state_reverted: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let attrs = storage_attributes(&Config::parse(PARAMS).unwrap(), &tx, &[(book, slot)]);
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].name, "word_0");
        assert_eq!(attrs[0].value, vec![3]);
        assert!(storage_attributes(&Config::parse(PARAMS).unwrap(), &tx, &[(book, B256::ZERO)])
            .is_empty());
    }
}
