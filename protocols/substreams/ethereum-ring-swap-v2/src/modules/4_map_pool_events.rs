use std::collections::HashMap;

use itertools::Itertools;
use substreams::{
    prelude::BigInt,
    store::{StoreGet, StoreGetBigInt, StoreGetProto, StoreGetString},
};
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::{event_handler::EventHandler, hex::Hexable};

use super::store_pool_reserves::exposed_reserves;
use crate::{abi::pool::events::Sync, store_key::StoreKey, traits::PoolAddresser};
use tycho_substreams::prelude::*;

struct TimedBalanceChange {
    ord: u64,
    transaction: Transaction,
    change: BalanceChange,
}

#[substreams::handlers::map]
#[allow(clippy::too_many_arguments)]
pub fn map_pool_events(
    block: eth::Block,
    block_entity_changes: BlockChanges,
    pools_store: StoreGetProto<ProtocolComponent>,
    pool_reserve_store: StoreGetBigInt,
    wrapper_backing_deltas: BlockBalanceDeltas,
    wrapper_backing_store: StoreGetBigInt,
    token_pools_store: StoreGetString,
) -> Result<BlockChanges, substreams::errors::Error> {
    let mut tx_changes: HashMap<u64, TransactionChangesBuilder> = HashMap::new();
    let mut timed_balance_changes = Vec::new();
    let component_creation_txs = component_creation_transactions(&block_entity_changes);

    merge_created_pools(block_entity_changes, &mut tx_changes);
    handle_sync(
        &block,
        &pools_store,
        &wrapper_backing_store,
        &mut tx_changes,
        &mut timed_balance_changes,
    );
    add_backing_component_changes(
        &wrapper_backing_deltas,
        &wrapper_backing_store,
        &token_pools_store,
        &pools_store,
        &pool_reserve_store,
        &component_creation_txs,
        &mut timed_balance_changes,
    );
    apply_timed_balance_changes(timed_balance_changes, &mut tx_changes);

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: tx_changes
            .into_iter()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect(),
        storage_changes: vec![],
    })
}

fn handle_sync(
    block: &eth::Block,
    pools_store: &StoreGetProto<ProtocolComponent>,
    wrapper_backing_store: &StoreGetBigInt,
    tx_changes: &mut HashMap<u64, TransactionChangesBuilder>,
    timed_balance_changes: &mut Vec<TimedBalanceChange>,
) {
    let mut on_sync = |event: Sync, tx: &eth::TransactionTrace, log: &eth::Log| {
        let pool_address = log.address.to_hex();
        let pool = pools_store.must_get_last(StoreKey::Pool.get_unique_key(&pool_address));
        let reserves = exposed_reserves(&pool, event.reserve0, event.reserve1);
        let transaction: Transaction = tx.into();
        let builder = tx_changes
            .entry(transaction.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&transaction));
        builder.add_entity_change(&reserve_change(&pool_address, &reserves));

        for (token, reserve) in pool.tokens.iter().zip(reserves) {
            let wrapper = component_wrapper(&pool, token).unwrap_or_else(|| {
                panic!("Ring pool {} has no wrapper for token {}", pool.id, hex::encode(token))
            });
            let backing = wrapper_backing_store
                .get_at(log.ordinal, backing_key(wrapper, token))
                .unwrap_or_else(BigInt::zero);
            timed_balance_changes.push(TimedBalanceChange {
                ord: log.ordinal,
                transaction: transaction.clone(),
                change: component_balance_change(&pool, token, reserve, backing),
            });
        }
    };

    let mut event_handler = EventHandler::new(block);
    event_handler.filter_by_address(PoolAddresser { store: pools_store });
    event_handler.on::<Sync, _>(&mut on_sync);
    event_handler.handle_events();
}

fn add_backing_component_changes(
    wrapper_backing_deltas: &BlockBalanceDeltas,
    wrapper_backing_store: &StoreGetBigInt,
    token_pools_store: &StoreGetString,
    pools_store: &StoreGetProto<ProtocolComponent>,
    pool_reserve_store: &StoreGetBigInt,
    component_creation_txs: &HashMap<String, u64>,
    timed_balance_changes: &mut Vec<TimedBalanceChange>,
) {
    for delta in &wrapper_backing_deltas.balance_deltas {
        let Some(transaction) = delta.tx.clone() else {
            continue;
        };
        let wrapper_id = String::from_utf8(delta.component_id.clone())
            .expect("FewToken wrapper balance delta is not valid UTF-8");
        let wrapper = hex::decode(&wrapper_id)
            .expect("FewToken wrapper balance delta has an invalid wrapper address");
        let token_key = hex::encode(&delta.token);
        let Some(component_ids) = token_pools_store.get_at(delta.ord, &token_key) else {
            continue;
        };
        let backing = wrapper_backing_store
            .get_at(delta.ord, backing_key(&wrapper, &delta.token))
            .unwrap_or_else(BigInt::zero);

        for component_id in component_ids
            .split(';')
            .filter(|component_id| !component_id.is_empty())
            .unique()
        {
            if !component_existed_by_transaction(
                component_creation_txs,
                component_id,
                transaction.index,
            ) {
                continue;
            }
            let pool = pools_store.must_get_last(StoreKey::Pool.get_unique_key(component_id));
            if component_wrapper(&pool, &delta.token) != Some(wrapper.as_slice()) {
                continue;
            }
            let reserve = pool_reserve_store
                .get_at(delta.ord, pool_reserve_key(component_id, &delta.token))
                .unwrap_or_else(BigInt::zero);
            timed_balance_changes.push(TimedBalanceChange {
                ord: delta.ord,
                transaction: transaction.clone(),
                change: component_balance_change(&pool, &delta.token, reserve, backing.clone()),
            });
        }
    }
}

fn component_creation_transactions(block_changes: &BlockChanges) -> HashMap<String, u64> {
    block_changes
        .changes
        .iter()
        .filter_map(|change| {
            change
                .tx
                .as_ref()
                .map(|tx| (tx.index, &change.component_changes))
        })
        .flat_map(|(tx_index, components)| {
            components
                .iter()
                .map(move |component| (component.id.clone(), tx_index))
        })
        .collect()
}

fn component_existed_by_transaction(
    component_creation_txs: &HashMap<String, u64>,
    component_id: &str,
    transaction_index: u64,
) -> bool {
    component_creation_txs
        .get(component_id)
        .is_none_or(|creation_index| *creation_index <= transaction_index)
}

fn apply_timed_balance_changes(
    mut balance_changes: Vec<TimedBalanceChange>,
    tx_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    balance_changes.sort_unstable_by_key(|change| change.ord);
    for timed_change in balance_changes {
        tx_changes
            .entry(timed_change.transaction.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&timed_change.transaction))
            .add_balance_change(&timed_change.change);
    }
}

fn component_balance_change(
    pool: &ProtocolComponent,
    token: &[u8],
    reserve: BigInt,
    backing: BigInt,
) -> BalanceChange {
    BalanceChange {
        token: token.to_vec(),
        balance: min_available_balance(reserve, backing).to_signed_bytes_be(),
        component_id: pool.id.as_bytes().to_vec(),
    }
}

fn min_available_balance(reserve: BigInt, backing: BigInt) -> BigInt {
    let reserve = reserve.max(BigInt::zero());
    let backing = backing.max(BigInt::zero());
    reserve.min(backing)
}

fn reserve_change(component_id: &str, reserves: &[BigInt; 2]) -> EntityChanges {
    EntityChanges {
        component_id: component_id.to_string(),
        attributes: reserves
            .iter()
            .enumerate()
            .map(|(index, reserve)| Attribute {
                name: format!("reserve{index}"),
                value: reserve.clone().to_signed_bytes_be(),
                change: ChangeType::Update.into(),
            })
            .collect(),
    }
}

fn component_wrapper<'a>(pool: &'a ProtocolComponent, token: &[u8]) -> Option<&'a [u8]> {
    if static_attribute(pool, "underlying_token0") == token {
        Some(static_attribute(pool, "fw_token0"))
    } else if static_attribute(pool, "underlying_token1") == token {
        Some(static_attribute(pool, "fw_token1"))
    } else {
        None
    }
}

fn static_attribute<'a>(pool: &'a ProtocolComponent, name: &str) -> &'a [u8] {
    pool.static_att
        .iter()
        .find(|attribute| attribute.name == name)
        .map(|attribute| attribute.value.as_slice())
        .unwrap_or_else(|| panic!("Ring pool {} is missing the {} static attribute", pool.id, name))
}

fn backing_key(wrapper: &[u8], token: &[u8]) -> String {
    format!("{}:{}", hex::encode(wrapper), hex::encode(token))
}

fn pool_reserve_key(component_id: &str, token: &[u8]) -> String {
    StoreKey::PoolReserve.get_unique_key(&format!("{}:{}", component_id, hex::encode(token)))
}

fn merge_created_pools(
    block_entity_changes: BlockChanges,
    tx_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    for change in block_entity_changes.changes {
        let Some(transaction) = change.tx else {
            continue;
        };
        let builder = tx_changes
            .entry(transaction.index)
            .or_insert_with(|| TransactionChangesBuilder::new(&transaction));
        for component in &change.component_changes {
            builder.add_protocol_component(component);
        }
        for entity_change in &change.entity_changes {
            builder.add_entity_change(entity_change);
        }
        for balance_change in &change.balance_changes {
            builder.add_balance_change(balance_change);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring_pool(id: &str, reserves_inverted: u8) -> ProtocolComponent {
        let underlying0 = if reserves_inverted == 0 { vec![1; 20] } else { vec![2; 20] };
        let underlying1 = if reserves_inverted == 0 { vec![2; 20] } else { vec![1; 20] };
        ProtocolComponent {
            id: id.to_string(),
            tokens: vec![vec![1; 20], vec![2; 20]],
            static_att: vec![
                Attribute {
                    name: "reserves_inverted".to_string(),
                    value: vec![reserves_inverted],
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "underlying_token0".to_string(),
                    value: underlying0,
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "underlying_token1".to_string(),
                    value: underlying1,
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "fw_token0".to_string(),
                    value: vec![3; 20],
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "fw_token1".to_string(),
                    value: vec![4; 20],
                    change: ChangeType::Creation.into(),
                },
            ],
            ..Default::default()
        }
    }

    fn transaction(index: u64) -> Transaction {
        Transaction { index, hash: vec![index as u8], ..Default::default() }
    }

    #[test]
    fn reserve_change_keeps_pair_order() {
        let reserves = exposed_reserves(&ring_pool("pool", 0), BigInt::from(5), BigInt::from(2));
        let change = reserve_change("pool", &reserves);

        assert_eq!(change.attributes[0].value, BigInt::from(5).to_signed_bytes_be());
        assert_eq!(change.attributes[1].value, BigInt::from(2).to_signed_bytes_be());
    }

    #[test]
    fn reserve_change_swaps_inverted_reserves() {
        let reserves = exposed_reserves(&ring_pool("pool", 1), BigInt::from(7), BigInt::from(9));
        let change = reserve_change("pool", &reserves);

        assert_eq!(change.attributes[0].value, BigInt::from(9).to_signed_bytes_be());
        assert_eq!(change.attributes[1].value, BigInt::from(7).to_signed_bytes_be());
    }

    #[test]
    fn component_balance_is_capped_by_reserve_and_backing() {
        assert_eq!(min_available_balance(BigInt::from(100), BigInt::from(70)), BigInt::from(70));
        assert_eq!(min_available_balance(BigInt::from(40), BigInt::from(70)), BigInt::from(40));
        assert_eq!(min_available_balance(BigInt::from(40), BigInt::from(-1)), BigInt::zero());
    }

    #[test]
    fn shared_backing_is_capped_against_each_pool_reserve() {
        let token = vec![1; 20];
        let first = component_balance_change(
            &ring_pool("pool-a", 0),
            &token,
            BigInt::from(80),
            BigInt::from(100),
        );
        let second = component_balance_change(
            &ring_pool("pool-b", 0),
            &token,
            BigInt::from(120),
            BigInt::from(100),
        );

        assert_eq!(first.balance, BigInt::from(80).to_signed_bytes_be());
        assert_eq!(second.balance, BigInt::from(100).to_signed_bytes_be());
    }

    #[test]
    fn later_balance_change_wins_within_the_same_transaction() {
        let token = vec![1; 20];
        let pool = ring_pool("pool", 0);
        let tx = transaction(7);
        let mut tx_changes = HashMap::new();

        apply_timed_balance_changes(
            vec![
                TimedBalanceChange {
                    ord: 1,
                    transaction: tx.clone(),
                    change: component_balance_change(
                        &pool,
                        &token,
                        BigInt::from(10),
                        BigInt::from(3),
                    ),
                },
                TimedBalanceChange {
                    ord: 2,
                    transaction: tx,
                    change: component_balance_change(
                        &pool,
                        &token,
                        BigInt::from(10),
                        BigInt::from(8),
                    ),
                },
            ],
            &mut tx_changes,
        );

        let changes = tx_changes
            .remove(&7)
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(changes.balance_changes.len(), 1);
        assert_eq!(changes.balance_changes[0].balance, BigInt::from(8).to_signed_bytes_be());
    }

    #[test]
    fn backing_change_does_not_update_a_component_created_in_a_later_transaction() {
        let creation_txs = HashMap::from([("future-pool".to_string(), 7)]);

        assert!(!component_existed_by_transaction(&creation_txs, "future-pool", 6));
        assert!(component_existed_by_transaction(&creation_txs, "future-pool", 7));
        assert!(component_existed_by_transaction(&creation_txs, "existing-pool", 1));
    }

    #[test]
    fn inverted_pool_maps_component_tokens_to_the_matching_wrappers() {
        let pool = ring_pool("pool", 1);

        assert_eq!(component_wrapper(&pool, &[1; 20]), Some([4; 20].as_slice()));
        assert_eq!(component_wrapper(&pool, &[2; 20]), Some([3; 20].as_slice()));
    }
}
