//! Metrics of the price level stream, emitted through the `metrics` facade. A consumer that
//! installs a recorder (Fynd, tycho-integration-test) sees them without any wiring; without a
//! recorder every call is a no-op. Label values are registered venue names or fixed
//! enumerations, never values from the wire.

use metrics::{counter, gauge};

pub(super) const FRAMES_ACCEPTED: &str = "price_level_stream_frames_accepted_total";
pub(super) const FRAMES_REJECTED: &str = "price_level_stream_frames_rejected_total";
pub(super) const LAST_SEEN: &str = "price_level_stream_last_seen_timestamp_seconds";
pub(super) const SERVED_COMPONENTS: &str = "price_level_stream_served_components";
pub(super) const STALE_REMOVALS: &str = "price_level_stream_stale_removals_total";
pub(super) const SERVING_STATE: &str = "price_level_stream_serving_state";
pub(super) const RECONNECTS: &str = "price_level_stream_reconnects_total";
pub(super) const WHITELIST_READS: &str = "price_level_stream_whitelist_reads_total";
pub(super) const WHITELISTED_VENUES: &str = "price_level_stream_whitelisted_venues";
pub(super) const UNREGISTERED_PAMM_ENTRIES: &str =
    "price_level_stream_unregistered_pamm_entries_total";

/// The stream's serving state, exported as the numeric value of `SERVING_STATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ServingState {
    /// The PropAMMRouter whitelist has not been read yet; nothing is served.
    AwaitingWhitelist = 0,
    /// Whitelist known (or not needed), no component currently served.
    Unserved = 1,
    /// At least one component is served.
    Serving = 2,
}

pub(super) fn record_frame_accepted() {
    counter!(FRAMES_ACCEPTED).increment(1);
}

pub(super) fn record_frame_rejected(reason: &'static str) {
    counter!(FRAMES_REJECTED, "reason" => reason).increment(1);
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

pub(super) fn record_reconnect(reason: &'static str) {
    counter!(RECONNECTS, "reason" => reason).increment(1);
}

pub(super) fn record_whitelist_read(outcome: &'static str) {
    counter!(WHITELIST_READS, "outcome" => outcome).increment(1);
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
    use std::collections::{BTreeMap, HashMap};

    use metrics_util::{
        debugging::{DebugValue, Snapshot},
        MetricKind,
    };

    pub(in super::super) type SnapshotMap =
        HashMap<(MetricKind, String, BTreeMap<String, String>), DebugValue>;

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
}

#[cfg(test)]
mod tests {
    use metrics_util::debugging::DebuggingRecorder;

    use super::{recorded::*, *};

    #[test]
    fn every_helper_emits_its_named_metric() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            record_frame_accepted();
            record_frame_rejected("too_old");
            record_frame_rejected("too_old");
            record_last_seen("fermiswap", 1_700_000_000);
            record_served_components("fermiswap", 3);
            record_stale_removal("fermiswap");
            record_serving_state(ServingState::Serving);
            record_reconnect("idle_timeout");
            record_whitelist_read("ok");
            record_whitelisted_venues(5);
            record_unregistered_pamm();
        });
        let snapshot = snapshot_map(snapshotter.snapshot());
        assert_eq!(counter_value(&snapshot, FRAMES_ACCEPTED, &[]), 1);
        assert_eq!(counter_value(&snapshot, FRAMES_REJECTED, &[("reason", "too_old")]), 2);
        assert_eq!(gauge_value(&snapshot, LAST_SEEN, &[("venue", "fermiswap")]), 1_700_000_000.0);
        assert_eq!(gauge_value(&snapshot, SERVED_COMPONENTS, &[("venue", "fermiswap")]), 3.0);
        assert_eq!(counter_value(&snapshot, STALE_REMOVALS, &[("venue", "fermiswap")]), 1);
        assert_eq!(gauge_value(&snapshot, SERVING_STATE, &[]), 2.0);
        assert_eq!(counter_value(&snapshot, RECONNECTS, &[("reason", "idle_timeout")]), 1);
        assert_eq!(counter_value(&snapshot, WHITELIST_READS, &[("outcome", "ok")]), 1);
        assert_eq!(gauge_value(&snapshot, WHITELISTED_VENUES, &[]), 5.0);
        assert_eq!(counter_value(&snapshot, UNREGISTERED_PAMM_ENTRIES, &[]), 1);
    }
}
