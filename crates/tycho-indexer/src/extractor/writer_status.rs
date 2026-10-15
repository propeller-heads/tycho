//! Whether this process writes extraction results, and what the writer has committed.
use std::collections::HashMap;

use tokio::sync::watch;

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
}

impl WriterStatus {
    /// A process that is the leader for its whole lifetime.
    pub fn leader() -> Self {
        let (_, role) = watch::channel(Role::Leader);
        let (_, committed_heights) = watch::channel(HashMap::new());
        Self { role, committed_heights }
    }

    /// Follows `role`, and reads the leader's committed heights, keyed by extractor name, from
    /// `committed_heights`.
    pub fn new(
        role: watch::Receiver<Role>,
        committed_heights: watch::Receiver<HashMap<String, u64>>,
    ) -> Self {
        Self { role, committed_heights }
    }

    /// Returns the current role of this process.
    pub fn role(&self) -> Role {
        *self.role.borrow()
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
