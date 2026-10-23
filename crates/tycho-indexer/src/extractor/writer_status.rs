//! Whether this process writes extraction results, and what the writer has committed.
use std::{collections::HashMap, time::Duration};

use chrono::NaiveDateTime;
use tokio::{sync::watch, time::Instant};
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
}
