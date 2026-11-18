//! Live check that a limit probe sees a simulation that underquotes.
//!
//! On the deployed router, the fee calculator takes any output above the swap's
//! `expectedAmountOut` as positive slippage. A probe encoded with its quote as `expectedAmountOut`
//! therefore never pays the receiver more than the quote, and an underquoting simulation would pass
//! an exact check. The probes encode `expectedAmountOut` at its maximum instead
//! (`encode_swap_without_surplus_capture`).
//!
//! This test takes a Uniswap V2 USDC -> WETH swap at the chain's latest block, or at
//! `PROBE_SURPLUS_BLOCK`, quotes it from the
//! pair's reserves at that block, and underquotes it by 1% on purpose. It simulates the swap
//! through the deployed router and fee calculator, with no router overrides (only the sell token's
//! balance and allowance are overridden, as for every simulated swap), encoded both ways:
//! - with the underquote as `expectedAmountOut`, the receiver gets at most the underquote: the
//!   surplus is captured, so the defect is invisible;
//! - with `expectedAmountOut` at its maximum, the swap still executes, and the receiver gets all
//!   the pair pays, the exact quote, more than the underquote, so a check against the quote sees
//!   it. The router takes no fee on this swap; a fee would show here as a payout short of the
//!   quote.
//!
//! It also catches a deployed calculator that stops behaving this way.
//!
//! Ignored by default: it calls the RPC in `RPC_URL`, which it requires. It only simulates and
//! never sends a transaction. Run it with:
//!
//! ```text
//! cargo test -p tycho-integration-test --test probe_surplus -- --ignored --nocapture
//! ```

use std::{collections::HashMap, str::FromStr, sync::Arc};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    sol,
    sol_types::SolCall,
};
use num_bigint::BigUint;
use tycho_common::{
    models::{token::Token, Chain},
    simulation::protocol_sim::ProtocolSim,
    Bytes,
};
use tycho_execution::encoding::models::Solution;
use tycho_simulation::{
    evm::protocol::uniswap_v2::state::UniswapV2State, protocol::models::ProtocolComponent,
};
use tycho_test::{
    execution::{
        encoding::{encode_swap, encode_swap_without_surplus_capture},
        models::{RouterOverwritesData, Transaction, TychoExecutionInput, TychoExecutionResult},
        simulate_swap_transaction,
    },
    rpc_tools::RPCTools,
};

const PAIR: &str = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";
const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";

sol! {
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
}

fn token(address: &str, symbol: &str, decimals: u32) -> Token {
    Token::new(&Bytes::from_str(address).unwrap(), symbol, decimals, 0, &[], Chain::Ethereum, 100)
}

/// The amount the receiver got, or why the simulation did not execute.
fn paid(result: Option<&TychoExecutionResult>) -> BigUint {
    match result {
        Some(TychoExecutionResult::Success { amount_out, .. }) => amount_out.clone(),
        Some(TychoExecutionResult::Revert { reason, .. }) => panic!("the swap reverted: {reason}"),
        Some(TychoExecutionResult::Failed { error_msg }) => {
            panic!("the swap could not be simulated: {error_msg}")
        }
        None => panic!("no result"),
    }
}

#[tokio::test]
#[ignore = "calls the RPC in RPC_URL"]
async fn a_probe_without_surplus_capture_sees_an_underquote() {
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse().unwrap());
    // `PROBE_SURPLUS_BLOCK` picks another block, for when the router is paused at the latest.
    let tag = std::env::var("PROBE_SURPLUS_BLOCK")
        .ok()
        .map_or(BlockNumberOrTag::Latest, |n| {
            BlockNumberOrTag::Number(
                n.parse()
                    .expect("PROBE_SURPLUS_BLOCK is a block number"),
            )
        });
    let block = provider
        .get_block_by_number(tag)
        .await
        .unwrap()
        .expect("the block the test runs at");
    let number = block.header.number;

    // The pair's reserves at that block: USDC is token0, WETH token1.
    let returned = provider
        .call(
            TransactionRequest::default()
                .to(Address::from_str(PAIR).unwrap())
                .input(getReservesCall {}.abi_encode().into()),
        )
        .block(number.into())
        .await
        .unwrap();
    let reserves = getReservesCall::abi_decode_returns(&returned).unwrap();
    let state = UniswapV2State::new(U256::from(reserves.reserve0), U256::from(reserves.reserve1));

    let usdc = token(USDC, "USDC", 6);
    let weth = token(WETH, "WETH", 18);
    let component = ProtocolComponent::new(
        Bytes::from_str(PAIR).unwrap(),
        "uniswap_v2".to_string(),
        "uniswap_v2_pool".to_string(),
        Chain::Ethereum,
        vec![usdc.clone(), weth.clone()],
        Vec::new(),
        HashMap::new(),
        Bytes::default(),
        Default::default(),
    );

    let amount_in = BigUint::from(1_000_000_000u64); // 1,000 USDC
    let quote = state
        .get_amount_out(amount_in.clone(), &usdc, &weth)
        .unwrap();
    // A simulation that underquotes by 1%.
    let underquote = &quote.amount * 99u32 / 100u32;
    println!("block {number}: quote {}, underquote {underquote}", quote.amount);

    let state: Arc<dyn ProtocolSim> = Arc::new(state);
    let input = |(solution, transaction): (Solution, Transaction)| TychoExecutionInput {
        solution,
        estimated_gas: transaction.estimated_gas().clone(),
        transaction,
        expected_amount_out: underquote.clone(),
        protocol_system: "uniswap_v2".to_string(),
        component_id: PAIR.to_lowercase(),
        token_in: USDC.to_string(),
        token_out: WETH.to_string(),
        account_overwrites: None,
    };
    let captured = input(
        encode_swap(
            &component,
            Some(state.clone()),
            &usdc,
            &weth,
            amount_in.clone(),
            Chain::Ethereum,
            None,
            quote.gas.clone(),
            underquote.clone(),
        )
        .unwrap(),
    );
    let uncaptured = input(
        encode_swap_without_surplus_capture(
            &component,
            Some(state.clone()),
            &usdc,
            &weth,
            amount_in.clone(),
            Chain::Ethereum,
            None,
            quote.gas.clone(),
            underquote.clone(),
        )
        .unwrap(),
    );

    let rpc_tools = RPCTools::new(&rpc_url, &Chain::Ethereum)
        .await
        .unwrap();
    let results = simulate_swap_transaction(
        &rpc_tools,
        HashMap::from([("captured".to_string(), captured), ("uncaptured".to_string(), uncaptured)]),
        &block,
        RouterOverwritesData::default(),
        None,
    )
    .await
    .map_err(|(e, _, _)| e)
    .unwrap();

    let with_capture = paid(results.get("captured"));
    let without_capture = paid(results.get("uncaptured"));
    println!("paid with capture {with_capture}, without capture {without_capture}");
    assert!(
        with_capture <= underquote,
        "with the underquote as expectedAmountOut, the surplus is captured: paid {with_capture}"
    );
    // The router takes no fee on this swap, so the receiver gets all the pair pays: exactly its
    // quote, which a partial capture would fall short of.
    assert_eq!(
        without_capture, quote.amount,
        "without capture, the receiver gets what the venue pays"
    );
}
