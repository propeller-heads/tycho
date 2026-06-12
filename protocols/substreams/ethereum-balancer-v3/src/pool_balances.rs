use crate::{
    abi::vault_contract::events::{LiquidityAdded, LiquidityRemoved, Swap},
    constants::VAULT_ADDRESS,
    utils::{address_id, mapping_storage_key_for_address},
};
use keccak_hash::keccak;
use std::collections::HashMap;
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetInt64, StoreGetProto},
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

type BalanceDeltaKey = (u64, Vec<u8>, Vec<u8>);
type CandidateComponents = HashMap<u64, HashMap<Vec<u8>, CandidateComponent>>;

struct CandidateComponent {
    component_id: String,
    component: ProtocolComponent,
}

struct PoolBalanceStorageDelta {
    tx_index: u64,
    tx: Transaction,
    first_ordinal: u64,
    ordinal: u64,
    component_id: Vec<u8>,
    token: Vec<u8>,
    old_raw: BigInt,
    new_raw: BigInt,
}

pub(crate) fn pool_balance_seed_deltas(
    block: &eth::v2::Block,
    store: &StoreGetProto<ProtocolComponent>,
) -> Vec<BalanceDelta> {
    let candidates = collect_pool_balance_candidates(block, store);
    let storage_deltas = get_pool_token_balance_storage_deltas(block, &candidates);
    get_pool_balance_seed_deltas(&storage_deltas)
}

fn get_pool_balance_seed_deltas(storage_deltas: &[PoolBalanceStorageDelta]) -> Vec<BalanceDelta> {
    let mut seed_ordinals = HashMap::<String, u64>::new();

    storage_deltas
        .iter()
        .filter(|delta| pool_balance_delta(delta, false) != BigInt::from(0))
        .for_each(|delta| {
            let component_id = String::from_utf8(delta.component_id.clone())
                .expect("component_id is not valid utf-8!");
            seed_ordinals
                .entry(component_id)
                .and_modify(|ordinal| *ordinal = (*ordinal).min(delta.first_ordinal))
                .or_insert(delta.first_ordinal);
        });

    seed_ordinals
        .into_iter()
        .map(|(component_id, ordinal)| BalanceDelta {
            ord: ordinal,
            tx: None,
            token: vec![],
            delta: vec![],
            component_id: component_id.into_bytes(),
        })
        .collect()
}

pub(crate) fn relative_pool_balance_deltas(
    block: &eth::v2::Block,
    components_store: &StoreGetProto<ProtocolComponent>,
    seeded_pool_balances: &StoreGetInt64,
) -> Vec<BalanceDelta> {
    let candidates = collect_pool_balance_candidates(block, components_store);
    let storage_deltas = get_pool_token_balance_storage_deltas(block, &candidates);

    get_pool_token_balance_deltas(&storage_deltas, seeded_pool_balances)
}

fn collect_pool_balance_candidates(
    block: &eth::v2::Block,
    store: &StoreGetProto<ProtocolComponent>,
) -> CandidateComponents {
    let mut candidate_components: CandidateComponents = HashMap::new();

    // Events are only used as hints for which pools may have touched Vault storage in this
    // transaction. The emitted amounts are not used as balances because fees, hooks, rates,
    // and rounding are already reflected in the final storage write.
    block
        .logs()
        .filter(|log| log.address() == VAULT_ADDRESS)
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

fn get_pool_token_balance_storage_deltas(
    block: &eth::v2::Block,
    candidate_components: &CandidateComponents,
) -> Vec<PoolBalanceStorageDelta> {
    let mut storage_deltas = Vec::new();

    for tx in &block.transaction_traces {
        let tx_index = u64::from(tx.index);
        let Some(candidates) = candidate_components.get(&tx_index) else {
            continue;
        };

        // A single Vault operation can write the same pool/token balance more than once
        // (for example yield-fee sync before the actual swap). Keep one transaction-level
        // delta per pool/token: earliest old raw balance -> latest new raw balance.
        let mut tx_storage_deltas: HashMap<BalanceDeltaKey, PoolBalanceStorageDelta> =
            HashMap::new();
        let tycho_tx = Transaction::from(tx);

        tx.calls
            .iter()
            .filter(|call| !call.state_reverted)
            .filter(|call| call.address == VAULT_ADDRESS)
            .for_each(|call| {
                for change in &call.storage_changes {
                    add_pool_token_balance_storage_delta(
                        &mut tx_storage_deltas,
                        tx_index,
                        &tycho_tx,
                        candidates,
                        change,
                    );
                }
            });

        storage_deltas.extend(tx_storage_deltas.into_values());
    }

    storage_deltas
}

fn get_pool_token_balance_deltas(
    storage_deltas: &[PoolBalanceStorageDelta],
    seeded_pool_balances: &StoreGetInt64,
) -> Vec<BalanceDelta> {
    let mut first_ordinals: HashMap<(u64, Vec<u8>), u64> = HashMap::new();
    storage_deltas.iter().for_each(|delta| {
        first_ordinals
            .entry((delta.tx_index, delta.component_id.clone()))
            .and_modify(|ordinal| *ordinal = (*ordinal).min(delta.first_ordinal))
            .or_insert(delta.first_ordinal);
    });

    let mut balance_deltas = Vec::new();

    for value in storage_deltas {
        let first_ordinal = first_ordinals
            .get(&(value.tx_index, value.component_id.clone()))
            .copied()
            .unwrap_or(value.first_ordinal);
        let is_seeded = if first_ordinal == 0 {
            false
        } else {
            let component_id = String::from_utf8(value.component_id.clone())
                .expect("component_id is not valid utf-8!");
            seeded_pool_balances.has_at(first_ordinal - 1, component_id)
        };
        let delta = pool_balance_delta(value, is_seeded);

        if delta != BigInt::from(0) {
            balance_deltas.push(BalanceDelta {
                ord: value.ordinal,
                tx: Some(value.tx.clone()),
                token: value.token.clone(),
                delta: delta.to_signed_bytes_be(),
                component_id: value.component_id.clone(),
            });
        }
    }

    balance_deltas
}

fn pool_balance_delta(value: &PoolBalanceStorageDelta, is_seeded: bool) -> BigInt {
    if is_seeded {
        value.new_raw.clone() - value.old_raw.clone()
    } else {
        value.new_raw.clone()
    }
}

fn add_pool_token_balance_storage_delta(
    tx_storage_deltas: &mut HashMap<BalanceDeltaKey, PoolBalanceStorageDelta>,
    tx_index: u64,
    tx: &Transaction,
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

            let old_raw = raw_balance_from_packed(&change.old_value);
            let new_raw = raw_balance_from_packed(&change.new_value);
            let component_id = candidate
                .component_id
                .as_bytes()
                .to_vec();
            let key = (tx_index, component_id.clone(), token.clone());

            tx_storage_deltas
                .entry(key)
                .and_modify(|value| {
                    if change.ordinal < value.first_ordinal {
                        value.first_ordinal = change.ordinal;
                        value.old_raw = old_raw.clone();
                    }
                    if change.ordinal > value.ordinal {
                        value.ordinal = change.ordinal;
                        value.new_raw = new_raw.clone();
                    }
                })
                .or_insert_with(|| PoolBalanceStorageDelta {
                    tx_index,
                    tx: tx.clone(),
                    first_ordinal: change.ordinal,
                    ordinal: change.ordinal,
                    component_id,
                    token: token.clone(),
                    old_raw,
                    new_raw,
                });
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
    fn existing_pool_balance_delta_uses_storage_diff() {
        assert_eq!(
            pool_balance_delta(&pool_balance_storage_delta(100, 150), true),
            BigInt::from(50)
        );
    }

    #[test]
    fn unseeded_pool_balance_delta_uses_absolute_new_raw_balance() {
        assert_eq!(
            pool_balance_delta(&pool_balance_storage_delta(100, 150), false),
            BigInt::from(150)
        );
    }

    fn pool_balance_storage_delta(old_raw: i32, new_raw: i32) -> PoolBalanceStorageDelta {
        PoolBalanceStorageDelta {
            tx_index: 0,
            tx: Transaction::default(),
            first_ordinal: 0,
            ordinal: 1,
            component_id: vec![],
            token: vec![],
            old_raw: BigInt::from(old_raw),
            new_raw: BigInt::from(new_raw),
        }
    }
}
