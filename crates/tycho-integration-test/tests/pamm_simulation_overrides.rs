//! Live check that a pAMM quote's simulation overrides reproduce the venue's own price.
//!
//! A pAMM ladder prices the block Titan is still building, so a client that simulates the swap on
//! mined state simulates a different venue state and the venue's freshness guard reverts. The
//! price level stream answers that by carrying, on the quote, the account state Titan priced the
//! venue on. This test holds that answer against the chain: it captures live quotes, waits for the
//! block each quote targets, and simulates the encoded swap at that block twice — once with the
//! quote's own overrides, once with none.
//!
//! The gate is the first run: the simulated output must match the quoted output. The run without
//! overrides is reported rather than asserted, because whether mined state alone reverts depends
//! on who built that block.
//!
//! Ignored by default: it dials the live Titan sockets and the RPC in `RPC_URL`. Read-only — it
//! simulates, and never sends a transaction. Run it with
//! `cargo test -p tycho-integration-test --test pamm_simulation_overrides -- --ignored
//! --nocapture`.

use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{
        map::{AddressHashMap, B256HashMap},
        Address, B256,
    },
    providers::Provider,
};
use futures::StreamExt;
use num_bigint::BigUint;
use tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim, Bytes};
use tycho_simulation::{
    price_level_stream::{
        overrides::PammSimulationOverrides, state::PriceLevelStreamState,
        stream::PriceLevelStreamBuilder,
    },
    protocol::models::ProtocolComponent,
};
use tycho_test::{
    execution::{
        encoding::encode_swap,
        models::{RouterOverwritesData, TychoExecutionInput, TychoExecutionResult},
        simulate_swap_transaction,
    },
    rpc_tools::RPCTools,
};

/// Selector of the venue freshness guard, `StaleUpdate()`.
const STALE_UPDATE: &str = "0x666a2814";

/// How many quotes to validate.
const CANDIDATES: usize = 2;

/// How long to wait for that many stamped quotes. Titan publishes about one frame a second.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long to wait for a quoted block to settle behind head before giving up on a candidate.
const BLOCK_TIMEOUT: Duration = Duration::from_secs(45);

/// How far a simulated output may sit from the quoted output, in basis points.
const TOLERANCE_BPS: u32 = 1;

/// One captured quote, with the overrides it carried.
struct Candidate {
    component: ProtocolComponent,
    state: Arc<dyn ProtocolSim>,
    token_in: Token,
    token_out: Token,
    amount_in: BigUint,
    quoted_out: BigUint,
    gas: BigUint,
    overrides: PammSimulationOverrides,
}

fn token(address: &str, symbol: &str, decimals: u32) -> (Bytes, Token) {
    let address = Bytes::from(
        address
            .parse::<Address>()
            .expect("hardcoded address must parse")
            .as_slice(),
    );
    let token = Token::new(
        &address,
        symbol,
        decimals,
        0,
        &[Some(10_000)],
        tycho_common::models::Chain::Ethereum,
        100,
    );
    (address, token)
}

/// The tokens the default symbol bindings cover.
fn tokens() -> HashMap<Bytes, Token> {
    [
        token("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", "WETH", 18),
        token("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", "USDC", 6),
        token("0xdAC17F958D2ee523a2206206994597C13D831ec7", "USDT", 6),
        token("0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599", "WBTC", 8),
        token("0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf", "cbBTC", 8),
    ]
    .into_iter()
    .collect()
}

/// The storage half of a snapshot, in the shape the simulator takes.
///
/// Balance and nonce are dropped, because the simulator applies storage only. The freshness guard
/// reads storage, so this is the half it depends on.
fn storage_of(overrides: &PammSimulationOverrides) -> AddressHashMap<B256HashMap<B256>> {
    let mut out: AddressHashMap<B256HashMap<B256>> = AddressHashMap::default();
    for (address, account) in &overrides.accounts {
        let Ok(address) = Address::try_from(&address[..]) else { continue };
        let slots = out.entry(address).or_default();
        for (slot, word) in &account.state_diff {
            let (Ok(slot), Ok(word)) = (B256::try_from(&slot[..]), B256::try_from(&word[..]))
            else {
                continue;
            };
            slots.insert(slot, word);
        }
    }
    out.retain(|_, slots| !slots.is_empty());
    out
}

/// Distance between two amounts in basis points of `quoted`.
fn distance_bps(simulated: &BigUint, quoted: &BigUint) -> BigUint {
    if quoted == &BigUint::ZERO {
        return BigUint::from(u32::MAX);
    }
    let spread = if simulated > quoted { simulated - quoted } else { quoted - simulated };
    spread * BigUint::from(10_000u32) / quoted
}

/// Captures quotes that carry a snapshot, and reports whether any quote was served at all.
async fn capture() -> (Vec<Candidate>, usize) {
    let tokens = tokens();
    let by_address = tokens.clone();
    let mut stream = Box::pin(
        PriceLevelStreamBuilder::new()
            .with_known_pamms()
            // The direct path executes on the venue itself, where the freshness guard lives. The
            // `fallback:` default routes through `TychoFallbackRouter`, which needs a backing
            // protocol named in `user_data` and would not exercise the venue's own guard.
            .without_fallback_router()
            .with_tokens(tokens)
            .build(),
    );

    let mut components: HashMap<String, ProtocolComponent> = HashMap::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut quotes_served = 0usize;
    let deadline = tokio::time::Instant::now() + CAPTURE_TIMEOUT;

    while candidates.len() < CANDIDATES {
        let Ok(Some(update)) = tokio::time::timeout_at(deadline, stream.next()).await else {
            break;
        };
        for (id, component) in &update.new_pairs {
            components.insert(id.clone(), component.clone());
        }

        for (id, state) in &update.states {
            let Some(pls) = state
                .as_any()
                .downcast_ref::<PriceLevelStreamState>()
            else {
                continue;
            };
            let Some(component) = components.get(id) else { continue };
            if component.tokens.len() < 2 {
                continue;
            }
            let (t0, t1) = (&component.tokens[0], &component.tokens[1]);
            let (Some(token_in), Some(token_out)) =
                (by_address.get(&t0.address), by_address.get(&t1.address))
            else {
                continue;
            };
            let Ok((max_in, _)) = pls.get_limits(t0.address.clone(), t1.address.clone()) else {
                continue;
            };
            let amount_in = &max_in / BigUint::from(100u32);
            if amount_in == BigUint::ZERO {
                continue;
            }
            let Ok(quote) = pls.get_amount_out(amount_in.clone(), token_in, token_out) else {
                continue;
            };
            quotes_served += 1;

            // One candidate per component, so the two runs cover two venues where Titan serves
            // more than one.
            if candidates.len() >= CANDIDATES ||
                candidates
                    .iter()
                    .any(|c| c.component.id == component.id)
            {
                continue;
            }
            let Some(overrides) = pls.simulation_overrides() else { continue };
            if overrides.block_number.is_none() {
                continue;
            }
            println!(
                "captured {} {} -> {} targeting block {}",
                component.id,
                token_in.symbol,
                token_out.symbol,
                overrides
                    .block_number
                    .unwrap_or_default(),
            );
            candidates.push(Candidate {
                component: component.clone(),
                state: Arc::from(pls.clone_box()),
                token_in: token_in.clone(),
                token_out: token_out.clone(),
                amount_in,
                quoted_out: quote.amount,
                gas: quote.gas,
                overrides: overrides.clone(),
            });
        }
    }
    (candidates, quotes_served)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "dials the live Titan sockets and RPC_URL"]
async fn a_quotes_overrides_reproduce_the_venues_own_price() {
    let Ok(rpc_url) = std::env::var("RPC_URL") else {
        println!("RPC_URL is not set; skipping");
        return;
    };
    let chain = tycho_common::models::Chain::Ethereum;

    let (candidates, quotes_served) = capture().await;
    if quotes_served == 0 {
        println!("the stream served no quote within the capture window; skipping");
        return;
    }
    // Quotes without a single snapshot between them means the join stopped attributing, which is
    // the failure this test exists to catch.
    assert!(
        !candidates.is_empty(),
        "{quotes_served} quote(s) served and not one carried a snapshot"
    );

    let rpc_tools = RPCTools::new(&rpc_url, &chain)
        .await
        .expect("RPC_URL must connect");
    let mut validated = 0usize;
    let mut guarded_without_overrides = 0usize;

    for candidate in &candidates {
        let target = candidate
            .overrides
            .block_number
            .expect("filtered at capture");
        println!(
            "\n{} {} -> {} at block {target}",
            candidate.component.id, candidate.token_in.symbol, candidate.token_out.symbol
        );

        // Trace only once the quoted block sits behind head: a load-balanced RPC serves the header
        // from one backend while another has not yet synced the block the trace needs.
        let settle_by = tokio::time::Instant::now() + BLOCK_TIMEOUT;
        let mut block = None;
        while tokio::time::Instant::now() < settle_by {
            if rpc_tools
                .provider
                .get_block_number()
                .await
                .unwrap_or_default() >
                target
            {
                if let Ok(Some(found)) = rpc_tools
                    .provider
                    .get_block_by_number(BlockNumberOrTag::Number(target))
                    .await
                {
                    block = Some(found);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let Some(block) = block else {
            println!("  the RPC never settled block {target}; skipping this candidate");
            continue;
        };

        let (solution, transaction) = encode_swap(
            &candidate.component,
            Some(candidate.state.clone()),
            &candidate.token_in,
            &candidate.token_out,
            candidate.amount_in.clone(),
            chain,
            None,
            candidate.gas.clone(),
            candidate.quoted_out.clone(),
        )
        .expect("a served pAMM quote must encode");
        let inputs: HashMap<String, TychoExecutionInput> = [(
            "one".to_string(),
            TychoExecutionInput {
                solution,
                transaction: transaction.clone(),
                expected_amount_out: candidate.quoted_out.clone(),
                protocol_system: candidate
                    .component
                    .protocol_system
                    .clone(),
                component_id: candidate.component.id.to_string(),
                token_in: candidate.token_in.address.to_string(),
                token_out: candidate.token_out.address.to_string(),
                estimated_gas: transaction.estimated_gas().clone(),
            },
        )]
        .into_iter()
        .collect();

        let storage = storage_of(&candidate.overrides);
        let slots: usize = storage.values().map(HashMap::len).sum();
        assert!(!storage.is_empty(), "a snapshot must override at least one account");
        println!("  snapshot: {} account(s), {slots} slot(s)", storage.len());

        // With the quote's own overrides: the gate.
        let with = simulate_swap_transaction(
            &rpc_tools,
            inputs.clone(),
            &block,
            RouterOverwritesData::default(),
            Some(storage),
        )
        .await;
        match with
            .as_ref()
            .map(|results| results.get("one"))
        {
            Ok(Some(TychoExecutionResult::Success { amount_out, .. })) => {
                let bps = distance_bps(amount_out, &candidate.quoted_out);
                println!(
                    "  with the quote's overrides: simulated {amount_out}, quoted {}, |Δ| {bps} bps",
                    candidate.quoted_out
                );
                assert!(
                    bps <= BigUint::from(TOLERANCE_BPS),
                    "simulated {amount_out} is {bps} bps from the quoted {}, over the {TOLERANCE_BPS} bps tolerance",
                    candidate.quoted_out
                );
                validated += 1;
            }
            Ok(Some(TychoExecutionResult::Revert { reason, .. })) => {
                panic!("the quote's own overrides still reverted: {reason}")
            }
            Ok(Some(TychoExecutionResult::Failed { error_msg })) => {
                println!("  simulation setup failed, skipping this candidate: {error_msg}");
                continue;
            }
            Ok(None) => panic!("the simulator returned no result"),
            Err(_) => {
                println!("  the RPC failed the simulation; skipping this candidate");
                continue;
            }
        }

        // Without them: reported, not asserted. Whether mined state alone is fresh depends on who
        // built the block.
        let without = simulate_swap_transaction(
            &rpc_tools,
            inputs,
            &block,
            RouterOverwritesData::default(),
            None,
        )
        .await;
        match without
            .as_ref()
            .map(|results| results.get("one"))
        {
            Ok(Some(TychoExecutionResult::Revert { reason, .. })) => {
                let guarded = reason.contains("StaleUpdate") || reason.contains(STALE_UPDATE);
                println!(
                    "  with no overrides: revert {reason}{}",
                    if guarded { "  <- the freshness guard" } else { "" }
                );
                if guarded {
                    guarded_without_overrides += 1;
                }
            }
            Ok(Some(TychoExecutionResult::Success { amount_out, .. })) => println!(
                "  with no overrides: simulated {amount_out} (mined state was fresh for this block)"
            ),
            _ => println!("  with no overrides: no comparable result"),
        }
    }

    println!(
        "\nvalidated {validated} of {} candidate(s); {guarded_without_overrides} hit the freshness \
         guard without overrides",
        candidates.len()
    );
    assert!(validated > 0, "no candidate could be validated against the chain");
}
