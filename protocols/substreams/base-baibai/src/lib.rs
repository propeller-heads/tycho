#![allow(clippy::not_unsafe_ptr_arg_deref)]
mod config;

use anyhow::Result;
use config::Config;
use std::collections::{BTreeMap, HashMap};
use substreams::{pb::substreams::StoreDeltas, prelude::*};
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::{
    abi::erc20::{events::Transfer, functions::BalanceOf},
    balances::{aggregate_balances_changes, store_balance_changes},
    prelude::*,
};

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
    } else if block.number > config.start_block {
        for view in block.logs() {
            let log = view.log;
            if log.address != config.base.as_slice() && log.address != config.quote.as_slice() {
                continue;
            }
            let Some(transfer) = Transfer::match_and_decode(log) else {
                continue;
            };
            if transfer.from == transfer.to {
                continue;
            }
            let value = if transfer.to == config.custodian.as_slice() {
                transfer.value
            } else if transfer.from == config.custodian.as_slice() {
                transfer.value.neg()
            } else {
                continue;
            };
            balance_deltas.push(BalanceDelta {
                ord: log.ordinal,
                tx: Some(view.receipt.transaction.into()),
                token: log.address.clone(),
                delta: value.to_signed_bytes_be(),
                component_id: config.id().into_bytes(),
            });
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
                .map(|i| attribute(&format!("word_{i}"), vec![0; 32]))
                .collect();
            attributes.push(attribute("balance_owner", config.custodian.to_vec()));
            builder.add_entity_change(&EntityChanges { component_id: component.id, attributes });
        }
    }
    if block.number >= config.start_block {
        for tx in block.transactions() {
            let attrs = storage_attributes(tx, &slots);
            if !attrs.is_empty() {
                changes
                    .entry(tx.index.into())
                    .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()))
                    .add_entity_change(&EntityChanges {
                        component_id: config.id(),
                        attributes: attrs,
                    });
            }
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

/// Calls are nested trace order; their writes must be ordered by execution ordinal.
fn storage_attributes(
    tx: &eth::v2::TransactionTrace,
    slots: &[(alloy_primitives::Address, alloy_primitives::B256)],
) -> Vec<Attribute> {
    let mut latest = HashMap::new();
    for call in tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
    {
        for write in &call.storage_changes {
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
        use eth::v2::{Block, Log, TransactionReceipt};
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
            receipt: Some(TransactionReceipt {
                logs: vec![
                    log(outside, config.custodian, 7, 2),
                    log(config.custodian, outside, 5, 1),
                    log(config.custodian, config.custodian, 100, 3),
                ],
                ..Default::default()
            }),
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
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0].ord, 1);
        assert_eq!(BigInt::from_signed_bytes_be(&deltas[0].delta), BigInt::from(-5));
        assert_eq!(deltas[1].ord, 2);
        assert_eq!(BigInt::from_signed_bytes_be(&deltas[1].delta), BigInt::from(7));
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
        let attrs = storage_attributes(&tx, &[(book, slot)]);
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].name, "word_0");
        assert_eq!(attrs[0].value, vec![3]);
        assert!(storage_attributes(&tx, &[(book, B256::ZERO)]).is_empty());
    }
}
