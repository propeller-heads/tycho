use std::{env, str::FromStr};

use alloy::{
    core::sol,
    eips::eip1898::BlockId,
    primitives::{Address, Bytes as AlloyBytes, TxKind, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::{TransactionInput, TransactionRequest},
    sol_types::SolCall,
    transports::http::reqwest::Url,
};
use anyhow::{Context, Result};
use num_bigint::BigUint;

use super::{
    math::{quote_buy_exact_in, quote_sell_exact_in},
    state::{BaselineCurve, BaselineQuoteState},
};
use crate::evm::protocol::u256_num::u256_to_biguint;

const RELAY: &str = "0xc81fd894c0ace037d133af4886550ac8133568e8";
const BASE_REPPO: &str = "0xff8104251e7761163fac3211ef5583fb3f8583d6";
const BASE_REPPO_SWAP_BLOCK: u64 = 46_598_229;

sol! {
    interface IBaselineRelay {
        struct CurveParams {
            uint256 BLV;
            uint256 circ;
            uint256 supply;
            uint256 swapFee;
            uint256 reserves;
            uint256 totalSupply;
            uint256 convexityExp;
            uint256 lastInvariant;
        }

        struct QuoteState {
            CurveParams snapshotCurveParams;
            uint256 quoteBlockBuyDeltaCirc;
            uint256 quoteBlockSellDeltaCirc;
            uint256 totalSupply;
            uint256 totalBTokens;
            uint256 totalReserves;
            uint8 reserveDecimals;
            uint256 liquidityFeePct;
            uint256 pendingSurplus;
            bool shouldSettlePendingSurplus;
            uint256 maxSellDelta;
            uint256 snapshotActivePrice;
        }

        function getQuoteState(address bToken) external view returns (QuoteState memory state);

        function quoteBuyExactIn(address bToken, uint256 reservesIn)
            external
            view
            returns (uint256 tokensOut, uint256 feesReceived, uint256 slippage);

        function quoteSellExactIn(address bToken, uint256 amountIn)
            external
            view
            returns (uint256 amountOut, uint256 feesReceived, uint256 slippage);
    }
}

#[tokio::test]
#[ignore = "requires BASE_RPC_URL and performs Base mainnet RPC calls"]
async fn base_reppo_native_quotes_match_live_relay_at_swap_block() -> Result<()> {
    let rpc_url = env::var("BASE_RPC_URL").context("BASE_RPC_URL must be set")?;
    let provider = ProviderBuilder::new().connect_http(Url::parse(&rpc_url)?);
    let relay_address = Address::from_str(RELAY)?;
    let b_token = Address::from_str(BASE_REPPO)?;
    let block_id = BlockId::from(BASE_REPPO_SWAP_BLOCK);

    let quote_state = call_relay(
        &provider,
        relay_address,
        IBaselineRelay::getQuoteStateCall { bToken: b_token },
        block_id,
    )
    .await
    .context("getQuoteState failed")?
    .into();

    let buy_amount_in = BigUint::from(1_000_000_000_000_000u64);
    let simulated_buy = quote_buy_exact_in(&quote_state, &buy_amount_in)?.amount_out;
    let live_buy = u256_to_biguint(
        call_relay(
            &provider,
            relay_address,
            IBaselineRelay::quoteBuyExactInCall {
                bToken: b_token,
                reservesIn: U256::from(1_000_000_000_000_000u64),
            },
            block_id,
        )
        .await
        .context("quoteBuyExactIn failed")?
        .tokensOut,
    );
    assert_eq!(simulated_buy, live_buy, "reserve -> bToken quote mismatch");

    let sell_amount_in = BigUint::from(1_000_000_000_000_000_000u64);
    let simulated_sell = quote_sell_exact_in(&quote_state, &sell_amount_in)?.amount_out;
    let live_sell = u256_to_biguint(
        call_relay(
            &provider,
            relay_address,
            IBaselineRelay::quoteSellExactInCall {
                bToken: b_token,
                amountIn: U256::from(1_000_000_000_000_000_000u64),
            },
            block_id,
        )
        .await
        .context("quoteSellExactIn failed")?
        .amountOut,
    );
    assert_eq!(simulated_sell, live_sell, "bToken -> reserve quote mismatch");

    Ok(())
}

async fn call_relay<C, P>(
    provider: &P,
    relay: Address,
    call: C,
    block_id: BlockId,
) -> Result<C::Return>
where
    C: SolCall,
    P: Provider,
{
    let tx = TransactionRequest {
        to: Some(TxKind::from(relay)),
        input: TransactionInput { input: Some(AlloyBytes::from(call.abi_encode())), data: None },
        ..Default::default()
    };
    let output = provider
        .call(tx)
        .block(block_id)
        .await
        .context("eth_call failed")?;
    C::abi_decode_returns(&output).context("eth_call response decode failed")
}

impl From<IBaselineRelay::QuoteState> for BaselineQuoteState {
    fn from(state: IBaselineRelay::QuoteState) -> Self {
        BaselineQuoteState {
            snapshot_curve: BaselineCurve {
                blv: state.snapshotCurveParams.BLV,
                circ: state.snapshotCurveParams.circ,
                supply: state.snapshotCurveParams.supply,
                swap_fee: state.snapshotCurveParams.swapFee,
                reserves: state.snapshotCurveParams.reserves,
                total_supply: state.snapshotCurveParams.totalSupply,
                convexity_exp: state.snapshotCurveParams.convexityExp,
                last_invariant: state.snapshotCurveParams.lastInvariant,
            },
            quote_block_buy_delta_circ: state.quoteBlockBuyDeltaCirc,
            quote_block_sell_delta_circ: state.quoteBlockSellDeltaCirc,
            total_supply: state.totalSupply,
            total_b_tokens: state.totalBTokens,
            total_reserves: state.totalReserves,
            reserve_decimals: U256::from(state.reserveDecimals),
            liquidity_fee_pct: state.liquidityFeePct,
            pending_surplus: state.pendingSurplus,
            should_settle_pending_surplus: state.shouldSettlePendingSurplus,
            max_sell_delta: state.maxSellDelta,
            snapshot_active_price: state.snapshotActivePrice,
        }
    }
}
