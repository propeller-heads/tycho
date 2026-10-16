//! Whether this process writes extraction results, and what the writer has committed.
use std::{collections::HashMap, time::Duration};

use chrono::NaiveDateTime;
use tokio::{sync::watch, time::Instant};
use tycho_storage::postgres::cache::CommittedBlock;

/// Most age of the leader's newest committed block at which a follower streams an extractor.
///
/// An older block means that the leader syncs history for the extractor. A follower then does not
/// open the extractor's stream: it would pay a second time for the same history, and it cannot
/// serve history. Larger than the finality delay of every chain (about 13 minutes on Ethereum),
/// because the leader commits only finalized blocks.
const LEADER_NEAR_HEAD_AGE: chrono::Duration = chrono::Duration::hours(1);

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

    /// Follows `role`, and reads the leader's committed blocks, keyed by extractor name, from
    /// `committed_blocks`.
    pub fn new(
        role: watch::Receiver<Role>,
        committed_blocks: watch::Receiver<HashMap<String, CommittedBlock>>,
    ) -> Self {
        Self { role, committed_blocks, lead_confirmed_at: None }
    }

    /// Reads, from `lead_confirmed_at`, when this process last confirmed that it still holds the
    /// leader lock.
    pub fn with_lead_confirmations(mut self, lead_confirmed_at: watch::Receiver<Instant>) -> Self {
        self.lead_confirmed_at = Some(lead_confirmed_at);
        self
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
    /// block for `extractor` is younger than [`LEADER_NEAR_HEAD_AGE`]. Without a committed block,
    /// the leader syncs the extractor for the first time, so a follower does not need it.
    pub fn needs_stream(&self, extractor: &str, now: NaiveDateTime) -> bool {
        if self.role() == Role::Leader {
            return true;
        }
        self.committed_blocks
            .borrow()
            .get(extractor)
            .is_some_and(|block| now.signed_duration_since(block.ts) < LEADER_NEAR_HEAD_AGE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(role: Role, confirmed_at: Instant) -> WriterStatus {
        let (_, role) = watch::channel(role);
        let (_, committed) = watch::channel(HashMap::new());
        let (_, confirmed_at) = watch::channel(confirmed_at);
        WriterStatus::new(role, committed).with_lead_confirmations(confirmed_at)
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
        WriterStatus::new(role, committed)
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
        let near_head = follower_with_committed_block_of_age(Some(chrono::Duration::minutes(59)));
        let syncing = follower_with_committed_block_of_age(Some(chrono::Duration::minutes(61)));

        assert!(near_head.needs_stream("uniswap_v2", now()));
        assert!(!syncing.needs_stream("uniswap_v2", now()));
        assert!(
            !near_head.needs_stream("sushiswap", now()),
            "a block committed for another extractor does not count"
        );
    }
}
