//! Latest-value snapshot feeds.
//!
//! A snapshot feed publishes a self-contained view of some live data source — never a delta —
//! over a [`tokio::sync::watch`] channel: the receiver always holds the newest snapshot, a
//! consumer that falls behind skips straight to it, and reading (`borrow`) never blocks. To
//! follow several feeds at once, wrap the receivers in
//! [`WatchStream`](tokio_stream::wrappers::WatchStream)s and merge them with a
//! [`StreamMap`](tokio_stream::StreamMap).

use std::future::Future;

use tokio::sync::watch;

/// A source that can be turned into a live snapshot feed.
///
/// `subscribe` consumes the source: one value, one feed. For a second independent feed,
/// clone the source; to share one feed, clone the returned receiver.
pub trait SnapshotFeed {
    /// The value the watch channel holds. Its state before the first publish is the
    /// implementor's choice — an `Option` for feeds with a warm-up phase, a meaningful
    /// `Default` for feeds that are ready immediately.
    type Snapshot;

    /// What the feed future resolves with — e.g. `Result<(), E>` for a feed that stops
    /// cleanly when every receiver is dropped and can give up with an error.
    type Output;

    /// Returns a live view of the source's latest snapshot and the feed future that keeps it
    /// fresh. The caller spawns the future; when it completes it drops the sender, which ends
    /// any [`WatchStream`](tokio_stream::wrappers::WatchStream) over the receiver.
    fn subscribe(
        self,
    ) -> (watch::Receiver<Self::Snapshot>, impl Future<Output = Self::Output> + Send + 'static);
}
