use std::collections::HashMap;

use prost::Message;
use rstest::rstest;
use tycho_common::{
    models::{
        blockchain::{
            Block, BlockChanges, BlockTag, EntryPoint, EntryPointWithTracingParams,
            RPCTracerParams, TracingParams,
        },
        token::{Token, TokenOwnerStore},
        Chain,
    },
    traits::{AccountExtractor, EntryPointTracer, StorageSnapshotRequest, TokenPreProcessor},
    Bytes,
};
use tycho_protobuf::{convert::TryFromMessage, pb::tycho::evm::v1 as pb};

use super::{
    rpc::{self, slot},
    streams::{StreamWriter, Tx, BLOCK_TIME},
    *,
};
use crate::{
    pb::sf::substreams::{rpc::v2::BlockUndoSignal, v1::BlockRef},
    testing::{aggregated_changes, with_state_delta},
};

/// The substreams module output `data` carries.
fn module_output(data: &BlockScopedData) -> pb::BlockChanges {
    let output = data
        .output
        .as_ref()
        .and_then(|output| output.map_output.as_ref())
        .expect("recorded blocks carry a map output");
    pb::BlockChanges::decode(output.value.as_slice()).expect("output decodes")
}

fn decode(kind: ExtractorKind, data: &BlockScopedData) -> BlockChanges {
    let message = module_output(data);
    let protocol_type = kind.protocol_type();
    BlockChanges::try_from_message((
        message,
        kind.name(),
        Chain::Ethereum,
        kind.name(),
        &HashMap::from([(protocol_type.name.clone(), protocol_type)]),
        data.final_block_height,
        None,
    ))
    .unwrap_or_else(|err| panic!("block {} does not convert: {err}", block_number(data)))
}

fn numbers(blocks: &[BlockScopedData]) -> Vec<u64> {
    blocks
        .iter()
        .map(block_number)
        .collect()
}

fn hash_of(message: &Recorded) -> String {
    match message {
        Recorded::Block(data) => block_id(data).to_string(),
        Recorded::Undo(_) => panic!("{message} is not a block"),
    }
}

/// Blocks 1 to `tip`, then an undo signal to `undo_to` and blocks up to `tip` again.
fn stream_with_one_undo(tip: u64, undo_to: u64) -> Vec<Recorded> {
    let mut stream = StreamWriter::new();
    for _ in 1..=tip {
        stream.block(vec![Tx::new()]);
    }
    stream.undo_to(undo_to);
    for _ in undo_to + 1..=tip {
        stream.block(vec![Tx::new()]);
    }
    stream.finish().messages
}

/// `blocks` with their final heights replaced by `final_heights`.
fn with_final_heights(blocks: &[BlockScopedData], final_heights: &[u64]) -> Vec<Recorded> {
    assert_eq!(blocks.len(), final_heights.len());
    blocks
        .iter()
        .zip(final_heights)
        .map(|(data, final_height)| {
            let mut data = data.clone();
            data.final_block_height = *final_height;
            Recorded::Block(Box::new(data))
        })
        .collect()
}

#[rstest]
#[case::native(ExtractorKind::Native)]
#[case::dci(ExtractorKind::Dci)]
fn recorded_streams_are_valid_substreams_streams(#[case] kind: ExtractorKind) {
    let mut chain: Vec<Block> = Vec::new();
    let mut final_height = 0;
    for message in recorded_stream(kind).messages {
        match &message {
            Recorded::Block(data) => {
                let block = decode(kind, data).block;
                assert_eq!(block.number, block_number(data), "{message} has a wrong clock");
                assert_eq!(block.hash.to_string(), block_id(data), "{message} has a wrong clock");
                if let Some(parent) = chain.last() {
                    assert_eq!(block.parent_hash, parent.hash, "{message} skips its parent");
                    assert_eq!(block.number, parent.number + 1, "{message} skips a height");
                }
                assert!(
                    (final_height..=block.number).contains(&data.final_block_height),
                    "{message} moves finality back or ahead of itself"
                );
                final_height = data.final_block_height;
                chain.push(block);
            }
            Recorded::Undo(signal) => {
                let target = signal
                    .last_valid_block
                    .as_ref()
                    .expect("undo signals name their target");
                assert!(target.number >= final_height, "{message} reverts a final block");
                chain.retain(|block| block.number <= target.number);
                assert_eq!(
                    chain
                        .last()
                        .map(|block| block.hash.to_string()),
                    Some(target.id.clone()),
                    "{message} names a block off the chain"
                );
            }
        }
    }
}

#[rstest]
#[case::native(ExtractorKind::Native)]
#[case::dci(ExtractorKind::Dci)]
fn block_timestamps_count_from_the_base_timestamp(#[case] kind: ExtractorKind) {
    let stream = recorded_stream(kind);
    for message in &stream.messages {
        let Recorded::Block(data) = message else {
            continue;
        };
        let block = decode(kind, data).block;
        let seconds = i64::try_from(block.number * BLOCK_TIME).expect("block times fit an i64");
        assert_eq!(block.ts, stream.base_ts + chrono::Duration::seconds(seconds), "{message}");
    }
}

#[rstest]
#[case::native(ExtractorKind::Native)]
#[case::dci(ExtractorKind::Dci)]
fn recorded_streams_commit_and_revert_on_both_sides_of_the_halfway_point(
    #[case] kind: ExtractorKind,
) {
    let stream = recorded_stream(kind).messages;
    let (before, after) = stream.split_at(stream.len() / 2);

    for half in [before, after] {
        let first_undo = half
            .iter()
            .position(|message| matches!(message, Recorded::Undo(_)))
            .expect("each half reverts");
        assert!(
            half[first_undo..]
                .iter()
                .any(|message| matches!(message, Recorded::Block(_))),
            "each half continues after a revert"
        );
    }

    let canonical = canonical_blocks(before);
    let final_height = canonical
        .last()
        .map(|data| data.final_block_height)
        .expect("the first half has blocks");
    let below_final = canonical
        .iter()
        .filter(|data| block_number(data) < final_height)
        .count();
    assert!(
        below_final >= 2 * COMMIT_BATCH_SIZE,
        "the first half must finalize enough blocks for several commits"
    );
    assert!(
        numbers(&canonical).last() > Some(&final_height),
        "the first half must end with blocks that are not final, so they stay uncommitted"
    );
}

/// How the partial-block stream sent one block.
struct SentBlock {
    /// The block's messages in order: one whole message, or its partials.
    messages: Vec<BlockScopedData>,
    /// Whether the block's last message arrived.
    sealed: bool,
    /// The undo signals naming the block itself, each with the number of partials before it.
    undos_to_itself: Vec<(usize, BlockUndoSignal)>,
}

/// Takes the messages of block `number` from the front of `stream`.
fn take_block(number: u64, stream: &mut &[Recorded]) -> SentBlock {
    let mut sent = SentBlock { messages: Vec::new(), sealed: false, undos_to_itself: Vec::new() };
    loop {
        match stream.split_first() {
            Some((Recorded::Block(data), rest)) if block_number(data) == number => {
                sent.messages
                    .push(data.as_ref().clone());
                *stream = rest;
                if !data.is_partial || data.is_last_partial == Some(true) {
                    sent.sealed = true;
                    return sent;
                }
            }
            Some((Recorded::Undo(undo), rest))
                if undo
                    .last_valid_block
                    .as_ref()
                    .is_some_and(|target| target.number == number) =>
            {
                sent.undos_to_itself
                    .push((sent.messages.len(), undo.clone()));
                *stream = rest;
            }
            _ => return sent,
        }
    }
}

#[test]
fn the_partial_block_stream_sends_the_native_blocks() {
    let full_blocks = recorded_stream(ExtractorKind::Native).messages;
    let partial_blocks = recorded_partial_block_stream().messages;
    let mut partials = partial_blocks.as_slice();
    let (mut whole, mut interrupted, mut undone_to_itself) = (0, 0, 0);

    for (position, message) in full_blocks.iter().enumerate() {
        let block = match message {
            Recorded::Block(block) => block,
            Recorded::Undo(undo) => {
                // The cursors differ: a partial's cursor names its index.
                let Some((Recorded::Undo(next), rest)) = partials.split_first() else {
                    panic!("the partial stream does not send {message}");
                };
                assert_eq!(next.last_valid_block, undo.last_valid_block);
                partials = rest;
                continue;
            }
        };
        let sent = take_block(block_number(block), &mut partials);
        if sent
            .messages
            .first()
            .is_some_and(|data| !data.is_partial)
        {
            assert_eq!(sent.messages, vec![block.as_ref().clone()], "{message} is sent whole");
            whole += 1;
            continue;
        }

        let expected = module_output(block);
        let expected_block = expected
            .block
            .expect("blocks carry their block");
        let mut changes = Vec::new();
        for (index, partial) in sent.messages.iter().enumerate() {
            let output = module_output(partial);
            let partial_block = output
                .block
                .expect("partials carry their block");
            let last = index + 1 == sent.messages.len();
            assert!(partial.is_partial, "{message} is sent as partials");
            assert_eq!(partial.partial_index, Some(index as u32), "{message}");
            assert_eq!(partial.is_last_partial, Some(last && sent.sealed), "{message}");
            assert_eq!(partial.final_block_height, block.final_block_height, "{message}");
            assert_eq!(
                partial_block.hash == expected_block.hash,
                last && sent.sealed,
                "only the last partial of {message} carries its hash"
            );
            assert_eq!(
                pb::Block { hash: expected_block.hash.clone(), ..partial_block },
                expected_block,
                "{message}"
            );
            assert_eq!(output.changes.len(), 1, "each partial of {message} sends one transaction");
            changes.extend(output.changes);
        }
        assert_eq!(changes, expected.changes, "the partials of {message} send its transactions");
        for (partials_before, undo) in &sent.undos_to_itself {
            assert_eq!(
                *partials_before, 1,
                "the undo signal naming {message} follows its first partial"
            );
            assert_eq!(
                undo.last_valid_block,
                Some(BlockRef { id: block_id(block).to_string(), number: block_number(block) }),
            );
            undone_to_itself += 1;
        }
        if !sent.sealed {
            interrupted += 1;
            assert!(
                matches!(full_blocks.get(position + 1), Some(Recorded::Undo(_))),
                "only an undo signal interrupts {message}"
            );
        }
    }

    assert!(partials.is_empty(), "the partial stream sends nothing else");
    assert_eq!(whole, 1, "the first block is sent whole");
    assert_eq!(interrupted, 2, "an undo signal interrupts each orphaned branch");
    assert_eq!(undone_to_itself, 1, "one undo signal names the block being streamed");
}

#[test]
fn finality_trails_the_newest_block() {
    let stream = stream_with_one_undo(6, 4);

    let finals: Vec<u64> = canonical_blocks(&stream)
        .iter()
        .map(|data| data.final_block_height)
        .collect();

    // Block 1 starts final; block 6 of the orphaned branch already moved finality to 4.
    assert_eq!(finals, vec![1, 1, 1, 2, 4, 4]);
}

#[test]
fn canonical_blocks_drop_the_blocks_an_undo_signal_orphans() {
    let stream = stream_with_one_undo(4, 2);
    let orphaned_three = hash_of(&stream[2]);

    let canonical = canonical_blocks(&stream);

    assert_eq!(numbers(&canonical), vec![1, 2, 3, 4]);
    assert_ne!(block_id(&canonical[2]), orphaned_three);
    assert_eq!(Recorded::Block(Box::new(canonical[3].clone())), stream[6]);
}

#[test]
fn canonical_blocks_follow_nested_undo_signals() {
    let mut stream = StreamWriter::new();
    for _ in 1..=4 {
        stream.block(vec![Tx::new()]);
    }
    stream.undo_to(3);
    stream.block(vec![Tx::new()]);
    stream.undo_to(2);
    stream.block(vec![Tx::new()]);
    let stream = stream.finish().messages;

    let canonical = canonical_blocks(&stream);

    assert_eq!(numbers(&canonical), vec![1, 2, 3]);
    assert_eq!(Recorded::Block(Box::new(canonical[2].clone())), stream[7]);
}

#[test]
#[should_panic(expected = "names a block that is not on the chain")]
fn canonical_blocks_reject_an_undo_signal_to_an_orphaned_block() {
    let mut stream = stream_with_one_undo(3, 2);
    stream.push(Recorded::Undo(BlockUndoSignal {
        last_valid_block: Some(BlockRef { id: hash_of(&stream[2]), number: 3 }),
        last_valid_cursor: "cursor@3".to_string(),
    }));

    canonical_blocks(&stream);
}

#[test]
fn replay_after_a_short_downtime_resends_the_canonical_blocks_with_the_last_final_height() {
    // Final heights 1, 1, 1, 2, 3, then 3 on the branch after the undo signal.
    let stream = stream_with_one_undo(5, 3);
    let canonical = canonical_blocks(&stream);

    let replay = replay_after_restart(&stream, 2, Downtime::Short);

    assert_eq!(replay, with_final_heights(&canonical[2..], &[3, 3, 3]));
}

#[test]
fn replay_after_a_long_downtime_resends_the_canonical_blocks_as_final() {
    let stream = stream_with_one_undo(5, 3);
    let canonical = canonical_blocks(&stream);

    let replay = replay_after_restart(&stream, 2, Downtime::Long);

    assert_eq!(replay, with_final_heights(&canonical[2..], &[3, 4, 5]));
}

#[rstest]
#[case::short(Downtime::Short)]
#[case::long(Downtime::Long)]
fn replay_after_restart_is_empty_when_everything_was_committed(#[case] downtime: Downtime) {
    let stream = stream_with_one_undo(5, 3);

    assert_eq!(replay_after_restart(&stream, 5, downtime), Vec::new());
}

#[test]
fn deltas_only_ignores_watermarks_but_not_deltas() {
    let original = aggregated_changes("extractor", 5, 3, Some(2));
    let replayed = aggregated_changes("extractor", 5, 5, None);
    let different = with_state_delta(replayed.clone(), "pool", 1);

    assert_eq!(deltas_only(&replayed), deltas_only(&original));
    assert_ne!(deltas_only(&different), deltas_only(&original));
}

#[test]
fn token_details_tell_apart_tokens_that_compare_equal() {
    let address = Bytes::from("0x01");
    let token = Token::new(&address, "TKN", 18, 0, &[], Chain::Ethereum, 100);
    let with_tax = Token { tax: 10, ..token.clone() };
    let mut message = aggregated_changes("extractor", 1, 1, None);
    message.new_tokens = HashMap::from([(address.clone(), token)]);
    let mut changed = message.clone();
    changed.new_tokens = HashMap::from([(address, with_tax)]);

    assert_eq!(changed, message);
    assert_ne!(token_details(&changed), token_details(&message));
}

#[tokio::test]
async fn token_metadata_answers_the_same_for_every_request() {
    let addresses: Vec<Bytes> = streams::TOKENS
        .iter()
        .map(|token| token.address())
        .collect();
    let read = || async {
        rpc::TokenMetadata
            .get_tokens(
                addresses.clone(),
                Arc::new(TokenOwnerStore::new(HashMap::new())),
                BlockTag::Latest,
            )
            .await
            .iter()
            .map(|token| format!("{token:?}"))
            .collect::<Vec<_>>()
    };

    let first = read().await;

    assert_eq!(first.len(), addresses.len());
    assert_eq!(read().await, first);
}

#[tokio::test]
async fn the_tracer_traces_every_entry_point_in_request_order_the_same_way_every_time() {
    let entry_points: Vec<EntryPointWithTracingParams> = [0x0a_u8, 0x0b]
        .into_iter()
        .map(|target| {
            EntryPointWithTracingParams::new(
                EntryPoint::new(format!("ep_{target}"), Bytes::from([target; 20]), "f()".into()),
                TracingParams::RPCTracer(RPCTracerParams::new(None, Bytes::from("0x01"))),
            )
        })
        .collect();
    let tracer = rpc::tracer();
    let block_hash = Bytes::from(7_u64).lpad(32, 0);

    let traces = tracer
        .trace(block_hash.clone(), entry_points.clone())
        .await;

    let traced: Vec<_> = traces
        .iter()
        .map(|trace| {
            trace
                .as_ref()
                .expect("the tracer succeeds")
                .entry_point_with_params
                .clone()
        })
        .collect();
    assert_eq!(traced, entry_points);
    assert_eq!(
        tracer
            .trace(block_hash, entry_points)
            .await,
        traces
    );
}

#[tokio::test]
async fn the_tracer_fails_the_untraceable_entry_point_every_time() {
    let untraceable = EntryPointWithTracingParams::new(
        EntryPoint::new("ep".into(), streams::UNTRACEABLE.target(), "f()".into()),
        TracingParams::RPCTracer(RPCTracerParams::new(None, Bytes::from("0x01"))),
    );
    let tracer = rpc::tracer();

    for block in [1_u64, 2] {
        let traces = tracer
            .trace(Bytes::from(block).lpad(32, 0), vec![untraceable.clone()])
            .await;

        assert!(matches!(traces.as_slice(), [Err(_)]), "{traces:?}");
    }
}

#[tokio::test]
async fn the_account_extractor_returns_every_requested_account_the_same_way_every_time() {
    let block = Block { number: 7, ..Default::default() };
    let requests = vec![
        StorageSnapshotRequest { address: Bytes::from([0x01; 20]), slots: None },
        StorageSnapshotRequest { address: Bytes::from([0x02; 20]), slots: Some(vec![slot(5)]) },
    ];
    let extractor = rpc::account_extractor();

    let accounts = extractor
        .get_accounts_at_block(&block, &requests)
        .await
        .expect("the account extractor succeeds");

    assert_eq!(accounts.len(), requests.len());
    assert_eq!(
        accounts[&requests[1].address]
            .slots
            .keys()
            .collect::<Vec<_>>(),
        vec![&slot(5)]
    );
    assert_eq!(
        extractor
            .get_accounts_at_block(&block, &requests)
            .await,
        Ok(accounts)
    );
}
