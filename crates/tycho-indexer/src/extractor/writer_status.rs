//! Whether this process writes extraction results, and what the writer has committed.
use std::{collections::HashMap, time::Duration};

use chrono::NaiveDateTime;
use tokio::{sync::watch, time::Instant};
use tycho_common::models::blockchain::Block;
use tycho_storage::postgres::cache::CommittedBlock;

use crate::extractor::protocol_extractor::FOLLOWER_RETENTION_SPAN;

/// The part this process plays in writing extraction results to the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Commits finalized blocks to the database.
    Leader,
    /// Keeps finalized blocks in memory until the leader has committed them.
    Follower,
}

/// Shared view of this process's [`Role`] and of the blocks the leader has committed.
///
/// A role only ever changes from [`Role::Follower`] to [`Role::Leader`].
#[derive(Clone, Debug)]
pub struct WriterStatus {
    role: watch::Receiver<Role>,
    committed_blocks: watch::Receiver<HashMap<String, CommittedBlock>>,
    lead_confirmed_at: Option<watch::Receiver<Instant>>,
}

impl WriterStatus {
    /// A process that is the leader for its whole lifetime.
    pub fn leader() -> Self {
        let (_, role) = watch::channel(Role::Leader);
        let (_, committed_blocks) = watch::channel(HashMap::new());
        Self { role, committed_blocks, lead_confirmed_at: None }
    }

    /// Follows `role`, reads the leader's committed blocks, keyed by extractor name, from
    /// `committed_blocks`, and reads from `lead_confirmed_at` when this process last confirmed
    /// that it still holds the leader lock.
    pub fn new(
        role: watch::Receiver<Role>,
        committed_blocks: watch::Receiver<HashMap<String, CommittedBlock>>,
        lead_confirmed_at: watch::Receiver<Instant>,
    ) -> Self {
        Self { role, committed_blocks, lead_confirmed_at: Some(lead_confirmed_at) }
    }

    /// Returns the current role of this process.
    pub fn role(&self) -> Role {
        *self.role.borrow()
    }

    /// Returns how long ago this process last confirmed its leader lock.
    ///
    /// `None` when this process is a follower, or when it does not track confirmations (a process
    /// that is the leader for its whole lifetime).
    pub fn time_since_lead_confirmed(&self) -> Option<Duration> {
        if self.role() != Role::Leader {
            return None;
        }
        let confirmed_at = self.lead_confirmed_at.as_ref()?;
        Some(confirmed_at.borrow().elapsed())
    }

    /// Returns the newest block the leader has committed for `extractor`, or `None` when it has
    /// not committed any block for it yet.
    pub fn committed_height(&self, extractor: &str) -> Option<u64> {
        self.committed_blocks
            .borrow()
            .get(extractor)
            .map(|block| block.number)
    }

    /// Returns whether this process needs the stream of `extractor` at `now`.
    ///
    /// A leader always needs it. A follower needs it only while the leader's newest committed
    /// block for `extractor` is younger than [`FOLLOWER_RETENTION_SPAN`]. An older block means
    /// that the leader syncs history for the extractor: a follower would pay a second time for the
    /// same history, it cannot serve history, and its retained blocks could not reach the head.
    /// Without a committed block, the leader syncs the extractor for the first time, so a follower
    /// does not need it.
    pub fn needs_stream(&self, extractor: &str, now: NaiveDateTime) -> bool {
        if self.role() == Role::Leader {
            return true;
        }
        self.committed_blocks
            .borrow()
            .get(extractor)
            .is_some_and(|block| now.signed_duration_since(block.ts) < FOLLOWER_RETENTION_SPAN)
    }

    /// Returns the extractors on which this process is behind the chain head at `now`, sorted by
    /// name. `newest_blocks` holds the newest block this process received for each extractor.
    ///
    /// An extractor is behind when it has no block, or when its newest block is older than
    /// `tolerance`. Only extractors for which [`Self::needs_stream`] is true count: for the
    /// others the leader syncs history or has committed nothing yet, so this process does not
    /// stream them and a new leader resumes them from the database cursor. A leader is never
    /// behind.
    pub(crate) fn extractors_behind_head(
        &self,
        newest_blocks: &HashMap<String, Option<Block>>,
        now: NaiveDateTime,
        tolerance: chrono::Duration,
    ) -> Vec<ExtractorBehindHead> {
        if self.role() == Role::Leader {
            return Vec::new();
        }
        let mut behind = Vec::new();
        for (extractor, newest) in newest_blocks {
            if !self.needs_stream(extractor, now) {
                continue;
            }
            let newest = newest
                .as_ref()
                .map(|block| NewestBlock { number: block.number, age: now - block.ts });
            if newest
                .as_ref()
                .is_none_or(|block| block.age > tolerance)
            {
                behind.push(ExtractorBehindHead { extractor: extractor.clone(), newest });
            }
        }
        behind.sort_by(|a, b| a.extractor.cmp(&b.extractor));
        behind
    }
}

/// An extractor whose newest block on this process is missing or too old.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExtractorBehindHead {
    extractor: String,
    /// The newest block this process received, `None` for none.
    newest: Option<NewestBlock>,
}

/// The number of a block and how long ago the chain produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NewestBlock {
    number: u64,
    age: chrono::Duration,
}

impl std::fmt::Display for ExtractorBehindHead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.newest {
            Some(block) => write!(
                f,
                "{} at block {}, {}s old",
                self.extractor,
                block.number,
                block.age.num_seconds()
            ),
            None => write!(f, "{} has no block", self.extractor),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(role: Role, confirmed_at: Instant) -> WriterStatus {
        let (_, role) = watch::channel(role);
        let (_, committed) = watch::channel(HashMap::new());
        let (_, confirmed_at) = watch::channel(confirmed_at);
        WriterStatus::new(role, committed, confirmed_at)
    }

    fn ago(duration: Duration) -> Instant {
        Instant::now()
            .checked_sub(duration)
            .expect("the clock should be past the duration")
    }

    #[test]
    fn test_leader_reports_time_since_its_last_lock_confirmation() {
        let leader = status(Role::Leader, ago(Duration::from_secs(40)));

        let since_confirmed = leader
            .time_since_lead_confirmed()
            .expect("a leader that tracks confirmations reports one");

        assert!(since_confirmed >= Duration::from_secs(40));
        assert!(since_confirmed < Duration::from_secs(41));
    }

    #[test]
    fn test_follower_and_untracked_leader_report_no_lock_confirmation() {
        let follower = status(Role::Follower, ago(Duration::from_secs(40)));

        assert_eq!(follower.time_since_lead_confirmed(), None);
        assert_eq!(WriterStatus::leader().time_since_lead_confirmed(), None);
    }

    fn now() -> NaiveDateTime {
        chrono::DateTime::from_timestamp(1_800_000_000, 0)
            .expect("a valid timestamp")
            .naive_utc()
    }

    fn follower_with_committed_block_of_age(age: Option<chrono::Duration>) -> WriterStatus {
        let (_, role) = watch::channel(Role::Follower);
        let committed = age
            .map(|age| {
                HashMap::from([(
                    "uniswap_v2".to_string(),
                    CommittedBlock { number: 100, ts: now() - age },
                )])
            })
            .unwrap_or_default();
        let (_, committed) = watch::channel(committed);
        let (_, confirmed_at) = watch::channel(Instant::now());
        WriterStatus::new(role, committed, confirmed_at)
    }

    fn follower_with_committed_blocks(committed: &[(&str, u64, chrono::Duration)]) -> WriterStatus {
        let (_, role) = watch::channel(Role::Follower);
        let committed = committed
            .iter()
            .map(|&(extractor, number, age)| {
                (extractor.to_string(), CommittedBlock { number, ts: now() - age })
            })
            .collect();
        let (_, committed) = watch::channel(committed);
        let (_, confirmed_at) = watch::channel(Instant::now());
        WriterStatus::new(role, committed, confirmed_at)
    }

    const TOLERANCE: chrono::Duration = chrono::Duration::seconds(60);

    /// The newest block of each extractor: `(name, Some((number, age)))`, or `None` for none.
    fn newest(
        blocks: &[(&str, Option<(u64, chrono::Duration)>)],
    ) -> HashMap<String, Option<Block>> {
        blocks
            .iter()
            .map(|&(extractor, newest)| {
                let block = newest.map(|(number, age)| {
                    let mut block = crate::testing::block(number);
                    block.ts = now() - age;
                    block
                });
                (extractor.to_string(), block)
            })
            .collect()
    }

    fn behind(extractor: &str, newest: Option<(u64, chrono::Duration)>) -> ExtractorBehindHead {
        ExtractorBehindHead {
            extractor: extractor.to_string(),
            newest: newest.map(|(number, age)| NewestBlock { number, age }),
        }
    }

    #[test]
    fn test_follower_is_behind_on_extractors_without_a_block_within_the_tolerance() {
        let minute = chrono::Duration::minutes(1);
        let second = chrono::Duration::seconds(1);
        let follower = follower_with_committed_blocks(&[
            ("uniswap_v3", 100, minute),
            ("uniswap_v2", 100, minute),
            ("sushiswap", 100, minute),
            ("balancer", 100, minute),
        ]);

        let lagging = follower.extractors_behind_head(
            &newest(&[
                ("uniswap_v3", Some((120, TOLERANCE + second))),
                ("uniswap_v2", None),
                ("sushiswap", Some((121, TOLERANCE))),
                ("balancer", Some((122, second))),
            ]),
            now(),
            TOLERANCE,
        );

        assert_eq!(
            lagging,
            vec![behind("uniswap_v2", None), behind("uniswap_v3", Some((120, TOLERANCE + second)))]
        );
    }

    #[test]
    fn test_follower_is_not_behind_on_extractors_it_does_not_stream() {
        let syncing_age = FOLLOWER_RETENTION_SPAN + chrono::Duration::minutes(1);
        let follower = follower_with_committed_blocks(&[("syncing", 100, syncing_age)]);

        let lagging = follower.extractors_behind_head(
            &newest(&[("syncing", None), ("never_committed", None)]),
            now(),
            TOLERANCE,
        );

        assert_eq!(lagging, vec![]);
    }

    #[test]
    fn test_leader_is_never_behind() {
        let lagging = WriterStatus::leader().extractors_behind_head(
            &newest(&[("uniswap_v2", None)]),
            now(),
            TOLERANCE,
        );

        assert_eq!(lagging, vec![]);
    }

    #[test]
    fn test_leader_needs_the_stream_without_a_committed_block() {
        assert!(WriterStatus::leader().needs_stream("uniswap_v2", now()));
    }

    #[test]
    fn test_follower_does_not_need_the_stream_before_the_first_commit() {
        let follower = follower_with_committed_block_of_age(None);

        assert!(!follower.needs_stream("uniswap_v2", now()));
    }

    #[test]
    fn test_follower_needs_the_stream_only_while_the_leader_is_near_the_head() {
        let one_minute = chrono::Duration::minutes(1);
        let near_head =
            follower_with_committed_block_of_age(Some(FOLLOWER_RETENTION_SPAN - one_minute));
        let syncing =
            follower_with_committed_block_of_age(Some(FOLLOWER_RETENTION_SPAN + one_minute));

        assert!(near_head.needs_stream("uniswap_v2", now()));
        assert!(!syncing.needs_stream("uniswap_v2", now()));
        assert!(
            !near_head.needs_stream("sushiswap", now()),
            "a block committed for another extractor does not count"
        );
    }
}
