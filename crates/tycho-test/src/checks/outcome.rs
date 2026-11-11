//! Which check ran, and how it ended, as protocol-testing reports them. Which checks a runner runs,
//! and in which mode, is the runner's: protocol-testing's README maps them to its modes.

use std::fmt;

use serde::Serialize;

/// A check, named by [`CheckId::as_str`].
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
    /// A component declared `tvl_only` quotes the same with its balances scaled down.
    BalancesTvlOnly,
    /// The snapshot decodes, and every expected component decodes from it.
    Decode,
    /// Limits, spot prices and quotes along the ladder, in every direction.
    SimLadder,
    /// Executing the ladder's quotes on chain matches the simulation.
    ExecLadder,
    /// Every direction quotes and executes at its limit.
    LimitsAtLimit,
    /// Past the limit, the venue behaves as the declared kind claims.
    LimitsPastLimit,
    /// The quote at the limit pays no more than the limit's max output.
    LimitsMaxOutput,
    /// The state streamed through a range quotes exactly as the snapshot at its stop block.
    StreamMatchesSnapshot,
    /// The streamed range exercised as much as a floor asks.
    StreamCoverage,
}

impl CheckId {
    /// The three checks of each direction's limit.
    pub const LIMITS: [CheckId; 3] =
        [CheckId::LimitsAtLimit, CheckId::LimitsPastLimit, CheckId::LimitsMaxOutput];

    /// The check's name, as a report writes it: `sim.ladder`, `limits.at_limit`, ...
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
    /// The check found a problem.
    Fail {
        /// What the problem is.
        detail: String,
    },
    /// The check was turned off, for `reason`.
    Skipped {
        reason: String,
    },
    /// An earlier check failed and this one needs its result.
    NotRun {
        reason: String,
    },
    /// The check found a problem that does not fail the test.
    Warn {
        /// What the problem is.
        detail: String,
    },
}

/// One check of one test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    /// The test's name.
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
    /// For a `limits.*` result, the declaration it is checked against and the limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<LimitReport>,
}

/// The declaration a `limits.*` result is checked against, and the limit it tested, whatever the
/// outcome, so a reader sees which directions pass on a `soft` or `under_cap` claim and how large
/// a non-hard limit is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LimitReport {
    /// `hard`, `soft` or `under_cap`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// A hard limit's declared rounding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rounding: Option<u64>,
    /// Which declaration applies: `component`, `component_default`, `package` or
    /// `package_default`.
    pub declared_by: String,
    /// `get_limits`' max input, after the `int128` cap, when the direction was quoted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output: Option<String>,
    /// Whether the `int128` cap replaced a larger limit.
    pub capped: bool,
}
