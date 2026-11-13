//! Simulates buying, forwarding and selling a token against the pool it was found in.
//!
//! One analysis takes a fixed number of network round trips, each running its calls
//! concurrently:
//!
//! 1. the block header;
//! 2. the base scenarios with `eth_simulateV1`, and the buy trace; meanwhile the code (and a
//!    proxy's implementation) and `owner()` are fetched, evmole recovers the bytecode's functions
//!    and storage on a blocking thread, and the `address` fields the functions read are fetched;
//! 3. the transfer and sell trace, on the state the buy left;
//! 4. calls by a privileged caller that may take tokens out of the pool; with them, only when
//!    needed, owner calls that may lift a gate (the base legs failed and not just the sell) or
//!    change the fee (the token charges one), or new values in the fields the transfer reads that
//!    may start a fee (the token charges none).

mod code;
mod fee_fields;
mod owner_calls;
mod pool_funding;
mod pool_seizure;
mod runner;
mod scenario;
mod swap;
mod trace;
mod verdict;

use std::{collections::HashMap, sync::Arc};

use alloy::{
    primitives::{Address, B256, U256},
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
    traits::TokenOwnerFinding,
    Bytes,
};

use self::{
    code::StoredAddresses,
    owner_calls::Probe,
    scenario::{BuySize, Outcome, Scenario, TokenContext, V2Pair},
    trace::TracedLegs,
    verdict::Evidence,
};
use super::map_block_tag;
use crate::{rpc::EthereumRpcClient, BytesCodec};

/// Identifies this analyzer and its version in stored analyses.
pub const ANALYZER_NAME: &str = "simulation/1";

/// Smallest pool balance, in the token's raw units, worth simulating: the small buy is 0.5% of
/// it and must be large enough to resolve fees to a basis point. A pool holding less gets a
/// balance written for the analysis.
const MIN_POOL_BALANCE: u64 = 2_000_000;

/// Smallest wrapped native reserve, in wei, of a pair worth swapping against: the small buy and
/// the sell's output are about 0.5% of it and must not round to nothing. A pair holding less,
/// e.g. 1 wei after its liquidity was removed, is funded like a pool holding too little of the
/// token: its token reserve is as worthless, and a token selling its fees into it during the
/// sell leaves the sell nothing.
const MIN_NATIVE_RESERVE: u64 = 2_000_000;

/// `TokenAnalyzer` that simulates the legs a router relies on: the pool paying out a buy, the
/// output forwarded to another address, and a sell back to the pool in the next block.
///
/// Requires `eth_simulateV1` and `debug_traceCall` on the RPC.
pub struct SimulationDetector {
    rpc: EthereumRpcClient,
    finder: Arc<dyn TokenOwnerFinding>,
    /// The token real swaps are made against; `None` on chains without one.
    wrapped_native: Option<Address>,
    /// Seconds between two blocks of the chain.
    block_secs: u64,
}

impl SimulationDetector {
    /// Real swaps are made against `chain`'s wrapped native token; on chains without one, every
    /// pool gets the direct legs. Each simulated block moves the time by `chain`'s block time.
    pub fn new(rpc: &EthereumRpcClient, finder: Arc<dyn TokenOwnerFinding>, chain: Chain) -> Self {
        let wrapped_native = chain
            .wrapped_native_token()
            .map(|token| Address::from_bytes(&token.address))
            .filter(|address| !address.is_zero());
        Self { rpc: rpc.clone(), finder, wrapped_native, block_secs: chain.block_time_secs() }
    }

    async fn analyze_at(&self, token: Address, block: BlockTag) -> Result<TokenAnalysis, String> {
        let block_tag = map_block_tag(block);
        let any_balance = Bytes::from(vec![0u8]);
        let found = self
            .finder
            .find_owner(token.to_bytes(), any_balance)
            .await?;
        let Some((pool, balance)) = found.filter(|(pool, _)| pool.len() == Address::len_bytes())
        else {
            let reason = "no pool with an address holds the token".to_string();
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
        let block_number = header.header.number;
        let mut context = TokenContext::new(
            &self.rpc,
            token,
            pool,
            balance,
            (block_number, header.header.timestamp, self.block_secs),
        );
        context.v2 = pair?;
        context.base_fee = header.header.base_fee_per_gas;
        let block = BlockNumberOrTag::Number(block_number);

        let (funded, prefetched) = match fund_pool_if_low(&mut context, block).await? {
            Funding::NotNeeded => (false, None),
            Funding::Unfunded(code) => (false, Some(code)),
            Funding::Funded(code) => (true, Some(code)),
            Funding::Impossible => {
                let reason = format!(
                    "pool {pool:#x} holds only {balance} of the token, and the analysis could \
                     not give it a balance"
                );
                return Ok(TokenAnalysis::verdict_only(
                    TokenVerdict::NoLiquidity,
                    Some(reason),
                    Some(block_number),
                    ANALYZER_NAME,
                ));
            }
        };

        // Only the probes need the code, the owner and the callers; find them while the base
        // scenarios and traces run.
        let code_and_callers = async {
            let code = async {
                match prefetched {
                    Some(code) => Ok(code),
                    None => fetch_code_and_stored_addresses(&self.rpc, token, block).await,
                }
            };
            let (code, owner) = tokio::join!(code, code::read_owner(&self.rpc, token, block));
            let ((implementation, summary, stored), owner) = (code?, owner?);
            let mut callers = owner_calls::find_callers(&summary, owner, &stored, &[token, pool]);
            owner_calls::settle_callers(&self.rpc, token, block, &summary, &mut callers).await?;
            Ok::<_, String>((implementation, summary, owner, callers))
        };
        let (code, observed) =
            tokio::join!(code_and_callers, simulate_and_trace_legs(&mut context));
        let (implementation, code_summary, owner, callers) = code?;
        let (scenarios, traces) = observed?;
        let base = scenarios
            .get("base")
            .ok_or("scenario base: no outcome")?;

        for (slot, value) in &callers.grants {
            context.set_storage(token, *slot, *value);
        }
        let probed = run_probes(&context, &code_summary, &callers, &traces, base).await?;

        Ok(verdict::classify(&Evidence {
            block: block_number,
            pool,
            swapped: context.v2.is_some(),
            funded,
            implementation,
            owner,
            code: &code_summary,
            traces: &traces,
            base,
            scenarios: &scenarios,
            probes: &probed.owner_calls,
            seizures: &probed.seizures,
            fee_starts: &probed.fee_starts,
        }))
    }
}

/// Whether the pool had enough of the token and a pair enough of the wrapped native token, or
/// got a balance for the analysis.
enum Funding {
    NotNeeded,
    /// A pair held too little of the wrapped native token and could not be funded; the legs are
    /// direct. Carries the code, which funding needed first.
    Unfunded(CodeAndAddresses),
    /// The pool's balance, and a pair's reserves, are written into `TokenContext::storage`.
    /// Carries the code, which funding needed first.
    Funded(CodeAndAddresses),
    /// The pool holds too little and its balance could not be written safely.
    Impossible,
}

/// Gives the pool a balance when it holds less than `MIN_POOL_BALANCE`, or when it is a Uniswap
/// V2 pair holding less than `MIN_NATIVE_RESERVE`. A pair also gets wrapped native token and
/// matching reserves, so that the legs still trade as swaps; when that fails, or for other pools,
/// the legs are direct: a pool's reserves would not match the written balance, and a swap would
/// price it wrongly. A pair that holds enough of the token but cannot be funded gets the direct
/// legs.
async fn fund_pool_if_low(
    context: &mut TokenContext,
    block: BlockNumberOrTag,
) -> Result<Funding, String> {
    let min = U256::from(MIN_POOL_BALANCE);
    let holds_enough = context.pool_balance >= min;
    let pair_without_native = context
        .v2
        .is_some_and(|pair| pair.reserve < U256::from(MIN_NATIVE_RESERVE));
    if holds_enough && !pair_without_native {
        return Ok(Funding::NotNeeded);
    }
    let (token, pool) = (context.token, context.pool);
    let code = fetch_code_and_stored_addresses(&context.rpc, token, block).await?;
    let Some(funded) =
        pool_funding::fund_pool(&context.rpc, token, pool, block, &code.1, min).await?
    else {
        if holds_enough {
            context.v2 = None;
            return Ok(Funding::Unfunded(code));
        }
        return Ok(Funding::Impossible);
    };
    debug!(?token, ?pool, balance = %funded.balance, "Funded the pool");
    context.set_storage(token, funded.slot, B256::from(funded.stored));
    context.pool_balance = funded.balance;
    let pair_storage = match context.v2 {
        Some(pair) => {
            let at = (block, context.block_time);
            pool_funding::fund_pair(&context.rpc, (token, pool), pair, funded, at).await?
        }
        None => None,
    };
    match (pair_storage, context.v2) {
        (Some(storage), Some(pair)) => {
            for (address, slot, value) in storage {
                context.set_storage(address, slot, value);
            }
            context.v2 = Some(V2Pair { reserve: pool_funding::funded_native_reserve(), ..pair });
        }
        (Some(_), None) | (None, _) => context.v2 = None,
    }
    Ok(Funding::Funded(code))
}

/// What the probes after the base scenarios found.
struct Probed {
    /// Owner-call scenarios with the probe each ran.
    owner_calls: Vec<(Probe, Outcome)>,
    /// Functions whose privileged caller lowered the pool's balance of the token.
    seizures: Vec<alloy::primitives::Bytes>,
    /// Field writes that started a fee in a token charging none.
    fee_starts: Vec<fee_fields::FeeStart>,
}

/// Runs the owner-call probes, the seizure calls and the field probes at the same time. A probe
/// that could not be simulated is left out; a seizure call that could not be simulated fails the
/// analysis.
async fn run_probes(
    context: &TokenContext,
    code: &code::CodeSummary,
    callers: &owner_calls::Callers,
    traces: &TracedLegs,
    base: &Outcome,
) -> Result<Probed, String> {
    let probes = select_owner_call_probes(context, code, callers, traces, base);
    let seizure_calls = pool_seizure::candidate_seizures(code, context, callers);
    let field_probes = select_field_probes(context, code, callers, traces, base);
    let runs: Vec<_> = probes
        .iter()
        .map(|probe| context.run(&probe.scenario))
        .collect();
    let (outcomes, seizures, fee_starts): (Vec<Result<Outcome, String>>, _, _) = tokio::join!(
        stream::iter(runs)
            .buffered(owner_calls::PRIVILEGED_CALL_CONCURRENCY)
            .collect(),
        pool_seizure::find_seizures(context, &seizure_calls),
        fee_fields::find_fee_starts(context, field_probes, &traces.read_only_slots, base),
    );
    let mut probed = Vec::with_capacity(probes.len());
    for (probe, outcome) in probes.into_iter().zip(outcomes) {
        match outcome {
            Ok(outcome) => probed.push((probe, outcome)),
            Err(error) => {
                warn!(token = ?context.token, scenario = probe.scenario.name, error, "Probe failed")
            }
        }
    }
    Ok(Probed { owner_calls: probed, seizures: seizures?, fee_starts })
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

/// Fails unless `rpc` answers `eth_simulateV1` and `debug_traceCall`, both of which
/// `SimulationDetector` relies on.
pub async fn ensure_rpc_supports_simulation(rpc: &EthereumRpcClient) -> Result<(), String> {
    let probe = scenario::call(Address::ZERO, Address::ZERO, Vec::new());
    let payload = alloy::rpc::types::simulate::SimulatePayload::default().extend(
        alloy::rpc::types::simulate::SimBlock {
            // The analysis overrides prevrandao, which some clients name differently.
            block_overrides: Some(alloy::rpc::types::BlockOverrides {
                random: Some(B256::ZERO),
                ..Default::default()
            }),
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

/// The implementation when the token is a proxy, the code summary, and the values of the
/// `address` fields its state-changing functions read.
type CodeAndAddresses = (Option<Address>, code::CodeSummary, StoredAddresses);

async fn fetch_code_and_stored_addresses(
    rpc: &EthereumRpcClient,
    token: Address,
    block: BlockNumberOrTag,
) -> Result<CodeAndAddresses, String> {
    let (implementation, summary) = fetch_and_summarize_code(rpc, token, block).await?;
    let fields = owner_calls::address_fields_read_by_setters(&summary);
    let stored = code::read_address_fields(rpc, token, block, &fields).await?;
    Ok((implementation, summary, stored))
}

/// The base scenarios and the traces of the legs. When the pool rejects the swap buy, the buy's
/// trace tells why: a token refusing the payout keeps the swap legs and gives `base` its reason;
/// a pair refusing the trade itself, e.g. over a fee the swap does not expect, makes the
/// scenarios run again with the direct legs, which leave the pool's swap out.
async fn simulate_and_trace_legs(
    context: &mut TokenContext,
) -> Result<(HashMap<String, Outcome>, TracedLegs), String> {
    let (scenarios, traces) = tokio::join!(run_base_scenarios(context), trace::trace_legs(context));
    let (mut scenarios, traces) = (scenarios?, traces?);
    let Some(base) = scenarios
        .get_mut("base")
        .filter(|base| base.pool_rejected_buy)
    else {
        return Ok((scenarios, traces));
    };
    if let Some(reason) = &traces.refused_payout {
        base.buy_revert = Some(reason.clone());
        return Ok((scenarios, traces));
    }
    debug!(token = ?context.token, pool = ?context.pool, "Pair rejected the swap; using the direct legs");
    context.v2 = None;
    let (scenarios, traces) = tokio::join!(run_base_scenarios(context), trace::trace_legs(context));
    Ok((scenarios?, traces?))
}

/// The scenarios every token runs. Fails when any of them could not be simulated: a missing
/// `base_large` or `time+30d` would leave a size limit or a fee that changes with time
/// unchecked, and the token could pass as `Ok`.
///
/// They trade through the pool as real swaps from funded wallets when it is a Uniswap V2 style
/// pair against the wrapped native token. Other pools get the direct legs.
async fn run_base_scenarios(context: &TokenContext) -> Result<HashMap<String, Outcome>, String> {
    let mut large = Scenario::realistic("base_large");
    large.size = BuySize::Large;
    let mut later = Scenario::realistic("time+30d");
    later.time_shift_days = 30;
    let scenarios = [Scenario::realistic("base"), large, later];
    let outcomes = join_all(
        scenarios
            .iter()
            .map(|scenario| context.run(scenario)),
    )
    .await;
    let mut base = HashMap::new();
    for (scenario, outcome) in scenarios.into_iter().zip(outcomes) {
        let outcome = outcome.map_err(|error| format!("scenario {}: {error}", scenario.name))?;
        base.insert(scenario.name, outcome);
    }
    Ok(base)
}

/// Owner-call scenarios worth running: unlocks when the legs fail for a reason other than the
/// sell alone, fee changes when the token charges a fee. Only functions with a caller run.
fn select_owner_call_probes(
    context: &TokenContext,
    code: &code::CodeSummary,
    callers: &owner_calls::Callers,
    traces: &TracedLegs,
    outcome: &Outcome,
) -> Vec<Probe> {
    let unlocks = owner_calls::candidate_unlocks(
        code,
        context.token,
        context.pool,
        callers,
        &traces.read_only_slots,
    );
    if outcome.all_legs_ok() {
        if outcome.fees().max_tax() > 0 {
            return owner_calls::fee_probes(&unlocks);
        }
        return Vec::new();
    }
    if outcome.only_sell_failed() {
        return Vec::new();
    }
    owner_calls::unlock_probes(&unlocks, context.pool)
}

/// Field writes worth running: only when the token would pass as `Ok`, with every leg exact and
/// no call to another contract. A token that charges a fee or calls out is a fee token already.
fn select_field_probes(
    context: &TokenContext,
    code: &code::CodeSummary,
    callers: &owner_calls::Callers,
    traces: &TracedLegs,
    base: &Outcome,
) -> Vec<fee_fields::FieldProbe> {
    let charges_nothing =
        base.all_legs_ok() && base.delivered_exact_amounts() && traces.calls.is_empty();
    if !charges_nothing {
        return Vec::new();
    }
    let fields = fee_fields::transfer_fields(
        code,
        context,
        callers,
        &traces.read_only_slots,
        &traces.forward_targets,
    );
    let probes = fee_fields::field_probes(&fields, &traces.read_only_slots);
    debug!(
        token = ?context.token,
        read_only = traces.read_only_slots.len(),
        fields = fields.len(),
        probes = probes.len(),
        "Selected field probes"
    );
    probes
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;
    use tycho_common::models::token::TokenOwnerStore;

    use super::*;

    fn detector(chain: Chain) -> SimulationDetector {
        let rpc = EthereumRpcClient::new("http://localhost:1").expect("client");
        let finder = Arc::new(TokenOwnerStore::new(HashMap::new()));
        SimulationDetector::new(&rpc, finder, chain)
    }

    #[test]
    fn swaps_against_the_chains_wrapped_native_token() {
        assert_eq!(
            detector(Chain::Ethereum).wrapped_native,
            Some(address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"))
        );
        assert_eq!(
            detector(Chain::Base).wrapped_native,
            Some(address!("4200000000000000000000000000000000000006"))
        );
        assert_eq!(
            detector(Chain::Bsc).wrapped_native,
            Some(address!("bb4cdb9cbd36b01bd1cbaebf2de08d9173bc095c"))
        );
        assert_eq!(detector(Chain::Starknet).wrapped_native, None);
    }

    #[tokio::test]
    async fn a_pool_id_that_is_not_an_address_gives_no_liquidity() {
        let token = Address::repeat_byte(7);
        let pool_id = Bytes::from([1u8; 32]);
        let owners = HashMap::from([(token.to_bytes(), (pool_id, Bytes::from(vec![1u8])))]);
        let rpc = EthereumRpcClient::new("http://localhost:1").expect("client");
        let finder = Arc::new(TokenOwnerStore::new(owners));
        let detector = SimulationDetector::new(&rpc, finder, Chain::Ethereum);

        let analysis = detector
            .analyze_at(token, BlockTag::Latest)
            .await
            .expect("analysis");

        assert_eq!(analysis.verdict, TokenVerdict::NoLiquidity);
    }
}
