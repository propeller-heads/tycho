//! The desks whose listings a feed serves.
//!
//! Anyone can open a listing on the front and post quotes to it, so a listing is served only
//! where its maker's desk contract is approved: the set given to
//! [`EfficientPropFeedBuilder::with_approved_desks`], or the list read from
//! [`EfficientPropFeedBuilder::with_approved_desks_url`] while that list is fresh.
//!
//! The URL answers a GET with `{"chainId": 1, "updatedAt": <Unix seconds>, "desks": ["0x…"]}`,
//! `updatedAt` being when the list last changed; fields beside those three are ignored, and an
//! address reads in either case. The list is read every [`READ_EVERY`] at most. The last answer
//! taken rules, in place of the builder's own set, until its read is [`DEFAULT_MAX_AGE`] old (or
//! the age the builder was given): an empty list approves nobody. Past that age the builder's set
//! rules less every desk that answer left out, and before any answer the builder's set alone, so
//! a URL that fails never brings a desk in: not one of the list's the builder did not name, and
//! not one of the builder's the list had dropped. An answer is refused whole, and changes
//! nothing, where its status is not 200, its body is over [`MAX_BODY`] bytes or does not parse,
//! it is for another chain, names more than [`MAX_DESKS`] desks or the zero address, or is
//! stamped zero, before the answer held or more than a minute ahead of the wall clock.
//!
//! [`EfficientPropFeedBuilder::with_approved_desks`]:
//! crate::pamm::protocols::efficient_prop::EfficientPropFeedBuilder::with_approved_desks
//! [`EfficientPropFeedBuilder::with_approved_desks_url`]:
//! crate::pamm::protocols::efficient_prop::EfficientPropFeedBuilder::with_approved_desks_url

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy::primitives::Address;
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use tracing::warn;

use crate::{
    pamm::protocols::efficient_prop::overrides::capped_body, snapshot_feed::errors::FeedError,
};

/// How old the read of a list may be while the list rules, by default.
pub(super) const DEFAULT_MAX_AGE: Duration = Duration::from_secs(600);

/// The largest answer read, and the most desks one may name.
pub(super) const MAX_BODY: usize = 64 * 1024;
pub(super) const MAX_DESKS: usize = 1024;

/// How far ahead of the wall clock an answer's `updatedAt` may be, seconds.
const MAX_AHEAD_SECS: u64 = 60;

/// How often the list is read at most, so how long a desk the list dropped is still served at
/// most.
pub(super) const READ_EVERY: Duration = Duration::from_secs(15);

/// The URL's answer.
#[derive(Debug, Deserialize)]
struct DeskList {
    #[serde(rename = "chainId")]
    chain_id: u64,
    #[serde(rename = "updatedAt")]
    updated_at: u64,
    desks: Vec<Address>,
}

/// The newest answer taken: its `updatedAt`, when it was read (both Unix seconds), its desks, and
/// those of them the builder's own set names, which rule once the read is too old.
#[derive(Debug)]
struct Taken {
    updated_at: u64,
    read_at: u64,
    desks: Arc<BTreeSet<Address>>,
    also_fixed: Arc<BTreeSet<Address>>,
}

/// What the last reads of the URL left: when it was last asked (Unix seconds), and the newest
/// answer taken.
#[derive(Debug, Default)]
struct Held {
    asked_at: Option<u64>,
    list: Option<Taken>,
}

/// The list read from a URL. Neither its `Debug` output nor its errors carry the URL, which may
/// carry a key.
#[derive(derive_more::Debug)]
struct Remote {
    #[debug(skip)]
    client: Client,
    #[debug(skip)]
    url: Url,
    chain_id: u64,
    max_age_secs: u64,
    held: Mutex<Held>,
}

/// The desk contracts approved, read by every poll.
#[derive(Clone, derive_more::Debug)]
pub(super) struct ApprovedDesks {
    fixed: Arc<BTreeSet<Address>>,
    remote: Option<Arc<Remote>>,
    /// The wall clock, Unix seconds.
    #[debug(skip)]
    pub(super) clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl ApprovedDesks {
    /// The desks `fixed` and, where `url` is given, the list it answers for chain `chain_id`,
    /// each read given `timeout` and ruling until it is `max_age` old.
    pub(super) fn new(
        fixed: BTreeSet<Address>,
        url: Option<Url>,
        chain_id: u64,
        max_age: Duration,
        timeout: Duration,
    ) -> Result<Self, FeedError> {
        let remote = url
            .map(|url| {
                let client = Client::builder()
                    .timeout(timeout)
                    .build()
                    .map_err(|e| {
                        FeedError::InvalidInput(format!("Efficient PropAMM desk list client: {e}"))
                    })?;
                Ok::<_, FeedError>(Arc::new(Remote {
                    client,
                    url,
                    chain_id,
                    max_age_secs: max_age.as_secs(),
                    held: Mutex::default(),
                }))
            })
            .transpose()?;
        Ok(Self {
            fixed: Arc::new(fixed),
            remote,
            clock: Arc::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs())
            }),
        })
    }

    /// The desks approved now: the URL's list while its read is fresh, the builder's own set less
    /// the desks that list left out once it is not, and the builder's set before any list. Reads
    /// the URL where it is due, and never fails: a read that fails or is refused is logged, and
    /// leaves what ruled before it.
    pub(super) async fn current(&self) -> Arc<BTreeSet<Address>> {
        let Some(remote) = &self.remote else {
            return Arc::clone(&self.fixed);
        };
        let now = (self.clock)();
        let due = {
            let mut held = remote.held.lock().unwrap();
            let due = held
                .asked_at
                .is_none_or(|asked| now.saturating_sub(asked) >= READ_EVERY.as_secs());
            if due {
                held.asked_at = Some(now);
            }
            due
        };
        if due {
            let answer = remote.read(now).await;
            let mut held = remote.held.lock().unwrap();
            match answer {
                Ok(list)
                    if held
                        .list
                        .as_ref()
                        .is_some_and(|held| list.updated_at < held.updated_at) =>
                {
                    warn!(
                        updated_at = list.updated_at,
                        "Efficient PropAMM desk list refused: stamped before the list held"
                    );
                }
                Ok(list) => {
                    let desks: BTreeSet<Address> = list.desks.into_iter().collect();
                    held.list = Some(Taken {
                        updated_at: list.updated_at,
                        read_at: now,
                        also_fixed: Arc::new(
                            desks
                                .intersection(&self.fixed)
                                .copied()
                                .collect(),
                        ),
                        desks: Arc::new(desks),
                    });
                }
                Err(error) => warn!(%error, "Efficient PropAMM desk list not read"),
            }
        }
        let held = remote.held.lock().unwrap();
        match &held.list {
            Some(taken) if now.saturating_sub(taken.read_at) <= remote.max_age_secs => {
                Arc::clone(&taken.desks)
            }
            Some(taken) => Arc::clone(&taken.also_fixed),
            None => Arc::clone(&self.fixed),
        }
    }
}

impl Remote {
    /// One read of the URL, the wall clock reading `now`.
    async fn read(&self, now: u64) -> Result<DeskList, FeedError> {
        let connection = |e: reqwest::Error| {
            FeedError::Connection(format!("Efficient PropAMM desk list: {}", e.without_url()))
        };
        let response = self
            .client
            .get(self.url.clone())
            .send()
            .await
            .map_err(connection)?;
        let status = response.status();
        if status != StatusCode::OK {
            return Err(FeedError::Connection(format!(
                "Efficient PropAMM desk list: HTTP {status}"
            )));
        }
        let body = capped_body(response, MAX_BODY, "Efficient PropAMM desk list").await?;
        let list: DeskList = serde_json::from_slice(&body)
            .map_err(|e| FeedError::Parsing(format!("Efficient PropAMM desk list: {e}")))?;
        if list.chain_id != self.chain_id {
            return Err(FeedError::Parsing(format!(
                "Efficient PropAMM desk list is for chain {}, not {}",
                list.chain_id, self.chain_id
            )));
        }
        if list.updated_at == 0 || list.updated_at > now.saturating_add(MAX_AHEAD_SECS) {
            return Err(FeedError::Parsing(format!(
                "Efficient PropAMM desk list is stamped {}, the clock reading {now}",
                list.updated_at
            )));
        }
        if list.desks.len() > MAX_DESKS || list.desks.contains(&Address::ZERO) {
            return Err(FeedError::Parsing(format!(
                "Efficient PropAMM desk list names {} desks, over {MAX_DESKS} or the zero address",
                list.desks.len()
            )));
        }
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use alloy::primitives::address;

    use super::*;
    use crate::snapshot_feed::http::test_support::spawn_http_server;

    const A: Address = Address::repeat_byte(0xa);
    const B: Address = Address::repeat_byte(0xb);
    const NOW: u64 = 1_800_000_000;

    fn list(chain: u64, updated_at: u64, desks: &[Address]) -> String {
        serde_json::json!({ "chainId": chain, "updatedAt": updated_at, "desks": desks }).to_string()
    }

    /// The desks approved through `url` beside the builder's own `[A]`, the clock reading `now`.
    fn approved(url: &str, now: &Arc<AtomicU64>) -> ApprovedDesks {
        let mut desks = ApprovedDesks::new(
            BTreeSet::from([A]),
            Some(Url::parse(url).unwrap()),
            1,
            DEFAULT_MAX_AGE,
            Duration::from_millis(200),
        )
        .unwrap();
        let now = Arc::clone(now);
        desks.clock = Arc::new(move || now.load(Ordering::Relaxed));
        desks
    }

    #[tokio::test]
    async fn without_a_url_the_builders_own_set_rules() {
        let desks =
            ApprovedDesks::new(BTreeSet::from([A]), None, 1, DEFAULT_MAX_AGE, Duration::ZERO)
                .unwrap();

        assert_eq!(*desks.current().await, BTreeSet::from([A]));
    }

    /// A list taken replaces the builder's set, desks it leaves out included, until its read is
    /// ten minutes old, however long ago it last changed. A read that fails meanwhile (a 503, as
    /// the list's server answers with no valid source) changes nothing, and once the read is too
    /// old the builder's set rules less what the list left out: the failing URL brings no desk in.
    #[rstest::rstest]
    #[case::a_list_dropping_the_builders_desk(&[B], BTreeSet::new())]
    #[case::a_list_keeping_it(&[A, B], BTreeSet::from([A]))]
    #[case::an_empty_list(&[], BTreeSet::new())]
    #[tokio::test]
    async fn a_list_rules_until_its_read_is_too_old(
        #[case] listed: &'static [Address],
        #[case] after: BTreeSet<Address>,
    ) {
        let up = Arc::new(AtomicU64::new(1));
        let served = Arc::clone(&up);
        let server = spawn_http_server(move |_| {
            Some(if served.load(Ordering::Relaxed) == 1 {
                ("200 OK", list(1, NOW - 86_400, listed))
            } else {
                ("503 Service Unavailable", list(1, NOW, &[A, B]))
            })
        })
        .await;
        let now = Arc::new(AtomicU64::new(NOW));
        let desks = approved(&server.url(), &now);
        let listed: BTreeSet<Address> = listed.iter().copied().collect();

        assert_eq!(*desks.current().await, listed);
        up.store(0, Ordering::Relaxed);
        now.store(NOW + 600, Ordering::Relaxed);
        assert_eq!(*desks.current().await, listed);
        assert_eq!(server.request_count(), 2);
        now.store(NOW + 601, Ordering::Relaxed);
        assert_eq!(*desks.current().await, after);
    }

    /// The list is asked again only once fifteen seconds have passed.
    #[tokio::test]
    async fn the_list_is_read_every_fifteen_seconds_at_most() {
        let server = spawn_http_server(|_| Some(("200 OK", list(1, NOW, &[B])))).await;
        let now = Arc::new(AtomicU64::new(NOW));
        let desks = approved(&server.url(), &now);

        desks.current().await;
        now.store(NOW + 14, Ordering::Relaxed);
        desks.current().await;
        assert_eq!(server.request_count(), 1);
        now.store(NOW + 15, Ordering::Relaxed);
        desks.current().await;
        assert_eq!(server.request_count(), 2);
    }

    /// The list's server sends fields beside the three read, and addresses in lower case; a
    /// checksummed address reads the same.
    #[rstest::rstest]
    #[case::lower_case("0x4f8dd270ace370de2f6ac5ebf3230cbe099a0753")]
    #[case::checksummed("0x4f8dd270acE370dE2f6Ac5EBf3230cBE099a0753")]
    #[tokio::test]
    async fn unknown_fields_are_ignored_and_an_address_reads_in_either_case(#[case] desk: &str) {
        let body = format!(
            r#"{{"chainId":1,"updatedAt":{NOW},"version":7,"bundleSha256":"0x{}","desks":["{desk}"]}}"#,
            "ab".repeat(32)
        );
        let server = spawn_http_server(move |_| Some(("200 OK", body.clone()))).await;
        let desks = approved(&server.url(), &Arc::new(AtomicU64::new(NOW)));

        assert_eq!(
            *desks.current().await,
            BTreeSet::from([address!("0x4f8dd270acE370dE2f6Ac5EBf3230cBE099a0753")])
        );
    }

    /// An answer that cannot be trusted leaves the builder's own set ruling: any status but 200
    /// whatever its body, another chain's, one stamped zero or ahead of the clock, one that is
    /// not the shape, one naming the zero address or too many desks, and one too large.
    #[rstest::rstest]
    #[case::no_valid_source("503 Service Unavailable", list(1, NOW, &[B]))]
    #[case::another_success_status("203 Non-Authoritative Information", list(1, NOW, &[B]))]
    #[case::another_chain("200 OK", list(8453, NOW, &[B]))]
    #[case::stamped_zero("200 OK", list(1, 0, &[B]))]
    #[case::ahead_of_the_clock("200 OK", list(1, NOW + 61, &[B]))]
    #[case::not_the_shape("200 OK", r#"{"chainId":1,"desks":["0x0b"]}"#.to_string())]
    #[case::the_zero_address("200 OK", list(1, NOW, &[B, Address::ZERO]))]
    #[case::too_many_desks("200 OK", list(1, NOW, &[B; MAX_DESKS + 1]))]
    #[case::too_large(
        "200 OK",
        format!(r#"{{"chainId":1,"updatedAt":{NOW},"desks":[],"pad":"{}"}}"#, "0".repeat(MAX_BODY))
    )]
    #[tokio::test]
    async fn a_refused_answer_leaves_the_builders_set(
        #[case] status: &'static str,
        #[case] body: String,
    ) {
        let server = spawn_http_server(move |_| Some((status, body.clone()))).await;
        let desks = approved(&server.url(), &Arc::new(AtomicU64::new(NOW)));

        assert_eq!(*desks.current().await, BTreeSet::from([A]));
    }

    #[tokio::test]
    async fn an_unreachable_url_leaves_the_builders_set() {
        let desks = approved("http://127.0.0.1:1/desks", &Arc::new(AtomicU64::new(NOW)));

        assert_eq!(*desks.current().await, BTreeSet::from([A]));
    }

    /// A list stamped before the one held does not replace it.
    #[tokio::test]
    async fn an_older_list_does_not_replace_a_newer_one() {
        let stamp = Arc::new(AtomicU64::new(NOW));
        let served = Arc::clone(&stamp);
        let server = spawn_http_server(move |_| {
            let at = served.load(Ordering::Relaxed);
            Some(("200 OK", list(1, at, if at == NOW { &[B] } else { &[A, B] })))
        })
        .await;
        let now = Arc::new(AtomicU64::new(NOW));
        let desks = approved(&server.url(), &now);

        desks.current().await;
        stamp.store(NOW - 1, Ordering::Relaxed);
        now.store(NOW + 60, Ordering::Relaxed);
        assert_eq!(*desks.current().await, BTreeSet::from([B]));
    }
}
