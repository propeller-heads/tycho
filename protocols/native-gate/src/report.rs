use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use alloy::rpc::types::Block;
use miette::{IntoDiagnostic, WrapErr};
use num_bigint::{BigInt, BigUint};
use num_traits::{ToPrimitive, Zero};
use serde::Serialize;
use tycho_common::{
    models::token::Token,
    simulation::{errors::SimulationError, protocol_sim::ProtocolSim},
    Bytes,
};
use tycho_simulation::protocol::models::ProtocolComponent;

/// One swap to compare: a native quote, waiting for its on-chain execution.
pub struct Point {
    pub component: ProtocolComponent,
    pub state: Box<dyn ProtocolSim>,
    pub sell: Token,
    pub buy: Token,
    pub rung: String,
    pub amount_in: BigUint,
    native: Result<(BigUint, BigUint), String>,
    /// Output of the partial swap a native `InvalidInput` error carries, if any.
    partial: Option<BigUint>,
}

impl Point {
    pub fn new(
        component: &ProtocolComponent,
        state: &dyn ProtocolSim,
        sell: &Token,
        buy: &Token,
        rung: String,
        amount_in: BigUint,
        native: Result<tycho_common::simulation::protocol_sim::GetAmountOutResult, SimulationError>,
    ) -> Self {
        let partial = match &native {
            Err(SimulationError::InvalidInput(_, Some(result))) => Some(result.amount.clone()),
            _ => None,
        };
        Self {
            partial,
            component: component.clone(),
            state: state.clone_box(),
            sell: sell.clone(),
            buy: buy.clone(),
            rung,
            amount_in,
            native: native
                .map(|result| (result.amount, result.gas))
                .map_err(|e| format!("{e:?}")),
        }
    }

    pub fn native_amount_out(&self) -> Option<&BigUint> {
        self.native
            .as_ref()
            .ok()
            .map(|(amount, _)| amount)
    }

    pub fn gas(&self) -> BigUint {
        self.native
            .as_ref()
            .map(|(_, gas)| gas.clone())
            .unwrap_or_default()
    }
}

/// What the chain did with a point's swap.
pub enum Outcome {
    Pending,
    Executed { amount_out: BigUint, gas_used: u64 },
    Reverted(String),
    EncodeFailed(String),
    SimFailed(String),
}

/// How a native quote compares with the chain.
#[derive(Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    /// Same output to the wei.
    Exact,
    /// Both sides refuse the swap, or both give nothing for it.
    BothReject,
    /// Both execute, outputs differ.
    AmountMismatch,
    /// Native quotes, the chain reverts.
    ChainReverts,
    /// Native refuses, the chain executes.
    NativeRejects,
    /// Native refuses the full amount but its partial result equals what the chain executed:
    /// both stop at the same liquidity exhaustion.
    PartialFill,
    /// Outputs differ by the same ratio at every non-dust amount of a pool direction: a token
    /// transfer tax the sim does not model, not a math error.
    Tax,
    /// The swap could not be encoded or simulated, so nothing was compared.
    Unverified,
}

#[derive(Serialize)]
struct FailureRecord<'a> {
    verdict: Verdict,
    block: u64,
    block_hash: String,
    timestamp: u64,
    protocol: &'a str,
    component: String,
    sell: String,
    buy: String,
    rung: &'a str,
    amount_in: String,
    native: Result<String, &'a str>,
    chain: Result<String, String>,
    /// Native minus chain output, when both executed.
    diff: Option<String>,
    gas_native: Option<String>,
    gas_chain: Option<u64>,
    /// Transfer tax of the sell and buy token in basis points, as Tycho's token analysis found.
    tax_bps: (u64, u64),
}

#[derive(Serialize, Default)]
struct ProtocolTally {
    /// Points per verdict.
    verdicts: BTreeMap<String, u64>,
    /// Points per verdict for each ladder rung, to tell limit edges from interior failures.
    by_rung: BTreeMap<String, BTreeMap<String, u64>>,
}

/// Accumulates verdicts and writes the failure log and the running summary.
pub struct Report {
    dir: PathBuf,
    failures: BufWriter<File>,
    tallies: BTreeMap<String, ProtocolTally>,
    blocks: Vec<u64>,
}

impl Report {
    pub fn create(dir: &Path) -> miette::Result<Self> {
        fs::create_dir_all(dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to create {}", dir.display()))?;
        let failures = File::create(dir.join("failures.jsonl")).into_diagnostic()?;
        Ok(Self {
            dir: dir.to_path_buf(),
            failures: BufWriter::new(failures),
            tallies: BTreeMap::new(),
            blocks: Vec::new(),
        })
    }

    pub fn record(
        &mut self,
        block: &Block,
        points: Vec<Point>,
        outcomes: Vec<Outcome>,
    ) -> miette::Result<()> {
        assert_eq!(points.len(), outcomes.len(), "every point needs exactly one outcome");
        self.blocks.push(block.header.number);
        let mut judged: Vec<_> = points
            .iter()
            .zip(outcomes)
            .map(|(point, outcome)| judge(point, outcome))
            .collect();
        label_taxes(&points, &mut judged);
        for (point, (verdict, chain, gas_chain)) in points.iter().zip(judged) {
            let tally = self
                .tallies
                .entry(point.component.protocol_system.clone())
                .or_default();
            let key = format!("{verdict:?}");
            *tally
                .verdicts
                .entry(key.clone())
                .or_default() += 1;
            *tally
                .by_rung
                .entry(point.rung.clone())
                .or_default()
                .entry(key)
                .or_default() += 1;
            if matches!(verdict, Verdict::Exact | Verdict::BothReject | Verdict::PartialFill) {
                continue;
            }
            let diff = match (&point.native, &chain) {
                (Ok((native, _)), Ok(chain)) => {
                    Some((BigInt::from(native.clone()) - BigInt::from(chain.clone())).to_string())
                }
                _ => None,
            };
            let record = FailureRecord {
                verdict,
                block: block.header.number,
                block_hash: block.header.hash.to_string(),
                timestamp: block.header.timestamp,
                protocol: &point.component.protocol_system,
                component: point.component.id.to_string(),
                sell: format!("{} {}", point.sell.symbol, point.sell.address),
                buy: format!("{} {}", point.buy.symbol, point.buy.address),
                rung: &point.rung,
                amount_in: point.amount_in.to_string(),
                native: point
                    .native
                    .as_ref()
                    .map(|(amount, _)| amount.to_string())
                    .map_err(String::as_str),
                chain: chain.map(|amount| amount.to_string()),
                diff,
                gas_native: point
                    .native
                    .as_ref()
                    .ok()
                    .map(|(_, gas)| gas.to_string()),
                gas_chain,
                tax_bps: (point.sell.tax, point.buy.tax),
            };
            serde_json::to_writer(&mut self.failures, &record).into_diagnostic()?;
            self.failures
                .write_all(b"\n")
                .into_diagnostic()?;
        }
        self.failures.flush().into_diagnostic()
    }

    pub fn write_summary(&self) -> miette::Result<()> {
        let summary = serde_json::json!({
            "checked_blocks": self.blocks,
            "protocols": self.tallies,
        });
        fs::write(
            self.dir.join("summary.json"),
            serde_json::to_vec_pretty(&summary).into_diagnostic()?,
        )
        .into_diagnostic()
    }
}

/// Amounts below this many raw units are left out of the tax test, where rounding dominates.
const TAX_MIN_AMOUNT: u64 = 1_000_000;
/// How far apart the chain/native ratios of one direction may be and still count as one tax.
const TAX_RATIO_SPREAD: f64 = 5e-4;

/// Relabels the amount mismatches of a pool direction as [`Verdict::Tax`] when the chain pays a
/// constant fraction less than native at every non-dust amount, with at least three such points
/// and no exact match among them.
fn label_taxes(points: &[Point], judged: &mut [(Verdict, Result<BigUint, String>, Option<u64>)]) {
    let mut by_direction: HashMap<(&Bytes, &Bytes), Vec<usize>> = HashMap::new();
    for (index, point) in points.iter().enumerate() {
        by_direction
            .entry((&point.component.id, &point.sell.address))
            .or_default()
            .push(index);
    }
    for indices in by_direction.into_values() {
        let mut ratios = Vec::new();
        let mut exact_seen = false;
        for &index in &indices {
            let (Ok((native, _)), Ok(chain)) = (&points[index].native, &judged[index].1) else {
                continue;
            };
            if *native < BigUint::from(TAX_MIN_AMOUNT) {
                continue;
            }
            match judged[index].0 {
                Verdict::Exact => exact_seen = true,
                Verdict::AmountMismatch => {
                    if let (Some(native), Some(chain)) = (native.to_f64(), chain.to_f64()) {
                        ratios.push((index, chain / native));
                    }
                }
                _ => {}
            }
        }
        if exact_seen || ratios.len() < 3 {
            continue;
        }
        let (low, high) = ratios
            .iter()
            .fold((f64::MAX, f64::MIN), |(low, high), (_, r)| (low.min(*r), high.max(*r)));
        if high < 1.0 && high - low <= TAX_RATIO_SPREAD {
            for (index, _) in ratios {
                judged[index].0 = Verdict::Tax;
            }
        }
    }
}

/// Revert reasons that come from the harness or the router's input checks, not from the pool:
/// such a swap never reached the protocol, so it proves nothing about the quote.
const HARNESS_REVERTS: [&str; 4] = [
    "Transaction failed",
    "TychoRouter__InvalidMinAmountOut",
    "TychoRouter__AmountOutZero",
    "TychoRouter__ZeroInput",
];

fn judge(point: &Point, outcome: Outcome) -> (Verdict, Result<BigUint, String>, Option<u64>) {
    let outcome = match outcome {
        Outcome::Reverted(reason)
            if HARNESS_REVERTS
                .iter()
                .any(|r| reason.contains(r)) =>
        {
            Outcome::SimFailed(reason)
        }
        outcome => outcome,
    };
    match (&point.native, outcome) {
        (Ok((native, _)), Outcome::Executed { amount_out, gas_used }) => {
            let verdict =
                if *native == amount_out { Verdict::Exact } else { Verdict::AmountMismatch };
            (verdict, Ok(amount_out), Some(gas_used))
        }
        (Err(_), Outcome::Executed { amount_out, gas_used })
            if point.partial.as_ref() == Some(&amount_out) =>
        {
            (Verdict::PartialFill, Ok(amount_out), Some(gas_used))
        }
        (Err(_), Outcome::Executed { amount_out, gas_used }) => {
            (Verdict::NativeRejects, Ok(amount_out), Some(gas_used))
        }
        // Harness reverts are filtered out above, so this revert came from the pool or router
        // settlement: a zero quote and a pool that pays nothing agree.
        (Ok((native, _)), Outcome::Reverted(reason)) if native.is_zero() => {
            (Verdict::BothReject, Err(reason), None)
        }
        (Ok(_), Outcome::Reverted(reason)) => (Verdict::ChainReverts, Err(reason), None),
        (Err(_), Outcome::Reverted(reason)) => (Verdict::BothReject, Err(reason), None),
        (_, Outcome::EncodeFailed(e) | Outcome::SimFailed(e)) => {
            (Verdict::Unverified, Err(e), None)
        }
        (_, Outcome::Pending) => (Verdict::Unverified, Err("no simulation result".into()), None),
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use tycho_common::models::Chain;
    use tycho_simulation::evm::protocol::uniswap_v2::state::UniswapV2State;

    use super::*;

    fn point(native: u64) -> Point {
        let token =
            |byte: u8| Token::new(&Bytes::from(vec![byte; 20]), "T", 6, 0, &[], Chain::Base, 100);
        Point {
            component: ProtocolComponent::new(
                Bytes::from(vec![9u8; 20]),
                "uniswap_v2".into(),
                "pool".into(),
                Chain::Base,
                vec![token(1), token(2)],
                vec![],
                HashMap::new(),
                Bytes::default(),
                Default::default(),
            ),
            state: Box::new(UniswapV2State::new(U256::from(1u8), U256::from(1u8))),
            sell: token(1),
            buy: token(2),
            rung: "random".into(),
            amount_in: BigUint::from(1u8),
            native: Ok((BigUint::from(native), BigUint::ZERO)),
            partial: None,
        }
    }

    fn mismatch(chain: u64) -> (Verdict, Result<BigUint, String>, Option<u64>) {
        (Verdict::AmountMismatch, Ok(BigUint::from(chain)), Some(0))
    }

    #[test]
    fn test_constant_shortfall_at_every_amount_is_a_tax() {
        let points = [point(10_000_000), point(50_000_000), point(900_000_000)];
        // The chain pays 2% less everywhere, give or take a rounding unit.
        let mut judged = vec![mismatch(9_800_000), mismatch(49_000_001), mismatch(882_000_000)];

        label_taxes(&points, &mut judged);

        assert!(judged
            .iter()
            .all(|(verdict, _, _)| *verdict == Verdict::Tax));
    }

    #[test]
    fn test_varying_shortfall_stays_a_mismatch() {
        let points = [point(10_000_000), point(50_000_000), point(900_000_000)];
        let mut judged = vec![mismatch(9_800_000), mismatch(49_900_000), mismatch(882_000_000)];

        label_taxes(&points, &mut judged);

        assert!(judged
            .iter()
            .all(|(verdict, _, _)| *verdict == Verdict::AmountMismatch));
    }
}
