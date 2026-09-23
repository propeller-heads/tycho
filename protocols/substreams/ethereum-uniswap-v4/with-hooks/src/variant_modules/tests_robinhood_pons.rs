//! The Pons V2 pool created in Robinhood block 58759099, run through pool extraction and Pons
//! enrichment.
//!
//! The block is hand-built from raw JSON-RPC responses captured under `fixtures/`: the PoolManager
//! `Initialize` log of that block, the receipt of the transaction that emitted it, and the two
//! `launches[poolId]` words the Pons hook holds for the pool. `registerPool` writes those words
//! once and rejects re-registration, so their latest values are the values that transaction wrote;
//! the public endpoint serves no historical state, so they cannot be read at a past block.
//!
//! This reproduces the on-chain values, it does NOT prove that Pinax extended blocks populate
//! `Call.storage_changes` on Robinhood. That proof needs the Substreams CLI and an API token,
//! neither of which is available here, and without it the module would emit `hook_identifier`
//! alone on the real stream.

use ethereum_uniswap_v4_shared::{
    pb::uniswap::v4::{Events, LiquidityChanges, TickDeltas},
    utils::protocol_changes::collect_transaction_changes,
};
use prost_types::Timestamp;
use substreams::pb::substreams::StoreDeltas;
use substreams_ethereum::pb::eth::v2::{
    block::DetailLevel, Block, BlockHeader, Call, Log, StorageChange, TransactionReceipt,
    TransactionTrace, TransactionTraceStatus,
};
use tycho_substreams::prelude::*;

use crate::{
    pons::PONS_LAUNCHES_SLOT,
    storage::{mapping_slot, pad32, word_at_offset, Word},
    variant_modules::{
        map_pons_enriched_block_changes::enrich_pons_creations, map_pool_created::get_new_pools,
    },
};

const INITIALIZE_LOGS: &str = include_str!("fixtures/robinhood_58759099_initialize_logs.json");
const RECEIPT: &str = include_str!("fixtures/robinhood_58759099_receipt.json");
const LAUNCHES_WORDS: &str = include_str!("fixtures/robinhood_58759099_pons_launches_words.json");
const BLOCK_HEADER: &str = include_str!("fixtures/robinhood_58759099_block_header.json");

const BLOCK_NUMBER: u64 = 58_759_099;
const BLOCK_HASH: &str = "d20be675928ed50f51d4ac3e16cc5195c7cce31403090f74669e67e2f4628cc0";
const PARENT_HASH: &str = "b81bb499925ce296febc0870153933e877d33180a5cc04cf922f14202359f4ca";
/// `0x6aa1a49b`, the block's timestamp.
const BLOCK_TIMESTAMP: i64 = 1_788_978_331;

const POOL_MANAGER: &str = "8366a39CC670B4001A1121B8F6A443A643e40951";
const PONS_HOOK: [u8; 20] = hex_literal::hex!("e5e702641ea86f4ae6cc3cdaed2b886f976be044");
const CREATION_TX: &str = "f66ca58190cc186683267cb486bbc6e1f0469176b7ad4f92e43eb68a1a0936e5";
const POOL_ID: &str = "c96847cc43f7595aafcbc1c99d335cb91be7ce1107524c87030f48a716a5f289";
const CURRENCY_0: &str = "ab5983fe30f186055095305c862b0e097dab3b52";
const CURRENCY_1: &str = "d0601ce157db5bdc3162bbac2a2c8af5320d9eec";

const INITIALIZE_TOPIC: &str = "dd466e674ea557f56295e2d0218a125ea4b4f0f6f3307b95f85e6110838d6438";
/// `fee` 0, `tickSpacing` 200, `hooks`, `sqrtPriceX96` and `tick`, in that order.
const INITIALIZE_DATA: &str = concat!(
    "0000000000000000000000000000000000000000000000000000000000000000",
    "00000000000000000000000000000000000000000000000000000000000000c8",
    "000000000000000000000000e5e702641ea86f4ae6cc3cdaed2b886f976be044",
    "0000000000000000000000000000000000000000001d96af77dc8202588b5cca",
    "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffda62c",
);

/// `launches[POOL_ID]` word 0: `registered` 1 at low byte 0, `memecoin` at low bytes 2..22.
const LAUNCH_WORD_0: &str = "00000000000000000000ab5983fe30f186055095305c862b0e097dab3b520101";
/// `launches[POOL_ID]` word 4: `creatorTaxBps` 100 at low byte 20, `hookFeeBps` 100 at low byte 26.
const LAUNCH_WORD_4: &str = "0000012c006413880bb80064263ed295dafae1d9aadd6e56c4b6f9f38ee019dd";

fn bytes(hex_str: &str) -> Vec<u8> {
    hex::decode(hex_str).expect("fixture value is hex")
}

fn word(hex_str: &str) -> Word {
    pad32(&bytes(hex_str)).expect("fixture value is a word")
}

fn padded_topic(address: &str) -> Vec<u8> {
    word(address).to_vec()
}

/// The `Initialize` log the PoolManager emitted, as `eth_getLogs` returned it.
fn initialize_log() -> Log {
    Log {
        address: bytes(POOL_MANAGER),
        topics: vec![
            bytes(INITIALIZE_TOPIC),
            bytes(POOL_ID),
            padded_topic(CURRENCY_0),
            padded_topic(CURRENCY_1),
        ],
        data: bytes(INITIALIZE_DATA),
        index: 6,
        block_index: 6,
        ordinal: 6,
    }
}

/// The writes `registerPool` made to the Pons hook, over slots that held zero.
fn registration_call() -> Call {
    let base = mapping_slot(&word(POOL_ID), PONS_LAUNCHES_SLOT);
    let storage_changes = [(0u64, LAUNCH_WORD_0), (4, LAUNCH_WORD_4)]
        .into_iter()
        .enumerate()
        .map(|(ordinal, (offset, value))| StorageChange {
            address: PONS_HOOK.to_vec(),
            key: word_at_offset(&base, offset).to_vec(),
            old_value: vec![0u8; 32],
            new_value: bytes(value),
            ordinal: ordinal as u64,
        })
        .collect();

    Call { index: 1, storage_changes, ..Default::default() }
}

fn creation_block() -> Block {
    let transaction = TransactionTrace {
        hash: bytes(CREATION_TX),
        from: bytes("49bbf2b70955fb3a106e084d4bfda92d334573d2"),
        to: bytes("7ed598bcef8bd9edd8c97a195c6d13f40801ec7e"),
        index: 2,
        status: i32::from(TransactionTraceStatus::Succeeded),
        receipt: Some(TransactionReceipt { logs: vec![initialize_log()], ..Default::default() }),
        calls: vec![registration_call()],
        ..Default::default()
    };

    Block {
        number: BLOCK_NUMBER,
        hash: bytes(BLOCK_HASH),
        header: Some(BlockHeader {
            parent_hash: bytes(PARENT_HASH),
            timestamp: Some(Timestamp { seconds: BLOCK_TIMESTAMP, nanos: 0 }),
            ..Default::default()
        }),
        // Storage changes only reach a module on extended blocks; `DetaillevelExtended` is 0, the
        // default, so this says out loud what the fixture assumes.
        detail_level: i32::from(DetailLevel::DetaillevelExtended),
        transaction_traces: vec![transaction],
        ..Default::default()
    }
}

/// The block changes the module builds: pool extraction and the shared aggregation with no events,
/// balances, ticks or liquidity, then Pons enrichment.
fn block_changes(block: &Block) -> BlockChanges {
    let mut new_pools = Vec::new();
    get_new_pools(block, &mut new_pools, POOL_MANAGER);

    let mut changes = collect_transaction_changes(
        BlockEntityChanges { block: None, changes: new_pools },
        Events::default(),
        BlockBalanceDeltas::default(),
        StoreDeltas::default(),
        TickDeltas::default(),
        StoreDeltas::default(),
        LiquidityChanges::default(),
        StoreDeltas::default(),
    );
    enrich_pons_creations(&PONS_HOOK, block, &mut changes);

    BlockChanges { block: Some(block.into()), changes, storage_changes: vec![] }
}

fn attribute<'a>(component: &'a ProtocolComponent, name: &str) -> &'a [u8] {
    component
        .static_att
        .iter()
        .find(|attribute| attribute.name == name)
        .unwrap_or_else(|| panic!("{name} is missing"))
        .value
        .as_slice()
}

#[test]
fn the_fixture_values_are_the_ones_the_endpoint_returned() {
    assert!(INITIALIZE_LOGS.contains(INITIALIZE_TOPIC), "Initialize topic");
    assert!(INITIALIZE_LOGS.contains(POOL_ID), "pool id");
    assert!(INITIALIZE_LOGS.contains(INITIALIZE_DATA), "Initialize data");
    assert!(INITIALIZE_LOGS.contains(CREATION_TX), "creation transaction");

    assert!(RECEIPT.contains(CREATION_TX), "creation transaction");
    assert!(RECEIPT.contains("\"status\":\"0x1\""), "the creation transaction succeeded");
    assert!(RECEIPT.contains(&hex::encode(PONS_HOOK)), "the Pons hook logged in that transaction");

    assert!(LAUNCHES_WORDS.contains(LAUNCH_WORD_0), "launches word 0");
    assert!(LAUNCHES_WORDS.contains(LAUNCH_WORD_4), "launches word 4");

    assert!(BLOCK_HEADER.contains(BLOCK_HASH), "block hash");
    assert!(BLOCK_HEADER.contains(PARENT_HASH), "parent hash");
    assert!(BLOCK_HEADER.contains(&format!("{BLOCK_TIMESTAMP:x}")), "block timestamp");
}

#[test]
fn the_creation_block_yields_a_pons_component_carrying_its_fee_terms() {
    let block = creation_block();

    let changes = block_changes(&block);

    let component = changes
        .changes
        .iter()
        .flat_map(|tx_changes| tx_changes.component_changes.iter())
        .find(|component| component.id == format!("0x{POOL_ID}"))
        .expect("the pool is created in this block");

    assert_eq!(
        component
            .static_att
            .iter()
            .map(|attribute| attribute.name.as_str())
            .collect::<Vec<_>>(),
        [
            "tick_spacing",
            "pool_id",
            "hooks",
            "key_lp_fee",
            "hook_identifier",
            "pons_hook_fee_bps",
            "pons_creator_tax_bps"
        ]
    );
    assert_eq!(attribute(component, "tick_spacing"), [0x00, 0xc8]);
    assert_eq!(attribute(component, "pool_id"), bytes(POOL_ID));
    assert_eq!(attribute(component, "hooks"), PONS_HOOK);
    assert_eq!(attribute(component, "key_lp_fee"), [0x00]);
    assert_eq!(attribute(component, "hook_identifier"), b"pons_v2");
    assert_eq!(attribute(component, "pons_hook_fee_bps"), [0x64]);
    assert_eq!(attribute(component, "pons_creator_tax_bps"), [0x64]);
    assert_eq!(component.tokens, [bytes(CURRENCY_0), bytes(CURRENCY_1)]);
}

#[test]
fn the_module_emits_no_block_storage_payload() {
    let block = creation_block();

    assert!(block_changes(&block)
        .storage_changes
        .is_empty());
}
