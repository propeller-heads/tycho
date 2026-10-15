//! Properties of the model, the generator and the spec over random histories.

use std::{collections::HashMap, fmt::Display, sync::Arc};

use rand::{rngs::StdRng, SeedableRng};

use super::{
    catch_up_spec, follower_states, generate_history, sample_gaps, spec::Window, through_the_wire,
    CatchUpSpec, DeltaStateModel, FallbackReason, Gap, History, HistoryConfig, HistoryEvent,
    RestartKind,
};
use crate::models::blockchain::BlockAggregatedChanges;

/// Histories each property runs over.
const SEEDS: u64 = 128;
/// Random gaps the catch-up property draws per history, besides the gaps around restarts.
const RANDOM_GAPS: usize = 40;

fn assert_canonical(
    history: &History,
    client: &DeltaStateModel,
    index: usize,
    context: impl Display,
) {
    assert_eq!(
        client.check_against(&history.canonical_states[index], &history.connected_states[index]),
        Ok(()),
        "{context}, after event {index}"
    );
}

/// How often the shapes that make catch-up hard occur in a set of histories.
#[derive(Debug, Default)]
struct Shapes {
    reverts: usize,
    /// Reverts that orphan the target of an earlier revert the window still remembers.
    nested_reverts: usize,
    /// Reverts that delete a component the reverted range created.
    orphaned_creations: usize,
    unchanged_restarts: usize,
    upgraded_restarts: usize,
    /// Restarts whose replay left the blocks sent before the restart.
    downtime_reorgs: usize,
    evicted_blocks: usize,
    /// Evictions stopped by the finalized or committed height rather than by the window depth.
    watermark_bound_evictions: usize,
}

impl Shapes {
    fn add_eviction(&mut self, window: &Window, evicted: usize) {
        let (Some(tip), Some(finalized), Some(committed)) =
            (window.blocks.last(), window.finalized, window.committed)
        else {
            return;
        };
        self.evicted_blocks += evicted;
        let by_watermark = finalized.min(committed) <
            tip.block
                .number
                .saturating_sub(window.depth);
        self.watermark_bound_evictions += usize::from(evicted > 0 && by_watermark);
    }

    fn add(&mut self, history: &History) {
        let mut window = Window::new(history.config.window_depth);
        // The window's blocks when the extractor last restarted, until the replay leaves
        // them or a revert makes the comparison moot.
        let mut abandoned: Vec<Arc<BlockAggregatedChanges>> = Vec::new();
        for event in &history.events {
            let held = window.blocks.len();
            match event {
                HistoryEvent::Message(message) if message.revert => {
                    self.reverts += 1;
                    if !message
                        .deleted_protocol_components
                        .is_empty()
                    {
                        self.orphaned_creations += 1;
                    }
                    let target = message.block.number;
                    let nested = window.journal.iter().any(|entry| {
                        entry.revert.block.number > target &&
                            window
                                .position(&entry.revert.block.hash)
                                .is_some()
                    });
                    self.nested_reverts += usize::from(nested);
                    abandoned.clear();
                }
                HistoryEvent::Message(message) => {
                    let replaced = abandoned.iter().any(|old| {
                        old.block.number == message.block.number &&
                            old.block.hash != message.block.hash
                    });
                    if replaced {
                        self.downtime_reorgs += 1;
                        abandoned.clear();
                    }
                }
                HistoryEvent::ExtractorRestarted { last_committed: _, kind } => {
                    match kind {
                        RestartKind::Unchanged => self.unchanged_restarts += 1,
                        RestartKind::Upgraded => self.upgraded_restarts += 1,
                    }
                    abandoned = window.blocks.clone();
                }
            }
            window.apply(event);
            if let HistoryEvent::Message(message) = event {
                if !message.revert {
                    self.add_eviction(&window, held + 1 - window.blocks.len());
                }
            }
        }
    }
}

#[test]
fn folding_the_ws_sequence_matches_the_canonical_chain() {
    let config = HistoryConfig { restart_probability: 0.0, ..Default::default() };
    for seed in 0..32 {
        let history = generate_history(&config, seed);

        let ws = DeltaStateModel::fold(history.messages());
        let canonical = DeltaStateModel::fold(
            history
                .canonical
                .iter()
                .map(Arc::as_ref),
        );

        assert_eq!(ws.check_against(&canonical, &ws), Ok(()), "seed {seed}");
        assert_eq!(history.canonical_states.last(), Some(&canonical), "seed {seed}");
        assert_eq!(history.connected_states.last(), Some(&ws), "seed {seed}");
    }
}

#[test]
fn a_client_following_the_ws_matches_the_canonical_chain_after_every_event() {
    let config = HistoryConfig::default();
    let mut shapes = Shapes::default();
    for seed in 0..SEEDS {
        let history = generate_history(&config, seed);
        shapes.add(&history);

        for (index, client) in follower_states(&history)
            .iter()
            .enumerate()
        {
            assert_canonical(&history, client, index, format!("seed {seed}"));
        }
        let canonical = DeltaStateModel::fold(
            history
                .canonical
                .iter()
                .map(Arc::as_ref),
        );
        assert_eq!(history.canonical_states.last(), Some(&canonical), "seed {seed}");
        assert_eq!(
            history.connected_states.last(),
            Some(&DeltaStateModel::fold(history.messages())),
            "seed {seed}"
        );
    }

    let Shapes {
        reverts,
        nested_reverts,
        orphaned_creations,
        unchanged_restarts,
        upgraded_restarts,
        downtime_reorgs,
        evicted_blocks,
        watermark_bound_evictions,
    } = shapes;
    for (shape, count) in [
        ("reverts", reverts),
        ("nested reverts", nested_reverts),
        ("components created on orphaned branches", orphaned_creations),
        ("unchanged restarts", unchanged_restarts),
        ("upgraded restarts", upgraded_restarts),
        ("reorgs during downtime", downtime_reorgs),
        ("evictions", evicted_blocks),
        ("evictions bound by the finalized or committed height", watermark_bound_evictions),
    ] {
        assert!(count > 0, "no {shape} in {SEEDS} histories");
    }
}

/// How often each outcome of [`catch_up_spec`] occurs over a set of gaps.
#[derive(Debug, Default)]
struct Outcomes {
    possible: usize,
    through_a_revert: usize,
    through_nested_reverts: usize,
    across_a_restart: usize,
    /// Catch-ups after a restart from the last committed block, the parent of the window's
    /// oldest block.
    from_the_last_committed_block: usize,
    /// Reverts on a path that re-create a component the client saw deleted.
    recreating_a_deleted_component: usize,
    /// Reverts on a path that remove a component the client saw created.
    removing_a_created_component: usize,
    /// Reverts on a path that delete an attribute the client holds although the reverted range
    /// deleted it again: the client applied the block that created it, not the one that deleted
    /// it.
    deleting_an_attribute_the_range_deleted: usize,
    fallbacks: HashMap<FallbackReason, usize>,
}

impl Outcomes {
    /// Records which components `message`, a step of a path, re-creates or removes from the
    /// client's state before it, and whether it deletes an attribute the client holds that the
    /// reverted range deleted.
    fn add_step(
        &mut self,
        history: &History,
        client: &DeltaStateModel,
        message: &Arc<BlockAggregatedChanges>,
    ) {
        if !message.revert {
            return;
        }
        let recreates = message
            .new_protocol_components
            .keys()
            .any(|id| !client.components.contains_key(id));
        let removes = message
            .deleted_protocol_components
            .keys()
            .any(|id| client.components.contains_key(id));
        self.recreating_a_deleted_component += usize::from(recreates);
        self.removing_a_created_component += usize::from(removes);

        let sent_at = history
            .events
            .iter()
            .position(|event| match event {
                HistoryEvent::Message(sent) => Arc::ptr_eq(sent, message),
                HistoryEvent::ExtractorRestarted { last_committed: _, kind: _ } => false,
            })
            .expect("a path holds messages of its history");
        // The chain's state at the reverted tip, right before the revert.
        let reverted_tip = &history.canonical_states[sent_at - 1];
        let holds = |state: &DeltaStateModel, id: &String, name: &String| {
            state
                .component_attributes
                .get(id)
                .is_some_and(|attributes| attributes.contains_key(name))
        };
        let deletes_an_attribute_the_range_deleted =
            message
                .state_deltas
                .iter()
                .any(|(id, delta)| {
                    delta
                        .deleted_attributes
                        .iter()
                        .any(|name| holds(client, id, name) && !holds(reverted_tip, id, name))
                });
        self.deleting_an_attribute_the_range_deleted +=
            usize::from(deletes_an_attribute_the_range_deleted);
    }

    fn add_possible(&mut self, history: &History, gap: Gap, path: &[Arc<BlockAggregatedChanges>]) {
        let reverts = path
            .iter()
            .filter(|message| message.revert)
            .count();
        self.possible += 1;
        self.through_a_revert += usize::from(reverts >= 1);
        self.through_nested_reverts += usize::from(reverts >= 2);
        let HistoryEvent::Message(last) = &history.events[gap.last_applied] else {
            panic!("the spec accepted a gap that does not start at a message");
        };
        for event in &history.events[gap.last_applied + 1..gap.first_live] {
            if let HistoryEvent::ExtractorRestarted { last_committed, kind: _ } = event {
                self.across_a_restart += 1;
                if last_committed == &last.block {
                    self.from_the_last_committed_block += 1;
                }
            }
        }
    }
}

#[test]
fn a_possible_catch_up_ends_in_the_canonical_state() {
    let config = HistoryConfig::default();
    let mut rng = StdRng::seed_from_u64(0);
    let mut outcomes = Outcomes::default();
    for seed in 0..SEEDS {
        let history = generate_history(&config, seed);
        let follower = follower_states(&history);
        for gap in sample_gaps(&history, RANDOM_GAPS, &mut rng) {
            let path = match catch_up_spec(&history, gap) {
                CatchUpSpec::Possible(path) => path,
                CatchUpSpec::Fallback(reason) => {
                    *outcomes
                        .fallbacks
                        .entry(reason)
                        .or_default() += 1;
                    continue;
                }
            };
            outcomes.add_possible(&history, gap, &path);
            let context = format!("seed {seed}, {gap:?}");

            // The client's state before the gap, the path, then the live messages until the
            // next restart ends the new subscription.
            let mut client = follower[gap.last_applied].clone();
            for message in &path {
                outcomes.add_step(&history, &client, message);
                client.apply(message);
            }
            let mut last_live = gap.first_live;
            for (index, event) in history
                .events
                .iter()
                .enumerate()
                .skip(gap.first_live)
            {
                let HistoryEvent::Message(message) = event else {
                    break;
                };
                client.apply(message);
                if index == gap.first_live {
                    assert_canonical(&history, &client, index, &context);
                }
                last_live = index;
            }
            assert_canonical(&history, &client, last_live, &context);
        }
    }

    assert!(outcomes.possible > 0, "{outcomes:?}");
    assert!(outcomes.through_a_revert > 0, "{outcomes:?}");
    assert!(outcomes.through_nested_reverts > 0, "{outcomes:?}");
    assert!(outcomes.across_a_restart > 0, "{outcomes:?}");
    assert!(outcomes.from_the_last_committed_block > 0, "{outcomes:?}");
    assert!(outcomes.recreating_a_deleted_component > 0, "{outcomes:?}");
    assert!(outcomes.removing_a_created_component > 0, "{outcomes:?}");
    assert!(outcomes.deleting_an_attribute_the_range_deleted > 0, "{outcomes:?}");
    // `ToNotInWindow` needs a window behind the WebSocket, which a history cannot express.
    for reason in [
        FallbackReason::FingerprintChanged,
        FallbackReason::EmptyWindow,
        FallbackReason::BelowFloor,
        FallbackReason::UnknownHash,
        FallbackReason::ToNotAhead,
    ] {
        assert!(outcomes.fallbacks.contains_key(&reason), "no {reason:?} in {outcomes:?}");
    }
}

#[test]
fn resuming_after_a_restart_is_right_only_when_unchanged() {
    let config = HistoryConfig::default();
    let mut resumed = HashMap::new();
    for seed in 0..SEEDS {
        let history = generate_history(&config, seed);
        let follower = follower_states(&history);
        for (restart, event) in history.events.iter().enumerate() {
            let HistoryEvent::ExtractorRestarted { last_committed, kind } = event else {
                continue;
            };
            let Some(last_applied) = (0..restart)
                .rev()
                .find(|&index| matches!(history.events[index], HistoryEvent::Message(_)))
            else {
                continue;
            };
            let client = &follower[last_applied];
            let head = client
                .head
                .clone()
                .expect("the client applied a message");
            if head.number == last_committed.number {
                continue;
            }
            // Today's client skips replayed messages up to its block and resumes with the
            // next one when it follows its block. Only replays of new blocks are compared.
            let mut next = None;
            for (index, event) in history
                .events
                .iter()
                .enumerate()
                .skip(restart + 1)
            {
                let HistoryEvent::Message(message) = event else {
                    break;
                };
                if message.revert {
                    break;
                }
                if message.block.number == head.number + 1 {
                    next = Some((index, message));
                    break;
                }
            }
            let Some((index, next)) = next else {
                continue;
            };
            let mut client = client.clone();
            client.apply(next);
            let result = client
                .check_against(&history.canonical_states[index], &history.connected_states[index]);

            match kind {
                RestartKind::Unchanged if next.block.parent_hash == head.hash => {
                    assert_eq!(result, Ok(()), "seed {seed}, restart at {restart}");
                }
                // The chain reorganised during the downtime; the client would not resume.
                RestartKind::Unchanged => continue,
                RestartKind::Upgraded => {
                    assert!(result.is_err(), "seed {seed}, restart at {restart}");
                }
            }
            *resumed.entry(*kind).or_insert(0) += 1;
        }
    }

    assert!(resumed.contains_key(&RestartKind::Unchanged), "{resumed:?}");
    assert!(resumed.contains_key(&RestartKind::Upgraded), "{resumed:?}");
}

#[test]
fn a_message_through_the_wire_loses_only_what_the_wire_lacks() {
    for seed in 0..8 {
        let history = generate_history(&HistoryConfig::default(), seed);
        let sent = history.messages();

        let received: Vec<BlockAggregatedChanges> = sent
            .iter()
            .map(|message| through_the_wire(message))
            .collect();

        for (sent, received) in sent.iter().zip(&received) {
            let mut expected = (*sent).clone();
            expected.db_committed_block_height = None;
            for delta in expected.state_deltas.values_mut() {
                delta.created_attributes.clear();
            }
            assert_eq!(received, &expected, "seed {seed}, block {}", sent.block.number);
        }
        assert_eq!(DeltaStateModel::fold(&received), DeltaStateModel::fold(sent), "seed {seed}");
    }
}

#[test]
fn without_nested_reverts_no_revert_orphans_an_earlier_target() {
    let config = HistoryConfig { nested_reverts: false, ..Default::default() };
    let mut shapes = Shapes::default();
    for seed in 0..32 {
        shapes.add(&generate_history(&config, seed));
    }

    assert!(shapes.reverts > 0, "{shapes:?}");
    assert_eq!(shapes.nested_reverts, 0, "{shapes:?}");
}

#[test]
fn the_window_holds_a_revert_to_the_finalized_block_at_the_deepest_finality() {
    // Finality one block short of the window depth, commits at finality and frequent deep
    // reverts: the window must still hold every revert target, or applying the revert panics.
    let config = HistoryConfig {
        length: 400,
        window_depth: 9,
        finality_depth: 8,
        max_revert_depth: 8,
        max_commit_lag: 0,
        revert_probability: 0.6,
        restart_probability: 0.0,
        ..Default::default()
    };
    let mut reverts_to_the_finalized_block = 0;
    for seed in 0..16 {
        let history = generate_history(&config, seed);
        let mut window = Window::new(config.window_depth);
        for event in &history.events {
            if let HistoryEvent::Message(message) = event {
                let to_finalized = window.finalized == Some(message.block.number);
                reverts_to_the_finalized_block += usize::from(message.revert && to_finalized);
            }
            window.apply(event);
        }
    }

    assert!(reverts_to_the_finalized_block > 0);
}
