//! Legs as real swaps against a Uniswap V2 style pair, sent by wallets through `SwapRunner`.
//!
//! The buyer wallet calls `SwapRunner.buy` with ETH: it wraps it, swaps it for the token and
//! forwards the output to the receiver wallet in the same transaction, as a router forwarding
//! pool output does. The receiver then approves `SwapRunner` and sells its whole balance. Every
//! sender is a funded wallet without code, so in the `Realistic` environment the node enforces
//! the base fee and the gas price paid.

use alloy::{
    primitives::{keccak256, Address, Bytes, U256},
    rpc::types::{
        simulate::{SimBlock, SimulatePayload},
        state::{AccountOverride, StateOverride},
        BlockNumberOrTag, BlockOverrides, Log, TransactionInput, TransactionRequest,
    },
    sol_types::SolCall,
};

use super::{
    runner::{
        buyCall, getReservesCall, sellCall, token0Call, token1Call, LegResult, SellResult,
        SWAP_RUNNER_BYTECODE,
    },
    scenario::{
        derived_address, return_data_unless_reverted, revert_reason, swap_topics, transferred,
        BlockGap, BuySize, Environment, LegTiming, Outcome, Scenario, SimCall, TokenContext,
        V2Pair,
    },
};
use crate::{erc20::approveCall, rpc::EthereumRpcClient};

const SWAP_GAS: u64 = 1_500_000;
/// Cap of the small buy, so that it stays below typical max-transaction limits.
const MAX_SMALL_BUY_WEI: u64 = 50_000_000_000_000_000;
const PRIORITY_FEE_WEI: u128 = 1_000_000_000;
/// Gas price for blocks before London, which have no base fee.
const LEGACY_GAS_PRICE_WEI: u128 = 30_000_000_000;

/// `pool` as a pair of `token` against the chain's wrapped native token, when it is a Uniswap V2
/// style pair holding some of it; `None` when it is not. Fails when the node could not answer,
/// so that a transient error does not pass for a pool that is not a pair.
pub(super) async fn detect_pair(
    rpc: &EthereumRpcClient,
    pool: Address,
    token: Address,
    wrapped_native: Address,
    block: BlockNumberOrTag,
) -> Result<Option<V2Pair>, String> {
    let read = |calldata: Vec<u8>| {
        let rpc = rpc.clone();
        async move {
            rpc.eth_call(super::scenario::call(Address::ZERO, pool, calldata), block)
                .await
        }
    };
    let (token0, token1, reserves) = tokio::join!(
        read(token0Call {}.abi_encode()),
        read(token1Call {}.abi_encode()),
        read(getReservesCall {}.abi_encode()),
    );
    let (Some(token0), Some(token1), Some(reserves)) = (
        return_data_unless_reverted(token0).map_err(|e| format!("token0() of {pool:#x}: {e}"))?,
        return_data_unless_reverted(token1).map_err(|e| format!("token1() of {pool:#x}: {e}"))?,
        return_data_unless_reverted(reserves)
            .map_err(|e| format!("getReserves() of {pool:#x}: {e}"))?,
    ) else {
        return Ok(None);
    };
    let (Ok(token0), Ok(token1), Ok(reserves)) = (
        token0Call::abi_decode_returns(token0.as_ref()),
        token1Call::abi_decode_returns(token1.as_ref()),
        getReservesCall::abi_decode_returns(reserves.as_ref()),
    ) else {
        return Ok(None);
    };
    let reserve = if token0 == token && token1 == wrapped_native {
        U256::from(reserves.reserve1)
    } else if token0 == wrapped_native && token1 == token {
        U256::from(reserves.reserve0)
    } else {
        return Ok(None);
    };
    Ok((!reserve.is_zero()).then_some(V2Pair { wrapped_native, reserve }))
}

pub(super) fn buyer() -> Address {
    derived_address(b"tycho-token-analysis-buyer")
}

pub(super) fn router() -> Address {
    derived_address(b"tycho-token-analysis-router")
}

/// ETH spent on the buy. The large buy takes 2% of the pair's token reserve, as the direct
/// legs do: with `SwapRunner`'s 5% fee, `x / (reserve + x) * 0.95 = 1 / 50` gives
/// `x = reserve * 1000 / (49 * 950)`.
fn eth_spent_on_buy(pair: &V2Pair, size: BuySize) -> U256 {
    match size {
        BuySize::Small => (pair.reserve / U256::from(200)).min(U256::from(MAX_SMALL_BUY_WEI)),
        BuySize::Large => pair.reserve * U256::from(1000) / U256::from(49 * 950),
    }
}

/// A transaction from a wallet paying what the chain asks in `Realistic`, nothing in `Bare`.
fn wallet_transaction(
    context: &TokenContext,
    environment: Environment,
    from: Address,
    to: Address,
    data: Vec<u8>,
) -> TransactionRequest {
    let tx = TransactionRequest::default()
        .from(from)
        .to(to)
        .input(TransactionInput::new(Bytes::from(data)))
        .gas_limit(SWAP_GAS);
    match (environment, context.base_fee) {
        (Environment::Realistic, Some(base_fee)) => tx
            .max_fee_per_gas(u128::from(base_fee) * 2 + PRIORITY_FEE_WEI)
            .max_priority_fee_per_gas(PRIORITY_FEE_WEI),
        (Environment::Realistic, None) => tx.gas_price(LEGACY_GAS_PRICE_WEI),
        (Environment::Bare, _) => tx.gas_price(0),
    }
}

/// The router runs `SwapRunner`, and each sender of `legs` holds what its transactions can
/// spend: the value sent plus the gas limit at the highest price offered. That is what a
/// validating node asks a wallet to hold, and no more, so the balance does not look unusual.
pub(super) fn wallet_and_router_state(legs: &[TransactionRequest]) -> StateOverride {
    let mut state = StateOverride::default();
    state.insert(
        router(),
        AccountOverride {
            code: Some(Bytes::from_static(SWAP_RUNNER_BYTECODE)),
            ..Default::default()
        },
    );
    for tx in legs {
        let Some(sender) = tx.from else { continue };
        let price = tx
            .max_fee_per_gas
            .or(tx.gas_price)
            .unwrap_or_default();
        let gas = U256::from(tx.gas.unwrap_or_default()) * U256::from(price);
        let cost = gas + tx.value.unwrap_or_default();
        let account = state.entry(sender).or_default();
        account.balance = Some(account.balance.unwrap_or_default() + cost);
    }
    state
}

/// The first simulated block, with a non-zero `prevrandao` in the `Realistic` environment.
pub(super) fn first_block(
    context: &TokenContext,
    environment: Environment,
    time_shift_days: u64,
) -> BlockOverrides {
    let mut block = context.first_block(time_shift_days);
    if environment == Environment::Realistic {
        block.random = Some(keccak256(context.block.to_be_bytes()));
    }
    block
}

/// The buy (with the forward to the receiver), the receiver's approval of the router, and the
/// receiver's sell, in that order.
pub(super) fn transactions(
    context: &TokenContext,
    pair: &V2Pair,
    environment: Environment,
    size: BuySize,
) -> [TransactionRequest; 3] {
    let buy = wallet_transaction(
        context,
        environment,
        buyer(),
        router(),
        buyCall {
            token: context.token,
            pair: context.pool,
            weth: pair.wrapped_native,
            receiver: context.wallet,
        }
        .abi_encode(),
    )
    .value(eth_spent_on_buy(pair, size));
    let approve = wallet_transaction(
        context,
        environment,
        context.wallet,
        context.token,
        approveCall { _spender: router(), _value: U256::MAX }.abi_encode(),
    );
    let sell = wallet_transaction(
        context,
        environment,
        context.wallet,
        router(),
        sellCall { token: context.token, pair: context.pool, weth: pair.wrapped_native }
            .abi_encode(),
    );
    [buy, approve, sell]
}

/// Simulates the swap legs of `scenario` against `pair`.
pub(super) async fn run(
    context: &TokenContext,
    pair: &V2Pair,
    scenario: &Scenario,
) -> Result<Outcome, String> {
    let legs = transactions(context, pair, scenario.environment, scenario.size);
    let state = wallet_and_router_state(&legs);
    let [buy, approve, sell] = legs;
    let mut first: Vec<TransactionRequest> = scenario.prefix.clone();
    let mut second = Vec::new();
    let mut gap = BlockGap::NEXT;
    match scenario.timing {
        LegTiming::SameBlock => first.extend([buy, approve, sell]),
        LegTiming::LegsNextBlock => {
            first.push(buy);
            second.extend([approve, sell]);
        }
        LegTiming::AfterPrefix(after) => {
            gap = after;
            second.extend([buy, approve, sell]);
        }
    }

    let block = first_block(context, scenario.environment, scenario.time_shift_days);
    let mut payload = SimulatePayload::default().extend(SimBlock {
        block_overrides: Some(block.clone()),
        state_overrides: Some(state),
        calls: first.clone(),
    });
    if !second.is_empty() {
        payload = payload.extend(SimBlock {
            block_overrides: Some(BlockOverrides {
                number: block
                    .number
                    .map(|n| n + U256::from(gap.blocks)),
                time: block.time.map(|t| t + gap.secs),
                random: block.random.map(keccak256),
                ..Default::default()
            }),
            state_overrides: None,
            calls: second.clone(),
        });
    }
    payload.validation = scenario.environment == Environment::Realistic;

    let blocks: Vec<super::scenario::SimBlockCalls> = context
        .rpc
        .eth_simulate_v1(&payload, BlockNumberOrTag::Number(context.block))
        .await
        .map_err(|e| e.to_string())?;
    let results: Vec<SimCall> = blocks
        .into_iter()
        .flat_map(|b| b.calls)
        .collect();
    let expected = first.len() + second.len();
    if results.len() != expected {
        return Err(format!(
            "eth_simulateV1 returned {} call results for {expected} calls",
            results.len()
        ));
    }
    let prefix_len = scenario.prefix.len();
    Ok(outcome(context, &results[..prefix_len], &results[prefix_len..]))
}

/// The outcome of the buy, approve and sell results, in that order, after the prefix's.
fn outcome(context: &TokenContext, prefix: &[SimCall], legs: &[SimCall]) -> Outcome {
    let (buy, approve, sell) = (&legs[0], &legs[1], &legs[2]);
    let prefix_ok = prefix
        .iter()
        .map(SimCall::succeeded)
        .collect();
    let mut outcome = Outcome {
        prefix_ok,
        sent: U256::ZERO,
        buy_revert: None,
        legs: None,
        sell_arrived: U256::ZERO,
        sell_logged: false,
        pool_rejected_buy: false,
        leg_logs: Vec::new(),
    };
    if !buy.succeeded() {
        outcome.buy_revert = Some(buy.revert_reason());
        return outcome;
    }
    let Ok(bought) = buyCall::abi_decode_returns(&buy.return_data) else {
        outcome.buy_revert = Some("undecodable buy result".to_string());
        return outcome;
    };
    outcome.sent = bought.expected;
    if !bought.swapOk {
        outcome.buy_revert = Some(revert_reason(&bought.swapRevert, None));
        outcome.pool_rejected_buy = true;
        return outcome;
    }
    let sold =
        if sell.succeeded() { sellCall::abi_decode_returns(&sell.return_data).ok() } else { None };
    let sold = sold.unwrap_or_else(|| SellResult {
        transferOk: false,
        sent: U256::ZERO,
        arrived: U256::ZERO,
        swapOk: false,
        wethOut: U256::ZERO,
        gas: U256::ZERO,
        transferRevert: sell
            .error
            .as_ref()
            .and_then(|e| e.data.clone())
            .unwrap_or_else(|| sell.return_data.clone()),
        swapRevert: Bytes::new(),
    });
    let logged = transferred(context.token, context.wallet, context.pool, &sell.logs);
    outcome.sell_logged = logged.is_some();
    outcome.sell_arrived = logged.map_or(sold.arrived, |logged| sold.arrived.min(logged));
    outcome.leg_logs = buy
        .logs
        .iter()
        .chain(&sell.logs)
        .filter(|log| !is_own_swap(log, context))
        .cloned()
        .collect();
    outcome.legs = Some(LegResult {
        bought: bought.bought,
        transferOk: bought.forwardOk,
        transferSent: bought.forwardSent,
        transferReceived: bought.forwardReceived,
        transferGas: bought.forwardGas,
        approveOk: approve.succeeded(),
        sellOk: sold.transferOk && sold.swapOk,
        sellSent: sold.sent,
        sellArrived: sold.arrived,
        sellGas: sold.gas,
        walletDrift: Default::default(),
        transferRevert: bought.forwardRevert,
        sellRevert: if sold.transferOk { sold.swapRevert } else { sold.transferRevert },
    });
    outcome
}

/// A `Swap` event of our own buy or sell, as opposed to one the token triggered.
fn is_own_swap(log: &Log, context: &TokenContext) -> bool {
    let topics = log.topics();
    topics.len() == 3 &&
        swap_topics().contains(&topics[0]) &&
        [router(), context.wallet].contains(&Address::from_word(topics[2]))
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::{LogData, B256},
        sol_types::{Revert, SolError},
    };

    use super::*;
    use crate::services::token_analyzer::simulation::scenario::SimCallError;

    fn context() -> TokenContext {
        let rpc = EthereumRpcClient::new("http://localhost:1").expect("client");
        TokenContext::new(
            &rpc,
            Address::repeat_byte(7),
            Address::repeat_byte(8),
            U256::from(1000),
            (1, 1),
        )
    }

    fn ok(return_data: Vec<u8>, logs: Vec<Log>) -> SimCall {
        SimCall {
            return_data: return_data.into(),
            logs,
            status: alloy::primitives::U64::from(1),
            error: None,
        }
    }

    fn log(address: Address, topics: Vec<B256>, data: U256) -> Log {
        Log {
            inner: alloy::primitives::Log {
                address,
                data: LogData::new_unchecked(topics, data.to_be_bytes::<32>().to_vec().into()),
            },
            ..Default::default()
        }
    }

    fn bought() -> Vec<u8> {
        buyCall::abi_encode_returns(&buy_result())
    }

    fn buy_result() -> super::super::runner::BuyResult {
        super::super::runner::BuyResult {
            swapOk: true,
            expected: U256::from(1000),
            bought: U256::from(950),
            forwardOk: true,
            forwardSent: U256::from(950),
            forwardReceived: U256::from(950),
            forwardGas: U256::from(50_000),
            swapRevert: Bytes::new(),
            forwardRevert: Bytes::new(),
        }
    }

    fn sold(arrived: u64) -> Vec<u8> {
        sellCall::abi_encode_returns(&SellResult {
            transferOk: true,
            sent: U256::from(950),
            arrived: U256::from(arrived),
            swapOk: true,
            wethOut: U256::from(1),
            gas: U256::from(70_000),
            transferRevert: Bytes::new(),
            swapRevert: Bytes::new(),
        })
    }

    #[test]
    fn maps_a_round_trip_and_ignores_its_own_swaps() {
        let context = context();
        let transfer = keccak256("Transfer(address,address,uint256)");
        let sell_transfer = log(
            context.token,
            vec![transfer, context.wallet.into_word(), context.pool.into_word()],
            U256::from(900),
        );
        let own_swap = log(
            context.pool,
            vec![swap_topics()[0], router().into_word(), context.wallet.into_word()],
            U256::ZERO,
        );
        let token_swap = log(
            context.pool,
            vec![swap_topics()[0], router().into_word(), Address::repeat_byte(9).into_word()],
            U256::ZERO,
        );
        let results = [
            ok(bought(), vec![]),
            ok(vec![], vec![]),
            ok(sold(1500), vec![sell_transfer, own_swap, token_swap]),
        ];
        let outcome = outcome(&context, &[], &results);
        assert!(outcome.all_legs_ok());
        assert_eq!(outcome.fees().buy, Some(500));
        // A swap-back moved more into the pool than the wallet sent; the events show 900.
        assert_eq!(outcome.sell_arrived, U256::from(900));
        assert_eq!(outcome.leg_logs.len(), 2);
    }

    #[test]
    fn a_reverted_sell_transaction_is_a_sell_failure() {
        let context = context();
        let reason = Revert { reason: "no sells".to_string() }.abi_encode();
        let sell = SimCall {
            status: alloy::primitives::U64::ZERO,
            error: Some(SimCallError {
                message: "execution reverted".to_string(),
                data: Some(reason.into()),
            }),
            ..Default::default()
        };
        let outcome = outcome(&context, &[], &[ok(bought(), vec![]), ok(vec![], vec![]), sell]);
        assert!(outcome.only_sell_failed());
        assert_eq!(outcome.failure_reason().as_deref(), Some("sell reverted: no sells"));
    }

    #[test]
    fn a_sell_without_a_transfer_event_counts_what_reached_the_pool() {
        let context = context();
        let results = [ok(bought(), vec![]), ok(vec![], vec![]), ok(sold(900), vec![])];
        let outcome = outcome(&context, &[], &results);
        assert!(outcome.all_legs_ok());
        assert!(!outcome.sell_logged);
        assert_eq!(outcome.sell_arrived, U256::from(900));
    }

    #[test]
    fn a_swap_the_pool_rejects_is_marked_as_such() {
        let context = context();
        let mut rejected = buy_result();
        rejected.swapOk = false;
        rejected.swapRevert = Revert { reason: "UniswapV2: K".to_string() }
            .abi_encode()
            .into();
        let buy = buyCall::abi_encode_returns(&rejected);
        let outcome =
            outcome(&context, &[], &[ok(buy, vec![]), ok(vec![], vec![]), ok(vec![], vec![])]);
        assert!(outcome.pool_rejected_buy);
        assert_eq!(outcome.failure_reason().as_deref(), Some("buy reverted: UniswapV2: K"));
    }

    #[rstest::rstest]
    #[case::realistic(Environment::Realistic, Some(10))]
    #[case::bare(Environment::Bare, Some(10))]
    #[case::legacy(Environment::Realistic, None)]
    fn wallets_hold_what_their_transactions_can_spend(
        #[case] environment: Environment,
        #[case] base_fee: Option<u64>,
    ) {
        let mut context = context();
        context.base_fee = base_fee;
        let pair = V2Pair { wrapped_native: Address::repeat_byte(9), reserve: U256::from(10_000) };
        let legs = transactions(&context, &pair, environment, BuySize::Large);
        let state = wallet_and_router_state(&legs);

        let price = U256::from(match (environment, base_fee) {
            (Environment::Realistic, Some(base_fee)) => u128::from(base_fee) * 2 + PRIORITY_FEE_WEI,
            (Environment::Realistic, None) => LEGACY_GAS_PRICE_WEI,
            (Environment::Bare, _) => 0,
        });
        let gas = U256::from(SWAP_GAS) * price;
        let balance = |address: Address| state[&address].balance;
        assert_eq!(balance(buyer()), Some(eth_spent_on_buy(&pair, BuySize::Large) + gas));
        assert_eq!(balance(context.wallet), Some(gas + gas));
        assert!(state[&router()].code.is_some());
    }

    #[test]
    fn the_large_buy_takes_two_percent_of_the_token_reserve() {
        let (weth, tokens) = (U256::from(2_000_000_000u64), U256::from(9_000_000_000u64));
        let pair = V2Pair { wrapped_native: Address::repeat_byte(9), reserve: weth };
        let paid = eth_spent_on_buy(&pair, BuySize::Large) * U256::from(950);
        let bought = paid * tokens / (weth * U256::from(1000) + paid);
        assert_eq!(bought, tokens / U256::from(50) - U256::from(1));
    }

    #[test]
    fn realistic_transactions_pay_the_base_fee_and_bare_ones_nothing() {
        let mut context = context();
        context.base_fee = Some(10);
        let realistic =
            wallet_transaction(&context, Environment::Realistic, buyer(), router(), vec![]);
        assert_eq!(realistic.max_fee_per_gas, Some(20 + PRIORITY_FEE_WEI));
        assert_eq!(realistic.gas_price, None);
        let bare = wallet_transaction(&context, Environment::Bare, buyer(), router(), vec![]);
        assert_eq!(bare.gas_price, Some(0));
        context.base_fee = None;
        let legacy =
            wallet_transaction(&context, Environment::Realistic, buyer(), router(), vec![]);
        assert_eq!(legacy.gas_price, Some(LEGACY_GAS_PRICE_WEI));
    }
}
