//! Camelot V3 pool discovery and the storage layout the package reads.
//!
//! Camelot V3 on Arbitrum One is an Algebra V1.9 deployment. `AlgebraFactory.createPool` does
//! two things that matter for indexing:
//!
//! 1. It deploys a `DataStorageOperator` with `CREATE`. That contract holds the pool's oracle
//!    timepoints and adaptive fee configuration; the pool calls it on every swap (`write`,
//!    `getFees`, `calculateVolumePerLiquidity`). Its address appears in no event; it is only an
//!    immutable of the pool.
//! 2. It calls `AlgebraPoolDeployer.deploy`, which deploys the pool with `CREATE2` and then the
//!    factory emits `Pool(token0, token1, pool)`.
//!
//! The package is a native integration: it emits the values a swap reads as state attributes,
//! taken from the storage words the pool and its operator write. Storage layouts below were
//! checked against the deployed contracts' getters (pool `0xB1026b8e…7526` and its operator at
//! block 512282036).
use anyhow::{bail, Result};
use substreams_ethereum::{
    pb::eth::v2::{Call, CallType, TransactionTrace},
    Event,
};
use substreams_helper::hex::Hexable;
use tiny_keccak::{Hasher, Keccak};
use tycho_substreams::prelude::*;

use crate::abi::factory::events::Pool as PoolCreated;

/// Tycho protocol type of every Camelot V3 pool component.
pub const PROTOCOL_TYPE: &str = "camelot_v3_pool";

/// Static attribute holding the pool's `DataStorageOperator` address as raw bytes.
pub const DATA_STORAGE_OPERATOR_ATTRIBUTE: &str = "data_storage_operator";

/// State attribute names, shared with the `camelot_v3` decoder in `tycho-simulation`.
pub mod attributes {
    pub const LIQUIDITY: &str = "liquidity";
    pub const SQRT_PRICE_X96: &str = "sqrt_price_x96";
    pub const TICK: &str = "tick";
    pub const FEE_ZTO: &str = "fee_zto";
    pub const FEE_OTZ: &str = "fee_otz";
    pub const TIMEPOINT_INDEX: &str = "timepoint_index";
    pub const VOLUME_PER_LIQUIDITY_IN_BLOCK: &str = "volume_per_liquidity_in_block";
    pub const FEE_CONFIG_ZTO: &str = "fee_config_zto";
    pub const FEE_CONFIG_OTZ: &str = "fee_config_otz";

    pub fn tick(tick: i32) -> String {
        format!("ticks/{tick}")
    }

    pub fn timepoint(index: u16) -> String {
        format!("timepoints/{index}")
    }
}

/// A 32-byte storage word.
pub type Word = [u8; 32];

/// `PoolState.globalState`: `price(20) tick(3) feeZto(2) feeOtz(2) timepointIndex(2)
/// communityFeeToken0(1) communityFeeToken1(1) unlocked(1)`, packed from the low end.
pub const GLOBAL_STATE_SLOT: u64 = 2;
/// `PoolState.liquidity` in the low 16 bytes, `volumePerLiquidityInBlock` in the high 16.
pub const LIQUIDITY_SLOT: u64 = 3;
/// `PoolState.ticks`: `TickManager.Tick` keyed by `int24`; its first word holds
/// `liquidityTotal` (low 16 bytes) and `liquidityDelta` (high 16 bytes).
pub const TICKS_MAP_SLOT: u64 = 5;
/// `DataStorageOperator.timepoints[65536]`: two words per entry, from slot 0.
pub const TIMEPOINT_WORDS: u64 = 2 * 65_536;
/// `DataStorageOperator.feeConfigZto` / `feeConfigOtz`: one word each, right after the ring.
pub const FEE_CONFIG_ZTO_SLOT: u64 = TIMEPOINT_WORDS;
pub const FEE_CONFIG_OTZ_SLOT: u64 = TIMEPOINT_WORDS + 1;
/// `DataStorage.WINDOW`: the adaptive fee averages over one day.
pub const WINDOW: u32 = 86_400;
const RING_SIZE: u32 = 65_536;

/// Store key of the component a pool or operator contract belongs to, by the contract's
/// `0x`-prefixed lowercase hex address.
pub fn contract_key(address_hex: &str) -> String {
    format!("contract:{address_hex}")
}

/// Store key of an operator's storage word.
pub fn operator_slot_key(operator: &[u8], slot: u64) -> String {
    format!("slot:{}:{slot}", hex::encode(operator))
}

/// Returns a component for every pool the factory created in `tx`, in call order.
///
/// Errors when a factory call emitted a `Pool` event but did not create exactly one contract
/// itself: the created contract is the pool's `DataStorageOperator`, whose storage the package
/// reads, so the shape is enforced rather than guessed around.
pub fn pools_created(factory: &[u8], tx: &TransactionTrace) -> Result<Vec<ProtocolComponent>> {
    let mut components = Vec::new();
    for call in tx
        .calls
        .iter()
        .filter(|call| !call.state_reverted && call.address == factory)
    {
        let events: Vec<PoolCreated> = call
            .logs
            .iter()
            .filter_map(PoolCreated::match_and_decode)
            .collect();
        let event = match events.as_slice() {
            [] => continue,
            [event] => event,
            _ => bail!(
                "factory call {} in tx 0x{} emitted {} Pool events, expected one per createPool",
                call.index,
                hex::encode(&tx.hash),
                events.len()
            ),
        };
        let operator = data_storage_operator(call, tx)?;
        components.push(
            ProtocolComponent::new(&event.pool.to_hex())
                .with_tokens(&[event.token0.as_slice(), event.token1.as_slice()])
                .with_attributes(&[(DATA_STORAGE_OPERATOR_ATTRIBUTE, operator.as_slice())])
                .as_swap_type(PROTOCOL_TYPE, ImplementationType::Custom),
        );
    }
    Ok(components)
}

/// The address of the single contract `create_pool` deployed directly with `CREATE`.
///
/// The pool itself is deployed by the pool deployer, one call level deeper, so it never matches.
fn data_storage_operator(create_pool: &Call, tx: &TransactionTrace) -> Result<Vec<u8>> {
    let mut created = tx.calls.iter().filter(|call| {
        call.parent_index == create_pool.index &&
            call.call_type() == CallType::Create &&
            !call.state_reverted
    });
    let Some(operator) = created.next() else {
        bail!(
            "factory call {} in tx 0x{} emitted a Pool event but created no DataStorageOperator",
            create_pool.index,
            hex::encode(&tx.hash)
        )
    };
    if let Some(extra) = created.next() {
        bail!(
            "factory call {} in tx 0x{} created more than one contract (0x{} and 0x{}), cannot \
             tell which is the DataStorageOperator",
            create_pool.index,
            hex::encode(&tx.hash),
            hex::encode(&operator.address),
            hex::encode(&extra.address)
        )
    }
    Ok(operator.address.clone())
}

/// The attributes a pool starts with: every scalar the decoder requires, at the zero the
/// contract holds before its constructor runs. The constructor sets `feeZto` and `feeOtz` to
/// `BASE_FEE` (100) and `initialize` sets the price, the tick and the first timepoint; those
/// values arrive from the pool's storage writes and take precedence over these defaults.
pub fn initial_attributes() -> Vec<Attribute> {
    [
        (attributes::LIQUIDITY, 16),
        (attributes::SQRT_PRICE_X96, 20),
        (attributes::TICK, 3),
        (attributes::FEE_ZTO, 2),
        (attributes::FEE_OTZ, 2),
        (attributes::TIMEPOINT_INDEX, 2),
        (attributes::VOLUME_PER_LIQUIDITY_IN_BLOCK, 16),
    ]
    .into_iter()
    .map(|(name, width)| Attribute {
        name: name.to_string(),
        value: vec![0u8; width],
        change: ChangeType::Creation.into(),
    })
    .collect()
}

/// The slot number a storage key names, for keys of fixed-position variables (not mapping or
/// dynamic array entries, whose hashed keys never fit).
pub fn slot_number(key: &[u8]) -> Option<u64> {
    if key.len() != 32 || key[..24].iter().any(|b| *b != 0) {
        return None;
    }
    Some(u64::from_be_bytes(key[24..].try_into().expect("8 bytes")))
}

/// The storage key of a fixed-position variable.
pub fn slot_word(slot: u64) -> Word {
    let mut key = [0u8; 32];
    key[24..].copy_from_slice(&slot.to_be_bytes());
    key
}

/// A storage change value as a word. Firehose delivers full 32-byte words; anything else is an
/// error rather than something to pad.
pub fn word(value: &[u8]) -> Result<Word> {
    value.try_into().map_err(|_| {
        anyhow::anyhow!("storage value of {} bytes is not a 32-byte word", value.len())
    })
}

/// Bytes `[offset, offset + len)` counted from the low end of a word, as Solidity packs struct
/// members and small variables.
fn low_end(word: &Word, offset: usize, len: usize) -> &[u8] {
    &word[32 - offset - len..32 - offset]
}

fn changed_fields(
    old: &Word,
    new: &Word,
    fields: &[(&str, usize, usize)],
    change: ChangeType,
) -> Vec<Attribute> {
    fields
        .iter()
        .filter(|(_, offset, len)| low_end(old, *offset, *len) != low_end(new, *offset, *len))
        .map(|(name, offset, len)| Attribute {
            name: name.to_string(),
            value: low_end(new, *offset, *len).to_vec(),
            change: change.into(),
        })
        .collect()
}

/// The attributes that changed between two values of the pool's `globalState` word.
pub fn global_state_attributes(old: &Word, new: &Word, change: ChangeType) -> Vec<Attribute> {
    changed_fields(
        old,
        new,
        &[
            (attributes::SQRT_PRICE_X96, 0, 20),
            (attributes::TICK, 20, 3),
            (attributes::FEE_ZTO, 23, 2),
            (attributes::FEE_OTZ, 25, 2),
            (attributes::TIMEPOINT_INDEX, 27, 2),
        ],
        change,
    )
}

/// The attributes that changed between two values of the pool's `liquidity` word.
pub fn liquidity_attributes(old: &Word, new: &Word, change: ChangeType) -> Vec<Attribute> {
    changed_fields(
        old,
        new,
        &[(attributes::LIQUIDITY, 0, 16), (attributes::VOLUME_PER_LIQUIDITY_IN_BLOCK, 16, 16)],
        change,
    )
}

/// Storage slot of `ticks[tick]`: `keccak256(abi.encode(int24(tick), uint256(5)))`.
pub fn tick_slot(tick: i32) -> Word {
    let mut key = if tick < 0 { [0xffu8; 32] } else { [0u8; 32] };
    key[29..].copy_from_slice(&tick.to_be_bytes()[1..]);
    let mut slot = [0u8; 32];
    slot[31] = TICKS_MAP_SLOT as u8;
    let mut output = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(&key);
    hasher.update(&slot);
    hasher.finalize(&mut output);
    output
}

/// The `ticks/{tick}` attribute for a change of the tick's first storage word, or `None` when
/// the word did not change.
///
/// The value is the tick's `liquidityDelta`. A tick exists while any position references it
/// (`liquidityTotal > 0`), including with a zero delta: the pool still stops a swap step there.
pub fn tick_attribute(tick: i32, old: &Word, new: &Word) -> Option<Attribute> {
    if old == new {
        return None;
    }
    let total_was_zero = low_end(old, 0, 16)
        .iter()
        .all(|b| *b == 0);
    let total_is_zero = low_end(new, 0, 16)
        .iter()
        .all(|b| *b == 0);
    let change = if total_is_zero {
        ChangeType::Deletion
    } else if total_was_zero {
        ChangeType::Creation
    } else {
        ChangeType::Update
    };
    Some(Attribute {
        name: attributes::tick(tick),
        value: if total_is_zero { Vec::new() } else { low_end(new, 16, 16).to_vec() },
        change: change.into(),
    })
}

/// The ring index and word position (`false` first word, `true` second) an operator slot holds.
pub fn timepoint_of_slot(slot: u64) -> Option<(u16, bool)> {
    (slot < TIMEPOINT_WORDS).then_some(((slot / 2) as u16, slot % 2 == 1))
}

/// The two operator slots holding `timepoints[index]`.
pub fn timepoint_slots(index: u16) -> (u64, u64) {
    let first = 2 * u64::from(index);
    (first, first + 1)
}

/// `initialized` and `blockTimestamp` from a timepoint's first word.
pub fn timepoint_header(first_word: &Word) -> (bool, u32) {
    (
        first_word[31] != 0,
        u32::from_be_bytes(
            first_word[27..31]
                .try_into()
                .expect("4 bytes"),
        ),
    )
}

/// The `timepoints/{index}` attribute: both words of the entry, first then second.
pub fn timepoint_attribute(
    index: u16,
    first_word: &Word,
    second_word: &Word,
    change: ChangeType,
) -> Attribute {
    let mut value = Vec::with_capacity(64);
    value.extend_from_slice(first_word);
    value.extend_from_slice(second_word);
    Attribute { name: attributes::timepoint(index), value, change: change.into() }
}

/// The fee configuration attribute an operator slot holds, if it is one of the two.
pub fn fee_config_attribute(slot: u64, new: &Word, change: ChangeType) -> Option<Attribute> {
    let name = match slot {
        FEE_CONFIG_ZTO_SLOT => attributes::FEE_CONFIG_ZTO,
        FEE_CONFIG_OTZ_SLOT => attributes::FEE_CONFIG_OTZ,
        _ => return None,
    };
    Some(Attribute { name: name.to_string(), value: new.to_vec(), change: change.into() })
}

/// `lteConsideringOverflow`: whether `a` is chronologically at or before `b`, for 32-bit
/// timestamps that may have wrapped relative to `current_time`.
fn lte(a: u32, b: u32, current_time: u32) -> bool {
    let mut res = a > current_time;
    if res == (b > current_time) {
        res = a <= b;
    }
    res
}

/// The ring indices that became unreachable when the timepoint at `new_index` was written at
/// `new_timestamp`, to be emitted as deletions.
///
/// The adaptive fee reads the oldest timepoint, the last two, and the pair around
/// `now - WINDOW`. Timestamps only grow along the ring and the execution time only moves
/// forward, so once a timepoint is older than the newest one at or before `now - WINDOW`, and
/// is not the one right before the last, nothing reads it again. The kept range starts at
/// `min(that timepoint, last - 1)`; the deleted range is what the previous write kept and this
/// one does not. `timestamp_at` reads a written timepoint's timestamp by ring index, `None`
/// for a slot never written.
pub fn unreachable_timepoints(
    new_index: u16,
    new_timestamp: u32,
    timestamp_at: impl Fn(u16) -> Option<u32>,
) -> Vec<u16> {
    let prev_index = new_index.wrapping_sub(1);
    let Some(prev_timestamp) = timestamp_at(prev_index) else {
        // `initialize` wrote the first timepoint: nothing precedes it.
        return Vec::new();
    };
    // The slot after the newest one is initialized only once the ring has wrapped, and is then
    // the oldest entry.
    let oldest = if timestamp_at(new_index.wrapping_add(1)).is_some() {
        new_index.wrapping_add(1)
    } else {
        0
    };
    let index_at =
        |position: u32| -> u16 { (u32::from(oldest) + position).rem_euclid(RING_SIZE) as u16 };
    let last_position =
        (u32::from(new_index).wrapping_sub(u32::from(oldest))).rem_euclid(RING_SIZE);
    let timestamp_at_position = |position: u32| -> Option<u32> {
        if position == last_position {
            Some(new_timestamp)
        } else {
            timestamp_at(index_at(position))
        }
    };
    // Largest position whose timestamp is at or before `target`, if any.
    let search = |target: u32| -> Option<u32> {
        let (mut low, mut high) = (0u32, last_position);
        let mut found = None;
        while low <= high {
            let mid = low + (high - low) / 2;
            let before =
                timestamp_at_position(mid).is_some_and(|ts| lte(ts, target, new_timestamp));
            if before {
                found = Some(mid);
                low = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                high = mid - 1;
            }
        }
        found
    };
    let keep_start = |timestamp: u32, before_last: u32| -> u32 {
        search(timestamp.wrapping_sub(WINDOW))
            .unwrap_or(0)
            .min(before_last)
    };
    let new_start = keep_start(new_timestamp, last_position.saturating_sub(1));
    let prev_start = keep_start(prev_timestamp, last_position.saturating_sub(2));
    (prev_start..new_start)
        .map(index_at)
        .collect()
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;
    use substreams_ethereum::pb::eth::v2::Log;

    use super::*;

    const FACTORY: [u8; 20] = [0xfa; 20];
    const DEPLOYER: [u8; 20] = [0xde; 20];
    const OPERATOR: [u8; 20] = [0x0b; 20];
    const POOL: [u8; 20] = [0xb0; 20];
    const TOKEN0: [u8; 20] = [0x01; 20];
    const TOKEN1: [u8; 20] = [0x02; 20];

    /// `globalState` of the WETH/USDC pool `0xB1026b8e…7526` at block 512282036.
    const GLOBAL_STATE: Word =
        hex!("019696be0800640064fcfd92000000000000000000036b49574c23670207f193");

    fn padded(address: &[u8; 20]) -> Vec<u8> {
        let mut word = vec![0u8; 12];
        word.extend_from_slice(address);
        word
    }

    /// The factory's `Pool(token0, token1, pool)` log.
    fn pool_log(emitter: &[u8; 20]) -> Log {
        let topic = hex::decode("91ccaa7a278130b65168c3a0c8d3bcae84cf5e43704342bd3ec0b59e59c036db")
            .unwrap();
        Log {
            address: emitter.to_vec(),
            topics: vec![topic, padded(&TOKEN0), padded(&TOKEN1)],
            data: padded(&POOL),
            ..Default::default()
        }
    }

    fn call(index: u32, parent_index: u32, call_type: CallType, address: &[u8; 20]) -> Call {
        Call {
            index,
            parent_index,
            call_type: call_type as i32,
            address: address.to_vec(),
            ..Default::default()
        }
    }

    /// The call tree of a real `createPool`: the factory frame emits `Pool`, creates the
    /// operator directly, and has the deployer create the pool one level deeper.
    fn create_pool_tx() -> TransactionTrace {
        let mut factory_call = call(1, 0, CallType::Call, &FACTORY);
        factory_call.logs = vec![pool_log(&FACTORY)];
        TransactionTrace {
            hash: vec![0xaa; 32],
            calls: vec![
                factory_call,
                call(2, 1, CallType::Create, &OPERATOR),
                call(3, 1, CallType::Call, &DEPLOYER),
                call(4, 3, CallType::Create, &POOL),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn discovers_pool_with_operator() {
        let components = pools_created(&FACTORY, &create_pool_tx()).unwrap();

        assert_eq!(components.len(), 1);
        let component = &components[0];
        assert_eq!(component.id, POOL.to_hex());
        assert_eq!(component.tokens, vec![TOKEN0.to_vec(), TOKEN1.to_vec()]);
        assert!(component.contracts.is_empty());
        assert_eq!(
            component.get_attribute_value(DATA_STORAGE_OPERATOR_ATTRIBUTE),
            Some(OPERATOR.to_vec())
        );
        let protocol_type = component
            .protocol_type
            .as_ref()
            .unwrap();
        assert_eq!(protocol_type.name, PROTOCOL_TYPE);
        assert_eq!(protocol_type.implementation_type, ImplementationType::Custom as i32);
    }

    #[test]
    fn ignores_reverted_factory_call() {
        let mut tx = create_pool_tx();
        tx.calls[0].state_reverted = true;

        assert!(pools_created(&FACTORY, &tx)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn ignores_pool_log_from_other_contract() {
        let mut tx = create_pool_tx();
        tx.calls[0].address = DEPLOYER.to_vec();
        tx.calls[0].logs = vec![pool_log(&DEPLOYER)];

        assert!(pools_created(&FACTORY, &tx)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn errors_without_operator_creation() {
        let mut tx = create_pool_tx();
        tx.calls.remove(1);

        assert!(pools_created(&FACTORY, &tx).is_err());
    }

    #[test]
    fn errors_with_ambiguous_operator_creation() {
        let mut tx = create_pool_tx();
        tx.calls
            .push(call(5, 1, CallType::Create, &[0x0c; 20]));

        assert!(pools_created(&FACTORY, &tx).is_err());
    }

    #[test]
    fn initial_attributes_cover_every_scalar_the_decoder_needs() {
        let names: Vec<_> = initial_attributes()
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "liquidity",
                "sqrt_price_x96",
                "tick",
                "fee_zto",
                "fee_otz",
                "timepoint_index",
                "volume_per_liquidity_in_block"
            ]
        );
    }

    #[test]
    fn slot_numbers_are_read_from_fixed_keys_only() {
        let mut key = [0u8; 32];
        key[31] = 3;
        assert_eq!(slot_number(&key), Some(3));
        assert_eq!(slot_number(&tick_slot(0)), None);
        assert_eq!(slot_number(&[0u8; 31]), None);
    }

    #[test]
    fn decodes_the_global_state_word() {
        // Against the `globalState()` getter: price 4133423575270025211670931, tick -197230,
        // fees 100/100, timepoint index 48648.
        let attributes = global_state_attributes(&[0u8; 32], &GLOBAL_STATE, ChangeType::Update);
        let value = |name: &str| {
            attributes
                .iter()
                .find(|a| a.name == name)
                .map(|a| a.value.clone())
                .unwrap()
        };
        assert_eq!(
            u128::from_be_bytes(
                value("sqrt_price_x96")[4..]
                    .try_into()
                    .unwrap()
            ),
            4_133_423_575_270_025_211_670_931
        );
        assert_eq!(value("tick"), hex!("fcfd92"));
        assert_eq!(value("fee_zto"), hex!("0064"));
        assert_eq!(value("fee_otz"), hex!("0064"));
        assert_eq!(value("timepoint_index"), hex!("be08"));
        assert_eq!(attributes.len(), 5);
    }

    #[test]
    fn emits_only_the_fields_that_changed() {
        let mut new = GLOBAL_STATE;
        new[0] = 0; // `unlocked` toggled, which no attribute tracks
        assert!(global_state_attributes(&GLOBAL_STATE, &new, ChangeType::Update).is_empty());
        new[3..5].copy_from_slice(&hex!("be09")); // timepointIndex
        let attributes = global_state_attributes(&GLOBAL_STATE, &new, ChangeType::Update);
        assert_eq!(attributes.len(), 1);
        assert_eq!(attributes[0].name, "timepoint_index");
    }

    #[test]
    fn decodes_the_liquidity_word() {
        // Slot 3 of the same pool: liquidity 52937052414055576, volumePerLiquidityInBlock
        // 715374378433376.
        let word = hex!("000000000000000000028aa113b53b60000000000000000000bc11f7fc989498");
        let attributes = liquidity_attributes(&[0u8; 32], &word, ChangeType::Update);
        assert_eq!(attributes[0].name, "liquidity");
        assert_eq!(
            u128::from_be_bytes(
                attributes[0]
                    .value
                    .clone()
                    .try_into()
                    .unwrap()
            ),
            52_937_052_414_055_576
        );
        assert_eq!(attributes[1].name, "volume_per_liquidity_in_block");
        assert_eq!(
            u128::from_be_bytes(
                attributes[1]
                    .value
                    .clone()
                    .try_into()
                    .unwrap()
            ),
            715_374_378_433_376
        );
    }

    #[test]
    fn tick_slot_matches_the_chain() {
        // `ticks(-197370)` of the WETH/USDC pool lives at this slot, holding liquidityTotal
        // 163079471885 and liquidityDelta -163079471885 at block 512282036.
        assert_eq!(
            tick_slot(-197_370),
            hex!("7945f471e081e35795d30e7f283f5fcfb2b546f7279711abe4c5900a5e6ba23d")
        );
        let word = hex!("ffffffffffffffffffffffda07b4bcf3000000000000000000000025f84b430d");
        let attribute = tick_attribute(-197_370, &[0u8; 32], &word).unwrap();
        assert_eq!(attribute.name, "ticks/-197370");
        assert_eq!(attribute.change, i32::from(ChangeType::Creation));
        assert_eq!(i128::from_be_bytes(attribute.value.try_into().unwrap()), -163_079_471_885);
    }

    #[test]
    fn tick_changes_follow_the_position_liquidity() {
        let mut referenced = [0u8; 32];
        referenced[31] = 1; // liquidityTotal 1, liquidityDelta 0
        let created = tick_attribute(60, &[0u8; 32], &referenced).unwrap();
        assert_eq!(created.change, i32::from(ChangeType::Creation));
        assert_eq!(created.value, vec![0u8; 16], "a zero delta keeps the tick");
        let mut more = referenced;
        more[31] = 2;
        assert_eq!(
            tick_attribute(60, &referenced, &more)
                .unwrap()
                .change,
            i32::from(ChangeType::Update)
        );
        let deleted = tick_attribute(60, &more, &[0u8; 32]).unwrap();
        assert_eq!(deleted.change, i32::from(ChangeType::Deletion));
        assert!(tick_attribute(60, &more, &more).is_none());
    }

    #[test]
    fn operator_slots_map_to_timepoints_and_fee_configs() {
        assert_eq!(timepoint_of_slot(0), Some((0, false)));
        assert_eq!(timepoint_of_slot(1), Some((0, true)));
        assert_eq!(timepoint_of_slot(97_296), Some((48_648, false)));
        assert_eq!(timepoint_of_slot(131_071), Some((65_535, true)));
        assert_eq!(timepoint_of_slot(131_072), None);
        assert_eq!(timepoint_slots(48_648), (97_296, 97_297));
        let word = [7u8; 32];
        assert_eq!(
            fee_config_attribute(131_072, &word, ChangeType::Update)
                .unwrap()
                .name,
            "fee_config_zto"
        );
        assert_eq!(
            fee_config_attribute(131_073, &word, ChangeType::Update)
                .unwrap()
                .name,
            "fee_config_otz"
        );
        assert!(fee_config_attribute(131_074, &word, ChangeType::Update).is_none());
    }

    #[test]
    fn reads_the_timepoint_header() {
        // `timepoints(0)` of the WETH/USDC operator: initialized, timestamp 1790273118.
        let first = hex!("000000000000000612fe4b4ffb93521573505bd9ffed77eba5c55b6ab5665e01");
        assert_eq!(timepoint_header(&first), (true, 1_790_273_118));
        assert_eq!(timepoint_header(&[0u8; 32]), (false, 0));
        let attribute = timepoint_attribute(0, &first, &[9u8; 32], ChangeType::Creation);
        assert_eq!(attribute.name, "timepoints/0");
        assert_eq!(attribute.value.len(), 64);
        assert_eq!(&attribute.value[..32], &first);
    }

    /// A ring whose timepoints are `base + 1000 * index` seconds apart.
    fn ring(written: u16) -> impl Fn(u16) -> Option<u32> {
        move |index| (index <= written).then(|| 1_000_000 + 1_000 * u32::from(index))
    }

    #[test]
    fn nothing_is_unreachable_while_the_ring_is_younger_than_the_window() {
        assert!(unreachable_timepoints(0, 1_000_000, ring(0)).is_empty());
        assert!(unreachable_timepoints(5, 1_005_000, ring(5)).is_empty());
    }

    #[test]
    fn timepoints_older_than_the_window_start_are_deleted_once() {
        // Index 100 is written at 1_100_000; `now - WINDOW` = 1_013_600 falls between 13 and
        // 14, so 13 is the newest timepoint at or before the window start and stays. The
        // previous write (99 at 1_099_000) kept from 12, so exactly 12 goes now.
        assert_eq!(unreachable_timepoints(100, 1_100_000, ring(100)), vec![12]);
        // Nothing older than 13 was kept, so the next write deletes exactly 13.
        assert_eq!(unreachable_timepoints(101, 1_101_000, ring(101)), vec![13]);
    }

    #[test]
    fn the_timepoint_before_the_last_is_always_kept() {
        // An idle pool: one timepoint at 0, the next two days later. The window start is at or
        // after the previous timepoint, but `last - 1` must stay for `prevTick`.
        let sparse = |index: u16| match index {
            0 => Some(1_000_000),
            1 => Some(1_000_000 + 2 * WINDOW),
            2 => Some(1_000_000 + 4 * WINDOW),
            _ => None,
        };
        assert!(unreachable_timepoints(1, 1_000_000 + 2 * WINDOW, sparse).is_empty());
        assert_eq!(unreachable_timepoints(2, 1_000_000 + 4 * WINDOW, sparse), vec![0]);
    }

    #[test]
    fn a_wrapped_ring_starts_after_the_newest_index() {
        // The ring has wrapped: every slot is written, index 10 was just overwritten, so the
        // oldest entry is 11. Timestamps grow along the ring from 11 around to 10.
        let base = 2_000_000u32;
        let wrapped = move |index: u16| {
            let position = (u32::from(index) + RING_SIZE - 11) % RING_SIZE;
            Some(base + 10 * position)
        };
        let new_timestamp = wrapped(10).unwrap();
        let deleted = unreachable_timepoints(10, new_timestamp, wrapped);
        // Each write ages one more 10-second timepoint past the window: exactly one deletion,
        // and it is a ring index far behind 10, never one between 11 and the window start.
        assert_eq!(deleted.len(), 1);
        let position = (u32::from(deleted[0]) + RING_SIZE - 11) % RING_SIZE;
        assert_eq!(base + 10 * position, new_timestamp - WINDOW - 10);
    }
}
