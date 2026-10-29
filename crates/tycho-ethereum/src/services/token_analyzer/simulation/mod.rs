//! `TokenAnalyzer` that simulates buying, forwarding and selling a token against the pool it
//! was found in.
//!
//! One analysis takes a fixed number of network round trips, each running its calls
//! concurrently:
//!
//! 1. the block header;
//! 2. the base scenarios with `eth_simulateV1`, and the buy trace; meanwhile the code (and a
//!    proxy's implementation) and `owner()` are fetched and evmole recovers the bytecode's
//!    functions and storage on a blocking thread;
//! 3. the transfer and sell trace, on the state the buy left;
//! 4. only when needed: owner calls that may lift a gate (the base legs failed and not just the
//!    sell) or change the fee (the token charges one).

mod code;
mod owner_calls;
mod runner;
mod scenario;
mod swap;
mod trace;
mod verdict;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use alloy::{
    primitives::{Address, U256},
    rpc::types::{BlockId, BlockNumberOrTag},
};
use futures::{
    future::join_all,
    stream::{self, StreamExt},
};
use tracing::{debug, warn};
use tycho_common::{
    models::{
        blockchain::BlockTag,
        token::{TokenAnalysis, TokenVerdict},
        Chain,
    },
    traits::{TokenAnalyzer, TokenOwnerFinding},
    Bytes,
};

use self::{
    owner_calls::Probe,
    scenario::{BuySize, Environment, LegPath, LegTiming, Outcome, Scenario, TokenContext},
    trace::TracedLegs,
    verdict::Evidence,
};
use super::map_block_tag;
use crate::{rpc::EthereumRpcClient, BytesCodec};

/// Identifies this analyzer and its version in stored analyses.
pub const ANALYZER_NAME: &str = "simulation/1";

/// Smallest pool balance, in the token's raw units, worth simulating: the small buy is 0.5% of
/// it and must be large enough to resolve fees to a basis point.
const MIN_POOL_BALANCE: u64 = 2_000_000;
/// Owner-call scenarios of one token simulated at the same time; most tokens that need probes
/// have fewer, so they run in one round trip.
const PROBE_CONCURRENCY: usize = 24;

/// `TokenAnalyzer` that simulates the legs a router relies on: the pool paying out a buy, the
/// output forwarded to another address, and a sell back to the pool, all in one block.
///
/// Requires `eth_simulateV1` and `debug_traceCall` on the RPC.
pub struct SimulationDetector {
    rpc: EthereumRpcClient,
    finder: Arc<dyn TokenOwnerFinding>,
    watchlist: Arc<HashSet<Address>>,
    /// The token real swaps are made against; `None` on chains without one.
    wrapped_native: Option<Address>,
}

impl SimulationDetector {
    /// `watchlist` holds contracts a token must never call during a transfer: a call to any of
    /// them makes the verdict `NoQuote`. Real swaps are made against `chain`'s wrapped native
    /// token; on chains without one, every pool gets the direct legs.
    pub fn new(
        rpc: &EthereumRpcClient,
        finder: Arc<dyn TokenOwnerFinding>,
        watchlist: Arc<HashSet<Address>>,
        chain: Chain,
    ) -> Self {
        let wrapped_native = chain
            .wrapped_native_token()
            .map(|token| Address::from_bytes(&token.address))
            .filter(|address| !address.is_zero());
        Self { rpc: rpc.clone(), finder, watchlist, wrapped_native }
    }

    async fn analyze_at(&self, token: Address, block: BlockTag) -> Result<TokenAnalysis, String> {
        let block_tag = map_block_tag(block);
        let min = Bytes::from(U256::from(MIN_POOL_BALANCE).to_be_bytes_vec());
        let Some((pool, balance)) = self
            .finder
            .find_owner(token.to_bytes(), min)
            .await?
        else {
            let reason = "no pool holds enough of the token to simulate a trade".to_string();
            return Ok(TokenAnalysis::verdict_only(
                TokenVerdict::NoLiquidity,
                Some(reason),
                None,
                ANALYZER_NAME,
            ));
        };
        let pool = Address::from_bytes(&pool);
        let balance = U256::from_be_slice(balance.as_ref());

        let (header, pair) = tokio::join!(
            self.rpc
                .eth_get_block_by_number(BlockId::Number(block_tag)),
            self.detect_pair(pool, token, block_tag),
        );
        let header = header.map_err(|e| format!("block {block_tag}: {e}"))?;
        let pair = pair?;
        let block_number = header.header.number;
        if balance < U256::from(MIN_POOL_BALANCE) {
            let reason = format!("pool {pool:#x} holds only {balance} of the token");
            return Ok(TokenAnalysis::verdict_only(
                TokenVerdict::NoLiquidity,
                Some(reason),
                Some(block_number),
                ANALYZER_NAME,
            ));
        }
        let mut context = TokenContext::new(
            &self.rpc,
            token,
            pool,
            balance,
            (block_number, header.header.timestamp),
        );
        context.v2 = pair;
        context.base_fee = header.header.base_fee_per_gas;

        // Only the owner-call probes need the code and the owner; fetch and analyse them while
        // the base scenarios and traces run.
        let block = BlockNumberOrTag::Number(block_number);
        let (code, owner, observed) = tokio::join!(
            fetch_and_summarize_code(&self.rpc, token, block),
            code::read_owner(&self.rpc, token, block),
            simulate_and_trace_legs(&mut context),
        );
        let (implementation, code_summary) = code?;
        let owner = owner?;
        let (scenarios, traces) = observed?;
        let base = scenarios
            .get("base")
            .ok_or("scenario base: no outcome")?;

        let mut probes =
            select_owner_call_probes(&context, &code_summary, owner, &traces, &scenarios, base);
        if context.v2.is_some() {
            // Swaps in the bare environment, comparable with `base_bare`: the owner may be a
            // contract, which a validating simulation rejects as a sender.
            for probe in &mut probes {
                probe.scenario.path = LegPath::Swap;
            }
        }
        let runs: Vec<_> = probes
            .iter()
            .map(|probe| context.run(&probe.scenario))
            .collect();
        let outcomes: Vec<Result<Outcome, String>> = stream::iter(runs)
            .buffered(PROBE_CONCURRENCY)
            .collect()
            .await;
        let mut probed = Vec::with_capacity(probes.len());
        for (probe, outcome) in probes.into_iter().zip(outcomes) {
            match outcome {
                Ok(outcome) => probed.push((probe, outcome)),
                Err(error) => warn!(?token, scenario = probe.scenario.name, error, "Probe failed"),
            }
        }

        Ok(verdict::classify(&Evidence {
            block: block_number,
            pool,
            swapped: context.v2.is_some(),
            implementation,
            owner,
            code: &code_summary,
            traces: &traces,
            base,
            scenarios: &scenarios,
            probes: &probed,
            watchlist: &self.watchlist,
        }))
    }
}

impl SimulationDetector {
    async fn detect_pair(
        &self,
        pool: Address,
        token: Address,
        block: BlockNumberOrTag,
    ) -> Result<Option<scenario::V2Pair>, String> {
        let Some(wrapped_native) = self.wrapped_native else { return Ok(None) };
        swap::detect_pair(&self.rpc, pool, token, wrapped_native, block).await
    }
}

#[async_trait::async_trait]
impl TokenAnalyzer for SimulationDetector {
    type Error = String;

    async fn analyze(&self, token: Bytes, block: BlockTag) -> Result<TokenAnalysis, String> {
        let analysis = self
            .analyze_at(Address::from_bytes(&token), block)
            .await?;
        debug!(?token, verdict = ?analysis.verdict, reason = ?analysis.reason, "Analysed token");
        Ok(analysis)
    }
}

/// The summary of the token's bytecode and, when it is a proxy, its implementation.
async fn fetch_and_summarize_code(
    rpc: &EthereumRpcClient,
    token: Address,
    block: BlockNumberOrTag,
) -> Result<(Option<Address>, code::CodeSummary), String> {
    let code = code::fetch_code(rpc, token, block).await?;
    let summary = tokio::task::spawn_blocking(move || code::summarize_bytecode(&code.bytecode))
        .await
        .map_err(|e| format!("bytecode analysis of {token:#x} panicked: {e}"))?;
    Ok((code.implementation, summary))
}

/// Fails unless `rpc` answers `eth_simulateV1` and `debug_traceCall`, both of which the
/// analysis relies on.
pub(crate) async fn ensure_rpc_supports_simulation(rpc: &EthereumRpcClient) -> Result<(), String> {
    let probe = scenario::call(Address::ZERO, Address::ZERO, Vec::new());
    let payload = alloy::rpc::types::simulate::SimulatePayload::default().extend(
        alloy::rpc::types::simulate::SimBlock {
            block_overrides: None,
            state_overrides: None,
            calls: vec![probe.clone()],
        },
    );
    let latest = alloy::rpc::types::BlockNumberOrTag::Latest;
    rpc.eth_simulate_v1::<serde_json::Value>(&payload, latest)
        .await
        .map_err(|e| format!("eth_simulateV1: {e}"))?;
    let options = alloy::rpc::types::trace::geth::GethDebugTracingCallOptions::default();
    rpc.debug_trace_call(&probe, latest, &options)
        .await
        .map_err(|e| format!("debug_traceCall: {e}"))?;
    Ok(())
}

/// The base scenarios and the traces of the legs. When the pool rejects the swap buy, which
/// a token refusing the payout and a pair charging a fee the swap does not expect both cause,
/// they run again with the direct legs, which leave the pool's swap out.
async fn simulate_and_trace_legs(
    context: &mut TokenContext,
) -> Result<(HashMap<String, Outcome>, TracedLegs), String> {
    let (scenarios, traces) = tokio::join!(run_base_scenarios(context), trace::trace_legs(context));
    let scenarios = scenarios?;
    let rejected = scenarios
        .get("base")
        .is_some_and(|base| base.pool_rejected_buy);
    if !rejected {
        return Ok((scenarios, traces?));
    }
    debug!(token = ?context.token, pool = ?context.pool, "Pool rejected the swap; using the direct legs");
    context.v2 = None;
    let (scenarios, traces) = tokio::join!(run_base_scenarios(context), trace::trace_legs(context));
    Ok((scenarios?, traces?))
}

/// The scenarios every token runs. Fails when `base` or `base_next_block`, which the verdict
/// rests on, could not be simulated; any other failed scenario only leaves what it checks
/// (size limits, simulation detection, time dependence) unchecked.
///
/// They trade through the pool as real swaps from funded wallets when it is a Uniswap V2 style
/// pair against the wrapped native token; `base_bare` repeats `base` in the `Bare` environment to
/// catch tokens that behave differently when they look simulated. Other pools get the direct legs.
async fn run_base_scenarios(context: &TokenContext) -> Result<HashMap<String, Outcome>, String> {
    let scenario = |name: &str, timing| match context.v2 {
        Some(_) => Scenario::swap(name, timing),
        None => Scenario::new(name, vec![], timing),
    };
    let mut large = scenario("base_large", LegTiming::SameBlock);
    large.size = BuySize::Large;
    let mut scenarios = vec![
        scenario("base", LegTiming::SameBlock),
        scenario("base_next_block", LegTiming::LegsNextBlock),
        large,
    ];
    if context.v2.is_some() {
        let mut bare = scenario("base_bare", LegTiming::SameBlock);
        bare.environment = Environment::Bare;
        scenarios.push(bare);
    }
    for days in [1, 30] {
        let mut later = scenario(&format!("time+{days}d"), LegTiming::SameBlock);
        later.time_shift_days = days;
        scenarios.push(later);
    }
    let outcomes = join_all(
        scenarios
            .iter()
            .map(|scenario| context.run(scenario)),
    )
    .await;
    let mut base = HashMap::new();
    for (scenario, outcome) in scenarios.iter().zip(outcomes) {
        match outcome {
            Ok(outcome) => {
                base.insert(scenario.name.clone(), outcome);
            }
            Err(error) if ["base", "base_next_block"].contains(&scenario.name.as_str()) => {
                return Err(format!("scenario {}: {error}", scenario.name));
            }
            Err(error) => {
                warn!(token = ?context.token, scenario = scenario.name, error, "Scenario failed");
            }
        }
    }
    Ok(base)
}

/// Owner-call scenarios worth running: unlocks when the legs fail for a reason other than the
/// sell alone, fee changes when the token charges a fee. None without a live owner.
fn select_owner_call_probes(
    context: &TokenContext,
    code: &code::CodeSummary,
    owner: Option<Address>,
    traces: &TracedLegs,
    scenarios: &HashMap<String, Outcome>,
    outcome: &Outcome,
) -> Vec<Probe> {
    let Some(owner) = owner.filter(|o| !o.is_zero()) else { return Vec::new() };
    let unlocks = owner_calls::candidate_unlocks(
        code,
        context.token,
        context.pool,
        owner,
        &traces.read_only_slots,
    );
    if outcome.all_legs_ok() {
        if outcome.fees().max_tax() > 0 {
            return owner_calls::fee_probes(&unlocks);
        }
        return Vec::new();
    }
    let sell_only = outcome.only_sell_failed() ||
        scenarios
            .get("base_next_block")
            .is_some_and(Outcome::only_sell_failed);
    if sell_only {
        return Vec::new();
    }
    owner_calls::unlock_probes(&unlocks, context.pool)
}
