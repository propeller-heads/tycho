//! Metrics of the listing doors' feed, emitted through the `metrics` facade. A consumer that
//! installs a `metrics` recorder receives them with no further setup; without a recorder every
//! call is a no-op. Label values are the enumerations below, never a listing, a desk, an address
//! or anything read from the node or a state URL.

use metrics::{counter, histogram};
use strum::IntoEnumIterator;
use strum_macros::{EnumIter, IntoStaticStr};

use super::overrides::NotTaken;

/// Counter, label `outcome`, one of [`PollOutcome`]. Incremented once per poll, so the sum over
/// `outcome` is the polls made. Every outcome starts at zero.
pub(super) const POLLS: &str = "efficient_prop_polls_total";
/// Counter, no labels. Incremented once per listing and poll, on a chain whose builders land
/// makers' updates, whose door the poll's read holds: the next block's state answer covers the
/// listing and its door was opened, asked at this poll or answered from the last poll that
/// sampled it. Counted when the read ends, so a poll that fails before then counts none. A door
/// read is not yet a book: one that fills nothing publishes none.
pub(super) const LISTINGS_COVERED: &str = "efficient_prop_listings_covered_total";
/// Counter, label `reason`, one of [`NotCovered`]. Incremented once per poll and listing the
/// front has reserved that the poll's read leaves out before its door is sampled, on a chain
/// whose builders land makers' updates. The reasons follow the read's order: what the first
/// request rules out (`desk_not_approved`, `no_gas`, `credit_mode`, `tip_cap_under_floor`), a
/// token the feed was not given (`unknown_token`), then the next block's state (`no_entry` is a
/// maker that sent no update for the block; the others up to `other_answer` say an update came
/// and could not be used), then `not_opened`. Each is counted where the read finds it, so a poll
/// that fails later has counted those it found, and a poll that finds no state map for the next
/// block (see `no_state_map` on [`POLLS`]) counts what its first request rules out and no more.
/// Every reason starts at zero.
pub(super) const LISTINGS_NOT_COVERED: &str = "efficient_prop_listings_not_covered_total";
/// Histogram, no labels. The age of the node's answer at each poll, in seconds: the wall clock at
/// the poll's start less the time of the node's latest block. Recorded as zero where that block's
/// time is ahead of the local clock, so every observation is zero or more and an exported sum
/// only rises: a clock skew that way shows as a run of zeros, not as its size. Under one slot on
/// a node at the chain's head; more after a missed slot or on a node that lags.
pub(super) const ANSWER_AGE: &str = "efficient_prop_answer_age_seconds";

/// How a poll ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoStaticStr, EnumIter)]
#[strum(serialize_all = "snake_case")]
pub(super) enum PollOutcome {
    /// Every door served was asked its quotes.
    Sampled,
    /// No door was asked: nothing its answers are read from had changed since the last poll that
    /// sampled them, whose samples it published again.
    Reused,
    /// No state URL answered a map for the block after the node's latest: nothing was served.
    NoStateMap,
    /// The poll failed and published nothing.
    Failed,
}

/// Why a poll does not serve a listing the front has reserved, on a chain whose builders land
/// makers' updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoStaticStr, EnumIter)]
#[strum(serialize_all = "snake_case")]
pub(super) enum NotCovered {
    /// Its maker's desk contract is not approved.
    DeskNotApproved,
    /// It has a credit mode, or trades a stock token, and the caller named no gas for it.
    NoGas,
    /// Its credit mode is one the feed does not serve.
    CreditMode,
    /// It caps the taker's tip under the feed's floor.
    TipCapUnderFloor,
    /// The feed was not given one of its tokens.
    UnknownToken,
    /// The listing is bound to no quote contract.
    NoQuoteContract,
    /// The node did not answer where its quote sits, or a word of its record.
    Unread,
    /// See [`NotTaken::NoEntry`].
    NoEntry,
    /// See [`NotTaken::Stale`].
    Stale,
    /// See [`NotTaken::Stopped`].
    Stopped,
    /// See [`NotTaken::OtherEpoch`].
    OtherEpoch,
    /// See [`NotTaken::Foreign`].
    Foreign,
    /// See [`NotTaken::NotHeld`].
    NotHeld,
    /// See [`NotTaken::Conflict`].
    Conflict,
    /// See [`NotTaken::Malformed`].
    Malformed,
    /// See [`NotTaken::Missing`].
    MissingPriceContract,
    /// See [`NotTaken::OtherAnswer`].
    OtherAnswer,
    /// A state answer covers it, and the read holds no door of it: it has no door yet, its door
    /// does not answer as this front's, or its record rules it out (a price source not approved,
    /// a tied listing, a session with a closed phase, a word of it unread).
    NotOpened,
}

impl From<NotTaken> for NotCovered {
    fn from(not_taken: NotTaken) -> Self {
        match not_taken {
            NotTaken::NoEntry => Self::NoEntry,
            NotTaken::Stale => Self::Stale,
            NotTaken::Stopped => Self::Stopped,
            NotTaken::OtherEpoch => Self::OtherEpoch,
            NotTaken::Foreign => Self::Foreign,
            NotTaken::NotHeld => Self::NotHeld,
            NotTaken::Conflict => Self::Conflict,
            NotTaken::Malformed => Self::Malformed,
            NotTaken::Missing(_) => Self::MissingPriceContract,
            NotTaken::OtherAnswer(_) => Self::OtherAnswer,
        }
    }
}

impl NotCovered {
    /// The reason as the poll's DEBUG log states it.
    pub(super) fn why(self) -> &'static str {
        match self {
            Self::DeskNotApproved => "its desk contract is not approved",
            Self::NoGas => "it has a credit mode or trades a stock token, and no gas is named",
            Self::CreditMode => "its credit mode is not served",
            Self::TipCapUnderFloor => "it caps the tip under the floor",
            Self::UnknownToken => "one of its tokens is not in the feed's",
            Self::NoQuoteContract => "it has no quote contract",
            Self::Unread => "its quote's place or its record is unread",
            Self::NoEntry => "its maker has no update in the next block's state map",
            Self::Stale => {
                "its maker's update in the next block's state map stamps a block it does not fill \
                 in"
            }
            Self::Stopped => "its desk is stopped at the head",
            Self::OtherEpoch => {
                "its maker's update in the next block's state map is of another desk epoch than \
                 the head's"
            }
            Self::Foreign => {
                "an answer wrote a slot of its desk contract that is the front's, and no answer \
                 covered it"
            }
            Self::NotHeld => {
                "its maker's word carried from the head block is not the word the head holds"
            }
            Self::Conflict => {
                "an answer's entries disagree on a slot of its maker's update or of a price \
                 contract it reads"
            }
            Self::Malformed => {
                "every answer with its maker's update has an entry that could not be read whole"
            }
            Self::MissingPriceContract => {
                "the answer with its update does not write a price contract it reads"
            }
            Self::OtherAnswer => "slots it reads are taken from another answer",
            Self::NotOpened => "its door is not open, or its record rules it out",
        }
    }
}

pub(super) fn record_poll(outcome: PollOutcome) {
    counter!(POLLS, "outcome" => <&'static str>::from(outcome)).increment(1);
}

pub(super) fn record_listings_covered(listings: usize) {
    counter!(LISTINGS_COVERED).increment(u64::try_from(listings).unwrap_or(u64::MAX));
}

pub(super) fn record_listing_not_covered(reason: NotCovered) {
    record_listings_not_covered(reason, 1);
}

pub(super) fn record_listings_not_covered(reason: NotCovered, listings: usize) {
    counter!(LISTINGS_NOT_COVERED, "reason" => <&'static str>::from(reason))
        .increment(u64::try_from(listings).unwrap_or(u64::MAX));
}

/// Records an answer's age, zero where it is negative (see [`ANSWER_AGE`]).
pub(super) fn record_answer_age(age_seconds: f64) {
    histogram!(ANSWER_AGE).record(age_seconds.max(0.0));
}

/// Registers every counter at zero, so a series exists before its first event and a rate over it
/// is defined from the feed's start.
pub(super) fn init() {
    for outcome in PollOutcome::iter() {
        counter!(POLLS, "outcome" => <&'static str>::from(outcome)).increment(0);
    }
    counter!(LISTINGS_COVERED).increment(0);
    for reason in NotCovered::iter() {
        counter!(LISTINGS_NOT_COVERED, "reason" => <&'static str>::from(reason)).increment(0);
    }
}
