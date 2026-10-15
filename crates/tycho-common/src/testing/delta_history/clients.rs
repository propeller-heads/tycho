//! The clients of a history: what one that follows the WebSocket holds, and where a client's view
//! of the history breaks.

use std::sync::Arc;

use rand::Rng;

use super::{
    generator::{History, HistoryEvent},
    model::DeltaStateModel,
    spec::Gap,
};
use crate::models::blockchain::{Block, BlockAggregatedChanges};

/// The state a snapshot at `at` gives: the canonical chain folded up to `at`, which is
/// committed and so the same at any later point of the history.
fn snapshot(history: &History, at: &Block) -> DeltaStateModel {
    let upto: Vec<&BlockAggregatedChanges> = history
        .canonical
        .iter()
        .map(Arc::as_ref)
        .take_while(|message| message.block.number <= at.number)
        .collect();
    assert_eq!(
        upto.last()
            .map(|message| &message.block),
        Some(at),
        "{at:?} is not canonical"
    );
    DeltaStateModel::fold(upto)
}

/// `states[i]` is what a client that subscribed at the start holds right after `events[i]`.
/// The client applies every message, and after every restart resyncs from a snapshot at the
/// last committed block before applying the replay.
///
/// # Panics
///
/// When a restart's last committed block is not on the history's canonical chain.
pub fn follower_states(history: &History) -> Vec<DeltaStateModel> {
    let mut client = DeltaStateModel::default();
    let mut resync_at = None;
    let mut states = Vec::with_capacity(history.events.len());
    for event in &history.events {
        match event {
            HistoryEvent::Message(message) => {
                if let Some(at) = resync_at.take() {
                    client = snapshot(history, &at);
                }
                client.apply(message);
            }
            HistoryEvent::ExtractorRestarted { last_committed, kind: _ } => {
                resync_at = Some(last_committed.clone());
            }
        }
        states.push(client.clone());
    }
    states
}

/// Gaps of every shape [`catch_up_spec`](super::catch_up_spec) tells apart: `random_gaps` random
/// ones up to three window depths long, and ones around every restart. A gap around a restart
/// starts at the client's last message before it, and at the last committed block when a message
/// before the restart carried it; it ends at each of the first three messages after the restart
/// and at the last one within three window depths.
///
/// The same `history`, `random_gaps` and `rng` state always give the same gaps.
///
/// # Panics
///
/// When the history holds fewer than two messages.
pub fn sample_gaps(history: &History, random_gaps: usize, rng: &mut impl Rng) -> Vec<Gap> {
    let mut messages = Vec::new();
    for (index, event) in history.events.iter().enumerate() {
        if let HistoryEvent::Message(_) = event {
            messages.push(index);
        }
    }
    let reach = 3 * history.config.window_depth as usize;
    let mut gaps = Vec::new();
    for _ in 0..random_gaps {
        let start = rng.gen_range(0..messages.len() - 1);
        let end = (start + rng.gen_range(1..=reach)).min(messages.len() - 1);
        gaps.push(Gap { last_applied: messages[start], first_live: messages[end] });
    }
    for (restart, event) in history.events.iter().enumerate() {
        let HistoryEvent::ExtractorRestarted { last_committed, kind: _ } = event else {
            continue;
        };
        let before: Vec<usize> = messages
            .iter()
            .copied()
            .filter(|&index| index < restart)
            .collect();
        let after: Vec<usize> = messages
            .iter()
            .copied()
            .filter(|&index| index > restart)
            .take(reach)
            .collect();
        let at_last_committed = before
            .iter()
            .rev()
            .copied()
            .find(|&index| {
                let HistoryEvent::Message(message) = &history.events[index] else {
                    return false;
                };
                &message.block == last_committed
            });
        let first_lives = after
            .iter()
            .take(3)
            .chain(after.last())
            .copied();
        for first_live in first_lives {
            for last_applied in before
                .last()
                .copied()
                .into_iter()
                .chain(at_last_committed)
            {
                gaps.push(Gap { last_applied, first_live });
            }
        }
    }
    gaps
}
