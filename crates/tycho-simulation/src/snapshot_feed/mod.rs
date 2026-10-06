//! Latest-value snapshot feeds, both ends: the contract a feed implements, what it publishes
//! through, and the ways to read one.
//!
//! A snapshot feed publishes a self-contained view of some live data source over a
//! [`tokio::sync::watch`] channel: each snapshot stands on its own, and the receiver always holds
//! the newest one, so a consumer that falls behind skips straight to it and reading (`borrow`)
//! never blocks. The channel holds an `Option`: `None` says the feed has nothing servable — it has
//! not published yet, or what it had was withdrawn — and every feed here makes that promise.
//!
//! [`SnapshotFeed`] is the contract, and a feed publishes through a [`Publisher`] it is handed
//! rather than through a channel of its own. Nothing outside this crate can make one, so reading
//! a feed is the only way to run it, and there are three ways to read: [`SnapshotFeedStream`]
//! takes one feed as a stream of events, [`SnapshotFeedStreams`] takes any number of them through
//! one await point, and [`SnapshotFeedWatch`] hands out receivers to a consumer that prices on
//! demand rather than reacting to every snapshot. The transport
//! loops in [`http`] and [`ws`] turn a provider-specific source into a feed that keeps the promise:
//! they publish what the source yields, count failures and back off, and withdraw a snapshot nobody
//! has refreshed.

use std::{
    future::Future,
    pin::Pin,
    task::{ready, Context, Poll},
};

use futures::{Stream, StreamExt as _};
use tokio::{
    sync::watch,
    task::{JoinError, JoinHandle},
};
use tokio_stream::{wrappers::WatchStream, StreamMap};
use tracing::{error_span, Instrument as _};

pub use crate::snapshot_feed::publisher::Publisher;

#[cfg(feature = "book-feeds")]
pub mod errors;
#[cfg(feature = "book-feeds")]
mod failures;
#[cfg(feature = "book-feeds")]
pub mod http;
mod publisher;
#[cfg(feature = "book-feeds")]
pub mod ws;

/// A source that can be turned into a live snapshot feed.
///
/// `run` consumes the source: one value, one feed. For a second independent feed, clone the
/// source. It publishes through a [`Publisher`] it is handed and cannot construct, so the
/// channel, its `None` seed, the task and the decision to stop all stay with whoever reads the
/// feed — a consumer cannot take a feed's future and give it their own pace.
pub trait SnapshotFeed {
    /// What the feed publishes. Each one replaces its predecessor entirely, so a consumer that
    /// falls behind may skip every snapshot but the newest without losing anything. A feed with
    /// nothing servable — nothing received yet, or nothing refreshing what it had — withdraws
    /// instead, and consumers stop using its data until it publishes again.
    type Snapshot: Send + Sync + 'static;

    /// Why a feed gives up. A feed that never does can use
    /// [`Infallible`](std::convert::Infallible).
    type Error: Send + 'static;

    /// Runs the feed, publishing through `publisher` until it ends: `Err` when it gives up, `Ok`
    /// when it has nothing more to publish or nobody left to publish to. Noticing the latter is
    /// optional — [`Publisher::publishing`] does it for a feed built on it — and a feed that does
    /// not notice is stopped by dropping its future, which drops the publisher and withdraws
    /// whatever the feed was serving.
    fn run(
        self,
        publisher: Publisher<Self::Snapshot>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;
}

/// One feed, read as a stream of what happens to it.
///
/// Creating the stream spawns the feed's task; dropping it aborts that task.
///
/// The stream yields what the feed publishes — the newest snapshot, so a consumer that falls
/// behind skips whatever it missed rather than working through a backlog — and its last item is
/// always [`Ended`](SnapshotFeedEvent::Ended), saying how the feed stopped, after which the
/// stream itself ends. The feed's sender is dropped with its future, so everything it published
/// — the withdrawal it performs on its way out included — is yielded before that.
///
/// A feed that simply ran out says so in an event rather than only by the stream ending, because
/// one read through a [`SnapshotFeedStreams`] has no end of its own to say it with.
///
/// ```
/// # use futures::StreamExt;
/// # use tycho_simulation::snapshot_feed::{
/// #     SnapshotFeed, SnapshotFeedEvent, SnapshotFeedOutcome, SnapshotFeedStream,
/// # };
/// # async fn consume<F>(feed: F)
/// # where
/// #     F: SnapshotFeed<Snapshot = u32, Error = std::convert::Infallible>,
/// # {
/// let mut stream = SnapshotFeedStream::spawn("my-venue", feed);
/// while let Some(event) = stream.next().await {
///     match event {
///         SnapshotFeedEvent::Published(snapshot) => { /* serve from it */ }
///         SnapshotFeedEvent::Withdrawn => { /* nothing servable: stop using its data */ }
///         // Whatever the outcome, this venue is done for the rest of the process.
///         SnapshotFeedEvent::Ended(SnapshotFeedOutcome::Failed(error)) => { /* it gave up */ }
///         SnapshotFeedEvent::Ended(SnapshotFeedOutcome::Panicked(error)) => { /* a bug */ }
///         SnapshotFeedEvent::Ended(SnapshotFeedOutcome::RanOut) => { /* nothing left to serve */ }
///     }
/// }
/// # }
/// ```
pub struct SnapshotFeedStream<T, E> {
    /// Every change to the snapshot; `None` once the feed dropped its sender.
    snapshots: Option<WatchStream<Option<T>>>,
    task: JoinHandle<Result<(), E>>,
    /// Whether the task has been joined, after which the stream is over whether or not joining
    /// it yielded an event.
    ended: bool,
}

/// What happened on a feed.
#[derive(Debug)]
pub enum SnapshotFeedEvent<T, E> {
    /// The feed published a snapshot, replacing whatever it served before.
    Published(T),
    /// The feed has nothing servable: what it served was withdrawn because nothing was refreshing
    /// it. Stop using its data until it publishes again.
    Withdrawn,
    /// The feed stopped, and how. Nothing follows it on that feed, which for a
    /// [`SnapshotFeedStream`] means the stream is over and for a [`SnapshotFeedStreams`] means
    /// that one label is: the set reads on until its last feed has ended.
    Ended(SnapshotFeedOutcome<E>),
}

/// How a feed ended. Whichever of the three it is, the feed publishes nothing further and the
/// venue is gone for the rest of the process.
#[derive(Debug)]
pub enum SnapshotFeedOutcome<E> {
    /// The feed ran out of snapshots to publish.
    RanOut,
    /// The feed gave up, and `E` says why.
    Failed(E),
    /// The task driving the feed panicked: a bug rather than a feed giving up. A [`JoinError`]
    /// also reports a cancelled task, but the only thing that cancels this one is the consumer's
    /// own `Drop`, so a reader never sees that.
    Panicked(JoinError),
}

impl<E> SnapshotFeedOutcome<E> {
    /// What joining the feed's task says about how it ended.
    fn of(joined: Result<Result<(), E>, JoinError>) -> Self {
        match joined {
            Ok(Ok(())) => SnapshotFeedOutcome::RanOut,
            Ok(Err(error)) => SnapshotFeedOutcome::Failed(error),
            Err(error) => SnapshotFeedOutcome::Panicked(error),
        }
    }
}

impl<T, E> SnapshotFeedStream<T, E>
where
    T: Clone + Send + Sync + 'static,
    E: Send + 'static,
{
    /// Spawns `feed` on the current runtime under `provider` and streams what happens to it.
    /// Everything the feed records while it runs is inside a `snapshot_feed` span naming
    /// `provider`, so a warning from a feed always says which one produced it.
    ///
    /// # Panics
    ///
    /// Spawning needs a runtime: call this from one.
    pub fn spawn(provider: &str, feed: impl SnapshotFeed<Snapshot = T, Error = E>) -> Self {
        let (publisher, rx) = Publisher::channel();
        // The seed is ours, not the feed's: mark it seen before the feed gets its publisher, so
        // the first thing this stream yields is the feed's first publication.
        let snapshots = WatchStream::from_changes(rx);
        let task = tokio::spawn(
            feed.run(publisher)
                .instrument(feed_span(provider)),
        );
        SnapshotFeedStream { snapshots: Some(snapshots), task, ended: false }
    }
}

impl<T, E> Stream for SnapshotFeedStream<T, E>
where
    T: Clone + Send + Sync + 'static,
    E: Send + 'static,
{
    type Item = SnapshotFeedEvent<T, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        // Drain the snapshots before looking at the task: the sender is dropped only when the
        // feed's future is done, so every change it made — its withdrawal included — is seen
        // before the outcome that ended it.
        if let Some(snapshots) = &mut this.snapshots {
            match ready!(snapshots.poll_next_unpin(cx)) {
                Some(Some(snapshot)) => {
                    return Poll::Ready(Some(SnapshotFeedEvent::Published(snapshot)))
                }
                Some(None) => return Poll::Ready(Some(SnapshotFeedEvent::Withdrawn)),
                None => this.snapshots = None,
            }
        }
        if this.ended {
            return Poll::Ready(None);
        }
        let joined = ready!(Pin::new(&mut this.task).poll(cx));
        this.ended = true;
        Poll::Ready(Some(SnapshotFeedEvent::Ended(SnapshotFeedOutcome::of(joined))))
    }
}

impl<T, E> Drop for SnapshotFeedStream<T, E> {
    /// A stream nobody holds is a feed nobody reads: the task goes with it.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Several feeds, read through one await point: a [`SnapshotFeedStream`] per label of the
/// consumer's choosing, every event arriving with the label of the feed it happened on.
///
/// The set is a [`Stream`] of those events and ends when its last feed does, so a consumer reads
/// it with [`StreamExt`](futures::StreamExt) like any other.
///
/// ```
/// # use futures::StreamExt as _;
/// # use tycho_simulation::snapshot_feed::{SnapshotFeedEvent, SnapshotFeedOutcome, SnapshotFeedStreams};
/// # async fn consume<T: Clone + Send + Sync + 'static, E: Send + 'static>(
/// #     mut feeds: SnapshotFeedStreams<T, E>,
/// # ) {
/// while let Some((provider, event)) = feeds.next().await {
///     match event {
///         SnapshotFeedEvent::Published(snapshot) => { /* serve from it */ }
///         SnapshotFeedEvent::Withdrawn => { /* nothing servable from `provider` */ }
///         // `provider` is out of the set from here on; the loop ends with the last one.
///         SnapshotFeedEvent::Ended(SnapshotFeedOutcome::Failed(error)) => { /* it gave up */ }
///         SnapshotFeedEvent::Ended(outcome) => { /* ran out, or its task died */ }
///     }
/// }
/// # }
/// ```
///
/// Dropping the set drops every stream, aborting the feeds' tasks.
pub struct SnapshotFeedStreams<T, E> {
    feeds: StreamMap<String, SnapshotFeedStream<T, E>>,
}

impl<T, E> Default for SnapshotFeedStreams<T, E> {
    fn default() -> Self {
        Self { feeds: StreamMap::new() }
    }
}

impl<T, E> SnapshotFeedStreams<T, E>
where
    T: Clone + Send + Sync + 'static,
    E: Send + 'static,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether no feed has been added, or every one that was has ended.
    pub fn is_empty(&self) -> bool {
        self.feeds.is_empty()
    }

    /// How many feeds are running.
    pub fn len(&self) -> usize {
        self.feeds.len()
    }

    /// The label of every feed still running, in no particular order.
    pub fn labels(&self) -> impl Iterator<Item = &str> {
        self.feeds.keys().map(String::as_str)
    }

    /// Spawns `feed` under `label` and streams what happens to it.
    ///
    /// A label names one feed for as long as the set holds it, so a label already in use is a
    /// caller's bug rather than a condition to recover from: the feed comes back in the `Err`
    /// having never run, to add under another label or to drop.
    ///
    /// # Panics
    ///
    /// Spawning needs a runtime: call this from one.
    pub fn add<F>(&mut self, label: impl Into<String>, feed: F) -> Result<(), F>
    where
        F: SnapshotFeed<Snapshot = T, Error = E>,
    {
        let label = label.into();
        if self.feeds.contains_key(&label) {
            return Err(feed);
        }
        let stream = SnapshotFeedStream::spawn(&label, feed);
        self.feeds.insert(label, stream);
        Ok(())
    }

    /// Takes the feed labelled `label` out of the set, still running. Dropping what comes back
    /// stops it; reading it goes on where the set left off.
    pub fn remove(&mut self, label: &str) -> Option<SnapshotFeedStream<T, E>> {
        self.feeds.remove(label)
    }
}

impl<T, E> Stream for SnapshotFeedStreams<T, E>
where
    T: Clone + Send + Sync + 'static,
    E: Send + 'static,
{
    type Item = (String, SnapshotFeedEvent<T, E>);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().feeds).poll_next(cx)
    }
}

/// One feed, read the way a consumer that prices on demand wants it: a channel holding the
/// newest snapshot, which [`watch::Receiver::borrow`] reads without awaiting and without
/// blocking the feed.
///
/// Creating the watch spawns the feed's task; dropping it aborts that task, so keep it for as
/// long as the receivers are used. Handing out receivers gives nothing away — a receiver cannot
/// slow a [`watch::Sender`] down, so a consumer reading at its own pace never reaches the venue's
/// connection.
///
/// `None` in the channel means nothing is servable: nothing published yet, what there was went
/// stale, or the feed is over. Those look alike to a reader, so the one that matters —
/// the venue is gone until the process restarts, rather than quiet for a moment — is
/// [`ended`](Self::ended), which resolves with the outcome when the feed stops.
///
/// ```
/// # use tycho_simulation::snapshot_feed::{SnapshotFeed, SnapshotFeedWatch};
/// # fn consume<F>(feed: F)
/// # where
/// #     F: SnapshotFeed<Snapshot = u32, Error = std::convert::Infallible>,
/// # {
/// let watch = SnapshotFeedWatch::spawn("my-venue", feed);
/// let snapshots = watch.receiver();
/// // Price a route: read the newest snapshot, or find there is nothing servable.
/// let newest = snapshots.borrow();
/// match newest.as_ref() {
///     Some(snapshot) => { /* quote from it */ }
///     None => { /* nothing servable from this venue right now */ }
/// }
/// # }
/// ```
pub struct SnapshotFeedWatch<T, E> {
    snapshots: watch::Receiver<Option<T>>,
    task: JoinHandle<Result<(), E>>,
    /// Whether the outcome has been taken, after which the task must not be polled again.
    ended: bool,
}

impl<T, E> SnapshotFeedWatch<T, E>
where
    T: Send + Sync + 'static,
    E: Send + 'static,
{
    /// Spawns `feed` on the current runtime under `provider` and holds what it publishes.
    ///
    /// # Panics
    ///
    /// Spawning needs a runtime: call this from one.
    pub fn spawn(provider: &str, feed: impl SnapshotFeed<Snapshot = T, Error = E>) -> Self {
        let (publisher, snapshots) = Publisher::channel();
        let task = tokio::spawn(
            feed.run(publisher)
                .instrument(feed_span(provider)),
        );
        SnapshotFeedWatch { snapshots, task, ended: false }
    }

    /// A receiver on the feed's snapshots. Clone it as often as the consumer needs: every one
    /// reads the same newest snapshot, and none of them slows the feed.
    pub fn receiver(&self) -> watch::Receiver<Option<T>> {
        self.snapshots.clone()
    }

    /// Resolves when the feed stops, with what stopped it — which is what a `None` in the channel
    /// cannot say on its own, since it means only that nothing is servable right now.
    ///
    /// The outcome is reported once, to whoever takes it. After that this stays pending forever,
    /// so a `select!` arm that keeps calling it waits on the other arms instead of spinning on an
    /// answer that has already been given.
    pub async fn ended(&mut self) -> SnapshotFeedOutcome<E> {
        if self.ended {
            return std::future::pending().await;
        }
        let joined = (&mut self.task).await;
        self.ended = true;
        SnapshotFeedOutcome::of(joined)
    }
}

impl<T, E> Drop for SnapshotFeedWatch<T, E> {
    /// A watch nobody holds is a feed nobody reads: the task goes with it.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The span everything a feed records while it runs is recorded in. `error` level so a filter
/// set to warn still enters it: one the filter rejects is never entered, and its fields then
/// attach to nothing, leaving a bare warning that names no venue.
fn feed_span(provider: &str) -> tracing::Span {
    error_span!("snapshot_feed", provider)
}

#[cfg(test)]
async fn expect_to_finish<T>(what: &str, f: impl Future<Output = T>) -> T {
    const DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

    tokio::time::timeout(DEADLINE, f)
        .await
        .unwrap_or_else(|_| panic!("{what} within {DEADLINE:?}"))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };

    use futures::stream;
    use tokio::time::sleep;

    use super::*;

    /// What a scripted feed does, in order.
    #[derive(Clone)]
    enum Step {
        Publish(u32),
        Withdraw,
        GiveUp(&'static str),
        Panic,
    }

    /// How long a scripted feed's snapshot stays servable, and how long [`Step::Withdraw`] waits
    /// to let one go stale.
    const GOES_STALE_AFTER: Duration = Duration::from_millis(10);

    /// A feed that plays a script through its publisher and then resolves `Ok`.
    #[derive(Clone)]
    struct Scripted(Vec<Step>);

    impl SnapshotFeed for Scripted {
        type Snapshot = u32;
        type Error = String;

        fn run(
            self,
            publisher: Publisher<u32>,
        ) -> impl Future<Output = Result<(), String>> + Send + 'static {
            let snapshots = stream::unfold(self.0.into_iter(), |mut steps| async move {
                loop {
                    // Real feeds wait for their venue between snapshots; without a wait here a
                    // script would publish everything before its reader is polled, and the
                    // watch would coalesce it all into the last one.
                    tokio::task::yield_now().await;
                    match steps.next()? {
                        Step::Publish(snapshot) => return Some((Ok(snapshot), steps)),
                        Step::GiveUp(why) => return Some((Err(why.to_string()), steps)),
                        Step::Withdraw => sleep(2 * GOES_STALE_AFTER).await,
                        Step::Panic => panic!("scripted"),
                    }
                }
            });
            publisher.publishing(Some(GOES_STALE_AFTER), snapshots)
        }
    }

    async fn events(feed: impl SnapshotFeed<Snapshot = u32, Error = String>) -> Vec<String> {
        let mut stream = SnapshotFeedStream::spawn("test", feed);
        let mut seen = Vec::new();
        while let Some(event) = expect_to_finish("stream did not end", stream.next()).await {
            seen.push(match event {
                SnapshotFeedEvent::Published(n) => format!("published {n}"),
                SnapshotFeedEvent::Withdrawn => "withdrawn".to_string(),
                SnapshotFeedEvent::Ended(SnapshotFeedOutcome::RanOut) => "ran out".to_string(),
                SnapshotFeedEvent::Ended(SnapshotFeedOutcome::Failed(why)) => {
                    format!("failed: {why}")
                }
                SnapshotFeedEvent::Ended(SnapshotFeedOutcome::Panicked(_)) => {
                    "panicked".to_string()
                }
            });
        }
        seen
    }

    #[tokio::test]
    async fn the_seed_is_not_an_event_and_the_failure_comes_last() {
        // Nothing is yielded for the `None` the channel starts with; the feed's own withdrawal
        // is, and it precedes the failure however the runtime interleaves the two.
        let seen = events(Scripted(vec![
            Step::Publish(1),
            Step::Publish(2),
            Step::Withdraw,
            Step::GiveUp("out of retries"),
        ]))
        .await;

        assert_eq!(seen, ["published 1", "published 2", "withdrawn", "failed: out of retries"]);
    }

    #[tokio::test]
    async fn a_feed_that_gives_up_withdraws_before_reporting_why() {
        // Nothing refreshes a snapshot once the feed is over, so a consumer is told to stop
        // serving it before it is told what went wrong.
        let seen = events(Scripted(vec![Step::Publish(1), Step::GiveUp("out of retries")])).await;

        assert_eq!(seen, ["published 1", "withdrawn", "failed: out of retries"]);
    }

    #[tokio::test]
    async fn a_feed_that_ends_without_failing_says_so() {
        assert_eq!(events(Scripted(vec![])).await, ["ran out"]);
    }

    #[tokio::test]
    async fn a_panicking_task_is_reported_as_such() {
        // A feed that dies of a bug stops being quotable the same way one that gives up does:
        // the publisher is dropped as the task unwinds, before the panic is reported.
        let seen = events(Scripted(vec![Step::Publish(1), Step::Panic])).await;

        assert_eq!(seen, ["published 1", "withdrawn", "panicked"]);
    }

    /// Flips its flag when the feed's future is dropped, wherever it had got to.
    struct StoppedWhenDropped(Arc<AtomicBool>);

    impl Drop for StoppedWhenDropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Publishes once and then works forever, so only being dropped can end it.
    struct Working {
        started: Arc<AtomicBool>,
        stopped: Arc<AtomicBool>,
    }

    impl Working {
        /// The feed, its started flag and its stopped flag.
        fn new() -> (Self, Arc<AtomicBool>, Arc<AtomicBool>) {
            let started = Arc::new(AtomicBool::new(false));
            let stopped = Arc::new(AtomicBool::new(false));
            let feed = Working { started: Arc::clone(&started), stopped: Arc::clone(&stopped) };
            (feed, started, stopped)
        }
    }

    impl SnapshotFeed for Working {
        type Snapshot = u32;
        type Error = String;

        async fn run(self, publisher: Publisher<u32>) -> Result<(), String> {
            let _stopped = StoppedWhenDropped(self.stopped);
            let started = self.started;
            // One snapshot, and then a wait for a second one that never comes — the shape of a
            // feed whose venue has gone quiet.
            let snapshots =
                stream::once(async { Ok::<u32, String>(1) }).chain(stream::once(async move {
                    started.store(true, Ordering::SeqCst);
                    std::future::pending().await
                }));
            publisher
                .publishing(None, snapshots)
                .await
        }
    }

    async fn until(flag: Arc<AtomicBool>) {
        while !flag.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    }

    /// Nobody reading is what stops a feed, whatever it is in the middle of, and it is the
    /// stream that does the stopping: the feed is never asked to notice.
    #[tokio::test]
    async fn dropping_the_stream_stops_a_feed_that_is_still_working() {
        let (feed, started, stopped) = Working::new();
        let mut stream = SnapshotFeedStream::spawn("test", feed);

        expect_to_finish("the feed never started", until(started)).await;
        assert!(matches!(
            expect_to_finish("no snapshot", stream.next()).await,
            Some(SnapshotFeedEvent::Published(1))
        ));

        drop(stream);

        expect_to_finish("the feed kept working after nobody was reading", until(stopped)).await;
    }

    /// The watch is the same arrangement read the other way: receivers see the newest snapshot
    /// without awaiting, and the feed stops with the watch rather than with the last receiver.
    #[tokio::test]
    async fn a_watch_hands_out_readers_and_stops_the_feed_when_dropped() {
        let (feed, started, stopped) = Working::new();
        let watch = SnapshotFeedWatch::spawn("test", feed);
        let mut snapshots = watch.receiver();

        expect_to_finish("the feed never started", until(started)).await;
        expect_to_finish("no snapshot", async {
            while snapshots.borrow_and_update().is_none() {
                snapshots
                    .changed()
                    .await
                    .expect("the feed is still running");
            }
        })
        .await;
        assert_eq!(*snapshots.borrow(), Some(1));

        // A receiver outliving the watch keeps nothing alive: the feed stops, and reading it
        // afterwards finds the withdrawal the feed left behind.
        drop(watch);

        expect_to_finish("the feed kept working after nobody was reading", until(stopped)).await;
        assert_eq!(*snapshots.borrow(), None);
    }

    /// `None` in the channel says only that nothing is servable now; a watch consumer learns from
    /// the outcome that the venue is gone for good, and learns it once.
    #[tokio::test]
    async fn a_watch_reports_why_its_feed_gave_up() {
        let mut watch =
            SnapshotFeedWatch::spawn("test", Scripted(vec![Step::GiveUp("no retries")]));
        let snapshots = watch.receiver();

        let outcome = expect_to_finish("the feed kept running", watch.ended()).await;

        assert!(matches!(outcome, SnapshotFeedOutcome::Failed(why) if why == "no retries"));
        assert_eq!(*snapshots.borrow(), None, "a feed that gave up serves nothing");
        // Taken once: an arm that polls this again waits here rather than being handed a second
        // answer.
        assert!(
            futures::poll!(std::pin::pin!(watch.ended())).is_pending(),
            "the outcome is reported to whoever takes it, once"
        );
    }

    /// A feed's events are attributable: the span carries the provider it was added under, and
    /// is entered even when the subscriber only passes warnings — an `info`-level span would not
    /// be, and the warning would print bare, which is the failure that produced this rule.
    #[tokio::test]
    async fn what_a_feed_warns_about_names_the_provider_it_runs_under() {
        /// Collects what a subscriber writes, so a test can read it back.
        struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for CaptureWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .unwrap()
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        /// Warns the way a transport loop does on its way to giving up.
        struct Noisy;

        impl SnapshotFeed for Noisy {
            type Snapshot = u32;
            type Error = String;

            async fn run(self, _publisher: Publisher<u32>) -> Result<(), String> {
                tracing::warn!("poll failed, backing off");
                Err("out of retries".to_string())
            }
        }

        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&logs);
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("warn"))
            .with_writer(move || CaptureWriter(Arc::clone(&writer)))
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        assert_eq!(events(Noisy).await, ["failed: out of retries"]);

        let logs = String::from_utf8(logs.lock().unwrap().clone()).expect("logs are utf-8");
        assert!(
            logs.contains(r#"WARN snapshot_feed{provider="test"}"#),
            "the feed's warning must name the provider it runs under, got: {logs}"
        );
    }

    #[tokio::test]
    async fn a_label_is_taken_once() {
        let mut feeds = SnapshotFeedStreams::new();

        assert!(feeds
            .add("a", Scripted(vec![Step::Publish(7)]))
            .is_ok());
        let rejected = feeds.add("a", Scripted(vec![Step::Publish(9)]));
        assert!(
            matches!(rejected, Err(Scripted(steps)) if matches!(steps[..], [Step::Publish(9)])),
            "the label is taken, and the feed comes back"
        );
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds.labels().collect::<Vec<_>>(), ["a"]);

        // The feed that was added first is the one running under the label; ending it withdraws
        // what it served.
        let mut seen = Vec::new();
        while let Some((label, event)) = expect_to_finish("set did not run dry", feeds.next()).await
        {
            assert_eq!(label, "a");
            seen.push(event);
        }
        assert!(matches!(
            seen[..],
            [
                SnapshotFeedEvent::Published(7),
                SnapshotFeedEvent::Withdrawn,
                SnapshotFeedEvent::Ended(SnapshotFeedOutcome::RanOut)
            ]
        ));
        assert!(feeds.is_empty());
    }
}
