//! Kuru parity: logs-derived state + native sim vs the chain, at random recent blocks.
//!
//! Per round: snapshot the market at B0 (Multicall3: getL2Book, getVaultParams,
//! getMarketParams, marketState), replay its logs over (B0, B], and require
//!   1. replayed levels == getL2Book() at B, exactly;
//!   2. sim quote on that state == `placeAndExecuteMarket{Buy,Sell}` eth_call'd from address(0) at
//!      B (the market's own quote path), to the wei, for random sizes on both sides.
//! Vault params are read at B: the indexer takes them from storage diffs, not events.
//!
//!   cargo run -p tycho-simulation --example kuru_parity -- \
//!     --market 0x065c9d28e428a0db40191a54d33d5b7c71a9c394 --rounds 5 --window 300 --samples 20
use alloy::{
    eips::BlockId,
    primitives::{Address, Bytes as ABytes, U256},
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::{
        client::RpcClient,
        types::{Filter, TransactionRequest},
    },
    sol,
    sol_types::{SolCall, SolValue},
    transports::layers::RetryBackoffLayer,
};
use anyhow::{anyhow, Result};
use clap::Parser;
use kuru_book::LevelBook;
use tycho_simulation::evm::protocol::kuru::{
    book::{decode, level_book, levels, state_from_views, IKuru},
    state::KuruState,
};

const MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

sol! {
    struct Call3 { address target; bool allowFailure; bytes callData; }
    struct Result3 { bool success; bytes returnData; }
    function aggregate3(Call3[] calls) external payable returns (Result3[] returnData);
}

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "https://rpc.monad.xyz")]
    rpc: String,
    #[arg(long)]
    market: Address,
    #[arg(long, default_value_t = 5)]
    rounds: usize,
    /// blocks replayed from the snapshot to the checked block
    #[arg(long, default_value_t = 300)]
    window: u64,
    /// random quotes per side per round
    #[arg(long, default_value_t = 20)]
    samples: usize,
    /// how far back the checked blocks may lie
    #[arg(long, default_value_t = 50_000)]
    lookback: u64,
    #[arg(long, default_value_t = 100)]
    log_chunk: u64,
    #[arg(long)]
    seed: Option<u64>,
    /// pin one round to (since, at] instead of random blocks (e.g. an anvil fork)
    #[arg(long, requires = "since")]
    at: Option<u64>,
    #[arg(long)]
    since: Option<u64>,
}

struct Rpc {
    p: DynProvider,
}

impl Rpc {
    async fn eth_call(
        &self,
        from: Option<Address>,
        to: Address,
        data: Vec<u8>,
        block: u64,
    ) -> Result<Vec<u8>> {
        let mut tx = TransactionRequest::default()
            .to(to)
            .input(ABytes::from(data).into());
        if let Some(f) = from {
            tx = tx.from(f);
        }
        Ok(self
            .p
            .call(tx)
            .block(BlockId::number(block))
            .await?
            .to_vec())
    }

    async fn multicall(&self, calls: Vec<(Address, Vec<u8>)>, block: u64) -> Result<Vec<Vec<u8>>> {
        let calls: Vec<Call3> = calls
            .into_iter()
            .map(|(target, d)| Call3 { target, allowFailure: false, callData: ABytes::from(d) })
            .collect();
        let raw = self
            .eth_call(None, MULTICALL3.parse()?, aggregate3Call { calls }.abi_encode(), block)
            .await?;
        Ok(aggregate3Call::abi_decode_returns(&raw)?
            .into_iter()
            .map(|r| r.returnData.to_vec())
            .collect())
    }
}

/// Multicall3 resync: the whole market state at `block` in one eth_call.
async fn snapshot(rpc: &Rpc, m: Address, block: u64) -> Result<KuruState> {
    let r = rpc
        .multicall(
            vec![
                (m, IKuru::getL2BookCall {}.abi_encode()),
                (m, IKuru::getVaultParamsCall {}.abi_encode()),
                (m, IKuru::getMarketParamsCall {}.abi_encode()),
                (m, IKuru::marketStateCall {}.abi_encode()),
            ],
            block,
        )
        .await?;
    let l2 = IKuru::getL2BookCall::abi_decode_returns(&r[0])?;
    let vault = IKuru::getVaultParamsCall::abi_decode_returns(&r[1])?;
    let market = IKuru::getMarketParamsCall::abi_decode_returns(&r[2])?;
    let st = IKuru::marketStateCall::abi_decode_returns(&r[3])?;
    state_from_views(&l2, &vault, &market, st).map_err(|e| anyhow!(e))
}

type Log = (Address, Vec<[u8; 32]>, Vec<u8>);

async fn logs(rpc: &Rpc, m: Address, from: u64, to: u64, chunk: u64) -> Result<Vec<Log>> {
    let mut out = Vec::new();
    let mut a = from;
    while a <= to {
        let b = (a + chunk - 1).min(to);
        let mut v = rpc
            .p
            .get_logs(
                &Filter::new()
                    .address(m)
                    .from_block(a)
                    .to_block(b),
            )
            .await?;
        v.retain(|l| !l.removed);
        v.sort_by_key(|l| (l.block_number, l.log_index));
        out.extend(v.into_iter().map(|l| {
            let topics = l.topics().iter().map(|t| t.0).collect();
            (m, topics, l.data().data.to_vec())
        }));
        a = b + 1;
    }
    Ok(out)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// log-uniform fraction in [1e-6, 1.2]: small fills, whole levels, and past the book
    fn frac(&mut self) -> f64 {
        let x = (self.next() % 1_000_000) as f64 / 1_000_000.0;
        10f64.powf(-6.0 + x * 6.08)
    }
}

fn scale(total: U256, f: f64) -> U256 {
    let ppm = U256::from((f * 1e9) as u64);
    (total * ppm / U256::from(1_000_000_000u64)).max(U256::from(1))
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = Args::parse();
    // Public RPC: throttles are retried with backoff, never read as an answer.
    let client = RpcClient::builder()
        .layer(RetryBackoffLayer::new(8, 500, 300))
        .http(a.rpc.parse()?);
    let rpc = Rpc {
        p: ProviderBuilder::new()
            .connect_client(client)
            .erased(),
    };
    let mut rng = Rng(a.seed.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64 |
            1
    }));
    println!("seed {}", rng.0);
    let head = rpc.p.get_block_number().await?;
    let (mut quotes, mut bad, mut level_bad) = (0usize, 0usize, 0usize);
    for round in 0..a.rounds {
        let (b, b0) = match (a.at, a.since) {
            (Some(b), Some(b0)) => (b, b0),
            _ => {
                let b = head - 5 - rng.next() % a.lookback;
                (b, b - a.window)
            }
        };
        let s0 = snapshot(&rpc, a.market, b0).await?;
        let lg = logs(&rpc, a.market, b0 + 1, b, a.log_chunk).await?;

        let evs = lg
            .iter()
            .filter_map(|(_, topics, data)| {
                topics
                    .first()
                    .map(|t0| decode(*t0, data))
            })
            .filter_map(|r| r.transpose())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!(e))?;
        let mut eb = level_book(&s0).map_err(|e| anyhow!(e))?;
        let need = LevelBook::unseeded(&evs);
        for ids in need.chunks(200) {
            let calls = ids
                .iter()
                .map(|id| {
                    (
                        a.market,
                        IKuru::s_ordersCall { id: alloy::primitives::Uint::from(*id) }.abi_encode(),
                    )
                })
                .collect();
            for (id, raw) in ids
                .iter()
                .zip(rpc.multicall(calls, b0).await?)
            {
                let o = IKuru::s_ordersCall::abi_decode_returns(&raw)?;
                eb.orders
                    .insert(*id, (o.price, o.isBuy, o.size.to()));
            }
        }
        for ev in &evs {
            eb.apply(ev)
                .map_err(|e| anyhow!("round {round} block {b}: {e}"))?;
        }
        let (bids, asks) = (levels(&eb.bids), levels(&eb.asks));

        let chain = snapshot(&rpc, a.market, b).await?;
        let levels_ok = bids == chain.bids && asks == chain.asks;
        if !levels_ok {
            level_bad += 1;
            let diff = |x: &std::collections::BTreeMap<u32, U256>,
                        y: &std::collections::BTreeMap<u32, U256>| {
                x.iter()
                    .filter(|(k, v)| y.get(k) != Some(v))
                    .map(|(k, v)| format!("{k}:{v}/{:?}", y.get(k)))
                    .take(5)
                    .collect::<Vec<_>>()
            };
            println!(
                "  LEVELS DIFF bids {:?} asks {:?}",
                diff(&bids, &chain.bids),
                diff(&asks, &chain.asks)
            );
        }
        let mut state = chain.clone();
        state.bids = bids;
        state.asks = asks;

        let ask_notional: U256 = state
            .asks
            .iter()
            .map(|(p, s)| *s * U256::from(*p) / state.size_precision)
            .sum();
        let bid_size: U256 = state.bids.values().copied().sum();
        let mut round_bad = 0;
        for i in 0..a.samples * 2 {
            let buy = i % 2 == 0;
            // Sample in token units and convert as `KuruState::swap` does: the market takes
            // uint96 amounts in its own precision, and a router hands it whole token units.
            let amt = if buy {
                let tok =
                    scale(ask_notional * state.quote_mult / state.price_precision, rng.frac());
                tok * state.price_precision / state.quote_mult
            } else {
                let tok = scale(bid_size * state.base_mult / state.size_precision, rng.frac());
                tok * state.size_precision / state.base_mult
            };
            let amt = amt.min(U256::from(u128::MAX >> 40)); // uint96
            let (data, sim) = if buy {
                (
                    IKuru::placeAndExecuteMarketBuyCall {
                        quoteSize: amt.to(),
                        minAmountOut: U256::ZERO,
                        isMargin: false,
                        isFillOrKill: false,
                    }
                    .abi_encode(),
                    state.clone().market_buy(amt),
                )
            } else {
                (
                    IKuru::placeAndExecuteMarketSellCall {
                        size: amt.to(),
                        minAmountOut: U256::ZERO,
                        isMargin: false,
                        isFillOrKill: false,
                    }
                    .abi_encode(),
                    state.clone().market_sell(amt),
                )
            };
            let onchain = rpc
                .eth_call(Some(Address::ZERO), a.market, data, b)
                .await
                .map(|r| U256::abi_decode(&r).unwrap_or_default());
            quotes += 1;
            let ok = match (&sim, &onchain) {
                (Ok((_, s)), Ok(c)) => s == c,
                (Err(_), Err(_)) => true,
                _ => false,
            };
            if !ok {
                bad += 1;
                round_bad += 1;
                println!(
                    "  MISMATCH {} amt {amt}: sim {sim:?} chain {onchain:?}",
                    if buy { "buy" } else { "sell" }
                );
            }
        }
        println!(
            "round {round}: B0 {b0} B {b} logs {} seeded {} levels {} (bids {} asks {}) vault_ask_size {} quotes {} bad {round_bad}",
            lg.len(),
            need.len(),
            if levels_ok { "==" } else { "!=" },
            state.bids.len(),
            state.asks.len(),
            state.vault.ask_size,
            a.samples * 2,
        );
    }
    println!("TOTAL quotes {quotes} mismatches {bad} level-diff rounds {level_bad}");
    if bad > 0 || level_bad > 0 {
        std::process::exit(1);
    }
    Ok(())
}
