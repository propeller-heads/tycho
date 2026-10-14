//! Metrics of the price level stream, emitted through the `metrics` facade. A consumer that
//! installs a `metrics` recorder receives them with no further setup; without a recorder every
//! call is a no-op. Label values are registered venue names, the name of one of the two Titan
//! sockets ([`TitanFrame::SOCKET`](super::titan::TitanFrame::SOCKET)), or the enumerations below,
//! never values from the stream, with one exception: an auto-detected venue is named by its
//! address, so its `venue` label carries the address the frame named it by.

use metrics::{counter, gauge, histogram};
use strum::IntoEnumIterator;
use strum_macros::IntoStaticStr;

use super::{
    overrides::IngestEvent,
    titan::{TitanFrame, TitanOverrideMessage, TitanPriceLevelMessage},
};

/// The two `socket` label values, taken from the frame types that name them so that a socket is
/// named identically in a log and in a series.
const PRICE_LEVEL_SOCKET: &str = TitanPriceLevelMessage::SOCKET;
const OVERRIDE_SOCKET: &str = TitanOverrideMessage::SOCKET;

/// Counter, no labels. Incremented once per frame the tracker accepts.
pub(super) const FRAMES_ACCEPTED: &str = "price_level_stream_frames_accepted_total";
/// Counter, labels `socket` and `reason`, one of [`RejectReason`]. Incremented once per frame the
/// stream drops. `socket` names which Titan socket sent it: only `parse_error` reaches this
/// counter from the override socket, because every other reason is a freshness check the tracker
/// makes of a price level frame.
pub(super) const FRAMES_REJECTED: &str = "price_level_stream_frames_rejected_total";
/// Histogram, no labels. The age of every accepted frame at acceptance, in seconds: the local
/// wall clock minus the frame's `timestamp`, negative when the frame is stamped ahead of
/// the local clock. It measures Titan's lag plus delivery delay, and a clock skew between Titan
/// and this host; a frame older than one slot yields states that refuse to quote.
pub(super) const FRAME_AGE: &str = "price_level_stream_frame_age_seconds";
/// Gauge, label `venue`. The frame's `timestamp` of the newest accepted frame that carried the
/// venue, in seconds since the Unix epoch; 0 until the first such frame. `venue` is the
/// registered venue name, or the address of an auto-detected venue.
pub(super) const LAST_SEEN: &str = "price_level_stream_last_seen_timestamp_seconds";
/// Gauge, label `venue`. The number of components the stream currently serves for the venue;
/// 0 for every registered venue from the start. `venue` is as on `LAST_SEEN`.
pub(super) const SERVED_COMPONENTS: &str = "price_level_stream_served_components";
/// Counter, label `venue`. Incremented once per component that turns stale at its `stale_at`
/// deadline and is emitted in `removed_pairs`. `venue` is as on `LAST_SEEN`.
pub(super) const STALE_REMOVALS: &str = "price_level_stream_stale_removals_total";
/// Gauge, no labels. The stream's serving state as a [`ServingState`] number: 0 = unserved,
/// 1 = serving.
pub(super) const SERVING_STATE: &str = "price_level_stream_serving_state";
/// Counter, labels `socket` and `reason`, one of [`ReconnectReason`]. Incremented once per Titan
/// connection the stream gives up on, or fails to establish, before backing off. Both sockets are
/// maintained by one function, so `socket` is what tells an override socket that keeps dropping
/// from the price level socket flapping — only the second puts quoting at risk.
pub(super) const RECONNECTS: &str = "price_level_stream_reconnects_total";
/// Counter, no labels. Incremented once per venue an accepted frame carries books of that is
/// neither registered nor served under auto-detection — once per venue and frame, however many
/// books the frame carried for it.
pub(super) const UNREGISTERED_PAMM_ENTRIES: &str =
    "price_level_stream_unregistered_pamm_entries_total";
/// Counter, label `reason`, one of [`SkipReason`]. Incremented once per book of a served venue
/// whose token symbols do not name a pair this consumer can serve, so the book is not priced at
/// all. Every reason starts at zero. The symbol itself is never a label — it appears only in the
/// accompanying DEBUG log.
pub(super) const SKIPPED_BOOKS: &str = "price_level_stream_skipped_books_total";
/// Counter, label `reason`, one of [`SideRejectReason`]. Incremented once per book side whose
/// levels are not a usable price function and which is therefore dropped whole, leaving that one
/// trade direction unquotable while the opposite one is still served. Every reason starts at
/// zero.
pub(super) const REJECTED_SIDES: &str = "price_level_stream_rejected_sides_total";
/// Counter, label `outcome`, one of [`MatchOutcome`]. Incremented once per pair whose ladders were
/// joined to the snapshots the override socket delivered — so `matched` against the rest is the
/// share of served pairs that carry the simulated pre-state they were priced on. A pair whose
/// books named no build at all is no join and is counted here under no outcome; it shows on
/// [`INVALID_PROVENANCE`] when the reference was unreadable rather than absent. The unit is one
/// **pair**, not one book — see [`INVALID_PROVENANCE`]. Every outcome starts at zero while
/// stamping is on.
pub(super) const OVERRIDE_MATCHES: &str = "price_level_stream_override_match_total";
/// Counter, label `event`, one of [`IngestEvent`]. Incremented once per event an ingest of the
/// override buffer produced: one per admission the frame attempted, plus what its rotating sweep
/// dropped. It counts **buffer events, not lookup misses** — an eviction here says the buffer
/// dropped a build it held, not that a ladder failed to find one, which is [`OVERRIDE_MATCHES`].
///
/// The seventeen events are a deliberate cardinality choice: they are a fixed, stream-independent
/// vocabulary, and the buffer fails closed in enough distinct ways (a contradiction, the stale
/// builds that contradiction bars, a saturated key, an exhausted buffer) that collapsing them
/// would leave an operator unable to tell a misbehaving producer from a buffer too small for the
/// traffic. Every event starts at zero while stamping is on.
pub(super) const OVERRIDE_BUFFER: &str = "price_level_stream_override_buffer_total";
/// Counter, label `reason`, one of [`EntryRefusalReason`]. Incremented once per venue entry the
/// validation refuses whole, before the buffer ever sees it: the pairs it carried go unstamped for
/// the rest of their build. Counted apart from [`OVERRIDE_BUFFER`] because a payload that
/// contradicts the documented shape is a producer-side fault, where a buffer event is this
/// process's own retention rule at work. Every reason starts at zero while stamping is on.
pub(super) const REFUSED_OVERRIDE_ENTRIES: &str =
    "price_level_stream_refused_override_entries_total";
/// Counter, no labels. Incremented once per **served** book whose `calculated_from` this parser
/// cannot read as a pair version: provenance is read only after a book's symbols resolve to a pair
/// this consumer can serve, so a book counted on [`SKIPPED_BOOKS`] never reaches this counter.
/// An absent reference is **not** counted either: the two are indistinguishable downstream, and
/// only this one says the producer sent something the parser does not understand, so counting both
/// would hide a parse regression inside the ordinary miss rate. Starts at zero.
///
/// The unit is one **book**, where [`OVERRIDE_MATCHES`] counts one **pair** — a frame quoting a
/// pair twice contributes two here and one there — so the two must not be ratioed against each
/// other.
pub(super) const INVALID_PROVENANCE: &str = "price_level_stream_invalid_provenance_total";

/// The `reason` label of [`FRAMES_REJECTED`]: why the stream dropped a frame. The label value
/// is the variant name in snake case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(super) enum RejectReason {
    /// The text did not parse as a frame.
    ParseError,
    /// The frame's `timestamp` is `stale_after` old or older.
    TooOld,
    /// The frame's `timestamp` is more than one slot ahead of the local wall clock.
    InFuture,
    /// The frame's `timestamp` is older than the newest accepted frame's.
    OutOfOrder,
    /// The block is below the newest accepted block.
    BlockRegression,
    /// The block is above the newest accepted block by more than one block per elapsed slot
    /// plus 2.
    BlockJump,
}

impl RejectReason {
    /// Every reason, so [`init_socket_counters`] can register all of them against the price level
    /// socket. A new variant belongs here too.
    const ALL: [Self; 6] = [
        Self::ParseError,
        Self::TooOld,
        Self::InFuture,
        Self::OutOfOrder,
        Self::BlockRegression,
        Self::BlockJump,
    ];
}

/// The `reason` label of [`RECONNECTS`]: why the stream gave up on a Titan connection. The
/// label value is the variant name in snake case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(super) enum ReconnectReason {
    /// No frame parsed within the idle timeout.
    IdleTimeout,
    /// The server hung up without a close frame.
    Ended,
    /// The server sent a close frame.
    Closed,
    /// A transport or protocol error.
    ReadError,
    /// The connection was refused, or the TLS handshake failed.
    ConnectFailed,
    /// The handshake did not complete within the connect timeout.
    ConnectTimeout,
}

impl ReconnectReason {
    /// Every reason, so [`init_socket_counters`] can register all of them against each socket this
    /// consumer reads. A new variant belongs here too.
    const ALL: [Self; 6] = [
        Self::IdleTimeout,
        Self::Ended,
        Self::Closed,
        Self::ReadError,
        Self::ConnectFailed,
        Self::ConnectTimeout,
    ];
}

/// The `reason` label of [`SKIPPED_BOOKS`]: why a book's symbols named no pair this consumer can
/// serve. The label value is the variant name in snake case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(super) enum SkipReason {
    /// No binding names one of the symbols, so nothing authenticates an address for it.
    UnboundSymbol,
    /// A symbol is bound, but the consumer's token metadata does not carry the bound address.
    UnadmittedToken,
    /// Both symbols resolved to one asset, so the book names no pair to quote.
    OneAssetPair,
}

impl SkipReason {
    /// Every reason, so [`init_book_counters`] can register all of them. A new variant belongs
    /// here too.
    const ALL: [Self; 3] = [Self::UnboundSymbol, Self::UnadmittedToken, Self::OneAssetPair];
}

/// The `reason` label of [`REJECTED_SIDES`]: why a book side was dropped whole. The label value
/// is the variant name in snake case. Mirrors the conversion's own rejection reasons, which
/// [`tracker`](super::tracker) maps onto these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(super) enum SideRejectReason {
    /// A level's stream price or volume is not a finite positive number.
    UnsupportedLevel,
    /// A level's amount overflowed `f64` to infinity.
    NonFiniteAmount,
    /// A level rounds to a zero amount, which is not a quote.
    ZeroAmount,
    /// Two levels round to the same input amount but disagree on the output.
    ConflictingKnots,
}

impl SideRejectReason {
    /// Every reason, so [`init_book_counters`] can register all of them. A new variant belongs
    /// here too.
    const ALL: [Self; 4] =
        [Self::UnsupportedLevel, Self::NonFiniteAmount, Self::ZeroAmount, Self::ConflictingKnots];
}

/// The `outcome` label of [`OVERRIDE_MATCHES`]: what joining one pair's ladders to the snapshots
/// held for its venue reached. The label value is the variant name in snake case. Mirrors the
/// buffer's own `MatchResult`, which [`tracker`](super::tracker) maps onto these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(super) enum MatchOutcome {
    /// The pair's ladders carry the snapshot the venue priced them on.
    Matched,
    /// No build of that pair version is held.
    Missing,
    /// The pair's books did not agree on one version, so nothing attributes the served ladders to
    /// a single snapshot.
    Ambiguous,
    /// A build of it was held, and its one slot of lifetime has passed.
    Expired,
    /// Two payloads claimed to be the build that priced it.
    Poisoned,
}

impl MatchOutcome {
    /// Every outcome, so [`init_override_counters`] can register all of them. A new variant
    /// belongs here too.
    const ALL: [Self; 5] =
        [Self::Matched, Self::Missing, Self::Ambiguous, Self::Expired, Self::Poisoned];
}

/// The `reason` label of [`REFUSED_OVERRIDE_ENTRIES`]: why a venue entry was refused whole. The
/// label value is the variant name in snake case. Mirrors the validation's own `EntryRejection`,
/// which [`tracker`](super::tracker) maps onto these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(super) enum EntryRefusalReason {
    /// The entry's venue is not an address, so it cannot be attributed to a venue at all.
    NotAVenue,
    /// An overridden account is not keyed by an address.
    NotAnAccount,
    /// A pair names something that is not an address.
    MalformedPair,
    /// A storage slot or a stored word is not a 32-byte EVM word.
    MalformedStorage,
    /// A balance or a nonce is not a hex quantity, or does not fit the field that carries it.
    MalformedQuantity,
    /// The entry named no build stamp, so nothing orders one build of its snapshot against
    /// another.
    Unstamped,
    /// The pairs and the sequences disagree in length, leaving it unknown which sequence belongs
    /// to which pair.
    SequenceMismatch,
    /// The entry carried no pair, or no account: nothing to match, or nothing to simulate.
    NoData,
    /// An account overrode its code, or replaced its whole storage instead of diffing it.
    UnsupportedOverride,
    /// The snapshot is past a per-entry cap on accounts, storage words or storage bytes.
    OverCap,
}

impl EntryRefusalReason {
    /// Every reason, so [`init_override_counters`] can register all of them. A new variant belongs
    /// here too.
    const ALL: [Self; 10] = [
        Self::NotAVenue,
        Self::NotAnAccount,
        Self::MalformedPair,
        Self::MalformedStorage,
        Self::MalformedQuantity,
        Self::Unstamped,
        Self::SequenceMismatch,
        Self::NoData,
        Self::UnsupportedOverride,
        Self::OverCap,
    ];
}

/// The stream's serving state, exported as the numeric value of `SERVING_STATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ServingState {
    /// No component is served.
    Unserved = 0,
    /// At least one component is served.
    Serving = 1,
}

pub(super) fn record_frame_accepted() {
    counter!(FRAMES_ACCEPTED).increment(1);
}

pub(super) fn record_frame_rejected(socket: &'static str, reason: RejectReason) {
    counter!(FRAMES_REJECTED, "socket" => socket, "reason" => <&'static str>::from(reason))
        .increment(1);
}

pub(super) fn record_frame_age(age_seconds: f64) {
    histogram!(FRAME_AGE).record(age_seconds);
}

pub(super) fn record_last_seen(venue: &str, unix_seconds: u64) {
    gauge!(LAST_SEEN, "venue" => venue.to_string()).set(unix_seconds as f64);
}

pub(super) fn record_served_components(venue: &str, count: usize) {
    gauge!(SERVED_COMPONENTS, "venue" => venue.to_string()).set(count as f64);
}

pub(super) fn record_stale_removal(venue: &str) {
    counter!(STALE_REMOVALS, "venue" => venue.to_string()).increment(1);
}

/// Registers the [`STALE_REMOVALS`] series of `venue` at zero, so a venue that never loses a
/// component to staleness reads as a visible zero rather than a missing series — matching the
/// per-venue gauges, which are set to zero for the same reason.
pub(super) fn init_stale_removals(venue: &str) {
    counter!(STALE_REMOVALS, "venue" => venue.to_string()).increment(0);
}

pub(super) fn record_serving_state(state: ServingState) {
    gauge!(SERVING_STATE).set(state as u8 as f64);
}

pub(super) fn record_reconnect(socket: &'static str, reason: ReconnectReason) {
    counter!(RECONNECTS, "socket" => socket, "reason" => <&'static str>::from(reason)).increment(1);
}

pub(super) fn record_unregistered_pamm() {
    counter!(UNREGISTERED_PAMM_ENTRIES).increment(1);
}

pub(super) fn record_skipped_book(reason: SkipReason) {
    counter!(SKIPPED_BOOKS, "reason" => <&'static str>::from(reason)).increment(1);
}

pub(super) fn record_rejected_side(reason: SideRejectReason) {
    counter!(REJECTED_SIDES, "reason" => <&'static str>::from(reason)).increment(1);
}

pub(super) fn record_override_match(outcome: MatchOutcome) {
    counter!(OVERRIDE_MATCHES, "outcome" => <&'static str>::from(outcome)).increment(1);
}

pub(super) fn record_override_buffer_event(event: IngestEvent) {
    counter!(OVERRIDE_BUFFER, "event" => <&'static str>::from(event)).increment(1);
}

pub(super) fn record_refused_override_entry(reason: EntryRefusalReason) {
    counter!(REFUSED_OVERRIDE_ENTRIES, "reason" => <&'static str>::from(reason)).increment(1);
}

pub(super) fn record_invalid_provenance() {
    counter!(INVALID_PROVENANCE).increment(1);
}

/// Registers every `reason` series of [`SKIPPED_BOOKS`] and [`REJECTED_SIDES`], and
/// [`INVALID_PROVENANCE`], at zero, so a reason that never fires reads as a visible zero instead
/// of a missing series — as the per-venue gauges do for a venue that never appears.
///
/// Every one of these is read off a book of the price level socket, which this consumer reads
/// whether or not it stamps anything, so all of them are registered unconditionally.
pub(super) fn init_book_counters() {
    for reason in SkipReason::ALL {
        counter!(SKIPPED_BOOKS, "reason" => <&'static str>::from(reason)).increment(0);
    }
    for reason in SideRejectReason::ALL {
        counter!(REJECTED_SIDES, "reason" => <&'static str>::from(reason)).increment(0);
    }
    counter!(INVALID_PROVENANCE).increment(0);
}

/// Registers every series of the override path at zero, for the same reason: a join that never
/// happens must read as a zero rather than a gap.
///
/// Called only by a consumer that stamps. With stamping off no override socket is connected, so
/// none of these can ever move, and a flat zero would advertise a path this process does not run.
///
/// The buffer's events are enumerated from the enum rather than listed here, so a variant added to
/// [`IngestEvent`] cannot miss its zero.
pub(super) fn init_override_counters() {
    for outcome in MatchOutcome::ALL {
        counter!(OVERRIDE_MATCHES, "outcome" => <&'static str>::from(outcome)).increment(0);
    }
    for event in IngestEvent::iter() {
        counter!(OVERRIDE_BUFFER, "event" => <&'static str>::from(event)).increment(0);
    }
    for reason in EntryRefusalReason::ALL {
        counter!(REFUSED_OVERRIDE_ENTRIES, "reason" => <&'static str>::from(reason)).increment(0);
    }
}

/// Registers the rejection and reconnect series of the sockets this consumer reads, at zero, so a
/// socket that has never failed reads as zeros rather than as a gap — the distinction an operator
/// needs to tell a healthy socket from one that was never configured.
///
/// Asymmetric on purpose. `parse_error` is the only [`RejectReason`] the override socket can ever
/// carry, because every other one is a freshness check [`tracker`](super::tracker) makes of a price
/// level frame; registering the rest against it would advertise five series no code path can reach.
/// Reconnects, by contrast, are recorded by the one function that maintains both sockets, so every
/// reason belongs to each socket that exists.
pub(super) fn init_socket_counters(override_socket: bool) {
    for reason in RejectReason::ALL {
        let reason = <&'static str>::from(reason);
        counter!(FRAMES_REJECTED, "socket" => PRICE_LEVEL_SOCKET, "reason" => reason).increment(0);
    }
    for reason in ReconnectReason::ALL {
        let reason = <&'static str>::from(reason);
        counter!(RECONNECTS, "socket" => PRICE_LEVEL_SOCKET, "reason" => reason).increment(0);
    }
    if !override_socket {
        return;
    }
    let parse_error = <&'static str>::from(RejectReason::ParseError);
    counter!(FRAMES_REJECTED, "socket" => OVERRIDE_SOCKET, "reason" => parse_error).increment(0);
    for reason in ReconnectReason::ALL {
        let reason = <&'static str>::from(reason);
        counter!(RECONNECTS, "socket" => OVERRIDE_SOCKET, "reason" => reason).increment(0);
    }
}

/// Reads a `DebuggingRecorder` snapshot back into plain values for assertions.
#[cfg(test)]
pub(super) mod recorded {
    use std::{
        collections::{BTreeMap, HashMap},
        future::Future,
    };

    use metrics_util::{
        debugging::{DebugValue, DebuggingRecorder, Snapshot},
        MetricKind,
    };

    pub(in super::super) type SnapshotMap =
        HashMap<(MetricKind, String, BTreeMap<String, String>), DebugValue>;

    /// Runs `future` on a current-thread runtime with a `DebuggingRecorder` installed and
    /// returns its output together with every metric it recorded. `metrics::with_local_recorder`
    /// takes a sync closure, which is why this cannot be a `#[tokio::test]`.
    pub(in super::super) fn record_async<T>(future: impl Future<Output = T>) -> (T, SnapshotMap) {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = metrics::with_local_recorder(&recorder, || runtime.block_on(future));
        (output, snapshot_map(snapshotter.snapshot()))
    }

    pub(in super::super) fn snapshot_map(snapshot: Snapshot) -> SnapshotMap {
        snapshot
            .into_vec()
            .into_iter()
            .map(|(composite_key, _unit, _description, value)| {
                let name = composite_key.key().name().to_string();
                let labels = composite_key
                    .key()
                    .labels()
                    .map(|label| (label.key().to_string(), label.value().to_string()))
                    .collect();
                ((composite_key.kind(), name, labels), value)
            })
            .collect()
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    pub(in super::super) fn counter_value(
        snapshot: &SnapshotMap,
        name: &str,
        label_pairs: &[(&str, &str)],
    ) -> u64 {
        match snapshot.get(&(MetricKind::Counter, name.to_string(), labels(label_pairs))) {
            Some(DebugValue::Counter(value)) => *value,
            Some(DebugValue::Gauge(_)) | Some(DebugValue::Histogram(_)) | None => 0,
        }
    }

    /// Whether the counter series was registered at all, whatever its value. A series registered
    /// at zero and a series that was never touched both read as 0 through [`counter_value`], so
    /// only this tells a visible zero from a missing series.
    pub(in super::super) fn counter_registered(
        snapshot: &SnapshotMap,
        name: &str,
        label_pairs: &[(&str, &str)],
    ) -> bool {
        snapshot.contains_key(&(MetricKind::Counter, name.to_string(), labels(label_pairs)))
    }

    /// How many series of `name` the snapshot holds, whatever their labels. Paired with per-value
    /// lookups it pins a label vocabulary exactly rather than as a lower bound.
    pub(in super::super) fn registered_series(snapshot: &SnapshotMap, name: &str) -> usize {
        snapshot
            .keys()
            .filter(|(_kind, series, _labels)| series == name)
            .count()
    }

    /// Every label value recorded on any metric, for checking that no value from the stream became
    /// a label.
    pub(in super::super) fn all_label_values(snapshot: &SnapshotMap) -> Vec<&str> {
        snapshot
            .keys()
            .flat_map(|(_kind, _name, labels)| labels.values().map(String::as_str))
            .collect()
    }

    pub(in super::super) fn gauge_value(
        snapshot: &SnapshotMap,
        name: &str,
        label_pairs: &[(&str, &str)],
    ) -> f64 {
        match snapshot.get(&(MetricKind::Gauge, name.to_string(), labels(label_pairs))) {
            Some(DebugValue::Gauge(value)) => value.into_inner(),
            Some(DebugValue::Counter(_)) | Some(DebugValue::Histogram(_)) | None => f64::NAN,
        }
    }

    /// Every value recorded on the histogram, in recording order; empty when nothing was
    /// recorded.
    pub(in super::super) fn histogram_values(
        snapshot: &SnapshotMap,
        name: &str,
        label_pairs: &[(&str, &str)],
    ) -> Vec<f64> {
        match snapshot.get(&(MetricKind::Histogram, name.to_string(), labels(label_pairs))) {
            Some(DebugValue::Histogram(values)) => values
                .iter()
                .map(|value| value.into_inner())
                .collect(),
            Some(DebugValue::Counter(_)) | Some(DebugValue::Gauge(_)) | None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    // The enum's own count, which only the vocabulary test reads.
    use strum::EnumCount;

    use super::{recorded::*, *};

    /// The metric names and label values are the contract with dashboards and alerts, so the
    /// lookups spell them out instead of reusing the constants and enums the helpers emit under.
    #[test]
    fn every_helper_emits_its_named_metric() {
        let ((), snapshot) = record_async(async {
            record_frame_accepted();
            record_frame_rejected("price_levels", RejectReason::TooOld);
            record_frame_rejected("price_levels", RejectReason::TooOld);
            record_frame_age(0.25);
            record_last_seen("fermiswap", 1_700_000_000);
            record_served_components("fermiswap", 3);
            record_stale_removal("fermiswap");
            record_serving_state(ServingState::Serving);
            record_reconnect("quote_stream", ReconnectReason::IdleTimeout);
            record_unregistered_pamm();
            record_skipped_book(SkipReason::UnboundSymbol);
            record_rejected_side(SideRejectReason::ConflictingKnots);
            record_override_match(MatchOutcome::Matched);
            record_override_buffer_event(IngestEvent::ConflictPoisoned);
            record_refused_override_entry(EntryRefusalReason::SequenceMismatch);
            record_invalid_provenance();
        });
        assert_eq!(counter_value(&snapshot, "price_level_stream_frames_accepted_total", &[]), 1);
        assert_eq!(
            counter_value(
                &snapshot,
                "price_level_stream_frames_rejected_total",
                &[("socket", "price_levels"), ("reason", "too_old")]
            ),
            2
        );
        assert_eq!(
            histogram_values(&snapshot, "price_level_stream_frame_age_seconds", &[]),
            vec![0.25]
        );
        assert_eq!(
            gauge_value(
                &snapshot,
                "price_level_stream_last_seen_timestamp_seconds",
                &[("venue", "fermiswap")]
            ),
            1_700_000_000.0
        );
        assert_eq!(
            gauge_value(
                &snapshot,
                "price_level_stream_served_components",
                &[("venue", "fermiswap")]
            ),
            3.0
        );
        assert_eq!(
            counter_value(
                &snapshot,
                "price_level_stream_stale_removals_total",
                &[("venue", "fermiswap")]
            ),
            1
        );
        assert_eq!(gauge_value(&snapshot, "price_level_stream_serving_state", &[]), 1.0);
        assert_eq!(
            counter_value(
                &snapshot,
                "price_level_stream_reconnects_total",
                &[("socket", "quote_stream"), ("reason", "idle_timeout")]
            ),
            1
        );
        assert_eq!(
            counter_value(&snapshot, "price_level_stream_unregistered_pamm_entries_total", &[]),
            1
        );
        assert_eq!(
            counter_value(
                &snapshot,
                "price_level_stream_skipped_books_total",
                &[("reason", "unbound_symbol")]
            ),
            1
        );
        assert_eq!(
            counter_value(
                &snapshot,
                "price_level_stream_rejected_sides_total",
                &[("reason", "conflicting_knots")]
            ),
            1
        );
        assert_eq!(
            counter_value(
                &snapshot,
                "price_level_stream_override_match_total",
                &[("outcome", "matched")]
            ),
            1
        );
        assert_eq!(
            counter_value(
                &snapshot,
                "price_level_stream_override_buffer_total",
                &[("event", "conflict_poisoned")]
            ),
            1
        );
        assert_eq!(
            counter_value(
                &snapshot,
                "price_level_stream_refused_override_entries_total",
                &[("reason", "sequence_mismatch")]
            ),
            1
        );
        assert_eq!(counter_value(&snapshot, "price_level_stream_invalid_provenance_total", &[]), 1);
    }

    /// The `reason` label values of the two book counters are the contract with dashboards, so
    /// they are spelled out here rather than derived from the enums that emit them. Every one
    /// exists at zero before anything is skipped, so a reason that never fires is a visible zero.
    #[test]
    fn book_counters_start_at_zero_under_every_reason() {
        let ((), snapshot) = record_async(async { init_book_counters() });

        for reason in ["unbound_symbol", "unadmitted_token", "one_asset_pair"] {
            let labels = [("reason", reason)];
            assert!(
                counter_registered(&snapshot, SKIPPED_BOOKS, &labels),
                "skipped_books {reason} is a missing series"
            );
            assert_eq!(counter_value(&snapshot, SKIPPED_BOOKS, &labels), 0);
        }
        for reason in ["unsupported_level", "non_finite_amount", "zero_amount", "conflicting_knots"]
        {
            let labels = [("reason", reason)];
            assert!(
                counter_registered(&snapshot, REJECTED_SIDES, &labels),
                "rejected_sides {reason} is a missing series"
            );
            assert_eq!(counter_value(&snapshot, REJECTED_SIDES, &labels), 0);
        }
        assert!(
            counter_registered(&snapshot, INVALID_PROVENANCE, &[]),
            "invalid_provenance_total is a missing series"
        );
        assert_eq!(counter_value(&snapshot, INVALID_PROVENANCE, &[]), 0);
    }

    /// The label values of the three override counters, spelled out for the same reason: they are
    /// the contract with dashboards, and every one of them exists at zero before anything is
    /// joined or admitted, so an outcome that never happens is a visible zero.
    ///
    /// Each counter's series are also counted, so the vocabulary is pinned exactly rather than as
    /// a lower bound: a variant renamed under the counter, or an extra series registered beside
    /// the named ones, fails here. The `event` list is every variant of `IngestEvent` in snake
    /// case, checked against `IngestEvent::COUNT` so that a variant *added* to the enum fails here
    /// too, naming the list that has to grow rather than showing up as a series-count mismatch.
    #[test]
    fn override_counters_start_at_zero_under_every_label() {
        let ((), snapshot) = record_async(async { init_override_counters() });
        let outcomes = ["matched", "missing", "ambiguous", "expired", "poisoned"];
        let events = [
            "admitted",
            "restamped",
            "replayed",
            "older_build",
            "conflict_poisoned",
            "rejected_poisoned_build",
            "rejected_older_than_poison",
            "rejected_spent_replay",
            "rejected_future_skew",
            "rejected_expired",
            "rejected_key_saturated",
            "rejected_keys_exhausted",
            "rejected_frame_entry_cap",
            "rejected_entry_pair_cap",
            "degenerate_pair",
            "evicted_entry",
            "evicted_key",
            "swept_expired",
        ];
        let refusals = [
            "not_a_venue",
            "not_an_account",
            "malformed_pair",
            "malformed_storage",
            "malformed_quantity",
            "unstamped",
            "sequence_mismatch",
            "no_data",
            "unsupported_override",
            "over_cap",
        ];

        assert_eq!(
            events.len(),
            IngestEvent::COUNT,
            "an IngestEvent variant is missing from this list"
        );
        for (name, key, values) in [
            (OVERRIDE_MATCHES, "outcome", outcomes.as_slice()),
            (OVERRIDE_BUFFER, "event", events.as_slice()),
            (REFUSED_OVERRIDE_ENTRIES, "reason", refusals.as_slice()),
        ] {
            for value in values {
                let labels = [(key, *value)];
                assert!(
                    counter_registered(&snapshot, name, &labels),
                    "{name} {value} is a missing series"
                );
                assert_eq!(counter_value(&snapshot, name, &labels), 0, "{name} {value}");
            }
            assert_eq!(
                registered_series(&snapshot, name),
                values.len(),
                "{name} registered a series its label vocabulary does not name"
            );
        }
    }

    /// The socket series are registered for exactly what each socket can record, so a socket that
    /// has never failed reads as zeros while one that was never configured has no series at all.
    ///
    /// The asymmetry is the point: `parse_error` is the only rejection reason the override socket
    /// can carry, so the other five must not exist under it. The counts pin that exactly — any
    /// cross-product would register more series than there are reachable ones.
    #[rstest]
    #[case::with_the_override_socket(true)]
    #[case::price_level_socket_only(false)]
    fn socket_counters_start_at_zero_for_every_reachable_series(#[case] override_socket: bool) {
        let ((), snapshot) = record_async(async { init_socket_counters(override_socket) });
        let reject_reasons = [
            "parse_error",
            "too_old",
            "in_future",
            "out_of_order",
            "block_regression",
            "block_jump",
        ];
        let reconnect_reasons =
            ["idle_timeout", "ended", "closed", "read_error", "connect_failed", "connect_timeout"];

        for reason in reject_reasons {
            let labels = [("socket", "price_levels"), ("reason", reason)];
            assert!(counter_registered(&snapshot, FRAMES_REJECTED, &labels), "{reason}");
            assert_eq!(counter_value(&snapshot, FRAMES_REJECTED, &labels), 0, "{reason}");
        }
        for reason in reconnect_reasons {
            for socket in ["price_levels", "quote_stream"] {
                let labels = [("socket", socket), ("reason", reason)];
                let reachable = socket == "price_levels" || override_socket;
                assert_eq!(
                    counter_registered(&snapshot, RECONNECTS, &labels),
                    reachable,
                    "{socket} {reason}"
                );
            }
        }
        assert_eq!(
            counter_registered(
                &snapshot,
                FRAMES_REJECTED,
                &[("socket", "quote_stream"), ("reason", "parse_error")]
            ),
            override_socket
        );
        for reason in ["too_old", "in_future", "out_of_order", "block_regression", "block_jump"] {
            assert!(
                !counter_registered(
                    &snapshot,
                    FRAMES_REJECTED,
                    &[("socket", "quote_stream"), ("reason", reason)]
                ),
                "{reason} cannot be recorded against the override socket, yet it is registered"
            );
        }
        assert_eq!(
            registered_series(&snapshot, FRAMES_REJECTED),
            if override_socket { reject_reasons.len() + 1 } else { reject_reasons.len() }
        );
        assert_eq!(
            registered_series(&snapshot, RECONNECTS),
            if override_socket { reconnect_reasons.len() * 2 } else { reconnect_reasons.len() }
        );
    }
}
