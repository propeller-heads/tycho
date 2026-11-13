//! Gives a pool that holds too little of the token a balance, so that a token can be analysed
//! before its liquidity arrives. A Uniswap V2 pair also gets a balance of the wrapped native
//! token and matching reserves, so that the legs still trade as real swaps.

use alloy::{
    primitives::{Address, B256, U256},
    rpc::types::{state::StateOverride, BlockNumberOrTag},
    sol_types::SolCall,
};
use tracing::debug;
use tycho_common::traits::BalanceSlotDetector;

use super::{
    code::CodeSummary,
    owner_calls::mapping_entry,
    runner::getReservesCall,
    scenario::{call, first_word, return_data_unless_reverted, V2Pair},
};
use crate::{
    erc20::{balanceOfCall, totalSupplyCall},
    rpc::EthereumRpcClient,
    services::entrypoint_tracer::balance_slot_detector::EVMBalanceSlotDetector,
    BytesCodec,
};

/// The pool gets `1 / SUPPLY_DIVISOR` of the total supply: launch liquidity usually holds most
/// of the supply, and the large buy (2% of the pool) stays below common max-transaction limits.
const SUPPLY_DIVISOR: u64 = 4;
/// Writes tried per balance slot while calibrating.
const CALIBRATION_STEPS: usize = 5;
/// `balanceOf(address)`.
const BALANCE_OF: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
/// Uniswap V2 pairs pack `reserve0`, `reserve1` and `blockTimestampLast` into slot 8.
const V2_RESERVES_SLOT: B256 = B256::with_last_byte(8);
/// Wrapped native token a funded pair gets: 10 units with 18 decimals. Any amount prices the
/// token; the buys are a share of it.
const FUNDED_NATIVE_RESERVE: u64 = 10_000_000_000_000_000_000;

/// The largest reserve a Uniswap V2 pair stores (`uint112`).
fn max_reserve() -> U256 {
    (U256::from(1) << 112) - U256::from(1)
}

/// The pool's balance written into the token's storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FundedBalance {
    pub slot: B256,
    /// The value written to `slot`.
    pub stored: U256,
    /// What `balanceOf(pool)` returns with it.
    pub balance: U256,
}

/// A balance of `1 / SUPPLY_DIVISOR` of the supply for `pool`, written to a slot `balanceOf`
/// reads and confirmed with `balanceOf`. Reflection tokens report a multiple of the stored value,
/// so the value is scaled once by what `balanceOf` reports. `None` when no slot gives a balance
/// within 0.1% of the target, or the supply is too small.
pub(super) async fn fund_pool(
    rpc: &EthereumRpcClient,
    token: Address,
    pool: Address,
    block: BlockNumberOrTag,
    code: &CodeSummary,
    min_balance: U256,
) -> Result<Option<FundedBalance>, String> {
    let slots: Vec<B256> = match balance_mappings(code).as_slice() {
        [] => detect_balance_slot(rpc, token, pool)
            .await
            .into_iter()
            .collect(),
        mappings => mappings
            .iter()
            .map(|mapping| mapping_entry(pool, *mapping))
            .collect(),
    };
    if slots.is_empty() {
        return Ok(None);
    }
    let supply = rpc
        .eth_call(call(Address::ZERO, token, totalSupplyCall {}.abi_encode()), block)
        .await;
    let Some(supply) = return_data_unless_reverted(supply)
        .map_err(|e| format!("totalSupply of {token:#x}: {e}"))?
    else {
        return Ok(None);
    };
    // Capped so that the balance also fits a V2 pair's reserve.
    let target = (first_word(supply.as_ref()) / U256::from(SUPPLY_DIVISOR)).min(max_reserve());
    if target < min_balance {
        return Ok(None);
    }
    for slot in slots {
        if let Some(funded) = calibrate(rpc, (token, pool), block, slot, target).await? {
            return Ok(Some(funded));
        }
    }
    debug!(?token, "balanceOf does not return the written balance in any slot");
    Ok(None)
}

/// Writes `target` to `slot`, then a value scaled by what `balanceOf` reported, until
/// `balanceOf` reports `target` within rounding. A reflection token divides the stored value by
/// a rate that can exceed 10^60, so a value reading as zero is raised by 2^64 before scaling.
async fn calibrate(
    rpc: &EthereumRpcClient,
    (token, pool): (Address, Address),
    block: BlockNumberOrTag,
    slot: B256,
    target: U256,
) -> Result<Option<FundedBalance>, String> {
    let mut stored = target;
    for _ in 0..CALIBRATION_STEPS {
        let Some(balance) = read_balance_with(rpc, token, pool, block, slot, stored).await? else {
            return Ok(None);
        };
        if close_to(balance, target) {
            return Ok(Some(FundedBalance { slot, stored, balance }));
        }
        let next =
            if balance.is_zero() { stored.checked_shl(64) } else { scale(stored, target, balance) };
        let Some(next) = next else { return Ok(None) };
        stored = next;
    }
    Ok(None)
}

/// `stored * target / balance`, without overflowing for large `stored`.
fn scale(stored: U256, target: U256, balance: U256) -> Option<U256> {
    match stored.checked_mul(target) {
        Some(product) => Some(product / balance),
        None => (stored / balance).checked_mul(target),
    }
}

/// Within 0.1% of `target`.
fn close_to(value: U256, target: U256) -> bool {
    let difference = if value > target { value - target } else { target - value };
    difference <= target / U256::from(1000)
}

/// Storage that makes `pair` a pair holding `token_balance` of `token` and
/// `FUNDED_NATIVE_RESERVE` of the wrapped native token, with matching reserves: slot and value
/// by contract. `None` when the wrapped native token's balance slot is not found or the pair
/// does not report the written reserves. The pair's reserve of the token is the balance
/// `balanceOf` reports.
pub(super) async fn fund_pair(
    rpc: &EthereumRpcClient,
    (token, pool): (Address, Address),
    pair: V2Pair,
    funded: FundedBalance,
    (block, block_time): (BlockNumberOrTag, u64),
) -> Result<Option<Vec<(Address, B256, B256)>>, String> {
    let Some(native_slot) = detect_balance_slot(rpc, pair.wrapped_native, pool).await else {
        return Ok(None);
    };
    let native = U256::from(FUNDED_NATIVE_RESERVE);
    let (reserve0, reserve1) = if token < pair.wrapped_native {
        (funded.balance, native)
    } else {
        (native, funded.balance)
    };
    // The pair keeps the last block time as a uint32, so modulo 2^32.
    let reserves = reserve0 | (reserve1 << 112) | (U256::from(block_time % (1 << 32)) << 224);
    let storage = vec![
        (token, funded.slot, B256::from(funded.stored)),
        (pair.wrapped_native, native_slot, B256::from(native)),
        (pool, V2_RESERVES_SLOT, B256::from(reserves)),
    ];
    let mut state = StateOverride::default();
    for (address, slot, value) in &storage {
        state
            .entry(*address)
            .or_default()
            .state_diff
            .get_or_insert_with(Default::default)
            .insert(*slot, *value);
    }
    let read_reserves = call(Address::ZERO, pool, getReservesCall {}.abi_encode());
    let read_native =
        call(Address::ZERO, pair.wrapped_native, balanceOfCall { _owner: pool }.abi_encode());
    let (reported, held) = tokio::join!(
        rpc.eth_call_with_state_overrides(read_reserves, block, state.clone()),
        rpc.eth_call_with_state_overrides(read_native, block, state),
    );
    let reported =
        reported.map_err(|e| format!("getReserves of the funded pair {pool:#x}: {e}"))?;
    let held =
        held.map_err(|e| format!("wrapped native balance of the funded pair {pool:#x}: {e}"))?;
    let Ok(reported) = getReservesCall::abi_decode_returns(reported.as_ref()) else {
        return Ok(None);
    };
    let matches = U256::from(reported.reserve0) == reserve0 &&
        U256::from(reported.reserve1) == reserve1 &&
        first_word(held.as_ref()) == native;
    if !matches {
        debug!(?pool, "the funded pair does not report the written reserves");
        return Ok(None);
    }
    Ok(Some(storage))
}

/// The pair's reserve of the wrapped native token after `fund_pair`.
pub(super) fn funded_native_reserve() -> U256 {
    U256::from(FUNDED_NATIVE_RESERVE)
}

/// Slots of the `address => uint256` mappings `balanceOf` reads.
fn balance_mappings(code: &CodeSummary) -> Vec<B256> {
    let mut mappings = Vec::new();
    for record in &code.storage {
        let read_by_balance_of = record
            .reads
            .iter()
            .any(|sel| sel.as_ref() == BALANCE_OF);
        if read_by_balance_of && record.r#type.replace(' ', "") == "mapping(address=>uint256)" {
            mappings.push(record.slot);
        }
    }
    mappings
}

/// The balance slot from tracing `balanceOf`, for code evmole could not read.
async fn detect_balance_slot(
    rpc: &EthereumRpcClient,
    token: Address,
    pool: Address,
) -> Option<B256> {
    let detector = EVMBalanceSlotDetector::new(rpc);
    let mut results = detector
        .detect_balance_slots(&[token.to_bytes()], pool.to_bytes())
        .await;
    match results.remove(&token.to_bytes()) {
        Some(Ok((storage, slot))) if storage == token.to_bytes() && slot.len() == 32 => {
            Some(B256::from_slice(slot.as_ref()))
        }
        Some(Ok((storage, _))) => {
            debug!(?token, ?storage, "balances live outside the token; not funding the pool");
            None
        }
        Some(Err(error)) => {
            debug!(?token, %error, "no balance slot found");
            None
        }
        None => None,
    }
}

async fn read_balance_with(
    rpc: &EthereumRpcClient,
    token: Address,
    pool: Address,
    block: BlockNumberOrTag,
    slot: B256,
    stored: U256,
) -> Result<Option<U256>, String> {
    let mut state = StateOverride::default();
    state
        .entry(token)
        .or_default()
        .state_diff = Some(
        [(slot, B256::from(stored))]
            .into_iter()
            .collect(),
    );
    let read = call(Address::ZERO, token, balanceOfCall { _owner: pool }.abi_encode());
    match rpc
        .eth_call_with_state_overrides(read, block, state)
        .await
    {
        Ok(out) => Ok(Some(first_word(out.as_ref()))),
        Err(error) if error.is_execution_failure() => Ok(None),
        Err(error) => Err(format!("balanceOf of {token:#x} with the funded balance: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{keccak256, Bytes};

    use super::*;
    use crate::services::token_analyzer::simulation::code::StorageField;

    fn mapping(slot: u8, reads: &[&str]) -> StorageField {
        StorageField {
            slot: B256::with_last_byte(slot),
            offset: 0,
            r#type: "mapping(address => uint256)".to_string(),
            reads: reads
                .iter()
                .map(|s| Bytes::copy_from_slice(&keccak256(s)[..4]))
                .collect(),
            writes: vec![],
        }
    }

    #[test]
    fn finds_the_mapping_balance_of_reads() {
        let code = CodeSummary {
            functions: vec![],
            storage: vec![
                mapping(0, &["balanceOf(address)", "transfer(address,uint256)"]),
                mapping(1, &["allowance(address,address)"]),
            ],
        };
        assert_eq!(balance_mappings(&code), [B256::with_last_byte(0)]);
    }

    #[test]
    fn a_full_reserve_fits_the_pair_slot() {
        let reserves = max_reserve() | (max_reserve() << 112) | (U256::from(u32::MAX) << 224);
        assert_eq!(reserves, U256::MAX);
    }

    #[test]
    fn reflection_tokens_read_several_mappings() {
        let code = CodeSummary {
            functions: vec![],
            storage: vec![mapping(2, &["balanceOf(address)"]), mapping(3, &["balanceOf(address)"])],
        };
        assert_eq!(balance_mappings(&code).len(), 2);
    }

    #[test]
    fn scaling_a_reflection_value_does_not_overflow() {
        let rate = U256::from(10).pow(U256::from(60));
        let target = U256::from(10).pow(U256::from(16));
        let stored = target << 192;
        let balance = stored / rate;
        let scaled = scale(stored, target, balance).expect("fits");
        assert!(close_to(scaled / rate, target));
    }

    #[test]
    fn a_balance_within_a_tenth_of_a_percent_is_close_enough() {
        let target = U256::from(1_000_000);
        assert!(close_to(U256::from(999_000), target));
        assert!(close_to(U256::from(1_001_000), target));
        assert!(!close_to(U256::from(998_999), target));
    }
}
