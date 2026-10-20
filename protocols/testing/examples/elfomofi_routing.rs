//! Select and simulate an ElFomoFi route at a pinned Base block.
use std::{collections::HashMap, env, path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{keccak256, Address, U256},
    providers::Provider,
    rpc::types::{Block, BlockId, TransactionRequest},
    sol,
    sol_types::{SolCall, SolValue},
};
use clap::Parser;
use futures::StreamExt;
use miette::{miette, IntoDiagnostic, WrapErr};
use num_bigint::BigUint;
use tokio::time::timeout;
use tycho_execution::encoding::{
    evm::{
        encoder_builders::TychoRouterEncoderBuilder, get_router_address,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
    },
    models::{ClientFeeParams, EncodedSolution, Solution, Swap},
};
use tycho_simulation::{
    book::{BookFeedEvent, BookFeedStream},
    evm::protocol::uniswap_v2::state::UniswapV2State,
    pamm::protocols::elfomofi::{
        models::{BASE_ROUTER, PARTNER_ID},
        state::ElfomofiState,
        ElfomofiBlock, ElfomofiFeedBuilder, ElfomofiOptions,
    },
    protocol::models::ProtocolComponent,
    tycho_common::{
        models::{protocol::GetAmountOutParams, token::Token, Chain},
        simulation::protocol_sim::ProtocolSim,
        Bytes,
    },
};
use tycho_test::{
    execution::{
        encoding::EXECUTOR_ADDRESS,
        models::{Transaction, TychoExecutionInput, TychoExecutionResult},
        simulate_swap_transaction,
    },
    RPCTools,
};

#[path = "../src/execution.rs"]
mod execution;

const USER: &str = "0xf847a638E44186F3287ee9F8cAF73FF4d4B80784";
const V2_EXECUTOR: &str = "0xaE04CA7E9Ed79cBD988f6c536CE11C621166f41C";
const UNISWAP_V2_FACTORY: &str = "0x8909dc15e40173ff4699343b6eb8132c65e18ec6";
const WAIT: Duration = Duration::from_secs(90);

sol! {
    function getPair(address tokenA, address tokenB) external view returns (address pair);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function factory() external view returns (address);
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 timestamp);
    function balanceOf(address owner) external view returns (uint256);
    function swapWithContractBalance(address from, address to, uint256 minimum, address receiver, uint256 partnerId) external;
}

#[derive(Parser)]
#[command(about = "Select and simulate an ElFomoFi direct route")]
struct Cli {
    #[arg(long, default_value = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913")]
    token_in: Address,
    #[arg(long, default_value = "0x4200000000000000000000000000000000000006")]
    token_out: Address,
    /// Raw input amount; default is 100 USDC.
    #[arg(long, default_value = "100000000")]
    amount_in: BigUint,
    /// Base Uniswap V2 factory (30 bps fee).
    #[arg(long, default_value = UNISWAP_V2_FACTORY)]
    competitor_factory: Address,
    /// Require this competitor pool.
    #[arg(long)]
    competitor_pool: Option<Address>,
    /// Allow a remote provider serving a pinned block.
    #[arg(long)]
    allow_remote: bool,
    /// HTTP polling interval in milliseconds.
    #[arg(long)]
    poll_ms: Option<u64>,
    /// Save a JSON report.
    #[arg(long)]
    report: Option<PathBuf>,
}

struct Candidate {
    component: tycho_simulation::tycho_common::models::protocol::ProtocolComponent,
    state: Arc<dyn ProtocolSim>,
    output: BigUint,
    gas: BigUint,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    dotenv::dotenv().ok();
    let cli = Cli::parse();
    let rpc_url = env::var("RPC_URL")
        .into_diagnostic()
        .wrap_err("Set RPC_URL to the frozen fork")?;
    if !cli.allow_remote && !is_local_url(&rpc_url) {
        return Err(miette!(
            "RPC_URL must be a localhost fork; --allow-remote still requires an unchanged block"
        ));
    }
    if cli.token_in == cli.token_out || cli.amount_in == BigUint::ZERO || cli.amount_in.bits() > 256
    {
        return Err(miette!("Provide distinct token addresses and a positive uint256 amount"));
    }
    if cli.competitor_factory != Address::from_str(UNISWAP_V2_FACTORY).into_diagnostic()? {
        return Err(miette!("Use the official Base Uniswap V2 factory; this native comparator and executor assume its 30 bps fee"));
    }
    let rpc = timeout(WAIT, RPCTools::new(&rpc_url, &Chain::Base))
        .await
        .into_diagnostic()
        .wrap_err("RPC connection timed out")??;
    let feed = ElfomofiFeedBuilder::new(Chain::Base)
        .options(ElfomofiOptions {
            poll_time: cli.poll_ms.map(Duration::from_millis),
            max_age: Duration::from_secs(300),
            quote_timeout: Duration::from_secs(60),
            ..Default::default()
        })
        .build()
        .into_diagnostic()?;
    let mut books = BookFeedStream::<ElfomofiBlock>::spawn("elfomofi", feed);
    timeout(Duration::from_secs(180), run(&cli, &rpc, &mut books))
        .await
        .into_diagnostic()
        .wrap_err("Routing simulation timed out")?
}

fn is_local_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else { return false };
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    let authority = rest
        .split('/')
        .next()
        .unwrap_or_default();
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or_default();
    host == "localhost" ||
        host.starts_with("localhost:") ||
        host == "127.0.0.1" ||
        host.starts_with("127.0.0.1:") ||
        host == "[::1]" ||
        host.starts_with("[::1]:")
}

async fn current_block(rpc: &RPCTools) -> miette::Result<Block> {
    timeout(
        WAIT,
        rpc.provider
            .get_block_by_number(BlockNumberOrTag::Latest),
    )
    .await
    .into_diagnostic()
    .wrap_err("Block read timed out")?
    .into_diagnostic()?
    .ok_or_else(|| miette!("Latest block unavailable"))
}

async fn ensure_same_block(rpc: &RPCTools, expected: &Block) -> miette::Result<()> {
    let actual = current_block(rpc).await?;
    if actual.header.number != expected.header.number ||
        actual.header.hash != expected.header.hash ||
        actual.header.timestamp != expected.header.timestamp
    {
        return Err(miette!("Block changed during the test; use Anvil --no-mining and rerun"));
    }
    Ok(())
}

async fn call<C: SolCall>(
    rpc: &RPCTools,
    target: Address,
    block: &Block,
    call: C,
) -> miette::Result<C::Return> {
    let request = TransactionRequest::default()
        .to(target)
        .input(call.abi_encode().into());
    let raw = timeout(
        WAIT,
        rpc.provider
            .call(request)
            .block(BlockId::from(block.header.number)),
    )
    .await
    .into_diagnostic()
    .wrap_err("Contract read timed out")?
    .into_diagnostic()?;
    C::abi_decode_returns_validate(&raw).into_diagnostic()
}

async fn elfomofi_book(
    cli: &Cli,
    books: &mut BookFeedStream<ElfomofiBlock>,
) -> miette::Result<(ProtocolComponent, Arc<dyn ProtocolSim>, Token, Token)> {
    timeout(WAIT, async {
        while let Some(event) = books.next().await {
            let snapshot = match event {
                BookFeedEvent::Published(snapshot) => snapshot,
                BookFeedEvent::Withdrawn => continue,
                BookFeedEvent::Ended(outcome) => {
                    return Err(miette!("ElFomoFi book feed ended: {outcome:?}"));
                }
            };
            for book in snapshot.books.values() {
                let component = &book.component;
                let sell = component
                    .tokens
                    .iter()
                    .find(|token| token.address.as_ref() == cli.token_in.as_slice());
                let buy = component
                    .tokens
                    .iter()
                    .find(|token| token.address.as_ref() == cli.token_out.as_slice());
                if let (Some(sell), Some(buy)) = (sell, buy) {
                    return Ok((component.clone(), book.state.clone(), sell.clone(), buy.clone()));
                }
            }
        }
        Err(miette!("ElFomoFi book feed ended without the requested pair"))
    })
    .await
    .into_diagnostic()
    .wrap_err("No ElFomoFi book within 90s; check RPC_URL and WS_RPC_URL")?
}

async fn run(
    cli: &Cli,
    rpc: &RPCTools,
    books: &mut BookFeedStream<ElfomofiBlock>,
) -> miette::Result<()> {
    let block = current_block(rpc).await?;
    if timeout(WAIT, rpc.provider.get_chain_id())
        .await
        .into_diagnostic()?
        .into_diagnostic()? !=
        Chain::Base.id()
    {
        return Err(miette!("This routing example requires Base chain ID 8453"));
    }
    let (component, state, sell, buy) = elfomofi_book(cli, books).await?;
    let snapshot = state
        .as_any()
        .downcast_ref::<ElfomofiState>()
        .ok_or_else(|| miette!("Book state is not ElFomoFi"))?
        .snapshot();
    if snapshot.block_number != block.header.number ||
        snapshot.block_hash.as_ref() != block.header.hash.as_slice()
    {
        return Err(miette!("ElFomoFi book and RPC block differ; use a frozen fork"));
    }
    let quote = state
        .get_amount_out(cli.amount_in.clone(), &sell, &buy)
        .into_diagnostic()?;
    let mut candidates = vec![Candidate {
        component: component.into(),
        state,
        output: quote.amount,
        gas: quote.gas,
    }];
    let pair = call(
        rpc,
        cli.competitor_factory,
        &block,
        getPairCall { tokenA: cli.token_in, tokenB: cli.token_out },
    )
    .await?;
    if pair.is_zero() {
        return Err(miette!(
            "The competitor factory has no pool for this pair; choose another real pool"
        ));
    }
    if cli
        .competitor_pool
        .is_some_and(|required| required != pair)
    {
        return Err(miette!("Discovered competitor pool differs from --competitor-pool"));
    }
    let (first, second, pair_factory, reserves) = tokio::try_join!(
        call(rpc, pair, &block, token0Call {}),
        call(rpc, pair, &block, token1Call {}),
        call(rpc, pair, &block, factoryCall {}),
        call(rpc, pair, &block, getReservesCall {}),
    )?;
    if first != cli.token_in.min(cli.token_out) ||
        second != cli.token_in.max(cli.token_out) ||
        pair_factory != cli.competitor_factory
    {
        return Err(miette!("Competitor pool's tokens or factory do not match the requested pair"));
    }
    let v2: Arc<dyn ProtocolSim> =
        Arc::new(UniswapV2State::new(U256::from(reserves.reserve0), U256::from(reserves.reserve1)));
    let quote = v2
        .get_amount_out(cli.amount_in.clone(), &sell, &buy)
        .into_diagnostic()?;
    let competitor_output = quote.amount.clone();
    candidates.push(Candidate {
        component: tycho_simulation::tycho_common::models::protocol::ProtocolComponent {
            id: format!("{pair:#x}"),
            protocol_system: "uniswap_v2".into(),
            chain: Chain::Base,
            tokens: vec![sell.address.clone(), buy.address.clone()],
            ..Default::default()
        },
        state: v2,
        output: quote.amount,
        gas: quote.gas,
    });
    println!("Base block {} ({})", block.header.number, block.header.hash);
    println!("{} raw {} -> {}", cli.amount_in, sell.symbol, buy.symbol);
    for candidate in &candidates {
        println!(
            "Candidate {} {}: {} raw {}",
            candidate.component.protocol_system,
            candidate.component.id,
            candidate.output,
            buy.symbol
        );
    }
    let candidate_report: Vec<_> = candidates.iter().map(|candidate| serde_json::json!({
        "protocol": candidate.component.protocol_system, "component_id": candidate.component.id,
        "amount_out": candidate.output.to_string(),
    })).collect();
    let selected = candidates
        .into_iter()
        .max_by(|left, right| left.output.cmp(&right.output))
        .ok_or_else(|| miette!("No routing candidates"))?;
    println!("Selected route: {}", selected.component.protocol_system);
    if selected.component.protocol_system != "elfomofi" {
        return Err(miette!("ElFomoFi was not selected at this block and amount; the competitor offered more output"));
    }
    if selected.output == BigUint::ZERO {
        return Err(miette!("Selected indicative quote must have positive output"));
    }
    ensure_same_block(rpc, &block).await?;
    let venue = Address::from_str(BASE_ROUTER).into_diagnostic()?;
    if call(rpc, cli.token_in, &block, balanceOfCall { owner: venue }).await? != U256::ZERO {
        return Err(miette!("Venue has existing input-token balance, so an exact quote/fill comparison is unavailable for this snapshot"));
    }
    let router = get_router_address(&Chain::Base)
        .into_diagnostic()?
        .clone();
    let fresh = timeout(
        WAIT,
        selected
            .state
            .as_indicatively_priced()
            .into_diagnostic()?
            .request_signed_quote(GetAmountOutParams {
                amount_in: cli.amount_in.clone(),
                token_in: sell.address.clone(),
                token_out: buy.address.clone(),
                sender: router.clone(),
                receiver: router.clone(),
                origin: Some(Bytes::from_str(USER).into_diagnostic()?),
            }),
    )
    .await
    .into_diagnostic()
    .wrap_err("Fresh quote timed out")?
    .into_diagnostic()?;
    let expected = fresh.amount_out;
    println!(
        "Fresh ranking: ElFomoFi {expected}, Uniswap V2 {competitor_output} raw {}",
        buy.symbol
    );
    if expected < competitor_output || expected == BigUint::ZERO {
        return Err(miette!(
            "Refreshed partner-{PARTNER_ID} ElFomoFi quote is no longer the best positive candidate"
        ));
    }
    let minimum = &expected * BigUint::from(9950u32) / BigUint::from(10_000u32);
    let quoted = selected.output.clone();
    let component_id = selected.component.id.clone();
    let swap = Swap::new(selected.component, sell.clone(), buy.clone(), selected.gas)
        .with_protocol_state(selected.state)
        .with_estimated_amount_in(cli.amount_in.clone());
    let user = Bytes::from_str(USER).into_diagnostic()?;
    let solution = Solution::new(
        user.clone(),
        user.clone(),
        sell.address.clone(),
        buy.address.clone(),
        cli.amount_in.clone(),
        expected.clone(),
        minimum.clone(),
        vec![swap],
    )
    .with_origin(user);
    let registry = SwapEncoderRegistry::new(Chain::Base)
        .add_default_encoders(Some(
            serde_json::json!({"base": {"elfomofi": EXECUTOR_ADDRESS, "uniswap_v2": V2_EXECUTOR}})
                .to_string(),
        ))
        .into_diagnostic()?;
    let encoder = TychoRouterEncoderBuilder::new()
        .chain(Chain::Base)
        .router_address(router.clone())
        .swap_encoder_registry(registry)
        .build()
        .into_diagnostic()?;
    let mut encoded = encoder
        .encode_solutions(vec![solution.clone()])
        .into_diagnostic()?;
    if encoded.len() != 1 {
        return Err(miette!("Encoder did not produce exactly one route"));
    }
    let transaction = transaction(encoded.remove(0), &solution)?;
    ensure_same_block(rpc, &block).await?;
    let mut overwrites = execution::create_router_overwrites_data(Chain::Base, "elfomofi")?;
    overwrites.executors.insert(
        Address::from_str(V2_EXECUTOR).into_diagnostic()?,
        Some(execution::load_executor_bytecode(Chain::Base, "uniswap_v2")?),
    );
    let input = TychoExecutionInput {
        solution,
        transaction: transaction.clone(),
        expected_amount_out: expected.clone(),
        protocol_system: "elfomofi".into(),
        component_id,
        token_in: sell.address.to_string(),
        token_out: buy.address.to_string(),
        estimated_gas: transaction.estimated_gas().clone(),
    };
    let results = simulate_swap_transaction(
        rpc,
        HashMap::from([("selected-route".into(), input)]),
        &block,
        overwrites,
        None,
    )
    .await
    .map_err(|(error, _, _)| error)?;
    ensure_same_block(rpc, &block).await?;
    match results.get("selected-route") {
        Some(TychoExecutionResult::Success { amount_out, gas_used, .. }) => {
            if amount_out != &expected || amount_out < &minimum {
                return Err(miette!("Executed output {amount_out} differs from fresh quote {expected} or falls below minimum {minimum}"));
            }
            let difference =
                if amount_out >= &quoted { amount_out - &quoted } else { &quoted - amount_out };
            let difference_bps = difference * BigUint::from(10_000u32) / &quoted;
            println!("Fresh partner-{PARTNER_ID} quote: {expected} raw {}", buy.symbol);
            println!("Minimum output (50 bps): {minimum}");
            println!("EVM simulation passed: {amount_out} raw {}; gas {gas_used}; indicative difference {difference_bps} bps", buy.symbol);
            verify_settlement_trace(
                rpc,
                &block,
                &transaction,
                results.get("selected-route").unwrap(),
                &router,
                cli,
            )
            .await?;
            ensure_same_block(rpc, &block).await?;
            if let Some(path) = &cli.report {
                let report = serde_json::json!({
                    "chain": "base", "block_number": block.header.number,
                    "block_hash": block.header.hash.to_string(), "block_timestamp": block.header.timestamp,
                    "token_in": sell.address.to_string(), "token_out": buy.address.to_string(),
                    "amount_in": cli.amount_in.to_string(), "candidates": candidate_report,
                    "selected_route": ["elfomofi"], "indicative_amount_out": quoted.to_string(),
                    "fresh_amount_out": expected.to_string(), "minimum_amount_out": minimum.to_string(),
                    "executed_amount_out": amount_out.to_string(), "gas_used": gas_used,
                    "indicative_difference_bps": difference_bps.to_string(), "verified_partner_id": PARTNER_ID,
                    "transaction": {"to": transaction.to().to_string(), "value": transaction.value().to_string(),
                        "data": format!("0x{}", hex::encode(transaction.data()))},
                });
                std::fs::write(path, serde_json::to_vec_pretty(&report).into_diagnostic()?)
                    .into_diagnostic()
                    .wrap_err("Failed to save routing report")?;
                println!("Saved report: {}", path.display());
            }
        }
        Some(TychoExecutionResult::Revert { reason, .. }) => {
            return Err(miette!("Selected route reverted: {reason}"))
        }
        Some(TychoExecutionResult::Failed { error_msg }) => {
            return Err(miette!("Selected route simulation failed: {error_msg}"))
        }
        None => return Err(miette!("Execution simulator returned no selected-route result")),
    }
    Ok(())
}

async fn verify_settlement_trace(
    rpc: &RPCTools,
    block: &Block,
    transaction: &Transaction,
    result: &TychoExecutionResult,
    router: &Bytes,
    cli: &Cli,
) -> miette::Result<()> {
    let TychoExecutionResult::Success { state_overwrites, .. } = result else {
        return Err(miette!("A successful execution is required before inspecting its trace"));
    };
    let request = TransactionRequest::default()
        .to(Address::from_slice(transaction.to()))
        .from(Address::from_str(USER).into_diagnostic()?)
        .input(transaction.data().clone().into())
        .gas_limit(100_000_000)
        .max_priority_fee_per_gas(0)
        .max_fee_per_gas(
            u128::from(
                block
                    .header
                    .base_fee_per_gas
                    .unwrap_or_default(),
            ) * 2,
        );
    let options = serde_json::json!({"tracer": "callTracer", "stateOverrides": state_overwrites});
    let trace: serde_json::Value = timeout(
        WAIT,
        rpc.provider.raw_request(
            "debug_traceCall".into(),
            (request, format!("0x{:x}", block.header.number), options),
        ),
    )
    .await
    .into_diagnostic()
    .wrap_err("Settlement trace timed out")?
    .into_diagnostic()?;
    if trace.get("error").is_some() {
        return Err(miette!("Verification trace unexpectedly reverted"));
    }
    let venue = Address::from_str(BASE_ROUTER).into_diagnostic()?;
    let call = find_settlement_call(&trace, venue).ok_or_else(|| {
        miette!("No swapWithContractBalance call found in successful route trace")
    })?;
    let input = call["input"]
        .as_str()
        .ok_or_else(|| miette!("Settlement trace has no input"))?;
    let raw = hex::decode(input.trim_start_matches("0x")).into_diagnostic()?;
    let decoded = swapWithContractBalanceCall::abi_decode_validate(&raw).into_diagnostic()?;
    let caller = call["from"]
        .as_str()
        .ok_or_else(|| miette!("Settlement trace has no caller"))?;
    if Address::from_str(caller).into_diagnostic()? != Address::from_slice(router) ||
        decoded.from != cli.token_in ||
        decoded.to != cli.token_out ||
        decoded.receiver != Address::from_str(USER).into_diagnostic()? ||
        decoded.partnerId != U256::from(PARTNER_ID) ||
        decoded.minimum != U256::ZERO
    {
        return Err(miette!(
            "Settlement trace caller, tokens, receiver or partner ID {PARTNER_ID} do not match encoded route"
        ));
    }
    println!(
        "Settlement trace verified: swapWithContractBalance, Tycho caller, partner {PARTNER_ID}"
    );
    Ok(())
}

fn find_settlement_call(trace: &serde_json::Value, venue: Address) -> Option<&serde_json::Value> {
    let is_settlement = trace["to"]
        .as_str()
        .and_then(|address| Address::from_str(address).ok()) ==
        Some(venue);
    let is_swap = trace["input"]
        .as_str()
        .is_some_and(|input| input.starts_with("0x9e61d496"));
    if is_settlement && is_swap {
        return Some(trace);
    }
    trace["calls"]
        .as_array()?
        .iter()
        .find_map(|call| find_settlement_call(call, venue))
}

fn transaction(encoded: EncodedSolution, solution: &Solution) -> miette::Result<Transaction> {
    let to_u256 = |amount: &BigUint| U256::from_be_slice(&amount.to_bytes_be());
    let mut args = (
        to_u256(solution.amount_in()),
        Address::from_slice(solution.token_in()),
        Address::from_slice(solution.token_out()),
        to_u256(solution.expected_amount_out()),
        to_u256(solution.min_amount_out()),
        Address::from_slice(solution.receiver()),
        ClientFeeParams::default().into_abi_params(),
        encoded.swaps(),
    )
        .abi_encode();
    // Router calldata omits the outer dynamic-tuple offset.
    if args.get(..32) ==
        Some(
            U256::from(32u32)
                .to_be_bytes::<32>()
                .as_slice(),
        )
    {
        args.drain(..32);
    }
    let mut calldata = keccak256(encoded.function_signature().as_bytes())[..4].to_vec();
    calldata.extend(args);
    Ok(Transaction::new(
        encoded.interacting_with().clone(),
        BigUint::ZERO,
        calldata,
        encoded.estimated_gas().clone(),
    ))
}
