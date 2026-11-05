use crate::{
    abi::vault_contract::events::{LiquidityAdded, LiquidityRemoved, Swap},
    utils::{address_id, mapping_storage_key_for_address},
};
use keccak_hash::keccak;
use std::collections::HashMap;
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetProto},
};
use substreams_ethereum::{
    pb::eth::{self, v2::StorageChange},
    Event,
};
use tycho_substreams::prelude::*;

// VaultStorage.sol:
// mapping(address pool => mapping(uint256 tokenIndex => bytes32 packedTokenBalance))
// internal _poolTokenBalances;
const POOL_TOKEN_BALANCES_SLOT: u8 = 5;

type BalanceKey = (Vec<u8>, Vec<u8>);
type CandidateComponents = HashMap<u64, HashMap<Vec<u8>, CandidateComponent>>;

struct CandidateComponent {
    component_id: String,
    component: ProtocolComponent,
}

/// Extracts absolute pool token balances from Vault `_poolTokenBalances` storage writes.
///
/// Returns one entry per transaction that wrote the balance of a tracked pool, carrying
/// the final raw balance of each written (component, token) pair as an absolute
/// `BalanceChange`. Storage holds the post-write balance, so no delta accounting is
/// needed and a missed write is corrected by the next observed one.
pub(crate) fn absolute_pool_balances(
    block: &eth::v2::Block,
    store: &StoreGetProto<ProtocolComponent>,
    vault_address: &[u8],
) -> Vec<(Transaction, Vec<BalanceChange>)> {
    let candidates = collect_pool_balance_candidates(block, store, vault_address);

    let mut tx_balances = Vec::new();
    for tx in &block.transaction_traces {
        let Some(candidates) = candidates.get(&u64::from(tx.index)) else {
            continue;
        };

        // A single Vault operation can write the same pool/token balance more than once
        // (for example yield-fee sync before the actual swap). The write with the highest
        // ordinal holds the transaction's final balance.
        let mut last_writes: HashMap<BalanceKey, (u64, BigInt)> = HashMap::new();
        tx.calls
            .iter()
            .filter(|call| !call.state_reverted)
            .filter(|call| call.address == vault_address)
            .for_each(|call| {
                for change in &call.storage_changes {
                    record_pool_token_balance_write(&mut last_writes, candidates, change);
                }
            });

        if !last_writes.is_empty() {
            let balances = last_writes
                .into_iter()
                .map(|((component_id, token), (_, raw_balance))| BalanceChange {
                    token,
                    balance: raw_balance.to_bytes_be().1,
                    component_id,
                })
                .collect();
            tx_balances.push((Transaction::from(tx), balances));
        }
    }

    tx_balances
}

fn collect_pool_balance_candidates(
    block: &eth::v2::Block,
    store: &StoreGetProto<ProtocolComponent>,
    vault_address: &[u8],
) -> CandidateComponents {
    let mut candidate_components: CandidateComponents = HashMap::new();

    // Events are only used as hints for which pools may have touched Vault storage in this
    // transaction. The emitted amounts are not used as balances because fees, hooks, rates,
    // and rounding are already reflected in the final storage write.
    block
        .logs()
        .filter(|log| log.address() == vault_address)
        .for_each(|vault_log| {
            if let Some(pool) = pool_from_balance_event(vault_log.log) {
                let tx_index = u64::from(vault_log.receipt.transaction.index);
                let component_id = address_id(&pool);
                if let Some(component) = store.get_last(format!("pool:{component_id}")) {
                    candidate_components
                        .entry(tx_index)
                        .or_default()
                        .entry(pool)
                        .or_insert(CandidateComponent { component_id, component });
                }
            }
        });

    candidate_components
}

fn pool_from_balance_event(log: &eth::v2::Log) -> Option<Vec<u8>> {
    if let Some(Swap { pool, .. }) = Swap::match_and_decode(log) {
        return Some(pool);
    }
    if let Some(LiquidityAdded { pool, .. }) = LiquidityAdded::match_and_decode(log) {
        return Some(pool);
    }
    if let Some(LiquidityRemoved { pool, .. }) = LiquidityRemoved::match_and_decode(log) {
        return Some(pool);
    }

    None
}

fn record_pool_token_balance_write(
    last_writes: &mut HashMap<BalanceKey, (u64, BigInt)>,
    candidates: &HashMap<Vec<u8>, CandidateComponent>,
    change: &StorageChange,
) {
    for (pool, candidate) in candidates {
        for (token_index, token) in candidate
            .component
            .tokens
            .iter()
            .enumerate()
        {
            if change.key != get_pool_token_balance_storage_key(pool, token_index) {
                continue;
            }

            let raw_balance = raw_balance_from_packed(&change.new_value);
            let key = (
                candidate
                    .component_id
                    .as_bytes()
                    .to_vec(),
                token.clone(),
            );

            last_writes
                .entry(key)
                .and_modify(|(ordinal, balance)| {
                    if change.ordinal > *ordinal {
                        *ordinal = change.ordinal;
                        *balance = raw_balance.clone();
                    }
                })
                .or_insert((change.ordinal, raw_balance));
        }
    }
}

fn get_pool_token_balance_storage_key(pool_address: &[u8], token_index: usize) -> Vec<u8> {
    // Solidity storage:
    // https://github.com/balancer/balancer-v3-monorepo/blob/80fd29ce4eb627139694db7fef5aba355759d303/pkg/vault/contracts/VaultStorage.sol#L93-L96
    //
    // mapping(address pool => mapping(uint256 tokenIndex => bytes32 packedTokenBalance))
    //     internal _poolTokenBalances;
    //
    // The outer mapping slot is:
    // keccak256(abi.encode(pool_address, POOL_TOKEN_BALANCES_SLOT))
    //
    // The inner mapping slot for one token balance is:
    // keccak256(abi.encode(token_index, outer_mapping_slot))
    //
    // ABI encoding pads an address to 32 bytes by left-padding 12 zero bytes.
    let pool_balances_slot =
        mapping_storage_key_for_address(pool_address, POOL_TOKEN_BALANCES_SLOT);

    let mut input = [0u8; 64];
    input[24..32].copy_from_slice(&(token_index as u64).to_be_bytes());
    input[32..64].copy_from_slice(&pool_balances_slot);

    keccak(input.as_slice())
        .as_bytes()
        .to_vec()
}

fn raw_balance_from_packed(packed_balance: &[u8]) -> BigInt {
    // PackedTokenBalance.sol stores two uint128 values in one bytes32:
    // https://github.com/balancer/balancer-v3-monorepo/blob/80fd29ce4eb627139694db7fef5aba355759d303/pkg/solidity-utils/contracts/helpers/PackedTokenBalance.sol#L18-L28
    //
    // raw balance:     least significant 128 bits
    // derived balance: most significant 128 bits
    //
    // We index raw token balances, so decode the low 16 bytes.
    let raw_balance = if packed_balance.len() > 16 {
        &packed_balance[packed_balance.len() - 16..]
    } else {
        packed_balance
    };

    if raw_balance.is_empty() {
        BigInt::from(0)
    } else {
        BigInt::from_unsigned_bytes_be(raw_balance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use substreams::hex;

    #[test]
    fn computes_pool_token_balance_storage_keys() {
        let pool = hex!("da66e8ddf9959e4db759bfd06256730d8a8b2d13");

        assert_eq!(
            get_pool_token_balance_storage_key(&pool, 0),
            hex!("b303074f1bc99073c51a5b3fcad64dfc27216608aef11ec1fe134554c7476e70").to_vec()
        );
        assert_eq!(
            get_pool_token_balance_storage_key(&pool, 1),
            hex!("01774b610d8b1db39156b04321e7d32c2fea9308da6596a46acd3e0cbe2112ca").to_vec()
        );
    }

    #[test]
    fn decodes_raw_balance_from_packed_pool_token_balance() {
        let packed_balance =
            hex!("00000000000000000319ed08777be07400000000000000000319ed08777be074");

        assert_eq!(
            raw_balance_from_packed(&packed_balance),
            BigInt::from_unsigned_bytes_be(&223470277151678580u128.to_be_bytes())
        );
    }

    #[test]
    fn keeps_the_last_write_per_pool_token() {
        let pool = hex!("da66e8ddf9959e4db759bfd06256730d8a8b2d13").to_vec();
        let token = hex!("6b175474e89094c44da98b954eedeac495271d0f").to_vec();
        let candidates = candidates_for(&pool, &token);

        let mut last_writes = HashMap::new();
        for (ordinal, raw) in [(5u64, 100u128), (3, 50), (9, 250)] {
            record_pool_token_balance_write(
                &mut last_writes,
                &candidates,
                &pool_balance_storage_change(&pool, ordinal, raw),
            );
        }

        let key = (address_id(&pool).into_bytes(), token);
        assert_eq!(last_writes[&key], (9, BigInt::from(250)));
    }

    #[test]
    fn ignores_writes_to_other_storage_keys() {
        let pool = hex!("da66e8ddf9959e4db759bfd06256730d8a8b2d13").to_vec();
        let token = hex!("6b175474e89094c44da98b954eedeac495271d0f").to_vec();
        let candidates = candidates_for(&pool, &token);

        let mut change = pool_balance_storage_change(&pool, 1, 100);
        change.key = vec![0xff; 32];

        let mut last_writes = HashMap::new();
        record_pool_token_balance_write(&mut last_writes, &candidates, &change);

        assert!(last_writes.is_empty());
    }

    fn candidates_for(pool: &[u8], token: &[u8]) -> HashMap<Vec<u8>, CandidateComponent> {
        let component = ProtocolComponent { tokens: vec![token.to_vec()], ..Default::default() };
        HashMap::from([(
            pool.to_vec(),
            CandidateComponent { component_id: address_id(pool), component },
        )])
    }

    fn pool_balance_storage_change(pool: &[u8], ordinal: u64, raw: u128) -> StorageChange {
        let mut new_value = vec![0u8; 32];
        new_value[16..32].copy_from_slice(&raw.to_be_bytes());
        StorageChange {
            key: get_pool_token_balance_storage_key(pool, 0),
            new_value,
            ordinal,
            ..Default::default()
        }
    }
}
