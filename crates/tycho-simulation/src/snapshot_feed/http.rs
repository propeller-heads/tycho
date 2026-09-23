//! Feeds that poll: the shared poll loop, the source trait a provider implements for it, and
//! the one-request helper those sources fetch with.

use std::time::Duration;

use async_stream::try_stream;
use bytes::Bytes;
use futures::Stream;
use reqwest::RequestBuilder;
use serde::de::DeserializeOwned;
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};
use tracing::{debug_span, error, info, warn, Instrument};

use super::{errors::FeedError, failures::FailureTracker, publisher::Publisher};

/// Feed tuning for HTTP-polling feeds. Set through the feed builder.
///
/// There is no `Default`: start from the builder's `default_feed_config()`, which holds the
/// values that fit that venue, and change individual fields with struct-update syntax.
#[derive(Clone, Debug)]
pub struct HttpFeedConfig {
    /// Minimum time between the starts of two polls, whatever a poll costs: one that outruns the
    /// interval is followed by the next as soon as it answers, and the cadence runs on from
    /// there. Must not be zero: the feed future resolves with `InvalidInput` right away.
    pub poll_interval: Duration,
    /// Deadline for one poll's requests; a hung poll counts as a failed poll.
    pub request_timeout: Duration,
    /// How many times a run of failed polls may double the gap between them before it stops
    /// growing, so a provider that is down or rate-limiting is not polled at the healthy cadence
    /// for the whole outage. The first failure waits one `poll_interval`, the second two, and so
    /// on up to `poll_interval * 2^max_backoff_exp`; `0` polls a failing provider at the healthy
    /// cadence.
    pub max_backoff_exp: u32,
    /// Consecutive failed polls before the feed gives up and resolves with an error.
    /// `None` retries forever.
    pub max_consecutive_failures: Option<u32>,
    /// Withdraw the published snapshot (publish `None`) once it is this old, so consumers stop
    /// using data nobody is refreshing; the next successful poll restores it.
    ///
    /// Age is measured on the machine's monotonic clock from the last publish, so it bounds
    /// staleness whether a failing provider refuses instantly or hangs to `request_timeout`.
    ///
    /// Leaving room for the healthy cadence — a poll starts every `poll_interval` and may take
    /// up to `request_timeout` — keeps a snapshot servable from one poll to the next. Below the
    /// interval it is a duty cycle instead: the feed serves each snapshot for `max_snapshot_age`
    /// and nothing for the rest of the interval, which is what a consumer that would rather
    /// decline than quote an old book wants from a venue whose rate limit sets the cadence. Only
    /// zero is rejected, with `InvalidInput`, since it would serve nothing at all.
    ///
    /// `None` keeps the last snapshot until the feed ends. Default: 30 s.
    pub max_snapshot_age: Option<Duration>,
}

/// The values every HTTP feed builder starts from: poll every 5 s with a 10 s deadline per poll,
/// stretch that to 10, 20 and at most 40 s while polls keep failing, retry forever, and withdraw
/// a snapshot that has gone 30 s without being refreshed — two polls' worth of silence with room
/// for one hung poll on top.
pub(crate) fn default_http_feed_config() -> HttpFeedConfig {
    HttpFeedConfig {
        poll_interval: Duration::from_secs(5),
        request_timeout: Duration::from_secs(10),
        max_backoff_exp: 3,
        max_consecutive_failures: None,
        max_snapshot_age: Some(Duration::from_secs(30)),
    }
}

/// The poll loop shared by all HTTP-based feeds.
///
/// Polls `source` on the cadence `config` asks for and publishes every snapshot it answers with,
/// withdrawing one that goes `max_snapshot_age` without a refresh. What each knob buys is on its
/// own field; this is where they are checked and handed to the pieces that act on them.
///
/// Resolves `Err` when the feed gives up: on a fatal poll error, whatever the failure budget,
/// after `max_consecutive_failures` failed polls where one is configured, or right away with
/// `InvalidInput` for a zero `poll_interval` or a zero `max_snapshot_age`. Resolves `Ok(())` as
/// soon as the last receiver goes away, mid-poll included, since nothing it fetched from then on
/// could reach anyone; polling itself never runs out, so a feed is otherwise stopped by dropping
/// its future, which withdraws the published snapshot on the way out.
pub(crate) async fn run_http_poll_feed<S: HttpSource>(
    config: HttpFeedConfig,
    publisher: Publisher<S::Snapshot>,
    source: S,
) -> Result<(), FeedError> {
    // Where the config comes apart, and the only place that sees all of it: the loop waits on
    // the two deadlines and spends the failure budget, the publisher withdraws what has gone
    // stale.
    let HttpFeedConfig {
        poll_interval,
        request_timeout,
        max_backoff_exp,
        max_consecutive_failures,
        max_snapshot_age,
    } = config;

    if poll_interval.is_zero() {
        return Err(FeedError::InvalidInput("poll_interval must not be zero".to_string()));
    }
    if let Some(age) = max_snapshot_age {
        if age.is_zero() {
            return Err(FeedError::InvalidInput("max_snapshot_age must not be zero".to_string()));
        }
        if age <= poll_interval {
            // Deliberate for a venue whose rate limit sets the cadence, a mistake otherwise, and
            // indistinguishable from here — so it is said once rather than refused.
            warn!(
                max_snapshot_age = ?age,
                ?poll_interval,
                "max_snapshot_age does not exceed poll_interval: this feed will serve each \
                 snapshot for max_snapshot_age and nothing until the next poll"
            );
        }
    }

    let snapshots = polled_snapshots(
        poll_interval,
        request_timeout,
        max_consecutive_failures,
        max_backoff_exp,
        source,
    );
    publisher
        .publishing(max_snapshot_age, snapshots)
        .await
}

/// The provider-specific half of an HTTP-polling feed: one call fetches one complete snapshot.
/// [`run_http_poll_feed`] owns the source and drives it by reference.
pub(crate) trait HttpSource: Send + Sync {
    type Snapshot: Send + Sync + 'static;

    async fn fetch(&self) -> Result<Self::Snapshot, FeedError>;
}

/// Every snapshot the provider answers with, and as a last item the error the feed gives up on.
///
/// One fetch is one poll, started every `poll_interval` and given `request_timeout` to answer; an
/// error or an overrun is one failed poll, after which the gap to the next one doubles per further
/// failure up to `max_backoff_exp` times, so a provider that is down or rate-limiting is not
/// polled at the healthy cadence for the whole outage. `max_consecutive_failures` failed polls in
/// a row end the stream with the last one's error. Each fetch runs in a `poll` span.
///
/// Waiting out the cadence and the backoff happens in here, so the one who awaits the next
/// snapshot waits exactly as long as it takes — which is how a published one can go stale on time
/// while a poll is still in flight.
fn polled_snapshots<S: HttpSource>(
    poll_interval: Duration,
    request_timeout: Duration,
    max_consecutive_failures: Option<u32>,
    max_backoff_exp: u32,
    source: S,
) -> impl Stream<Item = Result<S::Snapshot, FeedError>> {
    // A poller backs off by stretching the cadence it already has, so one poll interval is the
    // unit its failures double — which is what lets a backoff stand in for a tick below.
    let mut failures =
        FailureTracker::new(max_consecutive_failures, poll_interval, max_backoff_exp);

    let mut ticker = interval(poll_interval);
    // A poll slower than the interval runs through ticks. `Delay` drops them and re-anchors the
    // cadence on the poll that outran them, which is the only behaviour that keeps every gap at
    // the interval: `Burst` owes one poll per tick missed and fires them at once, and `Skip`
    // stays on the original phase, so its next poll can follow a late one by a fraction of the
    // interval.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    // Set by a failed poll and taken by the wait that serves it out, so a poll that succeeds
    // leaves nothing behind for the next one to wait through.
    let mut backoff = None;

    info!(poll_interval_secs = poll_interval.as_secs(), "starting polling");

    try_stream! {
        loop {
            // The backoff stands in for the tick rather than following it: it is already a
            // multiple of the interval, and the ticker's own deadline would expire during it.
            match backoff.take() {
                Some(backoff) => {
                    sleep(backoff).await;
                    // The tick this backoff stood in for came due while it was waiting, and an
                    // overdue tick completes at once: without restarting the interval here, the
                    // poll after a recovered one would follow it immediately.
                    ticker.reset();
                }
                None => {
                    ticker.tick().await;
                }
            }
            let polled = timeout(request_timeout, source.fetch())
                .instrument(debug_span!("poll"))
                .await;

            let error = match polled {
                Ok(Ok(snapshot)) => {
                    let cleared = failures.record_success();
                    if cleared > 0 {
                        info!(after_failures = cleared, "poll succeeded again");
                    }
                    yield snapshot;
                    continue;
                }
                Ok(Err(e)) => if e.is_fatal() { Err(e) } else { Ok(e) }?,
                Err(_) => FeedError::Connection(format!(
                    "poll timed out after {}s",
                    request_timeout.as_secs()
                )),
            };

            let retry = failures
                .record_failure()
                .inspect(|retry| {
                    warn!(
                        consecutive = retry.consecutive,
                        backoff = ?retry.backoff,
                        %error,
                        "poll failed, backing off"
                    )
                })
                // The feed ends with the failure that ended it, classified as its source
                // classified it: a run of unparseable answers is not a connection failure, and
                // how long the feed tolerated it is for the log rather than for the caller to
                // act on.
                .map_err(|consecutive| {
                    error!(consecutive, "giving up on the feed");
                    error
                })?;
            backoff = Some(retry.backoff);
        }
    }
}

/// Sends `request` and returns its body. A transport failure or a non-success status is a
/// [`FeedError::Connection`]; `what` names the resource in the message (e.g. "Hashflow price
/// levels").
///
/// For an API that reports failures inside the body of an otherwise successful response, which
/// therefore has to be classified before it is parsed. [`fetch_json`] is the plain case.
pub(crate) async fn fetch_bytes(request: RequestBuilder, what: &str) -> Result<Bytes, FeedError> {
    let response = request
        .send()
        .await
        .map_err(|e| FeedError::Connection(format!("Failed to fetch {what}: {e}")))?;

    // The body is read before the status is judged, so a failing response can report what the
    // server said.
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|e| FeedError::Connection(format!("Failed to read {what} response: {e}")))?;

    if !status.is_success() {
        return Err(FeedError::Connection(format!(
            "{what} HTTP error {status}: {}",
            String::from_utf8_lossy(&body)
        )));
    }
    Ok(body)
}

/// Sends `request` and parses its JSON body. A transport failure or a non-success status is a
/// [`FeedError::Connection`], an unparseable body a [`FeedError::Parsing`]; `what` names the
/// resource in both (e.g. "Hashflow price levels").
pub(crate) async fn fetch_json<T: DeserializeOwned>(
    request: RequestBuilder,
    what: &str,
) -> Result<T, FeedError> {
    let body = fetch_bytes(request, what).await?;
    serde_json::from_slice(&body)
        .map_err(|e| FeedError::Parsing(format!("Failed to parse {what} response: {e}")))
}

/// A local HTTP/1.1 server for feed tests: answers every connection through `respond`, which
/// maps the request line (e.g. `GET /price-levels?chainId=1 HTTP/1.1`) to a status and a JSON
/// body. Serves until the runtime drops it.
#[cfg(test)]
pub mod test_support {
    use std::{
        net::SocketAddr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    /// One canned HTTP response; `(status_line, body)` converts into it.
    pub struct MockResponse {
        pub status: &'static str,
        pub body: String,
        /// Wait before answering, for tests that exercise request deadlines.
        pub delay: Duration,
    }

    impl From<(&'static str, String)> for MockResponse {
        fn from((status, body): (&'static str, String)) -> Self {
            MockResponse { status, body, delay: Duration::ZERO }
        }
    }

    /// A running mock server: where it listens and how many requests it has answered.
    pub struct MockHttpServer {
        pub address: SocketAddr,
        pub requests: Arc<AtomicUsize>,
    }

    impl MockHttpServer {
        pub fn url(&self) -> String {
            format!("http://{}", self.address)
        }

        pub fn request_count(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    /// Serves HTTP/1.1 requests until the runtime drops it. `route` maps the request target
    /// (path plus query string) to a response; a target it returns `None` for gets a 404. Each
    /// request is counted before `route` runs, so a route closure holding its own counter can
    /// answer per attempt.
    pub async fn spawn_http_server<R: Into<MockResponse>>(
        route: impl Fn(&str) -> Option<R> + Send + 'static,
    ) -> MockHttpServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let served = Arc::clone(&requests);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                if reader
                    .read_line(&mut request_line)
                    .await
                    .is_err()
                {
                    continue;
                }
                served.fetch_add(1, Ordering::SeqCst);
                let target = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default();
                let response = route(target)
                    .map(Into::into)
                    .unwrap_or_else(|| {
                        MockResponse::from(("404 Not Found", format!("no route for {target}")))
                    });

                tokio::time::sleep(response.delay).await;

                let MockResponse { status, body, .. } = response;
                let payload = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let mut stream = reader.into_inner();
                let _ = stream
                    .write_all(payload.as_bytes())
                    .await;
                let _ = stream.shutdown().await;
            }
        });
        MockHttpServer { address, requests }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        sync::{
            atomic::{AtomicBool, AtomicU32, Ordering},
            Arc,
        },
    };

    use futures::StreamExt as _;
    use tokio::sync::watch;
    use tokio_stream::wrappers::WatchStream;

    use super::{super::expect_to_finish, *};

    /// Lets a closure stand in for a provider's source.
    impl<F, Fut, T> HttpSource for F
    where
        F: Fn() -> Fut + Send + Sync,
        Fut: Future<Output = Result<T, FeedError>> + Send,
        T: Send + Sync + 'static,
    {
        type Snapshot = T;

        async fn fetch(&self) -> Result<T, FeedError> {
            self().await
        }
    }

    /// The one snapshot the mock sources here publish.
    fn snapshot() -> u32 {
        1
    }

    fn config(max_consecutive_failures: Option<u32>) -> HttpFeedConfig {
        HttpFeedConfig {
            poll_interval: Duration::from_millis(5),
            request_timeout: Duration::from_millis(50),
            // A failed poll retries at the polling cadence: the tests that fail on purpose are
            // timing the loop, not the backoff curve.
            max_backoff_exp: 0,
            max_consecutive_failures,
            max_snapshot_age: None,
        }
    }

    #[tokio::test]
    async fn publishes_a_snapshot_and_success_resets_failures() {
        // One failed poll, then successes: with a limit of 2 the single failure must not
        // accumulate towards termination once a poll succeeds.
        let polls = Arc::new(AtomicU32::new(0));
        let polls_clone = Arc::clone(&polls);
        let (publisher, mut rx) = Publisher::channel();
        let feed = run_http_poll_feed(config(Some(2)), publisher, move || {
            let n = polls_clone.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(FeedError::Connection("boom".to_string()))
                } else {
                    Ok(snapshot())
                }
            }
        });
        let feed = tokio::spawn(feed);

        expect_to_finish("no snapshot published", async {
            loop {
                rx.changed().await.expect("feed ended");
                if rx.borrow_and_update().is_some() {
                    return;
                }
            }
        })
        .await;

        // Several more polls run without termination.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!feed.is_finished());
    }

    #[tokio::test]
    async fn withdraws_a_snapshot_that_goes_unrefreshed_and_restores_on_next_success() {
        // One successful poll, then failures until the flag flips: the published snapshot
        // must be withdrawn once it has gone max_snapshot_age without a refresh and come back
        // with the next successful poll.
        let polls = Arc::new(AtomicU32::new(0));
        let polls_clone = Arc::clone(&polls);
        let recovered = Arc::new(AtomicBool::new(false));
        let recovered_clone = Arc::clone(&recovered);
        let (publisher, mut rx) = Publisher::channel();
        let config =
            HttpFeedConfig { max_snapshot_age: Some(Duration::from_millis(20)), ..config(None) };
        let feed = run_http_poll_feed(config, publisher, move || {
            let n = polls_clone.fetch_add(1, Ordering::SeqCst);
            let recovered = recovered_clone.load(Ordering::SeqCst);
            async move {
                if n == 0 || recovered {
                    Ok(snapshot())
                } else {
                    Err(FeedError::Connection("boom".to_string()))
                }
            }
        });
        let _feed = tokio::spawn(feed);

        let wait_for = |rx: &mut watch::Receiver<Option<u32>>, want_some: bool| {
            let mut rx = rx.clone();
            async move {
                expect_to_finish("watch did not reach the expected state", async {
                    loop {
                        if rx.borrow_and_update().is_some() == want_some {
                            return;
                        }
                        rx.changed().await.expect("feed ended");
                    }
                })
                .await;
            }
        };
        wait_for(&mut rx, true).await;
        wait_for(&mut rx, false).await;
        recovered.store(true, Ordering::SeqCst);
        wait_for(&mut rx, true).await;
    }

    #[tokio::test]
    async fn rejects_zero_poll_interval() {
        let (publisher, _rx) = Publisher::<u32>::channel();
        let config = HttpFeedConfig { poll_interval: Duration::ZERO, ..config(None) };
        let result = run_http_poll_feed(config, publisher, || async { Ok(snapshot()) }).await;
        assert!(matches!(result, Err(FeedError::InvalidInput(_))));
    }

    #[tokio::test]
    async fn rejects_a_zero_max_snapshot_age() {
        let (publisher, _rx) = Publisher::<u32>::channel();
        let config = HttpFeedConfig { max_snapshot_age: Some(Duration::ZERO), ..config(None) };
        let result = run_http_poll_feed(config, publisher, || async { Ok(snapshot()) }).await;
        assert!(matches!(result, Err(FeedError::InvalidInput(_))));
    }

    /// A snapshot may be servable for less than the gap between polls: a consumer that would
    /// rather decline than quote an old book asks for exactly that when the venue's rate limit
    /// sets the cadence. The feed serves each snapshot for `max_snapshot_age` and withdraws it
    /// until the next poll.
    #[tokio::test(start_paused = true)]
    async fn a_max_snapshot_age_below_the_poll_interval_is_a_duty_cycle() {
        let (publisher, rx) = Publisher::<u32>::channel();
        let mut changes = WatchStream::from_changes(rx);
        let config = HttpFeedConfig {
            poll_interval: Duration::from_millis(100),
            max_snapshot_age: Some(Duration::from_millis(20)),
            ..config(None)
        };
        let _feed =
            tokio::spawn(run_http_poll_feed(config, publisher, || async { Ok(snapshot()) }));

        let start = tokio::time::Instant::now();
        let mut seen = Vec::new();
        for _ in 0..4 {
            let change = expect_to_finish("the feed stopped changing", changes.next()).await;
            seen.push((change.expect("the feed is still running"), start.elapsed()));
        }

        assert_eq!(
            seen,
            [
                // The first poll fires immediately; each snapshot stands for its 20 ms and the
                // feed serves nothing for the remaining 80 ms of the interval.
                (Some(snapshot()), Duration::ZERO),
                (None, Duration::from_millis(20)),
                (Some(snapshot()), Duration::from_millis(100)),
                (None, Duration::from_millis(120)),
            ]
        );
    }

    #[tokio::test]
    async fn gives_up_after_max_consecutive_failed_polls() {
        // A provider that answers every time with something unparseable is not a connection
        // failure, so the feed ends with the failure that ended it, as its source classified it.
        let (publisher, _rx) = Publisher::<u32>::channel();
        let feed = run_http_poll_feed(config(Some(3)), publisher, || async {
            Err(FeedError::Parsing("not a snapshot".to_string()))
        });
        let feed = tokio::spawn(feed);

        let result = expect_to_finish("feed did not give up", feed)
            .await
            .unwrap();
        assert!(matches!(result, Err(FeedError::Parsing(msg)) if msg == "not a snapshot"));
    }

    #[tokio::test]
    async fn gives_up_on_a_fatal_poll_error_despite_an_unlimited_failure_budget() {
        // A rejected key or an unsupported chain answers every poll the same way, so the
        // feed stops instead of asking forever — and the snapshot it had goes with it.
        let (publisher, mut rx) = Publisher::channel();
        let polls = Arc::new(AtomicU32::new(0));
        let polls_clone = Arc::clone(&polls);
        let feed = run_http_poll_feed(config(None), publisher, move || {
            let n = polls_clone.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Ok(snapshot())
                } else {
                    Err(FeedError::Fatal("api key rejected".to_string()))
                }
            }
        });
        let feed = tokio::spawn(feed);

        let result = expect_to_finish("feed did not give up", feed)
            .await
            .unwrap();

        assert!(matches!(result, Err(FeedError::Fatal(_))));
        assert_eq!(polls.load(Ordering::SeqCst), 2, "the feed polled again after the fatal");
        assert!(rx.borrow_and_update().is_none(), "the snapshot must be withdrawn");
    }

    #[tokio::test]
    async fn withdraws_the_published_snapshot_when_it_gives_up() {
        // A snapshot published before the feed gave up must not stay servable: nothing is
        // refreshing it, and a consumer holding the receiver would keep using it.
        let polls = Arc::new(AtomicU32::new(0));
        let polls_clone = Arc::clone(&polls);
        let (publisher, mut rx) = Publisher::channel();
        let feed = run_http_poll_feed(config(Some(2)), publisher, move || {
            let n = polls_clone.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Ok(snapshot())
                } else {
                    Err(FeedError::Connection("boom".to_string()))
                }
            }
        });
        let feed = tokio::spawn(feed);

        expect_to_finish("no snapshot published", async {
            loop {
                rx.changed()
                    .await
                    .expect("feed ended before publishing");
                if rx.borrow_and_update().is_some() {
                    return;
                }
            }
        })
        .await;

        let result = expect_to_finish("feed did not give up", feed)
            .await
            .unwrap();
        assert!(matches!(result, Err(FeedError::Connection(_))));
        assert!(rx.borrow().is_none(), "the snapshot must be withdrawn when the feed gives up");
    }

    #[tokio::test]
    async fn withdraws_the_published_snapshot_when_the_feed_is_dropped() {
        // Dropping the feed future — the consumer aborting its task — leaves the receiver
        // alive; the snapshot stops being refreshed and must stop being served with it.
        let (publisher, mut rx) = Publisher::channel();
        let feed = run_http_poll_feed(config(None), publisher, || async { Ok(snapshot()) });
        let feed = tokio::spawn(feed);

        expect_to_finish("no snapshot published", async {
            loop {
                rx.changed()
                    .await
                    .expect("feed ended before publishing");
                if rx.borrow_and_update().is_some() {
                    return;
                }
            }
        })
        .await;

        feed.abort();
        expect_to_finish("the snapshot was not withdrawn", rx.changed())
            .await
            .expect("the withdrawal must reach the receiver before the sender drops");
        assert!(rx.borrow().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn failing_polls_stretch_the_polling_cadence() {
        // A 50 ms cadence doubling at most three times: a provider that is down is polled at 0
        // and then after 50, 100, 200, 400 and 400 ms — 5 attempts in the first second, where the
        // healthy cadence would make 20.
        let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let attempts_clone = Arc::clone(&attempts);
        let start = tokio::time::Instant::now();
        let (publisher, _rx) = Publisher::<u32>::channel();
        let feed = tokio::spawn(run_http_poll_feed(
            HttpFeedConfig {
                poll_interval: Duration::from_millis(50),
                max_backoff_exp: 3,
                ..config(None)
            },
            publisher,
            move || {
                attempts_clone
                    .lock()
                    .unwrap()
                    .push(start.elapsed().as_millis());
                async { Err(FeedError::Connection("boom".to_string())) }
            },
        ));

        tokio::time::sleep(Duration::from_secs(1)).await;
        feed.abort();

        assert_eq!(*attempts.lock().unwrap(), vec![0, 50, 150, 350, 750]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_recovered_poll_resumes_the_cadence_instead_of_catching_up() {
        // Two failures stretch the 50 ms cadence to 50 and 100 ms, so the third attempt polls at
        // 150 ms and succeeds. The polls after it are one plain interval apart: the backoff took
        // the place of those ticks, so none of them are owed.
        let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let attempts_clone = Arc::clone(&attempts);
        let start = tokio::time::Instant::now();
        let (publisher, _rx) = Publisher::<u32>::channel();
        let feed = tokio::spawn(run_http_poll_feed(
            HttpFeedConfig {
                poll_interval: Duration::from_millis(50),
                max_backoff_exp: 3,
                ..config(None)
            },
            publisher,
            move || {
                let mut attempts = attempts_clone.lock().unwrap();
                attempts.push(start.elapsed().as_millis());
                let failing = attempts.len() <= 2;
                async move {
                    if failing {
                        Err(FeedError::Connection("boom".to_string()))
                    } else {
                        Ok(snapshot())
                    }
                }
            },
        ));

        tokio::time::sleep(Duration::from_millis(300)).await;
        feed.abort();

        assert_eq!(*attempts.lock().unwrap(), vec![0, 50, 150, 200, 250]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_poll_never_shortens_the_gap_to_the_next_one() {
        // The third poll starts at 100 ms and answers only at 220 ms, having run through two
        // ticks. The polls that follow it are a full 50 ms interval apart — the ticks it ran
        // through are not owed back, and none of them lands sooner than the cadence allows, which
        // is what a venue's rate limit is promised.
        let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let attempts_clone = Arc::clone(&attempts);
        let start = tokio::time::Instant::now();
        let (publisher, _rx) = Publisher::<u32>::channel();
        let feed = tokio::spawn(run_http_poll_feed(
            HttpFeedConfig {
                poll_interval: Duration::from_millis(50),
                request_timeout: Duration::from_millis(500),
                ..config(None)
            },
            publisher,
            move || {
                let mut attempts = attempts_clone.lock().unwrap();
                attempts.push(start.elapsed().as_millis());
                let slow = attempts.len() == 3;
                async move {
                    if slow {
                        tokio::time::sleep(Duration::from_millis(120)).await;
                    }
                    Ok(snapshot())
                }
            },
        ));

        tokio::time::sleep(Duration::from_millis(350)).await;
        feed.abort();

        assert_eq!(*attempts.lock().unwrap(), vec![0, 50, 100, 220, 270, 320]);
    }

    #[tokio::test]
    async fn retries_forever_without_limit() {
        let polls = Arc::new(AtomicU32::new(0));
        let polls_clone = Arc::clone(&polls);
        let (publisher, _rx) = Publisher::<u32>::channel();
        let feed = run_http_poll_feed(config(None), publisher, move || {
            polls_clone.fetch_add(1, Ordering::SeqCst);
            async { Err(FeedError::Connection("boom".to_string())) }
        });
        let feed = tokio::spawn(feed);

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(polls.load(Ordering::SeqCst) > 5, "should keep polling");
        assert!(!feed.is_finished());
    }

    #[tokio::test]
    async fn hung_poll_counts_as_failure() {
        let (publisher, _rx) = Publisher::<u32>::channel();
        let feed = run_http_poll_feed(
            HttpFeedConfig { request_timeout: Duration::from_millis(5), ..config(Some(2)) },
            publisher,
            || async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(snapshot())
            },
        );
        let feed = tokio::spawn(feed);

        let result = expect_to_finish("feed did not give up", feed)
            .await
            .unwrap();
        assert!(matches!(result, Err(FeedError::Connection(msg)) if msg.contains("timed out")));
    }

    mod fetch_json {

        use rstest::rstest;
        use serde::Deserialize;

        use super::{test_support::spawn_http_server, *};

        #[derive(Debug, Deserialize, PartialEq)]
        struct Payload {
            value: u32,
        }

        #[tokio::test]
        async fn parses_a_successful_json_body() {
            let server =
                spawn_http_server(|_| Some(("200 OK", r#"{"value":7}"#.to_string()))).await;

            let payload: Payload =
                fetch_json(reqwest::Client::new().get(format!("{}/x", server.url())), "thing")
                    .await
                    .unwrap();

            assert_eq!(payload, Payload { value: 7 });
        }

        /// A transport failure or a non-success status is a connection error naming the resource
        /// (and, for a status, carrying it and the body); a body that is not the expected JSON is a
        /// parsing error.
        #[rstest]
        #[case::non_success_status_with_body("503 Service Unavailable", "maintenance", false)]
        #[case::unparseable_success_body("200 OK", "<html>", true)]
        #[tokio::test]
        async fn classifies_status_and_body_failures(
            #[case] status: &'static str,
            #[case] body: &'static str,
            #[case] expect_parsing_error: bool,
        ) {
            let server = spawn_http_server(move |_| Some((status, body.to_string()))).await;

            let result: Result<Payload, FeedError> =
                fetch_json(reqwest::Client::new().get(format!("{}/x", server.url())), "thing")
                    .await;

            match result {
                Err(FeedError::Parsing(msg)) if expect_parsing_error => {
                    assert!(msg.contains("thing"), "{msg}")
                }
                Err(FeedError::Connection(msg)) if !expect_parsing_error => {
                    assert!(
                        msg.contains("thing") && msg.contains("503") && msg.contains(body),
                        "{msg}"
                    )
                }
                other => panic!("unexpected result: {other:?}"),
            }
        }

        #[tokio::test]
        async fn refused_connection_is_a_connection_error() {
            // Bind and drop so the port is known to be closed.
            let address = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap()
                .local_addr()
                .unwrap();

            let result: Result<Payload, FeedError> =
                fetch_json(reqwest::Client::new().get(format!("http://{address}/x")), "thing")
                    .await;

            assert!(matches!(result, Err(FeedError::Connection(msg)) if msg.contains("thing")));
        }
    }
}
