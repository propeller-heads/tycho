//! The vocabulary every runner reports checks in: which check, and how it ended.

use std::fmt;

use serde::Serialize;

/// The checks a test runs, named the way the report and the yaml refer to them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckId {
    /// Indexing the range and finding the block to read the snapshot at.
    Index,
    /// Every expected component is indexed, with its tokens and static attributes.
    StateComponents,
    /// No excluded component is indexed.
    StateExcluded,
    /// Indexed component balances match the chain.
    Balances,
    /// Strict: a component declared `tvl_only` quotes the same with its balances halved.
    BalancesTvlOnly,
    /// The snapshot decodes, and every expected component decodes from it.
    Decode,
    /// Limits, spot prices and quotes along the ladder, in every direction.
    SimLadder,
    /// Executing the ladder's quotes on chain matches the simulation.
    ExecLadder,
    /// Strict: every direction quotes and executes at its limit.
    LimitsAtLimit,
    /// Strict: past a hard limit, the quote errors and the execution reverts.
    LimitsPastLimit,
    /// Strict: the quote at the limit pays no more than the limit's max output.
    LimitsMaxOutput,
    /// Strict, streamed: the state streamed through the range quotes exactly as the snapshot at
    /// the stop block.
    StreamMatchesSnapshot,
    /// Strict, streamed: the range exercised as much as the floor asks. Report-only for now.
    StreamCoverage,
}

impl CheckId {
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckId::Index => "index",
            CheckId::StateComponents => "state.components",
            CheckId::StateExcluded => "state.excluded",
            CheckId::Balances => "balances",
            CheckId::BalancesTvlOnly => "balances.tvl_only",
            CheckId::Decode => "decode",
            CheckId::SimLadder => "sim.ladder",
            CheckId::ExecLadder => "exec.ladder",
            CheckId::LimitsAtLimit => "limits.at_limit",
            CheckId::LimitsPastLimit => "limits.past_limit",
            CheckId::LimitsMaxOutput => "limits.max_output",
            CheckId::StreamMatchesSnapshot => "stream.matches_snapshot",
            CheckId::StreamCoverage => "stream.coverage",
        }
    }
}

impl fmt::Display for CheckId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for CheckId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// How a check ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    Fail {
        detail: String,
    },
    /// The package's configuration turned the check off.
    Skipped {
        reason: String,
    },
    /// An earlier check failed and this one needs its result.
    NotRun {
        reason: String,
    },
    /// A check that only reports for now: it found a problem, which does not fail the test.
    Warn {
        detail: String,
    },
}

/// One check of one test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    pub test: String,
    pub check: CheckId,
    /// The component the result is about, when it is about one only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    /// The swap direction the result is about, `0x<token in>->0x<token out>`, when it is about one
    /// only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// The block a streamed check ran at; none for the checks at the snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block: Option<u64>,
    #[serde(flatten)]
    pub outcome: Outcome,
}
