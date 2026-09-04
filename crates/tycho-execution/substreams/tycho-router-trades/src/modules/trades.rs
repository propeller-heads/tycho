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
    executors,
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
            protocol_systems: executors::protocol_systems_for(&hop.executor),
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
        .flat_map(|ev| ev.fees.into_iter())
        .enumerate()
        .map(|(index, (recipient, amount))| FeeTaken {
            recipient,
            amount: amount.to_string(),
            role: match index {
                0 => "router",
                1 => "client",
                _ => "unknown",
            }
            .to_string(),
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
    let router_fee_config = resolve_fee_config(
        fee_store,
        router,
        call.begin_ordinal,
        &tx.from,
        swap.client_fee
            .as_ref()
            .map(|c| c.receiver.as_slice()),
    );

    Ok(Trade {
        chain: chain.to_string(),
        block_number: block.number,
        block_timestamp: timestamp,
        tx_hash: tx.hash.clone(),
        tx_index: tx.index,
        call_index: call.index,
        tx_success: tx.status == 1,
        call_success,
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

/// Resolves the router fee configuration in effect at `ordinal`, applying per-client overrides
/// the same way `FeeCalculator._resolveClient` does (zero client falls back to `tx.origin`).
fn resolve_fee_config(
    store: &StoreGetString,
    router: &RouterConfig,
    ordinal: u64,
    tx_origin: &[u8],
    client: Option<&[u8]>,
) -> Option<RouterFeeConfig> {
    let bps_scale = router.version.bps_scale()?;
    let fee_calculator = store
        .get_at(ordinal, keys::router_fee_calculator(&router.address))
        .and_then(|v| hex::decode(v.trim_start_matches("0x")).ok())
        .or_else(|| router.fee_calculator.clone())?;
    let client = match client {
        Some(c) if c != ZERO_ADDRESS => c,
        _ => tx_origin,
    };
    let get_u64 = |key: String| {
        store
            .get_at(ordinal, key)
            .and_then(|v| v.parse::<u64>().ok())
    };
    let custom_output = get_u64(keys::custom_fee_on_output(&fee_calculator, client));
    let custom_client = get_u64(keys::custom_fee_on_client_fee(&fee_calculator, client));
    let fee_on_output_bps = custom_output
        .or_else(|| get_u64(keys::fee_on_output(&fee_calculator)))
        .unwrap_or_default();
    let fee_on_client_fee_bps = custom_client
        .or_else(|| get_u64(keys::fee_on_client_fee(&fee_calculator)))
        .unwrap_or_default();
    let positive_slippage_enabled = router.version == RouterVersion::V3_1 &&
        store
            .get_at(ordinal, keys::positive_slippage(&fee_calculator))
            .map(|v| v == "1")
            .unwrap_or(false);
    Some(RouterFeeConfig {
        fee_calculator,
        fee_on_output_bps,
        fee_on_client_fee_bps,
        custom_fee_on_output: custom_output.is_some(),
        custom_fee_on_client_fee: custom_client.is_some(),
        positive_slippage_enabled,
        bps_scale,
    })
}
