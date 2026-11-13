//! Traces the legs with `debug_traceCall`: the calls the token makes to other contracts and the
//! storage slots its transfer path reads.
//!
//! The legs traced are real swaps in the realistic environment when the pool is a Uniswap V2 style
//! pair against the wrapped native token, the direct legs otherwise. Each transaction is traced on
//! top of the state the earlier ones left, rebuilt from their `prestateTracer` diffs as state
//! overrides.

use std::collections::{BTreeMap, BTreeSet};

use alloy::{
    primitives::{Address, B256},
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
    scenario::{
        block_after, first_word, revert_reason, BuySize, Environment, TokenContext, V2Pair,
    },
    swap,
};
use crate::{
    erc20::{transferCall, transferFromCall},
    BytesCodec,
};

/// What the traces of the legs showed.
#[derive(Debug, Clone, Default)]
pub(super) struct TracedLegs {
    /// Token slots a leg read without writing, with their value before the first leg that read
    /// them: configuration the transfer depends on, such as fee rates, flags, limits and
    /// exemption lists.
    pub read_only_slots: BTreeMap<B256, B256>,
    /// Calls the token made to addresses other than itself and the parties of the trade.
    pub calls: Vec<ExternalCall>,
    /// Contracts the token forwards the calls it receives to: a proxy's implementations.
    pub forward_targets: BTreeSet<Address>,
    /// Why the token refused to pay out the pool's side of the swap buy; `None` when it paid or
    /// the legs were direct.
    pub refused_payout: Option<String>,
}

impl TracedLegs {
    fn add_leg_trace(&mut self, trace: &LegTrace, context: &TokenContext) {
        for (slot, value) in &trace.read_only {
            self.read_only_slots
                .entry(*slot)
                .or_insert(*value);
        }
        collect_calls(&trace.root, context, "read", &mut self.calls);
        collect_forward_targets(&trace.root, context.token, &mut self.forward_targets);
    }
}

struct LegTrace {
    root: CallFrame,
    read_only: BTreeMap<B256, B256>,
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

/// Traces the swap legs in the realistic environment: the buy (with its forward) and, in the
/// next block, the receiver's approval, which are independent, then the sell on top of both. Only
/// the approval's state change is traced: the sell's `transferFrom` runs the token code that
/// matters.
async fn trace_swap(
    context: &TokenContext,
    pair: &V2Pair,
    traced: &mut TracedLegs,
) -> Result<(), String> {
    let block = swap::first_block(context, Environment::Realistic, 0);
    let next = block_after(&block, context.next_block());
    let legs = swap::transactions(context, pair, Environment::Realistic, BuySize::Small);
    let mut state = swap::wallet_and_router_state(&legs);
    context.add_overrides(&mut state);
    let [buy, approve, sell] = legs;
    let (buy, approve) = tokio::try_join!(
        trace_leg(context, &buy, &block, &state),
        trace_state_change(context, &approve, &next, &state),
    )?;
    traced.add_leg_trace(&buy, context);
    traced.refused_payout = refused_payout(&buy.root, context);
    if buy.root.error.is_some() {
        return Ok(());
    }
    apply_state_diff(&mut state, &buy.diff);
    apply_state_diff(&mut state, &approve);
    let sell = trace_leg(context, &sell, &next, &state).await?;
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
    let next = block_after(&block, context.next_block());
    let legs = trace_leg(context, &context.legs_tx(), &next, &state).await?;
    traced.add_leg_trace(&legs, context);
    Ok(())
}

/// The reason the token refused the pool's `transfer` of the bought tokens, depth first: the
/// call reverted or returned `false`.
fn refused_payout(frame: &CallFrame, context: &TokenContext) -> Option<String> {
    let payout = frame.from == context.pool &&
        frame.to == Some(context.token) &&
        transferCall::abi_decode(frame.input.as_ref()).is_ok();
    if payout {
        let output = frame
            .output
            .as_ref()
            .map(|o| o.as_ref())
            .unwrap_or_default();
        if frame.error.is_some() {
            return Some(revert_reason(output, frame.error.as_deref()));
        }
        if output.len() >= 32 && first_word(output).is_zero() {
            return Some("transfer returned false".to_string());
        }
    }
    frame
        .calls
        .iter()
        .find_map(|child| refused_payout(child, context))
}

/// Which leg a call into the token belongs to: paying the buyer out of the pool, forwarding to
/// the receiving wallet, moving tokens into the pool, or only reading.
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
    let written = storage_keys(diff.post.get(&context.token));
    let read_only = prestate
        .0
        .get(&context.token)
        .map(|account| {
            account
                .storage
                .iter()
                .filter(|(slot, _)| !written.contains(*slot))
                .map(|(slot, value)| (*slot, *value))
                .collect()
        })
        .unwrap_or_default();
    Ok(LegTrace { root, read_only, diff })
}

async fn trace_state_change(
    context: &TokenContext,
    tx: &TransactionRequest,
    block: &BlockOverrides,
    state: &StateOverride,
) -> Result<DiffMode, String> {
    let diff = debug_trace(context, tx, block, state, PreStateTracer, true).await?;
    let GethTrace::PreStateTracer(PreStateFrame::Diff(diff)) = diff else {
        return Err("prestateTracer diff returned an unexpected frame".to_string());
    };
    Ok(diff)
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
        CallTracer |
        GethDebugBuiltInTracerType::FourByteTracer |
        GethDebugBuiltInTracerType::FlatCallTracer |
        GethDebugBuiltInTracerType::NoopTracer |
        GethDebugBuiltInTracerType::MuxTracer |
        GethDebugBuiltInTracerType::Erc7562Tracer => json!({}),
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
/// parties of the trade and to precompiles, a proxy's delegate call that forwards the call it
/// received unchanged, and a beacon proxy's lookup of where to forward: a static call that
/// returns the forward's target, whatever the beacon names its getter. Any
/// other delegate call counts: its target's code runs as the token's, so a contract that can
/// change it can change how the token transfers. Each call is labelled with the leg of the call
/// into the token it happened under.
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
    let is_forward = |child: &CallFrame| forwards(frame, child);
    let forward_targets: Vec<Address> = frame
        .calls
        .iter()
        .filter(|child| is_forward(child))
        .filter_map(|child| child.to)
        .collect();
    for child in &frame.calls {
        let beacon_lookup = child
            .typ
            .eq_ignore_ascii_case("STATICCALL") &&
            child
                .output
                .as_ref()
                .filter(|output| output.len() == 32)
                .is_some_and(|output| {
                    forward_targets.contains(&Address::from_word(B256::from_slice(output)))
                });
        if child.from == context.token && !is_forward(child) && !beacon_lookup {
            if let Some(to) = child
                .to
                .filter(|to| !ignored.contains(to) && !is_precompile(*to))
            {
                out.push(ExternalCall {
                    leg: leg.to_string(),
                    target: to.to_bytes(),
                    selector: (child.input.len() >= 4)
                        .then(|| Bytes::from(child.input[..4].to_vec())),
                    reverted: child.error.is_some(),
                });
            }
        }
        collect_calls(child, context, leg, out);
    }
}

/// Whether `child` is a proxy's delegate call that forwards the call `frame` received unchanged.
fn forwards(frame: &CallFrame, child: &CallFrame) -> bool {
    child
        .typ
        .eq_ignore_ascii_case("DELEGATECALL") &&
        child.input == frame.input
}

/// Collects the contracts that calls into `token` were forwarded to, at any depth.
fn collect_forward_targets(frame: &CallFrame, token: Address, out: &mut BTreeSet<Address>) {
    for child in &frame.calls {
        if frame.to == Some(token) && forwards(frame, child) {
            out.extend(child.to);
        }
        collect_forward_targets(child, token, out);
    }
}

/// Precompiles and system contracts sit at the lowest addresses (0x01 to 0x0a, RIP-7212 at
/// 0x100); they cannot change how a token transfers.
fn is_precompile(address: Address) -> bool {
    address.0[..18]
        .iter()
        .all(|byte| *byte == 0)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Bytes as AlloyBytes, U256};

    use super::*;

    fn context() -> TokenContext {
        let rpc = crate::rpc::EthereumRpcClient::new("http://localhost:1").expect("client");
        TokenContext::new(
            &rpc,
            Address::repeat_byte(7),
            Address::repeat_byte(8),
            U256::from(1000),
            (1, 1, 12),
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
    fn collects_token_calls_except_to_known_parties_and_proxy_forwarding() {
        let context = context();
        let veto = Address::repeat_byte(9);
        let library = Address::repeat_byte(3);
        let transfer = [0xa9, 0x05, 0x9c, 0xbb];
        let mut forward = frame(context.token, Address::repeat_byte(5), "DELEGATECALL", &transfer);
        forward.calls = vec![
            frame(context.token, veto, "CALL", &[1, 2, 3, 4, 5]),
            frame(context.token, context.pool, "STATICCALL", &[0x09, 0x02, 0xf1, 0xac]),
            frame(context.token, library, "DELEGATECALL", &[1, 1, 1, 1]),
            frame(context.token, Address::repeat_byte(4), "CALL", &[]),
            frame(context.token, Address::with_last_byte(1), "STATICCALL", &[0; 128]),
        ];
        let mut root = frame(context.pool, context.token, "CALL", &transfer);
        root.calls = vec![forward];
        let mut calls = Vec::new();
        collect_calls(&root, &context, "buy", &mut calls);
        let targets: Vec<Bytes> = calls
            .iter()
            .map(|c| c.target.clone())
            .collect();
        assert_eq!(
            targets,
            [veto.to_bytes(), library.to_bytes(), Address::repeat_byte(4).to_bytes()]
        );
        assert_eq!(calls[0].selector, Some(Bytes::from(vec![1, 2, 3, 4])));
        assert_eq!(calls[1].selector, Some(Bytes::from(vec![1, 1, 1, 1])));
        assert_eq!(calls[2].selector, None);
    }

    #[test]
    fn skips_a_beacon_lookup_that_returns_the_forward_target() {
        let context = context();
        let (beacon, implementation) = (Address::repeat_byte(6), Address::repeat_byte(5));
        let transfer = [0xa9, 0x05, 0x9c, 0xbb];
        let lookup = |returns: Address| {
            // `childImplementation()`: beacons name their getter differently.
            let mut lookup = frame(context.token, beacon, "STATICCALL", &[0xda, 0x52, 0x57, 0x16]);
            lookup.output = Some(AlloyBytes::from(returns.into_word().to_vec()));
            lookup
        };
        let forward = || frame(context.token, implementation, "DELEGATECALL", &transfer);
        let call_with = |calls: Vec<CallFrame>| {
            let mut call = frame(context.pool, context.token, "CALL", &transfer);
            call.calls = calls;
            call
        };

        let mut calls = Vec::new();
        collect_calls(
            &call_with(vec![lookup(implementation), forward()]),
            &context,
            "buy",
            &mut calls,
        );
        assert!(calls.is_empty());
        collect_calls(&call_with(vec![lookup(implementation)]), &context, "buy", &mut calls);
        collect_calls(
            &call_with(vec![lookup(Address::repeat_byte(4)), forward()]),
            &context,
            "buy",
            &mut calls,
        );
        let targets: Vec<Bytes> = calls
            .iter()
            .map(|c| c.target.clone())
            .collect();
        assert_eq!(targets, [beacon.to_bytes(), beacon.to_bytes()]);
    }

    #[test]
    fn collects_the_implementations_a_call_into_the_token_is_forwarded_to() {
        let context = context();
        let transfer = [0xa9, 0x05, 0x9c, 0xbb];
        let implementation = Address::repeat_byte(5);
        let mut proxy_call = frame(context.pool, context.token, "CALL", &transfer);
        proxy_call.calls = vec![
            frame(context.token, implementation, "DELEGATECALL", &transfer),
            frame(context.token, Address::repeat_byte(6), "DELEGATECALL", &[0x01]),
        ];
        let mut other_proxy = frame(context.pool, Address::repeat_byte(4), "CALL", &transfer);
        other_proxy.calls = vec![frame(
            Address::repeat_byte(4),
            Address::repeat_byte(3),
            "DELEGATECALL",
            &transfer,
        )];
        let mut root = frame(context.trader, context.pool, "CALL", &[0x02]);
        root.calls = vec![proxy_call, other_proxy];

        let mut targets = BTreeSet::new();
        collect_forward_targets(&root, context.token, &mut targets);
        assert_eq!(targets, BTreeSet::from([implementation]));
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

    /// The buy as traced: the router calls the pair's `swap`, which reverts with "UniswapV2: K"
    /// when `payout_error` is `None`, and the pair calls the token's `transfer` to the router,
    /// which reverts with `payout_error` or returns `payout_output`.
    fn swap_buy(
        context: &TokenContext,
        payout_error: Option<&str>,
        payout_output: Vec<u8>,
    ) -> CallFrame {
        let mut payout = frame(
            context.pool,
            context.token,
            "CALL",
            &transferCall { _to: swap::router(), _value: U256::from(1) }.abi_encode(),
        );
        payout.error = payout_error.map(str::to_string);
        payout.output = Some(payout_output.into());
        let mut swap_frame = frame(swap::router(), context.pool, "CALL", &[0; 4]);
        swap_frame.error = Some("execution reverted".to_string());
        swap_frame.calls = vec![payout];
        let mut buy_tx = frame(swap::buyer(), swap::router(), "CALL", &[0; 4]);
        buy_tx.calls = vec![swap_frame];
        buy_tx
    }

    #[test]
    fn a_token_refusing_the_payout_is_told_apart_from_a_pair_refusing_the_swap() {
        let context = context();
        let reason = alloy::sol_types::Revert { reason: "Trading not open".to_string() };
        let reverted = swap_buy(
            &context,
            Some("execution reverted"),
            alloy::sol_types::SolError::abi_encode(&reason),
        );
        assert_eq!(refused_payout(&reverted, &context).as_deref(), Some("Trading not open"));
        let returned_false = swap_buy(&context, None, vec![0; 32]);
        assert_eq!(
            refused_payout(&returned_false, &context).as_deref(),
            Some("transfer returned false")
        );
        let paid = swap_buy(
            &context,
            None,
            U256::from(1)
                .to_be_bytes::<32>()
                .to_vec(),
        );
        assert_eq!(refused_payout(&paid, &context), None);
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
