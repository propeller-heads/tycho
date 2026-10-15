//! Whether this process writes extraction results, and what the writer has committed.
use std::{collections::HashMap, time::Duration};

use tokio::{sync::watch, time::Instant};

/// The part this process plays in writing extraction results to the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Commits finalized blocks to the database.
    Leader,
    /// Keeps finalized blocks in memory until the leader has committed them.
    Follower,
}

/// Shared view of this process's [`Role`] and of the block heights the leader has committed.
///
/// A role only ever changes from [`Role::Follower`] to [`Role::Leader`].
#[derive(Clone, Debug)]
pub struct WriterStatus {
    role: watch::Receiver<Role>,
    committed_heights: watch::Receiver<HashMap<String, u64>>,
    lead_confirmed_at: Option<watch::Receiver<Instant>>,
}

impl WriterStatus {
    /// A process that is the leader for its whole lifetime.
    pub fn leader() -> Self {
        let (_, role) = watch::channel(Role::Leader);
        let (_, committed_heights) = watch::channel(HashMap::new());
        Self { role, committed_heights, lead_confirmed_at: None }
    }

    /// Follows `role`, and reads the leader's committed heights, keyed by extractor name, from
    /// `committed_heights`.
    pub fn new(
        role: watch::Receiver<Role>,
        committed_heights: watch::Receiver<HashMap<String, u64>>,
    ) -> Self {
        Self { role, committed_heights, lead_confirmed_at: None }
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
        self.committed_heights
            .borrow()
            .get(extractor)
            .copied()
    }
}
