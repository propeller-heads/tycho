//! Trade extraction from router call traces.
use anyhow::Result;
use substreams::{prelude::*, scalar::BigInt, store::StoreGetString};
use substreams_ethereum::{
    pb::eth::v2::{Block, Call, TransactionTrace},
    Event,
};

use super::{block_timestamp, keys};
use crate::{
    abi::tycho_router_v3_1::events::FeesTaken,
    decode::{self, revert::decode_revert, swaps::decode_hops, SwapCall},
    params::{Params, RouterConfig, RouterVersion},
    pb::tycho::router::v1::{
        ClientFee, FeeTaken, Hop, RouterCallError, RouterFeeConfig, Trade, Trades,
    },
};

const ZERO_ADDRESS: [u8; 20] = [0u8; 20];

/// Clamps a uint256 to u64; the router ABI uses u16/u32 here so anything larger is garbage.
fn saturating_u64(value: &BigInt) -> u64 {
    value
        .to_string()
        .parse::<u64>()
        .unwrap_or(u64::MAX)
}

/// Finds every call into a configured router's swap entry points, in every transaction
/// (including reverted ones), and decodes it into a [`Trade`].
#[substreams::handlers::map]
pub fn map_trades(params: String, block: Block, fee_store: StoreGetString) -> Result<Trades> {
    let params = Params::parse(&params)?;
    let timestamp = block_timestamp(&block);
    let mut trades = Vec::new();
    let mut errors = Vec::new();
    for tx in &block.transaction_traces {
        for call in &tx.calls {
            let Some(router) = params.router(&call.address) else {
                continue;
            };
            let Some(decoded) = decode::decode_swap_call(router.version, call) else {
                continue;
            };
            let swap = match decoded {
                Ok(swap) => swap,
                Err(err) => {
                    errors.push(call_error(
                        &params.chain,
                        &block,
                        timestamp,
                        tx,
                        call,
                        router,
                        "calldata",
                        err.to_string(),
                    ));
                    continue;
                }
            };
            match build_trade(&params.chain, &block, timestamp, tx, call, router, swap, &fee_store)
            {
                Ok(trade) => trades.push(trade),
                Err(err) => errors.push(call_error(
                    &params.chain,
                    &block,
                    timestamp,
                    tx,
                    call,
                    router,
                    err.stage,
                    err.message,
                )),
            }
        }
    }
    Ok(Trades { trades, errors })
}

struct TradeDecodeError {
    stage: &'static str,
    message: String,
}

#[allow(clippy::too_many_arguments)]
fn call_error(
    chain: &str,
    block: &Block,
    timestamp: u64,
    tx: &TransactionTrace,
    call: &Call,
    router: &RouterConfig,
    stage: &'static str,
    error: String,
) -> RouterCallError {
    RouterCallError {
        chain: chain.to_string(),
        block_number: block.number,
        block_timestamp: timestamp,
        tx_hash: tx.hash.clone(),
        tx_index: tx.index,
        call_index: call.index,
        router: router.address.clone(),
        router_version: router.version.as_str().to_string(),
        stage: stage.to_string(),
        error,
        tx_success: tx.status == 1,
        call_success: !call.status_failed && !call.status_reverted,
        state_committed: !call.state_reverted,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_trade(
    chain: &str,
    block: &Block,
    timestamp: u64,
    tx: &TransactionTrace,
    call: &Call,
    router: &RouterConfig,
    swap: SwapCall,
    fee_store: &StoreGetString,
) -> std::result::Result<Trade, TradeDecodeError> {
    // Whether the call returned normally, which decides where the amount out comes from. It is
    // not whether the chain kept the result: see `Trade.state_committed`.
    let call_success = !call.status_failed && !call.status_reverted;
    let (amount_out, revert) = if call_success {
        match decode::decode_amount_out(&call.return_data) {
            Ok(v) => (v.to_string(), Default::default()),
            Err(err) => {
                return Err(TradeDecodeError { stage: "amount_out", message: err.to_string() });
            }
        }
    } else {
        let mut revert = decode_revert(&call.return_data);
        if revert.reason.is_empty() && revert.selector.is_empty() {
            revert.reason = call.failure_reason.clone();
        }
        (String::new(), revert)
    };

    let hops = match decode_hops(swap.method, &swap.swaps) {
        Ok(hops) => hops,
        Err(err) if call_success => {
            return Err(TradeDecodeError { stage: "hops", message: err });
        }
        Err(err) => {
            substreams::log::info!(
                "undecodable swaps payload in tx 0x{} call {}: {err}",
                hex::encode(&tx.hash),
                call.index
            );
            Vec::new()
        }
    };
    let hops = hops
        .into_iter()
        .enumerate()
        .map(|(i, hop)| Hop {
            index: i as u32,
            executor: hop.executor,
            token_in_index: hop
                .token_in_index
                .map(u32::from)
                .unwrap_or_default(),
            token_out_index: hop
                .token_out_index
                .map(u32::from)
                .unwrap_or_default(),
            split: hop.split.unwrap_or_default(),
            protocol_data: hop.protocol_data,
        })
        .collect();

    let fees_taken = call
        .logs
        .iter()
        .filter_map(FeesTaken::match_and_decode)
        .flat_map(|ev| {
            let token = ev.token;
            ev.fees
                .into_iter()
                .map(move |fee| (token.clone(), fee))
        })
        .enumerate()
        .map(|(index, (token, (recipient, amount)))| FeeTaken {
            recipient,
            amount: amount.to_string(),
            role: match index {
                0 => "router",
                1 => "client",
                _ => "unknown",
            }
            .to_string(),
            token,
        })
        .collect();

    let client_fee = swap
        .client_fee
        .as_ref()
        .map(|c| ClientFee {
            fee_bps: saturating_u64(&c.fee_bps),
            receiver: c.receiver.clone(),
            max_client_contribution: c.max_client_contribution.to_string(),
            deadline: c.deadline.to_string(),
            has_signature: !c.signature.is_empty(),
        });
    let lookup = FeeLookup {
        tx_origin: &tx.from,
        client: swap
            .client_fee
            .as_ref()
            .map(|c| c.receiver.as_slice()),
        token_in: &swap.token_in,
        token_out: &swap.token_out,
    };
    let router_fee_config = resolve_fee_config(fee_store, router, call.begin_ordinal, &lookup);

    Ok(Trade {
        chain: chain.to_string(),
        block_number: block.number,
        block_timestamp: timestamp,
        tx_hash: tx.hash.clone(),
        tx_index: tx.index,
        call_index: call.index,
        tx_success: tx.status == 1,
        call_success,
        state_committed: !call.state_reverted,
        router: router.address.clone(),
        router_version: router.version.as_str().to_string(),
        strategy: swap.method.as_str().to_string(),
        funding: swap.funding.as_str().to_string(),
        eoa: tx.from.clone(),
        msg_sender: call.caller.clone(),
        receiver: swap.receiver,
        token_in: swap.token_in,
        token_out: swap.token_out,
        amount_in: swap.amount_in.to_string(),
        expected_amount_out: swap
            .expected_amount_out
            .map(|v| v.to_string())
            .unwrap_or_default(),
        min_amount_out: swap.min_amount_out.to_string(),
        amount_out,
        native_value: call
            .value
            .as_ref()
            .map(|v| BigInt::from_unsigned_bytes_be(&v.bytes).to_string())
            .unwrap_or_else(|| "0".to_string()),
        gas_used: call.gas_consumed,
        revert_selector: revert.selector,
        revert_reason: revert.reason,
        client_fee,
        router_fee_config,
        fees_taken,
        n_tokens: swap.n_tokens.unwrap_or_default(),
        hops,
        watermark: swap.watermark,
        wrap_eth: swap.wrap_eth,
        unwrap_eth: swap.unwrap_eth,
    })
}

/// A router positive slippage of 100% in fee units. Router positive slippage bps exist only on the
/// `uint32` calculator generation, so they are always on its denominator.
const FULL_ROUTER_POSITIVE_SLIPPAGE_BPS: u64 = 100_000_000;

/// The denominator the fee bps of a calculator are on.
///
/// The scale belongs to the calculator, not to the router that resolves to it: a router rotated
/// onto a calculator of another generation reports bps on that generation's denominator. So an
/// observed scale wins, and the generation the router shipped with is only the fallback for a
/// calculator no observed event gives away.
fn bps_scale(observed: Option<u64>, router: RouterVersion) -> Option<u64> {
    observed.or_else(|| router.default_bps_scale())
}

/// A client cap of 100% in fee units, which captures as much as no cap does. The FeeCalculator
/// reports it for a client without a cap.
const UNCAPPED_POSITIVE_SLIPPAGE_BPS: u64 = 100_000_000;

/// What a trade's fee configuration depends on besides the calculator.
struct FeeLookup<'a> {
    tx_origin: &'a [u8],
    /// The signed client fee receiver, if the trade carries one.
    client: Option<&'a [u8]>,
    token_in: &'a [u8],
    token_out: &'a [u8],
}

/// The router fee on output for a trade, as `FeeCalculator._applyTokenPairFee` resolves it: a
/// pair fee replaces the default fee, and against a client's custom fee the lower one applies.
fn fee_on_output_with_pair_fee(fee_on_output: u64, custom: Option<u64>, pair: Option<u64>) -> u64 {
    match (pair, custom) {
        (None, _) => fee_on_output,
        (Some(pair), None) => pair,
        (Some(pair), Some(custom)) => pair.min(custom),
    }
}

/// Resolves the router fee configuration in effect at `ordinal`, applying per-client overrides
/// the same way `FeeCalculator._resolveClient` does (zero client falls back to `tx.origin`).
fn resolve_fee_config(
    store: &StoreGetString,
    router: &RouterConfig,
    ordinal: u64,
    lookup: &FeeLookup,
) -> Option<RouterFeeConfig> {
    if router.version == RouterVersion::V2 {
        return None;
    }
    let fee_calculator = store
        .get_at(ordinal, keys::router_fee_calculator(&router.address))
        .and_then(|v| hex::decode(v.trim_start_matches("0x")).ok())
        .or_else(|| router.fee_calculator.clone())?;
    let observed_scale = store
        .get_at(ordinal, keys::fee_bps_scale(&fee_calculator))
        .and_then(|v| v.parse::<u64>().ok());
    let bps_scale = bps_scale(observed_scale, router.version)?;
    let client = match lookup.client {
        Some(c) if c != ZERO_ADDRESS => c,
        _ => lookup.tx_origin,
    };
    let get_u64 = |key: String| {
        store
            .get_at(ordinal, key)
            .and_then(|v| v.parse::<u64>().ok())
    };
    let custom_output = get_u64(keys::custom_fee_on_output(&fee_calculator, client));
    let custom_client = get_u64(keys::custom_fee_on_client_fee(&fee_calculator, client));
    let (token0, token1) = if lookup.token_in <= lookup.token_out {
        (lookup.token_in, lookup.token_out)
    } else {
        (lookup.token_out, lookup.token_in)
    };
    let pair_fee = get_u64(keys::token_pair_fee(&fee_calculator, token0, token1));
    let fee_on_output_bps = fee_on_output_with_pair_fee(
        custom_output
            .or_else(|| get_u64(keys::fee_on_output(&fee_calculator)))
            .unwrap_or_default(),
        custom_output,
        pair_fee,
    );
    let fee_on_client_fee_bps = custom_client
        .or_else(|| get_u64(keys::fee_on_client_fee(&fee_calculator)))
        .unwrap_or_default();
    let global_router_bps = get_u64(keys::router_positive_slippage(&fee_calculator));
    // Older calculators turn capture on and off with `PositiveSlippageToggled`. A calculator with
    // a router positive slippage bps has no switch, so its router positive slippage bps alone
    // decides what the router captures.
    let positive_slippage_enabled = router.version == RouterVersion::V3_1 &&
        (global_router_bps.is_some() ||
            store
                .get_at(ordinal, keys::positive_slippage(&fee_calculator))
                .is_some_and(|v| v == "1"));
    // A calculator that never emitted a router positive slippage event captures the full surplus.
    let router_positive_slippage_bps =
        get_u64(keys::custom_router_positive_slippage(&fee_calculator, client))
            .or(global_router_bps)
            .unwrap_or(FULL_ROUTER_POSITIVE_SLIPPAGE_BPS);
    let client_positive_slippage_share_bps =
        get_u64(keys::client_positive_slippage_share(&fee_calculator, client)).unwrap_or_default();
    let positive_slippage_cap_bps = get_u64(keys::positive_slippage_cap(&fee_calculator, client))
        .unwrap_or(UNCAPPED_POSITIVE_SLIPPAGE_BPS);
    Some(RouterFeeConfig {
        fee_calculator,
        fee_on_output_bps,
        fee_on_client_fee_bps,
        custom_fee_on_output: custom_output.is_some(),
        custom_fee_on_client_fee: custom_client.is_some(),
        positive_slippage_enabled,
        router_positive_slippage_bps,
        client_positive_slippage_share_bps,
        bps_scale,
        positive_slippage_cap_bps,
        token_pair_fee: pair_fee.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_pair_fee_the_fee_on_output_stands() {
        assert_eq!(fee_on_output_with_pair_fee(100, None, None), 100);
        assert_eq!(fee_on_output_with_pair_fee(40, Some(40), None), 40);
    }

    #[test]
    fn a_pair_fee_replaces_the_default_fee_in_both_directions() {
        assert_eq!(fee_on_output_with_pair_fee(100, None, Some(300)), 300);
        assert_eq!(fee_on_output_with_pair_fee(100, None, Some(0)), 0);
    }

    #[test]
    fn against_a_custom_fee_the_lower_one_applies() {
        assert_eq!(fee_on_output_with_pair_fee(40, Some(40), Some(300)), 40);
        assert_eq!(fee_on_output_with_pair_fee(400, Some(400), Some(300)), 300);
    }

    /// The rotation this fixes: a v3_0 router pointed at a calculator of the next generation
    /// reports its bps on 100000000, and reading 10000 off the router makes the rate 10000 times
    /// too large.
    #[test]
    fn an_observed_scale_beats_the_router_generation() {
        assert_eq!(bps_scale(Some(100_000_000), RouterVersion::V3_0), Some(100_000_000));
        assert_eq!(bps_scale(Some(10_000), RouterVersion::V3_1), Some(10_000));
    }

    #[test]
    fn without_one_the_router_generation_answers() {
        assert_eq!(bps_scale(None, RouterVersion::V3_0), Some(10_000));
        assert_eq!(bps_scale(None, RouterVersion::V3_1), Some(100_000_000));
    }

    #[test]
    fn v2_has_no_scale_either_way() {
        assert_eq!(bps_scale(None, RouterVersion::V2), None);
    }
}
