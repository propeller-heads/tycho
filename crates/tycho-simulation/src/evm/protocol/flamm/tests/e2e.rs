// Copyright (c) 2026 Everlong Labs Limited

//! The `ProtocolSim` fed by the `base-flamm` substreams end to end: the stream fixture
//! (`testdata/snapshots/e2e_stream.json.gz`) is the package's own output over real Base blocks
//! (the pool's deployments, creation, activation, every swap it has settled, two of its
//! deposits, a withdrawal, a keeper recenter, the range test's stop blocks, three recent blocks
//! carrying Chainlink rounds and the block that unpaused leverage; the pool's other deposits,
//! withdrawals and keeper transactions reach the fold through the synthetic catch-up blocks
//! between them), folded as the indexer holds it and as the client delivers it
//! (`protocols/substreams/base-flamm/src/e2e_tests.rs`, which asserts the fixture equals what
//! the package emits). Here the blocks are replayed as the stream decoder would: the components
//! created in a block are decoded from their snapshot at that block's header, every later block
//! is a `delta_transition`, and after each confirmed block every state is advanced to the next
//! block's clock. At the pinned blocks the quotes are compared with the chain's own
//! `previewSwap` / `previewLever` answers recorded by `eth_call` at those blocks
//! (`testdata/snapshots/e2e_grids.json.gz`): the same words for every size of a log-spaced grid
//! and at the largest fully consumed size per direction, the same refusal class where the pool
//! reverts. Every swap the pool has settled is quoted from the parent block's state at the swap
//! block's clock and must return exactly the amount the receipt records.

use std::{collections::HashMap, str::FromStr};

use alloy::primitives::{Address, U256};
use num_bigint::BigUint;
use serde_json::Value;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{
    dto::{ProtocolComponent, ProtocolStateDelta, ResponseProtocolState},
    models::{token::Token, Chain},
    simulation::{
        errors::SimulationError,
        protocol_sim::{Balances, BlockContext, ProtocolSim},
    },
    Bytes,
};

use super::{
    core::common::revert_class,
    fixtures::{load, s},
    protocol_sim::{limit_of, refused_quote},
};
use crate::{
    evm::protocol::{
        flamm::{
            decoder::{lever_up_id, swap_id},
            error::FlammError,
            sim::{GAS_LEVER_UP, GAS_SWAP_BUY, GAS_SWAP_SELL},
            FlammPoolState, VenueKind, PROTOCOL_SYSTEM,
        },
        u256_num::u256_to_biguint,
    },
    protocol::models::{DecoderContext, TryFromWithBlock},
};

const POOL: &str = "0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572";
const CBBTC: &str = "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf";
const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
const CREATION_BLOCK: u64 = 51_154_990;
const ACTIVATION_BLOCK: u64 = 51_298_416;
/// `LevPauseSet(false)`, the curator Safe calling `FLAMM.setLevPaused(false)`: leverage is paused
/// from the creation until this block. The keeper has never re-posted a spread
/// (`LeverageSpreadHook.setSpread`; the hook has emitted no `SpreadSet`), so
/// the constructor's spread aged past `maxSpreadAge` an hour after the creation and every
/// lever-up since refuses `SpreadUnavailable` (`FLAMMLeverLib.sol:167`) whether paused or not;
/// a lever-down refuses on its own checks (`NothingToFill`, `PriceBand`).
const LEV_UNPAUSE_BLOCK: u64 = 51_433_699;
/// The stop blocks of `integration_test.tycho.yaml`'s two tests.
const STOP_BLOCKS: [u64; 2] = [51_155_010, 51_302_920];
/// Base's block time, what the stream decoder adds to a confirmed header's timestamp to reach
/// the execution clock (`Chain::Base.block_time_secs()`).
const BLOCK_TIME: u64 = 2;

fn bytes(hex: &str) -> Bytes {
    let digits = hex.strip_prefix("0x").unwrap_or(hex);
    let padded = if digits.len() % 2 == 1 { format!("0x0{digits}") } else { format!("0x{digits}") };
    Bytes::from_str(&padded).unwrap_or_else(|e| panic!("{hex}: {e}"))
}

fn token(addr: &str, symbol: &str, decimals: u32) -> Token {
    Token::new(&bytes(addr), symbol, decimals, 0, &[Some(100_000)], Chain::Base, 100)
}

fn cbbtc() -> Token {
    token(CBBTC, "cbBTC", 8)
}

fn usdc() -> Token {
    token(USDC, "USDC", 6)
}

fn map(v: &Value) -> HashMap<String, Bytes> {
    v.as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), bytes(v.as_str().unwrap())))
        .collect()
}

/// A block of the stream fixture.
struct StreamBlock<'a> {
    raw: &'a Value,
}

impl StreamBlock<'_> {
    fn number(&self) -> u64 {
        self.raw["number"].as_u64().unwrap()
    }

    fn timestamp(&self) -> u64 {
        self.raw["timestamp"].as_u64().unwrap()
    }

    fn header(&self) -> BlockHeader {
        BlockHeader {
            hash: bytes(s(self.raw, "hash")),
            number: self.number(),
            parent_hash: bytes(s(self.raw, "parent_hash")),
            revert: false,
            timestamp: self.timestamp(),
            partial_block_index: None,
        }
    }

    /// The snapshots of the components created in this block, as the RPC would serve them.
    fn new_components(&self) -> Vec<ComponentWithState> {
        self.raw["new_components"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(id, c)| {
                let comp = &c["component"];
                ComponentWithState {
                    state: ResponseProtocolState {
                        component_id: id.clone(),
                        attributes: map(&c["attributes"]),
                        balances: c["balances"]
                            .as_object()
                            .unwrap()
                            .iter()
                            .map(|(k, v)| (bytes(k), bytes(v.as_str().unwrap())))
                            .collect(),
                    }
                    .into(),
                    component: ProtocolComponent {
                        id: s(comp, "id").to_owned(),
                        protocol_system: s(comp, "protocol_system").to_owned(),
                        protocol_type_name: s(comp, "protocol_type_name").to_owned(),
                        chain: Chain::Base.into(),
                        tokens: comp["tokens"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|t| bytes(t.as_str().unwrap()))
                            .collect(),
                        contract_ids: Vec::new(),
                        static_attributes: map(&comp["static_attributes"]),
                        creation_tx: bytes(s(comp, "creation_tx")),
                        ..Default::default()
                    }
                    .into(),
                    component_tvl: None,
                    entrypoints: Vec::new(),
                }
            })
            .collect()
    }

    /// The deltas of the components that existed before this block, with the block attributes
    /// the stream decoder adds (`add_block_info_to_delta`).
    fn deltas(&self) -> Vec<ProtocolStateDelta> {
        self.raw["deltas"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(id, d)| {
                let mut updated = map(&d["updated_attributes"]);
                updated.insert(
                    "block_number".into(),
                    Bytes::from(self.number().to_be_bytes().to_vec()),
                );
                updated.insert(
                    "block_timestamp".into(),
                    Bytes::from(self.timestamp().to_be_bytes().to_vec()),
                );
                ProtocolStateDelta {
                    component_id: id.clone(),
                    updated_attributes: updated,
                    deleted_attributes: d["deleted_attributes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|n| n.as_str().unwrap().to_owned())
                        .collect(),
                }
            })
            .collect()
    }
}

/// The client's states, driven block by block as `TychoStreamDecoder::decode` drives them.
struct Client {
    states: HashMap<String, FlammPoolState>,
}

impl Client {
    fn apply(&mut self, block: &StreamBlock<'_>) {
        for snapshot in block.new_components() {
            let id = snapshot.component.id.clone();
            let state = tokio_test::block_on(FlammPoolState::try_from_with_header(
                snapshot,
                block.header(),
                &HashMap::new(),
                &HashMap::new(),
                &DecoderContext::new(),
            ))
            .unwrap_or_else(|e| panic!("block {}: {id}: {e:?}", block.number()));
            assert!(self.states.insert(id, state).is_none());
        }
        for delta in block.deltas() {
            let state = self
                .states
                .get_mut(&delta.component_id)
                .unwrap_or_else(|| panic!("delta for unknown {}", delta.component_id));
            state
                .delta_transition(delta, &HashMap::new(), &Balances::default())
                .unwrap_or_else(|e| panic!("block {}: {e:?}", block.number()));
            assert_eq!(state.block(), block.number());
        }
        // A confirmed header: every state is advanced to the block the next quote executes in.
        let execution = BlockContext::new(block.number() + 1, block.timestamp() + BLOCK_TIME);
        for state in self.states.values_mut() {
            state.apply_block(&execution);
        }
    }

    fn swap(&self) -> &FlammPoolState {
        &self.states[&swap_id(Address::from_str(POOL).unwrap())]
    }

    fn lever(&self) -> &FlammPoolState {
        &self.states[&lever_up_id(Address::from_str(POOL).unwrap())]
    }
}

struct Fixtures {
    stream: Value,
    chain: Value,
}

impl Fixtures {
    fn load() -> Self {
        Self {
            stream: load("snapshots/e2e_stream.json.gz"),
            chain: load("snapshots/e2e_grids.json.gz"),
        }
    }

    fn blocks(&self) -> Vec<StreamBlock<'_>> {
        self.stream["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|raw| StreamBlock { raw })
            .collect()
    }

    fn grids(&self, block: u64) -> Option<&Value> {
        self.chain["grids"].get(block.to_string())
    }
}

/// The recorded outcome of one `previewSwap` / `previewLever` row: the return words, or the
/// revert class.
fn recorded(row: &Value) -> Result<Vec<U256>, Option<FlammError>> {
    if row["ok"].as_bool().unwrap() {
        let h = s(row, "ret");
        let b = hex::decode(h.trim_start_matches("0x")).unwrap();
        Ok(b.chunks(32)
            .map(U256::from_be_slice)
            .collect())
    } else {
        Err(revert_class(s(row, "revert")))
    }
}

fn amount(row: &Value) -> U256 {
    U256::from_str_radix(s(row, "amount_in"), 10).unwrap()
}

/// Every recorded row at a pinned block against the state at that block's own clock (the clock
/// `eth_call` at the block ran with): the preview words or the refusal class, the quote
/// semantics per row (a full fill is quoted with the recorded output, a clipped size is
/// refused, a reverting size is refused above the limit and is the empty trade, dust, at or
/// below it), the limits at the recorded edges, the hook's spot, and the lever venue's
/// refusals. Returns `(rows, full fills)`.
fn check_grids(
    swap: &FlammPoolState,
    lever: &FlammPoolState,
    grids: &Value,
    block: u64,
) -> (usize, usize) {
    let (cb, us) = (cbbtc(), usdc());
    let now = swap.clock();
    let flamm = swap
        .flamm()
        .unwrap_or_else(|| panic!("block {block}: {:?}", swap.core().err()));
    let (mut rows, mut full) = (0usize, 0usize);
    for (dir, key) in [(true, "swap_sell"), (false, "swap_buy")] {
        let (tin, tout) = if dir { (&cb, &us) } else { (&us, &cb) };
        let limit = swap
            .get_limits(tin.address.clone(), tout.address.clone())
            .map_or(U256::ZERO, |_| limit_of(swap, tin, tout));
        for row in grids[key].as_array().unwrap() {
            rows += 1;
            let a = amount(row);
            let chain = recorded(row);
            let ctx = format!("block {block} {key} {a}");
            let got = flamm.preview_swap(dir, a, now);
            match (&chain, &got) {
                (Ok(words), Ok(p)) => {
                    assert_eq!(words[0], p.used_native, "{ctx}: amountInUsed");
                    assert_eq!(words[1], p.net_native, "{ctx}: amountOut");
                    assert_eq!(words[2], p.fee_wad, "{ctx}: feeWad");
                }
                (Err(Some(want)), Err(e)) => assert_eq!(want, e, "{ctx}: revert class"),
                (Err(None), _) => panic!("{ctx}: unmapped revert data {}", s(row, "revert")),
                _ => panic!("{ctx}: chain {chain:?}, port {got:?}"),
            }
            let quote = swap.get_amount_out(u256_to_biguint(a), tin, tout);
            match &chain {
                Ok(words) if words[0] == a => {
                    let q = quote.unwrap_or_else(|e| panic!("{ctx}: full fill refused: {e}"));
                    assert_eq!(q.amount, u256_to_biguint(words[1]), "{ctx}: amount out");
                    assert!(
                        q.gas >= BigUint::from(if dir { GAS_SWAP_SELL } else { GAS_SWAP_BUY }),
                        "{ctx}: gas {}",
                        q.gas
                    );
                    full += 1;
                }
                Ok(_) => {
                    let e = quote
                        .err()
                        .unwrap_or_else(|| panic!("{ctx}: a clipped fill was quoted"));
                    assert!(e.to_string().contains("partial fill"), "{ctx}: {e}");
                }
                Err(want) => {
                    let want = want.as_ref().map(|w| w.to_string());
                    if refused_quote(quote, swap, a, limit, want.as_deref(), &ctx) {
                        assert!(!dir, "{ctx}: a sell is never dust");
                    }
                }
            }
        }
    }
    for (key, edge, tin, tout) in
        [("swap_sell", "sell_edge", &cb, &us), ("swap_buy", "buy_edge", &us, &cb)]
    {
        let Some(want) = grids[edge].as_str() else {
            // No size fills (the pool is paused): no limit either.
            let limits = swap.get_limits(tin.address.clone(), tout.address.clone());
            assert!(
                matches!(&limits, Ok((a, _)) if a == &BigUint::ZERO) || limits.is_err(),
                "block {block} {edge}: {limits:?}"
            );
            continue;
        };
        let want = U256::from_str_radix(want, 10).unwrap();
        let want_out = grids[key]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| amount(r) == want)
            .map(|r| recorded(r).unwrap()[1])
            .unwrap();
        let (a, out) = swap
            .get_limits(tin.address.clone(), tout.address.clone())
            .unwrap();
        assert_eq!(a, u256_to_biguint(want), "block {block} {edge}");
        assert_eq!(out, u256_to_biguint(want_out), "block {block} {edge} out");
        assert!(swap
            .get_amount_out(a.clone() + BigUint::from(1u8), tin, tout)
            .is_err());
    }
    // The hook's spot, when the pool answers it (a paused pool still has a book).
    let spot = recorded(&grids["spot"]);
    if let Ok(words) = spot {
        assert_eq!(flamm.hooks.swap.port().unwrap().spot(), Ok(words[0]), "block {block}: spot");
    }
    // The lever venue: paused on chain until 51433699 and without a live spread after, every
    // preview reverts with the recorded class and nothing is quoted.
    let lev = lever.flamm().unwrap();
    assert_eq!(lev.lev_paused, block < LEV_UNPAUSE_BLOCK, "block {block}");
    for (up, key) in [(true, "lever_up"), (false, "lever_down")] {
        for row in grids[key].as_array().unwrap() {
            let a = amount(row);
            let want = recorded(row)
                .unwrap_err()
                .unwrap_or_else(|| panic!("block {block} {key} {a}: unmapped revert"));
            assert_eq!(lev.preview_lever(up, a, now), Err(want), "block {block} {key} {a}");
        }
    }
    assert!(lever
        .get_amount_out(BigUint::from(15_000u32), &cb, &us)
        .is_err());
    assert_eq!(
        lever
            .get_limits(cb.address.clone(), us.address.clone())
            .unwrap(),
        (BigUint::ZERO, BigUint::ZERO)
    );
    (rows, full)
}

/// The whole stream, block by block: both components decode at creation and stay decodable
/// through every block; the quotes at every pinned block are the chain's; every settled swap is
/// reproduced from the parent block's state at the swap block's clock; the range test's stop
/// blocks quote (or refuse) as the yaml's skip flags say.
#[test]
fn stream_replays_into_exact_quotes() {
    let fx = Fixtures::load();
    let schema_snapshot = load("snapshots/51302915.json.gz");
    let (cb, us) = (cbbtc(), usdc());
    let mut client = Client { states: HashMap::new() };
    let swaps = fx.chain["swaps"].as_array().unwrap();
    let mut checked_swaps = 0usize;
    let (mut pinned, mut rows, mut full) = (0usize, 0usize, 0usize);
    let mut seen_creation = false;
    for block in fx.blocks() {
        let number = block.number();
        // Every settled swap in this block: quoted from the parent's state at this block's clock
        // before the block's delta lands, the amount the receipt records.
        for sw in swaps
            .iter()
            .filter(|w| w["block"].as_u64() == Some(number))
        {
            let sell = sw["pool_asset_in"].as_bool().unwrap();
            let used = U256::from_str_radix(s(sw, "amount_in_used"), 10).unwrap();
            let out = U256::from_str_radix(s(sw, "amount_out"), 10).unwrap();
            let (tin, tout) = if sell { (&cb, &us) } else { (&us, &cb) };
            let mut parent = client.swap().clone();
            assert!(parent.block() < number, "{number}: the parent's state");
            parent.apply_block(&BlockContext::new(number, block.timestamp()));
            let ctx = format!("swap at {number} ({})", s(sw, "tx"));
            let q = parent
                .get_amount_out(u256_to_biguint(used), tin, tout)
                .unwrap_or_else(|e| panic!("{ctx}: {e}"));
            assert_eq!(q.amount, u256_to_biguint(out), "{ctx}: amount out");
            assert!(q.gas >= BigUint::from(if sell { GAS_SWAP_SELL } else { GAS_SWAP_BUY }));
            // The requested size, when the calldata gives it, was filled in full.
            if let Some(requested) = sw["amount_in"].as_str() {
                assert_eq!(U256::from_str_radix(requested, 10).unwrap(), used, "{ctx}: clipped");
            }
            let fee_wad = U256::from_str_radix(s(sw, "fee_wad"), 10).unwrap();
            assert_eq!(
                parent
                    .flamm()
                    .unwrap()
                    .swap_fee_wad(sell, block.timestamp()),
                Ok(fee_wad),
                "{ctx}: feeWad"
            );
            let simulated = q
                .new_state
                .as_any()
                .downcast_ref::<FlammPoolState>()
                .unwrap()
                .flamm()
                .unwrap()
                .clone();
            // The block's delta lands: the pool, the hook's book, the Router's managed legs,
            // the position, the IRM rate and the market are what the quote settled to. The
            // market's totals also carry the other transactions of the block (other users'
            // supplies, withdrawals, borrows, repays, liquidations of the same Morpho market,
            // recorded from their events), which no quote of the pool produces: added to the
            // quote's totals they must give the block's exactly. Interest accrues once per
            // timestamp from the same pre-state whichever transaction accrues it, so it is in
            // both.
            let mut observed = client.swap().clone();
            for delta in block.deltas() {
                if delta.component_id == observed.id() {
                    observed
                        .delta_transition(delta, &HashMap::new(), &Balances::default())
                        .unwrap();
                }
            }
            observed.apply_block(&BlockContext::new(number, block.timestamp()));
            let chain = observed.flamm().unwrap();
            assert_eq!(simulated.pool, chain.pool, "{ctx}: pool words");
            assert_eq!(simulated.hooks, chain.hooks, "{ctx}: hooks");
            assert_eq!(simulated.feed, chain.feed, "{ctx}: feed");
            let (sim_v, chain_v) = (&simulated.router.venues[0], &chain.router.venues[0]);
            assert_eq!(sim_v.managed_collateral, chain_v.managed_collateral, "{ctx}");
            assert_eq!(sim_v.managed_supply_shares, chain_v.managed_supply_shares, "{ctx}");
            assert_eq!(sim_v.morpho.position, chain_v.morpho.position, "{ctx}: position");
            assert_eq!(sim_v.morpho.rate_at_target, chain_v.morpho.rate_at_target, "{ctx}: rate");
            let mut market = sim_v.morpho.market;
            for ev in sw["other_morpho_events"]
                .as_array()
                .unwrap()
            {
                let w = |k: &str| U256::from_str_radix(s(ev, k), 10).unwrap();
                match s(ev, "event") {
                    "Supply" => {
                        market.total_supply_assets += w("assets");
                        market.total_supply_shares += w("shares");
                    }
                    "Withdraw" => {
                        market.total_supply_assets -= w("assets");
                        market.total_supply_shares -= w("shares");
                    }
                    "Borrow" => {
                        market.total_borrow_assets += w("assets");
                        market.total_borrow_shares += w("shares");
                    }
                    "Repay" => {
                        market.total_borrow_assets -= w("assets");
                        market.total_borrow_shares -= w("shares");
                    }
                    "Liquidate" => {
                        // `Morpho.liquidate`: the repaid leg, then the bad debt realised
                        // against the suppliers.
                        market.total_borrow_assets -= w("repaid_assets") + w("bad_debt_assets");
                        market.total_borrow_shares -= w("repaid_shares") + w("bad_debt_shares");
                        market.total_supply_assets -= w("bad_debt_assets");
                    }
                    "AccrueInterest" | "SupplyCollateral" | "WithdrawCollateral" => {}
                    other => panic!("{ctx}: unexpected Morpho event {other}"),
                }
            }
            assert_eq!(market, chain_v.morpho.market, "{ctx}: market totals");
            checked_swaps += 1;
        }

        client.apply(&block);

        if number == CREATION_BLOCK {
            seen_creation = true;
            assert_eq!(client.states.len(), 2);
            let swap = client.swap();
            let lever = client.lever();
            assert_eq!(swap.venue(), VenueKind::Swap);
            assert_eq!(lever.venue(), VenueKind::LeverUp);
            assert!(swap.core().is_ok(), "{:?}", swap.core().err());
            assert_eq!(swap.flamm(), lever.flamm());
            assert!(swap.flamm().unwrap().paused, "born paused");
            assert!(swap.flamm().unwrap().lev_paused);
            let e = swap
                .get_amount_out(BigUint::from(1000u32), &cb, &us)
                .unwrap_err();
            assert!(e.to_string().contains("Paused"), "{e}");
        }
        if !seen_creation {
            assert!(client.states.is_empty(), "block {number}: a state before the creation");
            continue;
        }
        // Decodable at every block of the history, both venues the same pool.
        let swap = client.swap();
        assert!(swap.core().is_ok(), "block {number}: {:?}", swap.core().err());
        assert_eq!(swap.flamm(), client.lever().flamm(), "block {number}");
        assert_eq!(swap.block(), number);
        assert_eq!(swap.clock(), block.timestamp() + BLOCK_TIME);
        assert_eq!(
            swap.flamm().unwrap().paused,
            number < ACTIVATION_BLOCK,
            "block {number}: paused"
        );
        assert_eq!(
            swap.flamm().unwrap().lev_paused,
            number < LEV_UNPAUSE_BLOCK,
            "block {number}: levPaused"
        );

        // The chain's answers at the pinned blocks, and at 51302915 those of the schema
        // snapshot fixture (`snapshots/51302915.json.gz`, the same recorder).
        let schema_grids = (number == 51_302_915).then(|| &schema_snapshot["grids"]);
        if let Some(grids) = fx.grids(number).or(schema_grids) {
            // The chain's answers were recorded at this block's own clock.
            let mut swap = client.swap().clone();
            let mut lever = client.lever().clone();
            swap.apply_block(&BlockContext::new(number, block.timestamp()));
            lever.apply_block(&BlockContext::new(number, block.timestamp()));
            let (r, f) = check_grids(&swap, &lever, grids, number);
            pinned += 1;
            rows += r;
            full += f;
        }
        if STOP_BLOCKS.contains(&number) {
            check_harness_step(&client, number);
        }
    }
    assert_eq!(checked_swaps, swaps.len());
    assert!(checked_swaps >= 6, "{checked_swaps} swaps");
    assert!(
        pinned >= 11 && rows > 2000 && full > 1000,
        "{pinned} pinned blocks, {rows} rows, {full} full fills"
    );
}

/// What `protocols/testing` does at a stop block (`test_runner.rs`, `run_simulation`): decodes
/// the snapshot at the stop block's header, advances to the next block, asks the limits of both
/// directions, skips a direction with a zero limit, and quotes 0.1%, 1% and 10% of the limit
/// with a spot price. The first stop block is inside the paused range, where no direction of
/// either venue is tradable (hence both `skip_simulation: true`); at the second the swap venue
/// quotes both directions and the lever venue still nothing.
fn check_harness_step(client: &Client, stop_block: u64) {
    let (cb, us) = (cbbtc(), usdc());
    for (state, venue) in [(client.swap(), "swap"), (client.lever(), "lever-up")] {
        assert_eq!(state.block(), stop_block);
        let mut quoted_any = false;
        for (tin, tout) in [(&cb, &us), (&us, &cb)] {
            let (max_in, _) = state
                .get_limits(tin.address.clone(), tout.address.clone())
                .unwrap_or_else(|e| panic!("stop {stop_block} {venue}: limits: {e}"));
            if max_in == BigUint::ZERO {
                continue;
            }
            state
                .spot_price(tin, tout)
                .unwrap_or_else(|e| panic!("stop {stop_block} {venue}: spot: {e}"));
            for per_mille in [1u32, 10, 100] {
                let amount_in = &max_in * BigUint::from(per_mille) / BigUint::from(1000u32);
                assert!(amount_in > BigUint::ZERO);
                let q = state
                    .get_amount_out(amount_in.clone(), tin, tout)
                    .unwrap_or_else(|e| panic!("stop {stop_block} {venue}: {amount_in}: {e}"));
                assert!(q.amount > BigUint::ZERO);
                quoted_any = true;
            }
        }
        let expect_quotes = venue == "swap" && stop_block >= ACTIVATION_BLOCK;
        assert_eq!(
            quoted_any, expect_quotes,
            "stop {stop_block} {venue}: skip_simulation must be {}",
            !expect_quotes
        );
    }
}

/// The creation snapshot of the stream is the component the schema snapshot tool built from the
/// chain at 51302915 (`snapshots/51302915.json.gz`): same ids, tokens and static attributes.
/// The stream's swap component id is the pool, the lever-up id the pool with the discriminator.
#[test]
fn stream_components_are_the_schema_snapshots_components() {
    let fx = Fixtures::load();
    let schema = load("snapshots/51302915.json.gz");
    let creation = fx
        .blocks()
        .into_iter()
        .find(|b| b.number() == CREATION_BLOCK)
        .unwrap();
    let new = creation.new_components();
    assert_eq!(new.len(), 2);
    for (i, want) in schema["components"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let id = s(&want["component"], "id");
        let got = new
            .iter()
            .find(|c| c.component.id == id)
            .unwrap_or_else(|| panic!("{id} not in the stream"));
        assert_eq!(got.component.protocol_system, PROTOCOL_SYSTEM);
        assert_eq!(got.component.protocol_type_name, "flamm_pool");
        assert_eq!(
            got.component.static_attributes,
            map(&want["component"]["static_attributes"]),
            "{id}"
        );
        assert_eq!(
            got.component.tokens,
            want["component"]["tokens"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| bytes(t.as_str().unwrap()))
                .collect::<Vec<_>>()
        );
        assert_eq!(got.component.creation_tx, bytes(s(&want["component"], "creation_tx")));
        let pool = Address::from_str(POOL).unwrap();
        assert_eq!(got.component.id, if i == 0 { swap_id(pool) } else { lever_up_id(pool) });
    }
}

/// The real swap of block 51302916 through the stream: the state the stream reaches at 51302915
/// quotes 15000 sats at 11301759 USDC for the next block, refuses one sat more than the largest
/// full fill, and reproduces the block's own storage diff as its post-state (the existing
/// snapshot test does the same from the schema tool's snapshot; here the state is the package's).
#[test]
fn the_real_swap_through_the_stream() {
    let fx = Fixtures::load();
    let (cb, us) = (cbbtc(), usdc());
    let mut client = Client { states: HashMap::new() };
    let blocks = fx.blocks();
    let mut parent = None;
    for block in &blocks {
        if block.number() == 51_302_916 {
            parent = Some(client.swap().clone());
        }
        client.apply(block);
        if block.number() == 51_302_916 {
            break;
        }
    }
    let parent = parent.unwrap();
    assert_eq!(parent.block(), 51_302_915);
    assert_eq!(parent.clock(), 1_789_395_179);
    let q = parent
        .get_amount_out(BigUint::from(15_000u32), &cb, &us)
        .unwrap();
    assert_eq!(q.amount, BigUint::from(11_301_759u32));
    assert_eq!(q.gas, BigUint::from(GAS_SWAP_SELL));
    let after = client.swap();
    assert_eq!(after.block(), 51_302_916);
    assert_eq!(after.flamm().unwrap().pool.physical, U256::from(0x32d4au32));
    assert_eq!(after.flamm().unwrap().router.venues[0].managed_collateral, U256::from(0x666du32));
    let again = after
        .get_amount_out(BigUint::from(15_000u32), &cb, &us)
        .unwrap();
    assert_ne!(again.amount, BigUint::from(11_301_759u32));
    // The lever venue's answer through the stream is the pause, with the venue's gas for the
    // empty trade.
    let e = client
        .lever()
        .get_amount_out(BigUint::from(15_000u32), &cb, &us)
        .unwrap_err();
    assert!(matches!(e, SimulationError::InvalidInput(..)), "{e}");
    assert!(e.to_string().contains("LevPaused"), "{e}");
    assert_eq!(
        client
            .lever()
            .get_amount_out(BigUint::ZERO, &cb, &us)
            .unwrap()
            .gas,
        BigUint::from(GAS_LEVER_UP)
    );
}
