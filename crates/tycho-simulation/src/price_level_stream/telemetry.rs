//! Metrics of the price level stream, emitted through the `metrics` facade. A consumer that
//! installs a `metrics` recorder receives them with no further setup; without a recorder every
//! call is a no-op. Label values are registered venue names or the enumerations below, never
//! values from the stream, with one exception: an auto-detected venue is named by its address, so
//! its `venue` label carries the address the frame named it by.

use metrics::{counter, gauge, histogram};
use strum_macros::IntoStaticStr;

/// Counter, no labels. Incremented once per frame the tracker accepts.
pub(super) const FRAMES_ACCEPTED: &str = "price_level_stream_frames_accepted_total";
/// Counter, label `reason`, one of [`RejectReason`]. Incremented once per frame the stream drops.
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
/// Counter, label `reason`, one of [`ReconnectReason`]. Incremented once per Titan connection
/// the stream gives up on, or fails to establish, before backing off.
pub(super) const RECONNECTS: &str = "price_level_stream_reconnects_total";
/// Counter, no labels. Incremented once per venue an accepted frame carries books of that is
/// neither registered nor served under auto-detection — once per venue and frame, however many
/// books the frame carried for it.
pub(super) const UNREGISTERED_PAMM_ENTRIES: &str =
    "price_level_stream_unregistered_pamm_entries_total";
/// Counter, label `reason`, one of [`SkipReason`]. Incremented once per book of a served venue
/// whose token addresses do not name a pair this consumer can serve, so the book is not priced at
/// all. Every reason starts at zero. The address itself is never a label — it appears only in the
/// accompanying DEBUG log.
pub(super) const SKIPPED_BOOKS: &str = "price_level_stream_skipped_books_total";
/// Counter, label `reason`, one of [`SideRejectReason`]. Incremented once per book side whose
/// levels are not a usable price function and which is therefore dropped whole, leaving that one
/// trade direction unquotable while the opposite one is still served. Every reason starts at
/// zero.
pub(super) const REJECTED_SIDES: &str = "price_level_stream_rejected_sides_total";

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

/// The `reason` label of [`SKIPPED_BOOKS`]: why a book named no pair this consumer can serve.
/// The label value is the variant name in snake case.
#[derive(Clone, Copy, Debug, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(super) enum SkipReason {
    /// One of the two addresses is not 20 bytes, so it identifies no asset. A producer-side
    /// fault, kept apart from an unadmitted token so it cannot hide inside the ordinary miss rate.
    MalformedAddress,
    /// The consumer's token metadata does not carry one of the two addresses.
    UnadmittedToken,
    /// Both addresses name one asset, so the book names no pair to quote.
    OneAssetPair,
}

impl SkipReason {
    /// Every reason, so [`init_book_counters`] can register all of them. A new variant belongs
    /// here too.
    const ALL: [Self; 3] = [Self::MalformedAddress, Self::UnadmittedToken, Self::OneAssetPair];
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

pub(super) fn record_frame_rejected(reason: RejectReason) {
    counter!(FRAMES_REJECTED, "reason" => <&'static str>::from(reason)).increment(1);
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

pub(super) fn record_reconnect(reason: ReconnectReason) {
    counter!(RECONNECTS, "reason" => <&'static str>::from(reason)).increment(1);
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

/// Registers every `reason` series of [`SKIPPED_BOOKS`] and [`REJECTED_SIDES`] at zero, so a
/// reason that never fires reads as a visible zero instead of a missing series — as the
/// per-venue gauges do for a venue that never appears.
pub(super) fn init_book_counters() {
    for reason in SkipReason::ALL {
        counter!(SKIPPED_BOOKS, "reason" => <&'static str>::from(reason)).increment(0);
    }
    for reason in SideRejectReason::ALL {
        counter!(REJECTED_SIDES, "reason" => <&'static str>::from(reason)).increment(0);
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
    use super::{recorded::*, *};

    /// The metric names and label values are the contract with dashboards and alerts, so the
    /// lookups spell them out instead of reusing the constants and enums the helpers emit under.
    #[test]
    fn every_helper_emits_its_named_metric() {
        let ((), snapshot) = record_async(async {
            record_frame_accepted();
            record_frame_rejected(RejectReason::TooOld);
            record_frame_rejected(RejectReason::TooOld);
            record_frame_age(0.25);
            record_last_seen("fermiswap", 1_700_000_000);
            record_served_components("fermiswap", 3);
            record_stale_removal("fermiswap");
            record_serving_state(ServingState::Serving);
            record_reconnect(ReconnectReason::IdleTimeout);
            record_unregistered_pamm();
            record_skipped_book(SkipReason::MalformedAddress);
            record_rejected_side(SideRejectReason::ConflictingKnots);
        });
        assert_eq!(counter_value(&snapshot, "price_level_stream_frames_accepted_total", &[]), 1);
        assert_eq!(
            counter_value(
                &snapshot,
                "price_level_stream_frames_rejected_total",
                &[("reason", "too_old")]
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
                &[("reason", "idle_timeout")]
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
                &[("reason", "malformed_address")]
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
    }

    /// The `reason` label values of the two book counters are the contract with dashboards, so
    /// they are spelled out here rather than derived from the enums that emit them. Every one
    /// exists at zero before anything is skipped, so a reason that never fires is a visible zero.
    #[test]
    fn book_counters_start_at_zero_under_every_reason() {
        let ((), snapshot) = record_async(async { init_book_counters() });

        for reason in ["malformed_address", "unadmitted_token", "one_asset_pair"] {
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
    }
}
