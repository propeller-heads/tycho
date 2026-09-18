//! Metrics of the price level stream, emitted through the `metrics` facade. A consumer that
//! installs a `metrics` recorder receives them with no further setup; without a recorder every
//! call is a no-op. Label values are registered venue names or the enumerations below, never
//! values from the wire, with one exception: an auto-detected venue is named by its address, so
//! its `venue` label carries the address the frame named it by.

use metrics::{counter, gauge, histogram};

/// Counter, no labels. Incremented once per frame the tracker accepts.
pub(super) const FRAMES_ACCEPTED: &str = "price_level_stream_frames_accepted_total";
/// Counter, label `reason`, one of [`RejectReason`]. Incremented once per frame the stream drops.
pub(super) const FRAMES_REJECTED: &str = "price_level_stream_frames_rejected_total";
/// Histogram, no labels. The age of every accepted frame at acceptance, in seconds: the local
/// wall clock minus the frame's wire `timestamp`, negative when the frame is stamped ahead of
/// the local clock. It measures Titan's lag plus delivery delay, and a clock skew between Titan
/// and this host; a frame older than one slot yields states that refuse to quote.
pub(super) const FRAME_AGE: &str = "price_level_stream_frame_age_seconds";
/// Gauge, label `venue`. The wire `timestamp` of the newest accepted frame that carried the
/// venue, in seconds since the Unix epoch; 0 until the first such frame. `venue` is the
/// registered venue name, or the address of an auto-detected venue.
pub(super) const LAST_SEEN: &str = "price_level_stream_last_seen_timestamp_seconds";
/// Gauge, label `venue`. The number of components the stream currently serves for the venue;
/// 0 for every registered venue from the start. `venue` is as on `LAST_SEEN`.
pub(super) const SERVED_COMPONENTS: &str = "price_level_stream_served_components";
/// Counter, label `venue`. Incremented once per component that turns stale at its `stale_at`
/// deadline and is emitted in `removed_pairs`. `venue` is as on `LAST_SEEN`.
pub(super) const STALE_REMOVALS: &str = "price_level_stream_stale_removals_total";
/// Gauge, no labels. The stream's serving state as a [`ServingState`] number: 0 = awaiting the
/// whitelist, 1 = unserved, 2 = serving.
pub(super) const SERVING_STATE: &str = "price_level_stream_serving_state";
/// Counter, label `reason`, one of [`ReconnectReason`]. Incremented once per Titan connection
/// the stream gives up on, or fails to establish, before backing off.
pub(super) const RECONNECTS: &str = "price_level_stream_reconnects_total";
/// Counter, label `outcome`, one of [`ReadOutcome`]. Incremented once per PropAMMRouter
/// whitelist read.
pub(super) const WHITELIST_READS: &str = "price_level_stream_whitelist_reads_total";
/// Gauge, no labels. The number of venues on the whitelist as of the last successful read.
pub(super) const WHITELISTED_VENUES: &str = "price_level_stream_whitelisted_venues";
/// Counter, no labels. Incremented once per pAMM entry in an accepted frame that names a venue
/// which is neither registered nor denied while auto-detection is off.
pub(super) const UNREGISTERED_PAMM_ENTRIES: &str =
    "price_level_stream_unregistered_pamm_entries_total";

/// The `reason` label of [`FRAMES_REJECTED`]: why the stream dropped a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RejectReason {
    /// The text did not parse as a frame.
    ParseError,
    /// The wire `timestamp` is `stale_after` old or older.
    TooOld,
    /// The wire `timestamp` is more than one slot ahead of the local wall clock.
    InFuture,
    /// The wire `timestamp` is older than the newest accepted frame's.
    OutOfOrder,
    /// The block is below the newest accepted block.
    BlockRegression,
    /// The block is above the newest accepted block by more than one block per elapsed slot
    /// plus 2.
    BlockJump,
}

impl RejectReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            RejectReason::ParseError => "parse_error",
            RejectReason::TooOld => "too_old",
            RejectReason::InFuture => "in_future",
            RejectReason::OutOfOrder => "out_of_order",
            RejectReason::BlockRegression => "block_regression",
            RejectReason::BlockJump => "block_jump",
        }
    }
}

/// The `reason` label of [`RECONNECTS`]: why the stream gave up on a Titan connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    pub(super) fn as_str(self) -> &'static str {
        match self {
            ReconnectReason::IdleTimeout => "idle_timeout",
            ReconnectReason::Ended => "ended",
            ReconnectReason::Closed => "closed",
            ReconnectReason::ReadError => "read_error",
            ReconnectReason::ConnectFailed => "connect_failed",
            ReconnectReason::ConnectTimeout => "connect_timeout",
        }
    }
}

/// The `outcome` label of [`WHITELIST_READS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadOutcome {
    /// The read returned a whitelist.
    Ok,
    /// The `eth_call` failed, returned undecodable data, or timed out.
    Error,
}

impl ReadOutcome {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            ReadOutcome::Ok => "ok",
            ReadOutcome::Error => "error",
        }
    }
}

/// The stream's serving state, exported as the numeric value of `SERVING_STATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ServingState {
    /// The PropAMMRouter whitelist has not been read yet; nothing is served.
    AwaitingWhitelist = 0,
    /// The whitelist is known or not needed, and no component is served.
    Unserved = 1,
    /// At least one component is served.
    Serving = 2,
}

pub(super) fn record_frame_accepted() {
    counter!(FRAMES_ACCEPTED).increment(1);
}

pub(super) fn record_frame_rejected(reason: RejectReason) {
    counter!(FRAMES_REJECTED, "reason" => reason.as_str()).increment(1);
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

pub(super) fn record_serving_state(state: ServingState) {
    gauge!(SERVING_STATE).set(state as u8 as f64);
}

pub(super) fn record_reconnect(reason: ReconnectReason) {
    counter!(RECONNECTS, "reason" => reason.as_str()).increment(1);
}

pub(super) fn record_whitelist_read(outcome: ReadOutcome) {
    counter!(WHITELIST_READS, "outcome" => outcome.as_str()).increment(1);
}

pub(super) fn record_whitelisted_venues(count: usize) {
    gauge!(WHITELISTED_VENUES).set(count as f64);
}

pub(super) fn record_unregistered_pamm() {
    counter!(UNREGISTERED_PAMM_ENTRIES).increment(1);
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

    /// The metric names are the contract with dashboards and alerts, so the lookups spell them
    /// out instead of reusing the constants the helpers emit under.
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
            record_whitelist_read(ReadOutcome::Ok);
            record_whitelisted_venues(5);
            record_unregistered_pamm();
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
        assert_eq!(gauge_value(&snapshot, "price_level_stream_serving_state", &[]), 2.0);
        assert_eq!(
            counter_value(
                &snapshot,
                "price_level_stream_reconnects_total",
                &[("reason", "idle_timeout")]
            ),
            1
        );
        assert_eq!(
            counter_value(
                &snapshot,
                "price_level_stream_whitelist_reads_total",
                &[("outcome", "ok")]
            ),
            1
        );
        assert_eq!(gauge_value(&snapshot, "price_level_stream_whitelisted_venues", &[]), 5.0);
        assert_eq!(
            counter_value(&snapshot, "price_level_stream_unregistered_pamm_entries_total", &[]),
            1
        );
    }

    /// Dashboards match on the label strings, so every variant maps to a distinct one.
    #[test]
    fn label_values_are_distinct() {
        let reject: Vec<&str> = [
            RejectReason::ParseError,
            RejectReason::TooOld,
            RejectReason::InFuture,
            RejectReason::OutOfOrder,
            RejectReason::BlockRegression,
            RejectReason::BlockJump,
        ]
        .into_iter()
        .map(RejectReason::as_str)
        .collect();
        let reconnect: Vec<&str> = [
            ReconnectReason::IdleTimeout,
            ReconnectReason::Ended,
            ReconnectReason::Closed,
            ReconnectReason::ReadError,
            ReconnectReason::ConnectFailed,
            ReconnectReason::ConnectTimeout,
        ]
        .into_iter()
        .map(ReconnectReason::as_str)
        .collect();
        for values in [reject, reconnect, vec!["ok", "error"]] {
            let distinct: std::collections::HashSet<&str> = values.iter().copied().collect();
            assert_eq!(distinct.len(), values.len(), "{values:?}");
        }
    }
}
