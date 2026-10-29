//! Traces the legs with `debug_traceCall`: the calls the token makes to other contracts and the
//! storage slots its transfer path reads.
//!
//! The legs traced are real swaps in the realistic environment when the pool is a Uniswap V2 style
//! pair against the wrapped native token, the direct legs otherwise. Each transaction is traced on
//! top of the state the earlier ones left, rebuilt from their `prestateTracer` diffs as state
//! overrides.

use std::collections::BTreeSet;

use alloy::{
    primitives::B256,
    rpc::types::{
        state::StateOverride,
        trace::geth::{
            AccountState, CallFrame, DiffMode,
            GethDebugBuiltInTracerType::{self, CallTracer, PreStateTracer},
            GethDebugTracerConfig, GethDebugTracerType, GethDebugTracingCallOptions,
            GethDebugTracingOptions, GethTrace, PreStateFrame,
        },
        BlockNumberOrTag, BlockOverrides, TransactionRequest,
    },
    sol_types::SolCall,
};
use serde_json::json;
use tycho_common::{models::token::ExternalCall, Bytes};

use super::{
    scenario::{BuySize, Environment, TokenContext, V2Pair, SECONDS_PER_BLOCK},
    swap,
};
use crate::{
    erc20::{approveCall, transferCall, transferFromCall},
    BytesCodec,
};

/// What the traces of the legs showed.
#[derive(Debug, Clone, Default)]
pub(super) struct TracedLegs {
    /// Token slots a leg read without writing: configuration the transfer depends on, such as
    /// fee rates, flags, limits and exemption lists.
    pub read_only_slots: BTreeSet<B256>,
    /// Calls the token made to addresses other than itself and the parties of the trade.
    pub calls: Vec<ExternalCall>,
}

impl TracedLegs {
    fn add_leg_trace(&mut self, trace: &LegTrace, context: &TokenContext) {
        self.read_only_slots
            .extend(trace.read_only.iter().copied());
        collect_calls(&trace.root, context, "read", &mut self.calls);
    }
}

struct LegTrace {
    root: CallFrame,
    read_only: BTreeSet<B256>,
    diff: DiffMode,
}

/// Traces the legs. Fails when a trace call fails, so that an incomplete trace is never taken
/// for a token without external calls.
pub(super) async fn trace_legs(context: &TokenContext) -> Result<TracedLegs, String> {
    let mut traced = TracedLegs::default();
    match &context.v2 {
        Some(pair) => trace_swap(context, pair, &mut traced).await,
        None => trace_direct(context, &mut traced).await,
    }
    .map_err(|error| format!("tracing the legs: {error}"))?;
    Ok(traced)
}

/// Traces the swap legs in the realistic environment: the buy (with its forward) and the
/// receiver's approval, which are independent, then the sell on top of both.
async fn trace_swap(
    context: &TokenContext,
    pair: &V2Pair,
    traced: &mut TracedLegs,
) -> Result<(), String> {
    let block = swap::first_block(context, Environment::Realistic, 0);
    let legs = swap::transactions(context, pair, Environment::Realistic, BuySize::Small);
    let mut state = swap::wallet_and_router_state(&legs);
    let [buy, approve, sell] = legs;
    let (buy, approve) = tokio::try_join!(
        trace_leg(context, &buy, &block, &state),
        trace_leg(context, &approve, &block, &state),
    )?;
    traced.add_leg_trace(&buy, context);
    traced.add_leg_trace(&approve, context);
    if buy.root.error.is_some() || approve.root.error.is_some() {
        return Ok(());
    }
    apply_state_diff(&mut state, &buy.diff);
    apply_state_diff(&mut state, &approve.diff);
    let sell = trace_leg(context, &sell, &block, &state).await?;
    traced.add_leg_trace(&sell, context);
    Ok(())
}

/// Traces the direct legs: the pool paying out the buy, then the transfer and sell one block
/// later.
async fn trace_direct(context: &TokenContext, traced: &mut TracedLegs) -> Result<(), String> {
    let block = context.first_block(0);
    let mut state = context.state();
    let buy =
        trace_leg(context, &context.buy_tx(context.amount(BuySize::Small)), &block, &state).await?;
    traced.add_leg_trace(&buy, context);
    if buy.root.error.is_some() {
        return Ok(());
    }
    apply_state_diff(&mut state, &buy.diff);
    let next = BlockOverrides {
        number: block
            .number
            .map(|n| n + alloy::primitives::U256::from(1)),
        time: block
            .time
            .map(|t| t + SECONDS_PER_BLOCK),
        ..Default::default()
    };
    let legs = trace_leg(context, &context.legs_tx(), &next, &state).await?;
    traced.add_leg_trace(&legs, context);
    Ok(())
}

/// Which leg a call into the token belongs to: paying the buyer out of the pool, forwarding to
/// the receiving wallet, moving tokens into the pool, approving, or only reading.
fn leg_of(frame: &CallFrame, context: &TokenContext) -> &'static str {
    let input = frame.input.as_ref();
    if let Ok(call) = transferCall::abi_decode(input) {
        return match call._to {
            to if to == context.pool => "sell",
            to if to == context.wallet => "transfer",
            _ => "buy",
        };
    }
    if transferFromCall::abi_decode(input).is_ok() {
        return "sell";
    }
    if approveCall::abi_decode(input).is_ok() {
        return "approve";
    }
    "read"
}

async fn trace_leg(
    context: &TokenContext,
    tx: &TransactionRequest,
    block: &BlockOverrides,
    state: &StateOverride,
) -> Result<LegTrace, String> {
    let (prestate, diff, calls) = tokio::try_join!(
        debug_trace(context, tx, block, state, PreStateTracer, false),
        debug_trace(context, tx, block, state, PreStateTracer, true),
        debug_trace(context, tx, block, state, CallTracer, false),
    )?;
    let GethTrace::PreStateTracer(PreStateFrame::Default(prestate)) = prestate else {
        return Err("prestateTracer returned an unexpected frame".to_string());
    };
    let GethTrace::PreStateTracer(PreStateFrame::Diff(diff)) = diff else {
        return Err("prestateTracer diff returned an unexpected frame".to_string());
    };
    let GethTrace::CallTracer(root) = calls else {
        return Err("callTracer returned an unexpected frame".to_string());
    };
    let touched = storage_keys(prestate.0.get(&context.token));
    let written = storage_keys(diff.post.get(&context.token));
    let read_only = touched
        .difference(&written)
        .copied()
        .collect();
    Ok(LegTrace { root, read_only, diff })
}

async fn debug_trace(
    context: &TokenContext,
    tx: &TransactionRequest,
    block: &BlockOverrides,
    state: &StateOverride,
    tracer: GethDebugBuiltInTracerType,
    diff_mode: bool,
) -> Result<GethTrace, String> {
    let tracer_config = match tracer {
        PreStateTracer => json!({ "diffMode": diff_mode }),
        _ => json!({}),
    };
    let options = GethDebugTracingCallOptions {
        tracing_options: GethDebugTracingOptions {
            tracer: Some(GethDebugTracerType::BuiltInTracer(tracer)),
            tracer_config: GethDebugTracerConfig(tracer_config),
            ..Default::default()
        },
        state_overrides: Some(state.clone()),
        block_overrides: Some(block.clone()),
        tx_index: None,
    };
    context
        .rpc
        .debug_trace_call(tx, BlockNumberOrTag::Number(context.block), &options)
        .await
        .map_err(|e| format!("{tracer:?} (diffMode={diff_mode}): {e}"))
}

/// Folds a leg's state changes into `state`. Geth leaves slots that became zero out of `post`,
/// so slots only in `pre` are set to zero explicitly.
fn apply_state_diff(state: &mut StateOverride, diff: &DiffMode) {
    for (address, pre) in &diff.pre {
        let post = diff.post.get(address);
        let entry = state.entry(*address).or_default();
        let slots = entry
            .state_diff
            .get_or_insert_with(Default::default);
        for slot in pre.storage.keys() {
            if post.is_none_or(|p| !p.storage.contains_key(slot)) {
                slots.insert(*slot, B256::ZERO);
            }
        }
    }
    for (address, post) in &diff.post {
        let entry = state.entry(*address).or_default();
        let slots = entry
            .state_diff
            .get_or_insert_with(Default::default);
        slots.extend(
            post.storage
                .iter()
                .map(|(k, v)| (*k, *v)),
        );
        if let Some(balance) = post.balance {
            entry.balance = Some(balance);
        }
    }
}

fn storage_keys(account: Option<&AccountState>) -> BTreeSet<B256> {
    account
        .map(|a| a.storage.keys().copied().collect())
        .unwrap_or_default()
}

/// Collects every call the token made, depth first, except calls back into itself, to the
/// parties of the trade, and delegate calls into its own implementation. Each call is labelled
/// with the leg of the call into the token it happened under.
fn collect_calls(
    frame: &CallFrame,
    context: &TokenContext,
    leg: &'static str,
    out: &mut Vec<ExternalCall>,
) {
    let ignored = [
        context.token,
        context.pool,
        context.trader,
        context.wallet,
        swap::router(),
        swap::buyer(),
    ];
    let leg = if frame.to == Some(context.token) && frame.from != context.token {
        leg_of(frame, context)
    } else {
        leg
    };
    let delegate = frame
        .typ
        .eq_ignore_ascii_case("DELEGATECALL");
    if frame.from == context.token && !delegate {
        if let Some(to) = frame
            .to
            .filter(|to| !ignored.contains(to))
        {
            out.push(ExternalCall {
                leg: leg.to_string(),
                target: to.to_bytes(),
                selector: (frame.input.len() >= 4).then(|| Bytes::from(frame.input[..4].to_vec())),
                reverted: frame.error.is_some(),
            });
        }
    }
    for child in &frame.calls {
        collect_calls(child, context, leg, out);
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, Bytes as AlloyBytes, U256};

    use super::*;

    fn context() -> TokenContext {
        let rpc = crate::rpc::EthereumRpcClient::new("http://localhost:1").expect("client");
        TokenContext::new(
            &rpc,
            Address::repeat_byte(7),
            Address::repeat_byte(8),
            U256::from(1000),
            (1, 1),
        )
    }

    fn frame(from: Address, to: Address, typ: &str, input: &[u8]) -> CallFrame {
        CallFrame {
            from,
            to: Some(to),
            typ: typ.to_string(),
            input: AlloyBytes::copy_from_slice(input),
            ..Default::default()
        }
    }

    #[test]
    fn collects_token_calls_except_to_known_parties_and_delegates() {
        let context = context();
        let veto = Address::repeat_byte(9);
        let mut root = frame(context.pool, context.token, "CALL", &[0xa9, 0x05, 0x9c, 0xbb]);
        root.calls = vec![
            frame(context.token, veto, "CALL", &[1, 2, 3, 4, 5]),
            frame(context.token, context.pool, "STATICCALL", &[0x09, 0x02, 0xf1, 0xac]),
            frame(context.token, Address::repeat_byte(3), "DELEGATECALL", &[1, 1, 1, 1]),
            frame(context.token, Address::repeat_byte(4), "CALL", &[]),
        ];
        let mut calls = Vec::new();
        collect_calls(&root, &context, "buy", &mut calls);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].target, veto.to_bytes());
        assert_eq!(calls[0].selector, Some(Bytes::from(vec![1, 2, 3, 4])));
        assert_eq!(calls[1].selector, None);
    }

    #[test]
    fn labels_calls_with_the_leg_they_happen_under() {
        let context = context();
        let tax = Address::repeat_byte(9);
        let under = |caller: Address, input: Vec<u8>| {
            let mut frame = frame(caller, context.token, "CALL", &input);
            frame.calls = vec![frame_to(context.token, tax)];
            frame
        };
        let mut sell_tx = frame(context.wallet, swap::router(), "CALL", &[0; 4]);
        sell_tx.calls = vec![under(
            swap::router(),
            transferFromCall { _from: context.wallet, _to: context.pool, _value: U256::from(1) }
                .abi_encode(),
        )];
        let mut swap_frame = frame(swap::router(), context.pool, "CALL", &[0; 4]);
        swap_frame.calls = vec![under(
            context.pool,
            transferCall { _to: swap::router(), _value: U256::from(1) }.abi_encode(),
        )];
        let mut buy_tx = frame(swap::buyer(), swap::router(), "CALL", &[0; 4]);
        buy_tx.calls = vec![
            swap_frame,
            under(
                swap::router(),
                transferCall { _to: context.wallet, _value: U256::from(1) }.abi_encode(),
            ),
        ];

        let mut calls = Vec::new();
        collect_calls(&buy_tx, &context, "read", &mut calls);
        collect_calls(&sell_tx, &context, "read", &mut calls);
        let legs: Vec<&str> = calls
            .iter()
            .map(|c| c.leg.as_str())
            .collect();
        assert_eq!(legs, ["buy", "transfer", "sell"]);
    }

    fn frame_to(from: Address, to: Address) -> CallFrame {
        frame(from, to, "CALL", &[1, 2, 3, 4])
    }

    #[test]
    fn carry_forward_zeroes_slots_cleared_by_the_leg() {
        let token = Address::repeat_byte(7);
        let (kept, cleared) = (B256::repeat_byte(1), B256::repeat_byte(2));
        let mut pre = AccountState::default();
        pre.storage
            .insert(kept, B256::repeat_byte(5));
        pre.storage
            .insert(cleared, B256::repeat_byte(6));
        let mut post = AccountState::default();
        post.storage
            .insert(kept, B256::repeat_byte(9));
        let diff = DiffMode {
            pre: [(token, pre)].into_iter().collect(),
            post: [(token, post)].into_iter().collect(),
        };
        let mut state = StateOverride::default();
        apply_state_diff(&mut state, &diff);
        let slots = state[&token]
            .state_diff
            .as_ref()
            .expect("diff set");
        assert_eq!(slots[&kept], B256::repeat_byte(9));
        assert_eq!(slots[&cleared], B256::ZERO);
    }
}
