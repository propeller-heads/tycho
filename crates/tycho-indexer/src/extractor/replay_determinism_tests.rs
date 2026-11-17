//! Replay determinism: an extractor restarted mid-stream must emit, for every block it replays,
//! the message it emitted for that block before the restart.
//!
//! The extractor resumes after its last committed block without a cursor. Clients resume across
//! the restart without a snapshot, whether through the first-message skip or through catch-up
//! from the delta window. Either way a client ends up with messages from both runs, so any
//! difference leaves it with state no client that stayed connected has.
//!
//! The replayed blocks carry the chain's finality at the restart, which depends on how long the
//! extractor was down, so both a short and a long downtime are checked.

use rstest::rstest;
use tycho_storage::postgres::testing::run_against_db;

use crate::extractor::recorded_extractors::{
    block_number, build_extractor, canonical_blocks, deltas_only, feed, recorded_stream,
    replay_after_restart, seed_database, token_details, Downtime, ExtractorKind,
};

#[rstest]
#[case::native_after_a_short_downtime(ExtractorKind::Native, Downtime::Short)]
#[case::native_after_a_long_downtime(ExtractorKind::Native, Downtime::Long)]
#[case::dci_after_a_short_downtime(ExtractorKind::Dci, Downtime::Short)]
#[case::dci_after_a_long_downtime(ExtractorKind::Dci, Downtime::Long)]
#[tokio::test]
async fn test_serial_db_replayed_blocks_emit_the_original_messages(
    #[case] kind: ExtractorKind,
    #[case] downtime: Downtime,
) {
    run_against_db(move |pool| async move {
        let database = seed_database(&pool).await;
        let stream = recorded_stream(kind).messages;
        let (before_restart, _) = stream.split_at(stream.len() / 2);

        let first_run = build_extractor(&database, kind).await;
        let originals = feed(&first_run, before_restart).await;
        drop(first_run);
        let tip = canonical_blocks(before_restart)
            .last()
            .map(block_number)
            .expect("the first run fed blocks");

        // A fresh extractor resumes from what the first run committed, as after a real restart;
        // the first run's own last processed block is its in-memory tip.
        let after_restart = build_extractor(&database, kind).await;
        let last_committed = after_restart
            .get_last_processed_block()
            .await
            .expect("the first run committed a block")
            .number;
        assert!(last_committed < tip, "the first run must leave blocks uncommitted");
        let replay = replay_after_restart(before_restart, last_committed, downtime);
        let replayed = feed(&after_restart, &replay).await;

        assert!(!replay.is_empty(), "the restarted extractor must replay blocks");
        assert_eq!(replayed.len(), replay.len(), "every replayed block must emit a message");
        for again in &replayed {
            let original = originals
                .iter()
                .rev()
                .find(|message| !message.revert && message.block.hash == again.block.hash)
                .unwrap_or_else(|| {
                    panic!(
                        "block {} was replayed but not emitted before the restart",
                        again.block.number
                    )
                });
            assert_eq!(
                deltas_only(again),
                deltas_only(original),
                "block {} differs after the restart",
                again.block.number
            );
            assert_eq!(
                token_details(again),
                token_details(original),
                "token metadata of block {} differs after the restart",
                again.block.number
            );
        }
    })
    .await;
}
