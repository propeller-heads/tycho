//! Replays AERO v1.1 (`aero_cl`) pools against the chain.
//!
//! Each fixture holds pool states at a block and what the pools returned on-chain for the next
//! block: `fee()` and the exact amounts of swaps from 1e-6x to 0.5x of the input token's pool
//! balance, in both directions (quoted with `eth_call` and next-block overrides, reverting from the
//! swap callback).
//! - Base: five pools, with every attribute the `evm-aero-cl` Substreams package indexed up to
//!   block 52924436.
//! - Arc: all seven pools at block 27989605, read from storage in the package's encoding. The next
//!   block has the same timestamp, as Arc's sub-second blocks often do.

use std::{collections::HashMap, str::FromStr};

use num_bigint::BigUint;
use serde::Deserialize;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{
    models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        token::Token,
        Chain,
    },
    simulation::protocol_sim::{BlockContext, ProtocolSim},
    Bytes,
};

use super::AerodromeSlipstreamsState;
use crate::protocol::models::{DecoderContext, TryFromWithBlock};

#[derive(Deserialize)]
struct Capture {
    block: u64,
    timestamp: u64,
    next_block: u64,
    next_timestamp: u64,
    pools: Vec<CapturedPool>,
}

#[derive(Deserialize)]
struct CapturedPool {
    pool: String,
    tokens: Vec<String>,
    decimals: Vec<u32>,
    static_attributes: HashMap<String, String>,
    attributes: HashMap<String, String>,
    fee: u32,
    quotes: Vec<CapturedQuote>,
}

#[derive(Deserialize)]
struct CapturedQuote {
    token_in: String,
    amount_in: String,
    amount_out: Option<String>,
}

fn bytes_map(values: &HashMap<String, String>) -> HashMap<String, Bytes> {
    values
        .iter()
        .map(|(name, value)| (name.clone(), Bytes::from_str(value).unwrap()))
        .collect()
}

const BASE_CAPTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/assets/aero_cl/onchain_quotes_base.json"
));
const ARC_CAPTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/assets/aero_cl/onchain_quotes_arc.json"
));

#[tokio::test]
async fn replays_base_pools_wei_exact() {
    assert_eq!(replay(BASE_CAPTURE).await, 50);
}

#[tokio::test]
async fn replays_arc_pools_wei_exact() {
    assert_eq!(replay(ARC_CAPTURE).await, 68);
}

/// Checks every pool's fee and quotes, returning the number of quotes compared.
async fn replay(capture: &str) -> usize {
    let capture: Capture = serde_json::from_str(capture).unwrap();

    let mut quotes = 0;
    for pool in &capture.pools {
        let tokens: Vec<Token> = pool
            .tokens
            .iter()
            .zip(&pool.decimals)
            .map(|(address, decimals)| {
                Token::new(
                    &Bytes::from_str(address).unwrap(),
                    "T",
                    *decimals,
                    0,
                    &[],
                    Chain::Base,
                    100,
                )
            })
            .collect();
        let all_tokens: HashMap<Bytes, Token> = tokens
            .iter()
            .map(|token| (token.address.clone(), token.clone()))
            .collect();
        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: pool.pool.clone(),
                attributes: bytes_map(&pool.attributes),
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: pool.pool.clone(),
                tokens: tokens
                    .iter()
                    .map(|token| token.address.clone())
                    .collect(),
                static_attributes: bytes_map(&pool.static_attributes),
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        };
        let header = BlockHeader {
            number: capture.block,
            timestamp: capture.timestamp,
            ..Default::default()
        };
        let mut state = AerodromeSlipstreamsState::try_from_with_header(
            snapshot,
            header,
            &HashMap::new(),
            &all_tokens,
            &DecoderContext::new(),
        )
        .await
        .unwrap_or_else(|err| panic!("{}: {err}", pool.pool));
        state.apply_block(&BlockContext::new(capture.next_block, capture.next_timestamp));

        assert_eq!(state.get_fee().unwrap().fee, pool.fee, "{} fee", pool.pool);

        for quote in &pool.quotes {
            let (token_in, token_out) = if quote.token_in == pool.tokens[0] {
                (&tokens[0], &tokens[1])
            } else {
                (&tokens[1], &tokens[0])
            };
            let amount_in = BigUint::from_str(&quote.amount_in).unwrap();
            let context =
                format!("{} amount_in {} of {}", pool.pool, quote.amount_in, quote.token_in);
            let result = state.get_amount_out(amount_in, token_in, token_out);
            match &quote.amount_out {
                Some(expected) => {
                    let result = result.unwrap_or_else(|err| panic!("{context}: {err}"));
                    assert_eq!(result.amount, BigUint::from_str(expected).unwrap(), "{context}");
                    quotes += 1;
                }
                None => assert!(result.is_err(), "{context}: the chain reverted"),
            }
        }
    }
    quotes
}

/// A second swap in the same block pays the fee the block's first swap stored, even though the
/// first swap moved the tick.
#[tokio::test]
async fn chained_swap_pays_the_frozen_block_fee() {
    let capture: Capture = serde_json::from_str(BASE_CAPTURE).unwrap();
    let pool = &capture.pools[0];
    let tokens: Vec<Token> = pool
        .tokens
        .iter()
        .zip(&pool.decimals)
        .map(|(address, decimals)| {
            Token::new(&Bytes::from_str(address).unwrap(), "T", *decimals, 0, &[], Chain::Base, 100)
        })
        .collect();
    let snapshot = ComponentWithState {
        state: ProtocolComponentState {
            component_id: pool.pool.clone(),
            attributes: bytes_map(&pool.attributes),
            balances: HashMap::new(),
        },
        component: ProtocolComponent {
            id: pool.pool.clone(),
            tokens: tokens
                .iter()
                .map(|token| token.address.clone())
                .collect(),
            static_attributes: bytes_map(&pool.static_attributes),
            ..Default::default()
        },
        component_tvl: None,
        entrypoints: Vec::new(),
    };
    let header =
        BlockHeader { number: capture.block, timestamp: capture.timestamp, ..Default::default() };
    let mut state = AerodromeSlipstreamsState::try_from_with_header(
        snapshot,
        header,
        &HashMap::new(),
        &HashMap::new(),
        &DecoderContext::new(),
    )
    .await
    .unwrap();
    state.apply_block(&BlockContext::new(capture.next_block, capture.next_timestamp));
    let first_fee = state.get_fee().unwrap().fee;

    let big = BigUint::from_str(&pool.quotes[4].amount_in).unwrap();
    let after = state
        .get_amount_out(big, &tokens[0], &tokens[1])
        .unwrap()
        .new_state;
    let after = after
        .as_any()
        .downcast_ref::<AerodromeSlipstreamsState>()
        .unwrap();

    assert_ne!(after.tick, state.tick);
    assert_eq!(after.get_fee().unwrap().fee, first_fee);
}
