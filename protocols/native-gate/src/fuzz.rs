use std::{
    collections::{hash_map::DefaultHasher, HashMap},
    hash::{Hash, Hasher},
};

use alloy::primitives::Address;
use num_bigint::BigUint;
use num_traits::{One, Zero};
use rand::{rngs::StdRng, Rng, SeedableRng};
use tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim, Bytes};

use crate::history::History;

/// Above this many whole tokens a limit is a placeholder, so amounts are capped here instead.
const PLAUSIBLE_DECADES: u32 = 12;
/// Points of the native sweep that looks for gas breakpoints.
const SWEEP_POINTS: u32 = 64;
/// Breakpoints refined per direction; each yields the amounts on both sides of it.
const MAX_BREAKPOINTS: usize = 8;

/// Where a candidate amount came from, kept with the result to rank sources by yield.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// Smallest amount whose native gas exceeds that of the amount below, offset by `i64` wei.
    /// Native sims charge gas per tick or bin crossed, so these sit on crossings.
    GasBreakpoint(i64),
    /// A recent transfer size of the given kind, offset by `i64` wei.
    History(&'static str, i64),
    Edge(&'static str),
    Limit(i64),
    Random,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::GasBreakpoint(offset) => write!(f, "gas_breakpoint{offset:+}"),
            Source::History(kind, offset) => write!(f, "history:{kind}{offset:+}"),
            Source::Edge(name) => write!(f, "edge:{name}"),
            Source::Limit(offset) => write!(f, "limit{offset:+}"),
            Source::Random => write!(f, "random"),
        }
    }
}

/// Seed for one pool, direction and block, so a run can be replayed exactly.
pub fn seed(pool: &str, sell: &Token, block: u64) -> u64 {
    let mut hasher = DefaultHasher::new();
    (pool, &sell.address, block).hash(&mut hasher);
    hasher.finish()
}

/// Inputs that shape the candidate draw for one pool direction besides the pool state.
pub struct Context<'a> {
    pub limit: Option<&'a BigUint>,
    pub history: &'a History,
    pub pool: Option<Address>,
    pub prices: &'a HashMap<Bytes, f64>,
    pub seed: u64,
}

/// Share of the budget each source gets; a source that yields fewer amounts leaves its unused
/// share to the others, in this order.
const QUOTAS: [(Bucket, usize); 5] = [
    (Bucket::GasBreakpoint, 30),
    (Bucket::History, 30),
    (Bucket::Edge, 20),
    (Bucket::Random, 12),
    (Bucket::Limit, 8),
];

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Bucket {
    GasBreakpoint,
    History,
    Edge,
    Random,
    Limit,
}

/// Returns up to `budget` distinct non-zero amounts to execute on chain for one direction.
///
/// Draws from gas breakpoints, historical transfer sizes, integer edges, seeded log-uniform
/// amounts and the limit with its neighbours, each up to its share of the budget. Amounts are
/// capped at the plausible size for the token unless they are the limit itself or an
/// `int128`/`uint128` edge, which probe overflow handling.
pub fn candidates(
    state: &dyn ProtocolSim,
    sell: &Token,
    buy: &Token,
    context: &Context,
    budget: usize,
) -> Vec<(Source, BigUint)> {
    let plausible = BigUint::from(10u8).pow(sell.decimals + PLAUSIBLE_DECADES);
    let cap = match context.limit {
        Some(limit) if !limit.is_zero() && *limit < plausible => limit.clone(),
        _ => plausible,
    };
    let mut rng = StdRng::seed_from_u64(context.seed);
    let mut buckets: HashMap<Bucket, Vec<(Source, BigUint)>> = HashMap::new();

    for breakpoint in gas_breakpoints(state, sell, buy, &cap) {
        for offset in [-1i64, 0, 1, 2] {
            if let Some(amount) = offset_amount(&breakpoint, offset) {
                buckets
                    .entry(Bucket::GasBreakpoint)
                    .or_default()
                    .push((Source::GasBreakpoint(offset), amount));
            }
        }
    }
    let history =
        context
            .history
            .samples(context.pool, sell, buy, context.prices, &mut rng, budget);
    buckets
        .entry(Bucket::History)
        .or_default()
        .extend(
            history
                .into_iter()
                .filter(|(_, amount)| *amount <= cap),
        );
    for (name, amount) in integer_edges(sell.decimals) {
        let probes_overflow = name.starts_with("int128") || name.starts_with("uint128");
        if probes_overflow || amount <= cap {
            buckets
                .entry(Bucket::Edge)
                .or_default()
                .push((Source::Edge(name), amount));
        }
    }
    for _ in 0..budget {
        buckets
            .entry(Bucket::Random)
            .or_default()
            .push((Source::Random, log_uniform(&mut rng, &cap)));
    }
    if let Some(limit) = context
        .limit
        .filter(|limit| !limit.is_zero())
    {
        for offset in [-1i64, 0, 1] {
            if let Some(amount) = offset_amount(limit, offset) {
                buckets
                    .entry(Bucket::Limit)
                    .or_default()
                    .push((Source::Limit(offset), amount));
            }
        }
    }

    let mut picked: Vec<(Source, BigUint)> = Vec::with_capacity(budget);
    let mut take = |bucket: &mut Vec<(Source, BigUint)>, want: usize| {
        let mut taken = 0;
        while taken < want && picked.len() < budget {
            let Some((source, amount)) = (!bucket.is_empty()).then(|| bucket.remove(0)) else {
                break;
            };
            if amount.is_zero() ||
                picked
                    .iter()
                    .any(|(_, seen)| *seen == amount)
            {
                continue;
            }
            picked.push((source, amount));
            taken += 1;
        }
    };
    for (bucket, share) in QUOTAS {
        if let Some(values) = buckets.get_mut(&bucket) {
            take(values, budget * share / 100);
        }
    }
    for (bucket, _) in QUOTAS {
        if let Some(values) = buckets.get_mut(&bucket) {
            take(values, budget);
        }
    }
    picked
}

/// Finds amounts where the native gas estimate steps up, by sweeping log-spaced amounts up to
/// `cap` and bisecting each step to the smallest amount with the higher gas. Costs only native
/// quotes.
fn gas_breakpoints(
    state: &dyn ProtocolSim,
    sell: &Token,
    buy: &Token,
    cap: &BigUint,
) -> Vec<BigUint> {
    let gas = |amount: &BigUint| {
        state
            .get_amount_out(amount.clone(), sell, buy)
            .ok()
            .map(|result| result.gas)
    };
    let bits = cap.bits().max(1);
    let mut sweep: Vec<BigUint> = (0..=SWEEP_POINTS)
        .map(|i| {
            let exponent = bits as f64 * f64::from(i) / f64::from(SWEEP_POINTS);
            float_to_biguint(2f64.powf(exponent)).min(cap.clone())
        })
        .filter(|amount| !amount.is_zero())
        .collect();
    sweep.dedup();

    let mut breakpoints = Vec::new();
    for pair in sweep.windows(2) {
        let (mut low, mut high) = (pair[0].clone(), pair[1].clone());
        let (Some(low_gas), Some(high_gas)) = (gas(&low), gas(&high)) else { continue };
        if high_gas <= low_gas {
            continue;
        }
        while &high - &low > BigUint::one() {
            let mid = (&low + &high) / 2u8;
            match gas(&mid) {
                Some(mid_gas) if mid_gas > low_gas => high = mid,
                Some(_) => low = mid,
                None => break,
            }
        }
        breakpoints.push(high);
        if breakpoints.len() == MAX_BREAKPOINTS {
            break;
        }
    }
    breakpoints
}

fn integer_edges(decimals: u32) -> Vec<(&'static str, BigUint)> {
    let unit = BigUint::from(10u8).pow(decimals);
    let two = BigUint::from(2u8);
    let mut edges = vec![
        ("dust:1", BigUint::one()),
        ("dust:2", two.clone()),
        ("dust:3", BigUint::from(3u8)),
        ("dust:1000", BigUint::from(1000u16)),
        ("unit-1", &unit - BigUint::one()),
        ("unit", unit.clone()),
        ("unit+1", &unit + BigUint::one()),
    ];
    for (name, k) in [("2^64-1", 64u32), ("2^96-1", 96), ("2^112-1", 112)] {
        edges.push((name, two.pow(k) - BigUint::one()));
    }
    edges.push(("int128max", two.pow(127) - BigUint::one()));
    edges.push(("int128max+1", two.pow(127)));
    edges.push(("uint128max", two.pow(128) - BigUint::one()));
    edges.push(("uint128max+1", two.pow(128)));
    edges
}

fn log_uniform(rng: &mut StdRng, cap: &BigUint) -> BigUint {
    let bits = cap.bits().max(1) as f64;
    float_to_biguint(2f64.powf(rng.gen_range(0.0..bits))).min(cap.clone())
}

fn float_to_biguint(value: f64) -> BigUint {
    BigUint::parse_bytes(format!("{value:.0}").as_bytes(), 10).unwrap_or_default()
}

pub fn offset_amount(amount: &BigUint, offset: i64) -> Option<BigUint> {
    let magnitude = BigUint::from(offset.unsigned_abs());
    if offset >= 0 {
        Some(amount + magnitude)
    } else if *amount > magnitude {
        Some(amount - magnitude)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use tycho_common::{models::Chain, Bytes};
    use tycho_simulation::evm::protocol::uniswap_v2::state::UniswapV2State;

    use super::*;

    fn token(byte: u8, decimals: u32) -> Token {
        Token::new(&Bytes::from(vec![byte; 20]), "T", decimals, 0, &[], Chain::Base, 100)
    }

    fn pool() -> UniswapV2State {
        UniswapV2State::new(
            U256::from(10u64).pow(U256::from(24)),
            U256::from(10u64).pow(U256::from(12)),
        )
    }

    fn context(limit: &BigUint) -> Context<'_> {
        static HISTORY: std::sync::LazyLock<History> = std::sync::LazyLock::new(History::default);
        static PRICES: std::sync::LazyLock<HashMap<Bytes, f64>> =
            std::sync::LazyLock::new(HashMap::new);
        Context { limit: Some(limit), history: &HISTORY, pool: None, prices: &PRICES, seed: 7 }
    }

    #[test]
    fn test_candidates_replay_from_the_same_seed() {
        let (sell, buy) = (token(1, 18), token(2, 6));
        let limit = BigUint::from(10u64).pow(23);

        let first = candidates(&pool(), &sell, &buy, &context(&limit), 40);
        let second = candidates(&pool(), &sell, &buy, &context(&limit), 40);

        assert_eq!(first, second);
    }

    #[test]
    fn test_candidates_stay_under_the_limit_except_limit_and_overflow_probes() {
        let (sell, buy) = (token(1, 18), token(2, 6));
        let limit = BigUint::from(10u64).pow(23);

        let picked = candidates(&pool(), &sell, &buy, &context(&limit), 100);

        for (source, amount) in &picked {
            let allowed_above = matches!(source, Source::Limit(1)) ||
                matches!(source, Source::Edge(name) if name.contains("128"));
            assert!(amount <= &limit || allowed_above, "{source} = {amount} exceeds the limit");
            assert!(!amount.is_zero());
        }
        assert!(picked.contains(&(Source::Limit(1), &limit + BigUint::one())));
    }

    #[test]
    fn test_constant_gas_pool_has_no_gas_breakpoints() {
        let (sell, buy) = (token(1, 18), token(2, 6));

        let breakpoints = gas_breakpoints(&pool(), &sell, &buy, &BigUint::from(10u64).pow(23));

        assert!(breakpoints.is_empty());
    }

    #[test]
    fn test_offset_amount_never_reaches_zero() {
        assert_eq!(offset_amount(&BigUint::one(), -1), None);
        assert_eq!(offset_amount(&BigUint::from(5u8), -1), Some(BigUint::from(4u8)));
        assert_eq!(offset_amount(&BigUint::from(5u8), 2), Some(BigUint::from(7u8)));
    }
}
