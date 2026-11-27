//! Calls that take the token out of the pool without the pool's consent: forced transfers and
//! burns by a privileged caller. They leave the pool's reserves above its balance, which breaks
//! quotes until someone syncs the pool.

use std::collections::BTreeSet;

use alloy::{
    primitives::{Address, Bytes, U256},
    rpc::types::{
        simulate::{SimBlock, SimulatePayload},
        state::StateOverride,
        BlockNumberOrTag, TransactionRequest,
    },
    sol_types::SolCall,
};
use futures::stream::{self, StreamExt, TryStreamExt};

use super::{
    code::CodeSummary,
    owner_calls::{
        privileged_calls, Callers, PrivilegedCall, HOLDER_FUNCTIONS, MAX_PRIVILEGED_CALLS,
        PRIVILEGED_CALL_CONCURRENCY, TRANSFER_SELECTORS,
    },
    scenario::{call, first_word, SimBlockCalls, TokenContext},
};
use crate::erc20::balanceOfCall;

/// A balance drop of at most `1 / ROUNDING_DIVISOR` of the pool's balance is rounding, as in
/// reflection tokens moving an address between reward modes.
const ROUNDING_DIVISOR: u64 = 1000;
/// The argument of a `(uint256)` function that takes a share of the pool in basis points:
/// 1000 is the 10% cap of `manualBurnLiquidityPairTokens(percent)`.
const MAX_POOL_SHARE_ARGUMENT: U256 = U256::from_limbs([1000, 0, 0, 0]);

/// One call that may take tokens out of the pool.
pub(super) type SeizureCall = PrivilegedCall<()>;

/// Calls to every function that may change state and takes the pool as its first argument:
/// `f(pool)`, `f(pool, balance)` and `f(pool, wallet, balance)`, each from the caller `callers`
/// finds for it. A function that takes only an amount is called as `f(balance)` and
/// `f(MAX_POOL_SHARE_ARGUMENT)`: such functions read the pool from storage. Functions that write
/// the balances come first when the list is cut to `MAX_PRIVILEGED_CALLS`; evmole misses writes,
/// so the others are tried as well.
pub(super) fn candidate_seizures(
    code: &CodeSummary,
    context: &TokenContext,
    callers: &Callers,
) -> Vec<SeizureCall> {
    let balance_writers = balance_writers(code);
    let pool = context.pool.into_word().0;
    let balance = context.pool_balance.to_be_bytes::<32>();
    let mut calls = privileged_calls(code, context.token, callers, |function| {
        let selector = function.selector.as_ref();
        let holder_function = HOLDER_FUNCTIONS
            .iter()
            .any(|h| selector == h);
        if holder_function {
            return Vec::new();
        }
        let argument_sets = match function.arguments.as_deref() {
            Some("address") => vec![vec![pool]],
            Some("address,uint256") => vec![vec![pool, balance]],
            Some("address,address,uint256") => {
                vec![vec![pool, context.wallet.into_word().0, balance]]
            }
            Some("uint256") => {
                vec![vec![balance], vec![MAX_POOL_SHARE_ARGUMENT.to_be_bytes::<32>()]]
            }
            Some(_) | None => Vec::new(),
        };
        argument_sets
            .into_iter()
            .map(|words| ((), words))
            .collect()
    });
    calls.sort_by_key(|c| !balance_writers.contains(&c.selector));
    calls.truncate(MAX_PRIVILEGED_CALLS);
    calls
}

/// The selectors of `calls` that lowered the pool's balance. Fails when a simulation could not
/// run: a missed seizure would let the token be quoted.
pub(super) async fn find_seizures(
    context: &TokenContext,
    calls: &[SeizureCall],
) -> Result<Vec<Bytes>, String> {
    let runs: Vec<_> = calls
        .iter()
        .map(|seizure| selector_if_lowered(context, seizure))
        .collect();
    let lowered: Vec<Option<Bytes>> = stream::iter(runs)
        .buffered(PRIVILEGED_CALL_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(lowered.into_iter().flatten().collect())
}

async fn selector_if_lowered(
    context: &TokenContext,
    seizure: &SeizureCall,
) -> Result<Option<Bytes>, String> {
    let lowered = lowers_pool_balance(context, &seizure.tx)
        .await
        .map_err(|e| format!("seizure call {}: {e}", seizure.selector))?;
    Ok(lowered.then(|| seizure.selector.clone()))
}

/// Whether `tx` succeeds and leaves the pool with less of the token than before, beyond
/// rounding.
async fn lowers_pool_balance(
    context: &TokenContext,
    tx: &TransactionRequest,
) -> Result<bool, String> {
    let read =
        call(Address::ZERO, context.token, balanceOfCall { _owner: context.pool }.abi_encode());
    let mut state = StateOverride::default();
    context.add_overrides(&mut state);
    let payload = SimulatePayload::default().extend(SimBlock {
        block_overrides: Some(context.first_block(0)),
        state_overrides: Some(state),
        calls: vec![read.clone(), tx.clone(), read],
    });
    let blocks: Vec<SimBlockCalls> = context
        .rpc
        .eth_simulate_v1(&payload, BlockNumberOrTag::Number(context.block))
        .await
        .map_err(|e| e.to_string())?;
    let results: Vec<_> = blocks
        .into_iter()
        .flat_map(|b| b.calls)
        .collect();
    let [before, seizure, after] = results.as_slice() else {
        return Err(format!("eth_simulateV1 returned {} call results for 3 calls", results.len()));
    };
    if !seizure.succeeded() || !before.succeeded() || !after.succeeded() {
        return Ok(false);
    }
    Ok(lowered_beyond_rounding(
        first_word(before.return_data.as_ref()),
        first_word(after.return_data.as_ref()),
    ))
}

fn lowered_beyond_rounding(before: U256, after: U256) -> bool {
    after < before && before - after > before / U256::from(ROUNDING_DIVISOR)
}

/// Functions that write the `address => uint256` mapping `transfer` writes: the balances.
fn balance_writers(code: &CodeSummary) -> BTreeSet<Bytes> {
    let mut writers = BTreeSet::new();
    for record in &code.storage {
        let is_balances = record.r#type.replace(' ', "") == "mapping(address=>uint256)";
        let written_by_transfer = record.writes.iter().any(|w| {
            TRANSFER_SELECTORS
                .iter()
                .any(|t| w.as_ref() == t)
        });
        if is_balances && written_by_transfer {
            writers.extend(record.writes.iter().cloned());
        }
    }
    writers
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::primitives::{keccak256, B256};

    use super::*;
    use crate::services::token_analyzer::simulation::{
        code::{FunctionSummary, StorageField},
        owner_calls::find_callers,
    };

    const OWNER: Address = Address::repeat_byte(1);

    fn selector(signature: &str) -> Bytes {
        Bytes::copy_from_slice(&keccak256(signature)[..4])
    }

    fn function(
        signature: &str,
        arguments: Option<&str>,
        mutability: Option<&str>,
    ) -> FunctionSummary {
        FunctionSummary {
            selector: selector(signature),
            arguments: arguments.map(str::to_string),
            state_mutability: mutability.map(str::to_string),
        }
    }

    fn context() -> TokenContext {
        let rpc = crate::rpc::EthereumRpcClient::new("http://localhost:1").expect("client");
        TokenContext::new(
            &rpc,
            Address::repeat_byte(7),
            Address::repeat_byte(8),
            U256::from(1_000_000),
            (1, 1, 12),
        )
    }

    /// A token whose balances `transfer` and `forceBurn(address,uint256)` write, with other
    /// functions taking an address.
    fn code() -> CodeSummary {
        CodeSummary {
            functions: vec![
                function("transfer(address,uint256)", Some("address,uint256"), Some("nonpayable")),
                function("approve(address,uint256)", Some("address,uint256"), Some("nonpayable")),
                function("transferOwnership(address)", Some("address"), Some("nonpayable")),
                function("balanceOf(address)", Some("address"), Some("view")),
                function("setBot(address)", Some("address"), Some("nonpayable")),
                function("forceBurn(address,uint256)", Some("address,uint256"), Some("nonpayable")),
                function("sweep(address,address,uint256)", Some("address,address,uint256"), None),
                function("setFee(uint256)", Some("uint256"), Some("nonpayable")),
                function("mystery()", None, Some("nonpayable")),
            ],
            storage: vec![StorageField {
                slot: B256::ZERO,
                offset: 0,
                r#type: "mapping(address => uint256)".to_string(),
                reads: vec![selector("balanceOf(address)")],
                writes: vec![
                    selector("transfer(address,uint256)"),
                    selector("forceBurn(address,uint256)"),
                ],
            }],
        }
    }

    #[test]
    fn functions_taking_the_pool_or_an_amount_are_tried_balance_writers_first() {
        let code = code();
        let context = context();
        let callers = find_callers(&code, Some(OWNER), &HashMap::new(), &[]);
        let calls = candidate_seizures(&code, &context, &callers);
        let selectors: Vec<Bytes> = calls
            .iter()
            .map(|c| c.selector.clone())
            .collect();
        assert_eq!(
            selectors,
            [
                selector("forceBurn(address,uint256)"),
                selector("setBot(address)"),
                selector("sweep(address,address,uint256)"),
                selector("setFee(uint256)"),
                selector("setFee(uint256)"),
            ]
        );
        let amounts: Vec<U256> = calls[3..]
            .iter()
            .map(|c| U256::from_be_slice(&c.tx.input.input().expect("calldata")[4..36]))
            .collect();
        assert_eq!(amounts, [context.pool_balance, MAX_POOL_SHARE_ARGUMENT]);
        let burn = calls[0]
            .tx
            .input
            .input()
            .expect("calldata");
        assert_eq!(&burn[4..36], context.pool.into_word().as_slice());
        assert_eq!(U256::from_be_slice(&burn[36..68]), context.pool_balance);
        assert_eq!(calls[0].tx.from, Some(OWNER));
    }

    #[test]
    fn functions_without_a_caller_are_not_tried() {
        let code = code();
        let callers = find_callers(&code, None, &HashMap::new(), &[]);
        assert!(candidate_seizures(&code, &context(), &callers).is_empty());
    }

    #[test]
    fn only_a_drop_beyond_rounding_is_a_seizure() {
        let before = U256::from(1_000_000);
        assert!(lowered_beyond_rounding(before, U256::ZERO));
        assert!(lowered_beyond_rounding(before, U256::from(998_999)));
        assert!(!lowered_beyond_rounding(before, U256::from(999_000)));
        assert!(!lowered_beyond_rounding(before, U256::from(2_000_000)));
    }
}
