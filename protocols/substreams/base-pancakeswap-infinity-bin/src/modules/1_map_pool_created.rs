use crate::{
    abi::bin_pool_manager::events::Initialize,
    parameters::{self, BIN_POOLS_MAPPING_SLOT},
};
use ethabi::ethereum_types::Address;
use std::str::FromStr;
use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::{self as eth};
use substreams_helper::{event_handler::EventHandler, hex::Hexable};
use tycho_substreams::prelude::*;

/// Holds every pool's funds, so it is `balance_owner` on every component. Same address on Base
/// and BNB (CREATE3).
pub const VAULT_ADDRESS: &str = "238a358808379702088667322f80aC48bAd5e6c4";

/// One `ProtocolComponent` per `BinPoolManager.Initialize`, for pools with no swap hook and a
/// static LP fee.
///
/// The attribute names are this package's own, and `PancakeswapInfinityBinState` is written
/// against them, so they are load-bearing.
///
/// Two things the CL package gets differently, both of which compile either way: Bin's slot0 packs
/// `[0,24) activeId | [24,48) protocolFee | [48,72) lpFee` (`BinSlot0.sol:9`), and its pools
/// mapping is at slot 5, not 4.
#[substreams::handlers::map]
pub fn map_pools_created(
    params: String,
    block: eth::Block,
) -> Result<BlockEntityChanges, substreams::errors::Error> {
    let mut new_pools: Vec<TransactionEntityChanges> = vec![];
    let pool_manager = params.as_str();

    get_new_pools(&block, &mut new_pools, pool_manager);

    Ok(BlockEntityChanges { block: None, changes: new_pools })
}

/// Collects the pools worth indexing from this block's `Initialize` logs.
fn get_new_pools(
    block: &eth::Block,
    new_pools: &mut Vec<TransactionEntityChanges>,
    pool_manager_address: &str,
) {
    let pool_manager = hex::decode(pool_manager_address).expect("pool manager is hex");
    let mut on_pool_created = |event: Initialize, tx: &eth::TransactionTrace, log: &eth::Log| {
        if let Some(changes) = pool_created(event, tx, log, &pool_manager) {
            new_pools.push(changes);
        }
    };

    let mut eh = EventHandler::new(block);
    eh.filter_by_address(vec![Address::from_str(pool_manager_address).expect("pool manager")]);
    eh.on::<Initialize, _>(&mut on_pool_created);
    eh.handle_events();
}

/// One component per pool, or `None` for a pool with a swap hook or a dynamic fee: both make the
/// quote depend on code this package does not index.
fn pool_created(
    event: Initialize,
    tx: &eth::TransactionTrace,
    log: &eth::Log,
    pool_manager: &[u8],
) -> Option<TransactionEntityChanges> {
    let fee: u32 = event.fee.clone().into();
    if parameters::has_swap_hooks(&event.parameters) || parameters::is_dynamic_fee(fee) {
        return None;
    }

    let component_id = event.id.to_vec().to_hex();
    let (fee_zero2one, fee_one2zero) = initial_protocol_fees(&event, tx, log, pool_manager);

    let mut static_att = vec![
        attribute(
            "bin_step",
            parameters::bin_step(&event.parameters)
                .to_be_bytes()
                .to_vec(),
        ),
        // Raw bytes32, as the v4 packages do. The reserved-attributes doc says UTF-8 string, but
        // no consumer reads it.
        attribute("pool_id", event.id.to_vec()),
        // Static LP fee in hundredths of a bip, kept raw because rebuilding the PoolKey needs it.
        attribute("key_lp_fee", event.fee.to_signed_bytes_be()),
        attribute("parameters", event.parameters.to_vec()),
        attribute("pool_manager", pool_manager.to_vec()),
    ];
    if event
        .hooks
        .iter()
        .any(|byte| *byte != 0)
    {
        static_att.push(attribute("hook_address", event.hooks.to_vec()));
    }

    Some(TransactionEntityChanges {
        tx: Some(tx.into()),
        entity_changes: vec![EntityChanges {
            component_id: component_id.clone(),
            attributes: vec![
                attribute("balance_owner", hex::decode(VAULT_ADDRESS).expect("vault is hex")),
                // Mutable, so state rather than static: every Swap moves it.
                attribute("active_id", event.active_id.to_signed_bytes_be()),
                // Mirrors key_lp_fee and never changes for a static-fee pool, but the decoder
                // prefers `fee`, so it has to exist.
                attribute("fee", event.fee.to_signed_bytes_be()),
                attribute(
                    "protocol_fees/zero2one",
                    BigInt::from(fee_zero2one).to_signed_bytes_be(),
                ),
                attribute(
                    "protocol_fees/one2zero",
                    BigInt::from(fee_one2zero).to_signed_bytes_be(),
                ),
            ],
        }],
        component_changes: vec![ProtocolComponent {
            id: component_id.clone(),
            tokens: vec![event.currency0.clone(), event.currency1.clone()],
            contracts: vec![],
            static_att,
            change: i32::from(ChangeType::Creation),
            protocol_type: Some(ProtocolType {
                name: "pancakeswap_infinity_bin_pool".to_string(),
                financial_type: FinancialType::Swap.into(),
                attribute_schema: vec![],
                implementation_type: ImplementationType::Custom.into(),
            }),
        }],
        balance_changes: [event.currency0, event.currency1]
            .into_iter()
            .map(|token| BalanceChange {
                token,
                balance: BigInt::from(0).to_signed_bytes_be(),
                component_id: component_id.as_bytes().to_vec(),
            })
            .collect(),
    })
}

/// Protocol fee halves from the slot0 write `initialize` makes just before emitting the log, since
/// `Initialize` does not carry them. The last non-reverted write below the log's ordinal is the
/// pool's initial state.
fn initial_protocol_fees(
    event: &Initialize,
    tx: &eth::TransactionTrace,
    log: &eth::Log,
    pool_manager: &[u8],
) -> (u32, u32) {
    let slot0_key = parameters::pool_state_base_slot(&event.id, BIN_POOLS_MAPPING_SLOT);
    let slot0_write = tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted)
        .flat_map(|call| call.storage_changes.iter())
        .filter(|change| {
            change.address == pool_manager &&
                change.key == slot0_key &&
                change.ordinal < log.ordinal
        })
        .max_by_key(|change| change.ordinal)
        .unwrap_or_else(|| {
            panic!(
                "no slot0 storage write for pool {} in tx {}",
                event.id.to_vec().to_hex(),
                tx.hash.to_hex()
            )
        });
    let slot0: [u8; 32] = slot0_write
        .new_value
        .clone()
        .try_into()
        .expect("slot0 is 32 bytes");

    parameters::split_protocol_fee(parameters::protocol_fee_from_slot0(&slot0))
}

/// Every attribute this module emits is a creation.
fn attribute(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_string(), value, change: ChangeType::Creation.into() }
}

#[cfg(test)]
mod tests {
    use substreams_ethereum::pb::eth::v2::{
        Block, Call, Log, StorageChange, TransactionReceipt, TransactionTrace,
    };
    use tiny_keccak::{Hasher, Keccak};

    use super::*;
    use crate::parameters::BIN_POOLS_MAPPING_SLOT;

    const POOL_MANAGER: &str = "C697d2898e0D09264376196696c51D7aBbbAA4a9";
    /// 2^23, the reference bin. Real pools start here, and its signed encoding widens to 4 bytes.
    const ACTIVE_ID: u32 = 1 << 23;
    const CURRENCY0: [u8; 20] = [0x11; 20];
    const CURRENCY1: [u8; 20] = [0x22; 20];
    const POOL_ID: [u8; 32] = [0xab; 32];
    const LOG_ORDINAL: u64 = 5;

    /// `PoolKey.parameters`: hook bitmap in bits [0, 16), bin step in [16, 32).
    fn parameters(hook_bitmap: u16, bin_step: u16) -> [u8; 32] {
        let value: u128 = ((bin_step as u128) << 16) | hook_bitmap as u128;
        let mut out = [0u8; 32];
        out[16..].copy_from_slice(&value.to_be_bytes());
        out
    }

    /// Bin slot0: `[0,24) activeId | [24,48) protocolFee | [48,72) lpFee` (`BinSlot0.sol:9`).
    /// CL packs it differently, so this cannot be copied.
    fn slot0(active_id: u32, zero_for_one: u32, one_for_zero: u32, lp_fee: u32) -> [u8; 32] {
        let protocol_fee = (one_for_zero << 12) | zero_for_one;
        let mut out = [0u8; 32];
        out[29..32].copy_from_slice(&active_id.to_be_bytes()[1..]);
        out[26..29].copy_from_slice(&protocol_fee.to_be_bytes()[1..]);
        out[23..26].copy_from_slice(&lp_fee.to_be_bytes()[1..]);
        out
    }

    fn keccak(input: &[u8]) -> [u8; 32] {
        let mut hasher = Keccak::v256();
        hasher.update(input);
        let mut out = [0u8; 32];
        hasher.finalize(&mut out);
        out
    }

    fn word_from_address(address: &[u8; 20]) -> Vec<u8> {
        let mut word = vec![0u8; 12];
        word.extend_from_slice(address);
        word
    }

    fn word_from_u128(value: u128) -> Vec<u8> {
        let mut word = vec![0u8; 16];
        word.extend_from_slice(&value.to_be_bytes());
        word
    }

    /// `Initialize` as emitted: 3 indexed topics after the signature, then four words in `data`
    /// (hooks, fee, parameters, activeId). `match_log` wants exactly 4 topics and 128 bytes; CL
    /// has two more words and wants 160.
    fn initialize_log(hooks: &[u8; 20], fee: u32, params: &[u8; 32], active_id: u32) -> Log {
        let mut data = Vec::with_capacity(128);
        data.extend(word_from_address(hooks));
        data.extend(word_from_u128(fee as u128));
        data.extend_from_slice(params);
        data.extend(word_from_u128(active_id as u128));
        debug_assert_eq!(data.len(), 128, "Initialize data must be 4 words");
        Log {
            address: hex::decode(POOL_MANAGER).unwrap(),
            topics: vec![
                keccak(b"Initialize(bytes32,address,address,address,uint24,bytes32,uint24)")
                    .to_vec(),
                POOL_ID.to_vec(),
                word_from_address(&CURRENCY0),
                word_from_address(&CURRENCY1),
            ],
            data,
            index: 0,
            block_index: 0,
            ordinal: LOG_ORDINAL,
        }
    }

    /// One successful tx: `log` in the receipt, `storage_changes` in one call. `EventHandler`
    /// walks receipt logs, `get_new_pools` walks `tx.calls[..].storage_changes`.
    fn block_with(log: Log, storage_changes: Vec<StorageChange>) -> Block {
        block_with_calls(log, vec![Call { storage_changes, ..Default::default() }])
    }

    fn block_with_calls(log: Log, calls: Vec<Call>) -> Block {
        Block {
            transaction_traces: vec![TransactionTrace {
                hash: vec![0x33; 32],
                from: vec![0x44; 20],
                to: hex::decode(POOL_MANAGER).unwrap(),
                index: 7,
                status: 1,
                receipt: Some(TransactionReceipt { logs: vec![log], ..Default::default() }),
                calls,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// The slot0 storage write `map_pools_created` looks for, at `keccak(pool_id ++ 5)`.
    fn slot0_write(new_value: [u8; 32]) -> StorageChange {
        slot0_write_at(new_value, 1)
    }

    /// A write to some other slot of the same contract, at `ordinal`.
    fn other_slot_write_at(key: [u8; 32], new_value: [u8; 32], ordinal: u64) -> StorageChange {
        StorageChange {
            address: hex::decode(POOL_MANAGER).unwrap(),
            key: key.to_vec(),
            old_value: vec![0u8; 32],
            new_value: new_value.to_vec(),
            ordinal,
        }
    }

    fn slot0_write_at(new_value: [u8; 32], ordinal: u64) -> StorageChange {
        StorageChange {
            address: hex::decode(POOL_MANAGER).unwrap(),
            key: parameters::pool_state_base_slot(&POOL_ID, BIN_POOLS_MAPPING_SLOT).to_vec(),
            old_value: vec![0u8; 32],
            new_value: new_value.to_vec(),
            ordinal,
        }
    }

    fn run(block: &Block) -> Vec<TransactionEntityChanges> {
        let mut new_pools = vec![];
        get_new_pools(block, &mut new_pools, POOL_MANAGER);
        new_pools
    }

    fn attribute<'a>(attrs: &'a [Attribute], name: &str) -> Option<&'a Attribute> {
        attrs.iter().find(|a| a.name == name)
    }

    fn assert_attr(attrs: &[Attribute], name: &str, expected: impl Into<Vec<u8>>) {
        assert_eq!(attribute(attrs, name).unwrap().value, expected.into(), "attribute {name}");
    }

    fn static_pool_block(hooks: &[u8; 20], fee: u32, hook_bitmap: u16) -> Block {
        pool_block_with_active_id(hooks, fee, hook_bitmap, ACTIVE_ID)
    }

    fn pool_block_with_active_id(
        hooks: &[u8; 20],
        fee: u32,
        hook_bitmap: u16,
        active_id: u32,
    ) -> Block {
        block_with(
            initialize_log(hooks, fee, &parameters(hook_bitmap, 10), active_id),
            vec![slot0_write(slot0(active_id, 200, 300, fee))],
        )
    }

    /// The `active_id` state attribute of the single component the block produces.
    fn emitted_active_id(active_id: u32) -> Vec<u8> {
        let changes = run(&pool_block_with_active_id(&[0u8; 20], 7, 0, active_id));
        attribute(&changes[0].entity_changes[0].attributes, "active_id")
            .expect("active_id attribute")
            .value
            .clone()
    }

    /// Pins the whole attribute schema. A renamed or dropped attribute fails here instead of as
    /// an undecodable snapshot later.
    #[test]
    fn static_hookless_pool_is_emitted_with_infinity_attributes() {
        let changes = run(&static_pool_block(&[0u8; 20], 7, 0));

        assert_eq!(changes.len(), 1);
        let change = &changes[0];

        assert_eq!(change.component_changes.len(), 1);
        let component = &change.component_changes[0];
        assert_eq!(component.id, POOL_ID.to_vec().to_hex());
        assert_eq!(component.tokens, vec![CURRENCY0.to_vec(), CURRENCY1.to_vec()]);
        assert_eq!(
            component
                .protocol_type
                .as_ref()
                .unwrap()
                .name,
            "pancakeswap_infinity_bin_pool"
        );

        let statics = &component.static_att;
        assert_attr(statics, "bin_step", 10u16.to_be_bytes());
        assert_attr(statics, "pool_id", POOL_ID.to_vec());
        assert_attr(statics, "key_lp_fee", BigInt::from(7).to_signed_bytes_be());
        assert_attr(statics, "parameters", parameters(0, 10));
        assert_attr(statics, "pool_manager", hex::decode(POOL_MANAGER).unwrap());
        assert!(attribute(statics, "hook_address").is_none());
        assert!(attribute(statics, "hooks").is_none());
        assert!(attribute(statics, "tick").is_none());
        assert!(attribute(statics, "sqrt_price_x96").is_none());
        assert!(attribute(statics, "liquidity").is_none());

        assert_eq!(change.entity_changes.len(), 1);
        let state = &change.entity_changes[0].attributes;
        assert_attr(state, "balance_owner", hex::decode(VAULT_ADDRESS).unwrap());
        assert_attr(state, "active_id", BigInt::from(ACTIVE_ID).to_signed_bytes_be());
        assert_attr(state, "fee", BigInt::from(7).to_signed_bytes_be());
        assert_attr(state, "protocol_fees/zero2one", BigInt::from(200).to_signed_bytes_be());
        assert_attr(state, "protocol_fees/one2zero", BigInt::from(300).to_signed_bytes_be());
    }

    /// 2^23 encodes to 4 bytes, not 3. Pins it so a fixed-width decoder fails here.
    #[test]
    fn active_id_at_the_reference_bin_round_trips() {
        // Top bit set within three bytes, so a signed encoding prepends a zero byte.
        let at_reference = emitted_active_id(ACTIVE_ID);
        assert_eq!(at_reference, BigInt::from(ACTIVE_ID).to_signed_bytes_be());
        assert_eq!(at_reference.len(), 4, "2^23 needs a leading zero byte to stay positive");

        // One bin lower fits in three. Both widths occur around the active bin.
        let below = emitted_active_id(ACTIVE_ID - 1);
        assert_eq!(below, BigInt::from(ACTIVE_ID - 1).to_signed_bytes_be());
        assert_eq!(below.len(), 3);

        // Width aside, decoding must give the number back. A fixed-3-byte decoder reads 2^23 as
        // 0 and still passes the length checks above.
        assert_eq!(BigInt::from_signed_bytes_be(&at_reference), BigInt::from(ACTIVE_ID));
        assert_eq!(BigInt::from_signed_bytes_be(&below), BigInt::from(ACTIVE_ID - 1));

        // Top of the uint24 range, the widest a bin id gets.
        let max = emitted_active_id(0xff_ffff);
        assert_eq!(BigInt::from_signed_bytes_be(&max), BigInt::from(0xff_ffffu32));
    }

    /// Non-swap callbacks still quote normally, so the pool stays in scope and the hook address
    /// is recorded. Bits 2 and 3 are beforeMint/afterMint on Bin.
    #[test]
    fn liquidity_only_hook_pool_is_emitted_with_hook_address() {
        let hooks = [0x77u8; 20];
        let before_add_liquidity = 1 << 2;
        let changes = run(&static_pool_block(&hooks, 7, before_add_liquidity));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].component_changes.len(), 1);
        let statics = &changes[0].component_changes[0].static_att;
        assert_attr(statics, "hook_address", hooks);
        assert!(attribute(statics, "hooks").is_none());
    }

    /// Any of the four swap callbacks puts the pool out of scope.
    #[rstest::rstest]
    #[case::before_swap(crate::parameters::HOOKS_BEFORE_SWAP_OFFSET)]
    #[case::after_swap(crate::parameters::HOOKS_AFTER_SWAP_OFFSET)]
    #[case::before_swap_returns_delta(crate::parameters::HOOKS_BEFORE_SWAP_RETURNS_DELTA_OFFSET)]
    #[case::after_swap_returns_delta(crate::parameters::HOOKS_AFTER_SWAP_RETURNS_DELTA_OFFSET)]
    fn swap_hook_pool_is_skipped(#[case] bit: u8) {
        let hook_bitmap = 1 << bit;
        assert!(
            run(&static_pool_block(&[0u8; 20], 7, hook_bitmap)).is_empty(),
            "swap hook bit {bit} must put the pool out of scope"
        );
    }

    #[test]
    fn dynamic_fee_pool_is_skipped() {
        let fee = parameters::DYNAMIC_FEE_FLAG;
        let changes = run(&static_pool_block(&[0u8; 20], fee, 0));

        assert!(changes.is_empty(), "dynamic-fee pools are out of scope");
    }

    /// The protocol fee is read from a storage write, not the event, so a pool whose slot0 write
    /// is missing is a broken assumption rather than a pool to skip.
    #[test]
    #[should_panic(expected = "no slot0 storage write")]
    fn missing_slot0_write_panics() {
        let block =
            block_with(initialize_log(&[0u8; 20], 500, &parameters(0, 60), ACTIVE_ID), vec![]);
        run(&block);
    }

    /// A real `initialize` writes many slots, and one tx can create several pools. Only
    /// `keccak(pool_id ++ 5)` is ours.
    #[test]
    fn slot0_writes_for_other_slots_are_ignored() {
        let other_pool = parameters::pool_state_base_slot(&[0xcd; 32], BIN_POOLS_MAPPING_SLOT);
        let block = block_with(
            initialize_log(&[0u8; 20], 7, &parameters(0, 10), ACTIVE_ID),
            vec![
                slot0_write_at(slot0(ACTIVE_ID, 200, 300, 7), 2),
                // Higher ordinal, so it wins on recency alone if the key is not checked.
                other_slot_write_at(other_pool, slot0(ACTIVE_ID, 999, 888, 7), 3),
            ],
        );

        let changes = run(&block);
        let state = &changes[0].entity_changes[0].attributes;
        assert_attr(state, "protocol_fees/zero2one", BigInt::from(200u32).to_signed_bytes_be());
        assert_attr(state, "protocol_fees/one2zero", BigInt::from(300u32).to_signed_bytes_be());
    }

    /// Two traps in one: a write from a reverted call must not be used, and neither must a write
    /// that lands AFTER the log. `initialize` writes slot0 before emitting, so only writes below
    /// the log ordinal count, and the last of those wins.
    #[test]
    fn reverted_and_later_slot0_writes_are_ignored() {
        // Four writes to the same slot in one transaction. Only the third may be picked: it is
        // the last non-reverted write below the log's ordinal.
        let block = block_with_calls(
            initialize_log(&[0u8; 20], 7, &parameters(0, 10), ACTIVE_ID),
            vec![
                // Real, but superseded by the write at ordinal 3.
                Call {
                    storage_changes: vec![slot0_write_at(slot0(ACTIVE_ID, 50, 60, 7), 2)],
                    ..Default::default()
                },
                // The pool's initial state: last non-reverted write below LOG_ORDINAL.
                Call {
                    storage_changes: vec![slot0_write_at(slot0(ACTIVE_ID, 200, 300, 7), 3)],
                    ..Default::default()
                },
                // Reverted, so its state never happened. Deliberately the HIGHEST ordinal below
                // the log: it is what max_by_key would pick if the revert guard were dropped, so
                // the guard is only covered while this sorts above the write that should win.
                Call {
                    storage_changes: vec![slot0_write_at(slot0(ACTIVE_ID, 111, 222, 7), 4)],
                    state_reverted: true,
                    ..Default::default()
                },
                // After the log, so it belongs to whatever happened next in the same tx, not to
                // initialize. Picking this would read a later pool state as the initial one.
                Call {
                    storage_changes: vec![slot0_write_at(
                        slot0(ACTIVE_ID, 999, 888, 7),
                        LOG_ORDINAL + 1,
                    )],
                    ..Default::default()
                },
            ],
        );

        let changes = run(&block);
        assert_eq!(changes.len(), 1);
        let state = &changes[0].entity_changes[0].attributes;

        assert_attr(state, "protocol_fees/zero2one", BigInt::from(200u32).to_signed_bytes_be());
        assert_attr(state, "protocol_fees/one2zero", BigInt::from(300u32).to_signed_bytes_be());
    }

    /// Logs from a different contract must be ignored even when they decode cleanly.
    #[test]
    fn logs_from_other_addresses_are_ignored() {
        let mut block = static_pool_block(&[0u8; 20], 7, 0);
        // Same well-formed Initialize, emitted by some other contract. Another Infinity pool
        // manager on the same chain would decode cleanly here, so the address filter is the only
        // thing keeping CL and Bin pools apart.
        block.transaction_traces[0]
            .receipt
            .as_mut()
            .unwrap()
            .logs[0]
            .address = vec![0x99; 20];

        assert!(run(&block).is_empty(), "logs from another contract must be ignored");
    }
}
