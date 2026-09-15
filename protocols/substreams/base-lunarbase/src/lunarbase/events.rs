use std::fmt;

use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::prelude as tycho;

use crate::{
    abi::pool::events as pool_events,
    lunarbase::{
        state::{attribute, attrs},
        Address,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LunarBaseEvent {
    StateUpdated { anchor_price_x96: [u8; 20], fee_ask_x24: u32, fee_bid_x24: u32 },
    Sync { reserve_x: u128, reserve_y: u128 },
    BlockDelaySet { block_delay: u64 },
    MaxPunishmentX24Set { max_punishment_x24: u32 },
    PunishmentApplied { fee_ask_x24: u32, fee_bid_x24: u32 },
    BlacklistFeeMultiplierSet { multiplier: [u8; 32] },
    QuoteCallerWhitelistSet { whitelisted: bool },
    Paused,
    Unpaused,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogDecodeError {
    Decode { event: &'static str, message: String },
    IntegerOverflow(&'static str),
}

impl fmt::Display for LogDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode { event, message } => write!(f, "{event}: {message}"),
            Self::IntegerOverflow(field) => write!(f, "{field} exceeds its ABI integer width"),
        }
    }
}

impl std::error::Error for LogDecodeError {}

pub fn decode_lunarbase_state_log(
    log: &eth::v2::Log,
    quote_caller: &Address,
) -> Result<Option<LunarBaseEvent>, LogDecodeError> {
    if log.topics.is_empty() {
        return Ok(None);
    }

    if matches_event::<pool_events::StateUpdated>(log, &topics::STATE_UPDATED)? {
        let event = pool_events::StateUpdated::decode(log)
            .map_err(|message| LogDecodeError::Decode { event: "StateUpdated", message })?;
        return Ok(Some(LunarBaseEvent::StateUpdated {
            anchor_price_x96: bigint_to_u160("StateUpdated.anchorPrice", &event.anchor_price)?,
            fee_ask_x24: bigint_to_u24("StateUpdated.feeAskX24", &event.fee_ask_x24)?,
            fee_bid_x24: bigint_to_u24("StateUpdated.feeBidX24", &event.fee_bid_x24)?,
        }));
    }

    if matches_event::<pool_events::Sync>(log, &topics::SYNC)? {
        let event = pool_events::Sync::decode(log)
            .map_err(|message| LogDecodeError::Decode { event: "Sync", message })?;
        return Ok(Some(LunarBaseEvent::Sync {
            reserve_x: bigint_to_u128("Sync.reserveX", &event.reserve_x)?,
            reserve_y: bigint_to_u128("Sync.reserveY", &event.reserve_y)?,
        }));
    }

    if matches_event::<pool_events::BlockDelaySet>(log, &topics::BLOCK_DELAY_SET)? {
        let event = pool_events::BlockDelaySet::decode(log)
            .map_err(|message| LogDecodeError::Decode { event: "BlockDelaySet", message })?;
        return Ok(Some(LunarBaseEvent::BlockDelaySet {
            block_delay: bigint_to_u48("BlockDelaySet.blockDelay", &event.block_delay)?,
        }));
    }

    if matches_event::<pool_events::MaxPunishmentX24Set>(log, &topics::MAX_PUNISHMENT_X24_SET)? {
        let event = pool_events::MaxPunishmentX24Set::decode(log)
            .map_err(|message| LogDecodeError::Decode { event: "MaxPunishmentX24Set", message })?;
        return Ok(Some(LunarBaseEvent::MaxPunishmentX24Set {
            max_punishment_x24: bigint_to_u24(
                "MaxPunishmentX24Set.maxPunishmentX24",
                &event.max_punishment_x24,
            )?,
        }));
    }

    if matches_event::<pool_events::PunishmentApplied>(log, &topics::PUNISHMENT_APPLIED)? {
        let event = pool_events::PunishmentApplied::decode(log)
            .map_err(|message| LogDecodeError::Decode { event: "PunishmentApplied", message })?;
        bigint_to_u24("PunishmentApplied.punishmentX24", &event.punishment_x24)?;
        return Ok(Some(LunarBaseEvent::PunishmentApplied {
            fee_ask_x24: bigint_to_u24("PunishmentApplied.feeAskX24", &event.fee_ask_x24)?,
            fee_bid_x24: bigint_to_u24("PunishmentApplied.feeBidX24", &event.fee_bid_x24)?,
        }));
    }

    if matches_event::<pool_events::BlacklistFeeMultiplierSet>(
        log,
        &topics::BLACKLIST_FEE_MULTIPLIER_SET,
    )? {
        let event = pool_events::BlacklistFeeMultiplierSet::decode(log).map_err(|message| {
            LogDecodeError::Decode { event: "BlacklistFeeMultiplierSet", message }
        })?;
        return Ok(Some(LunarBaseEvent::BlacklistFeeMultiplierSet {
            multiplier: bigint_to_fixed_bytes(
                "BlacklistFeeMultiplierSet.multiplier",
                &event.multiplier,
            )?,
        }));
    }

    if matches_event::<pool_events::WhitelistSet>(log, &topics::WHITELIST_SET)? {
        let event = pool_events::WhitelistSet::decode(log)
            .map_err(|message| LogDecodeError::Decode { event: "WhitelistSet", message })?;
        if event.account.as_slice() != quote_caller {
            return Ok(None);
        }
        return Ok(Some(LunarBaseEvent::QuoteCallerWhitelistSet { whitelisted: event.whitelisted }));
    }

    if matches_event::<pool_events::Paused>(log, &topics::PAUSED)? {
        pool_events::Paused::decode(log)
            .map_err(|message| LogDecodeError::Decode { event: "Paused", message })?;
        return Ok(Some(LunarBaseEvent::Paused));
    }

    if matches_event::<pool_events::Unpaused>(log, &topics::UNPAUSED)? {
        pool_events::Unpaused::decode(log)
            .map_err(|message| LogDecodeError::Decode { event: "Unpaused", message })?;
        return Ok(Some(LunarBaseEvent::Unpaused));
    }

    Ok(None)
}

pub fn event_attributes(event: &LunarBaseEvent, block_number: u64) -> Vec<tycho::Attribute> {
    match event {
        LunarBaseEvent::StateUpdated { anchor_price_x96, fee_ask_x24, fee_bid_x24 } => vec![
            attribute(attrs::ANCHOR_PRICE_X96, anchor_price_x96.to_vec()),
            attribute(attrs::FEE_ASK_X24, fee_ask_x24.to_be_bytes().to_vec()),
            attribute(attrs::FEE_BID_X24, fee_bid_x24.to_be_bytes().to_vec()),
            attribute(attrs::LATEST_UPDATE_BLOCK, block_number.to_be_bytes().to_vec()),
        ],
        LunarBaseEvent::Sync { reserve_x, reserve_y } => vec![
            attribute(attrs::RESERVE_X, reserve_x.to_be_bytes().to_vec()),
            attribute(attrs::RESERVE_Y, reserve_y.to_be_bytes().to_vec()),
        ],
        LunarBaseEvent::BlockDelaySet { block_delay } => {
            vec![attribute(attrs::BLOCK_DELAY, block_delay.to_be_bytes().to_vec())]
        }
        LunarBaseEvent::MaxPunishmentX24Set { max_punishment_x24 } => {
            vec![attribute(
                attrs::MAX_PUNISHMENT_X24,
                max_punishment_x24
                    .to_be_bytes()
                    .to_vec(),
            )]
        }
        // Swaps increase directional fees but do not refresh the operator's price.
        LunarBaseEvent::PunishmentApplied { fee_ask_x24, fee_bid_x24 } => vec![
            attribute(attrs::FEE_ASK_X24, fee_ask_x24.to_be_bytes().to_vec()),
            attribute(attrs::FEE_BID_X24, fee_bid_x24.to_be_bytes().to_vec()),
        ],
        LunarBaseEvent::BlacklistFeeMultiplierSet { multiplier } => {
            vec![attribute(attrs::BLACKLIST_FEE_MULTIPLIER, multiplier.to_vec())]
        }
        LunarBaseEvent::QuoteCallerWhitelistSet { whitelisted } => {
            vec![attribute(attrs::QUOTE_CALLER_WHITELISTED, vec![u8::from(*whitelisted)])]
        }
        LunarBaseEvent::Paused => vec![attribute(attrs::PAUSED, vec![1u8])],
        LunarBaseEvent::Unpaused => vec![attribute(attrs::PAUSED, vec![0u8])],
    }
}

fn matches_event<E: Event>(log: &eth::v2::Log, topic: &[u8; 32]) -> Result<bool, LogDecodeError> {
    if log.topics.first().map(Vec::as_slice) != Some(topic.as_slice()) {
        return Ok(false);
    }
    if !E::match_log(log) {
        return Err(LogDecodeError::Decode {
            event: E::NAME,
            message: format!(
                "invalid event shape: {} topics, {} data bytes",
                log.topics.len(),
                log.data.len()
            ),
        });
    }
    Ok(true)
}

fn bigint_to_u24(
    name: &'static str,
    value: &substreams::scalar::BigInt,
) -> Result<u32, LogDecodeError> {
    value
        .to_string()
        .parse::<u32>()
        .ok()
        .filter(|value| *value <= 0x00ff_ffff)
        .ok_or(LogDecodeError::IntegerOverflow(name))
}

fn bigint_to_u48(
    name: &'static str,
    value: &substreams::scalar::BigInt,
) -> Result<u64, LogDecodeError> {
    value
        .to_string()
        .parse::<u64>()
        .ok()
        .filter(|value| *value <= 0x0000_ffff_ffff_ffff)
        .ok_or(LogDecodeError::IntegerOverflow(name))
}

fn bigint_to_u160(
    name: &'static str,
    value: &substreams::scalar::BigInt,
) -> Result<[u8; 20], LogDecodeError> {
    bigint_to_fixed_bytes(name, value)
}

fn bigint_to_fixed_bytes<const N: usize>(
    name: &'static str,
    value: &substreams::scalar::BigInt,
) -> Result<[u8; N], LogDecodeError> {
    let value = value
        .to_string()
        .parse::<num_bigint::BigUint>()
        .map_err(|_| LogDecodeError::IntegerOverflow(name))?;
    let bytes = value.to_bytes_be();
    if bytes.len() > N {
        return Err(LogDecodeError::IntegerOverflow(name));
    }
    let mut result = [0; N];
    result[N - bytes.len()..].copy_from_slice(&bytes);
    Ok(result)
}

fn bigint_to_u128(
    name: &'static str,
    value: &substreams::scalar::BigInt,
) -> Result<u128, LogDecodeError> {
    value
        .to_string()
        .parse()
        .map_err(|_| LogDecodeError::IntegerOverflow(name))
}

#[cfg(test)]
mod tests {
    use ethabi::{ethereum_types::U256, ParamType, Token};

    use super::*;
    use crate::lunarbase::indexed;

    const QUOTE_CALLER: Address = [0x42; 20];

    fn decode_lunarbase_state_log(
        log: &eth::v2::Log,
    ) -> Result<Option<LunarBaseEvent>, LogDecodeError> {
        super::decode_lunarbase_state_log(log, &QUOTE_CALLER)
    }

    fn event_log(
        name: &str,
        types: &[ParamType],
        indexed: &[Token],
        data: &[Token],
    ) -> eth::v2::Log {
        let mut topics = vec![ethabi::long_signature(name, types)
            .as_bytes()
            .to_vec()];
        topics.extend(
            indexed
                .iter()
                .map(|token| ethabi::encode(&[token.clone()])),
        );
        eth::v2::Log { topics, data: ethabi::encode(data), ..Default::default() }
    }

    fn state_log(anchor: U256, ask: u32, bid: u32) -> eth::v2::Log {
        event_log(
            "StateUpdated",
            &[ParamType::Uint(160), ParamType::Uint(24), ParamType::Uint(24)],
            &[],
            &[Token::Uint(anchor), Token::Uint(ask.into()), Token::Uint(bid.into())],
        )
    }

    fn punishment_log(x_to_y: bool, punishment: u32, ask: u32, bid: u32) -> eth::v2::Log {
        event_log(
            "PunishmentApplied",
            &[ParamType::Bool, ParamType::Uint(24), ParamType::Uint(24), ParamType::Uint(24)],
            &[Token::Bool(x_to_y)],
            &[Token::Uint(punishment.into()), Token::Uint(ask.into()), Token::Uint(bid.into())],
        )
    }

    #[test]
    fn caller_multiplier_preserves_all_uint256_bits() {
        for multiplier in [U256::one(), U256::from(100), U256::MAX] {
            let log = event_log(
                "BlacklistFeeMultiplierSet",
                &[ParamType::Uint(256)],
                &[],
                &[Token::Uint(multiplier)],
            );
            let event = decode_lunarbase_state_log(&log)
                .unwrap()
                .unwrap();
            let attributes = event_attributes(&event, 100);
            assert_eq!(attributes.len(), 1);
            assert_eq!(attributes[0].name, attrs::BLACKLIST_FEE_MULTIPLIER);
            assert_eq!(attributes[0].value.len(), 32);
            assert_eq!(U256::from_big_endian(&attributes[0].value), multiplier);
        }
    }

    #[test]
    fn whitelist_updates_only_affect_the_configured_quote_caller() {
        for whitelisted in [true, false] {
            for account in [QUOTE_CALLER, [0x99; 20]] {
                let log = event_log(
                    "WhitelistSet",
                    &[ParamType::Address, ParamType::Bool],
                    &[Token::Address(account.into())],
                    &[Token::Bool(whitelisted)],
                );
                let decoded = decode_lunarbase_state_log(&log).unwrap();
                if account == QUOTE_CALLER {
                    let attributes = event_attributes(&decoded.unwrap(), 100);
                    assert_eq!(attributes.len(), 1);
                    assert_eq!(attributes[0].name, attrs::QUOTE_CALLER_WHITELISTED);
                    assert_eq!(attributes[0].value, vec![u8::from(whitelisted)]);
                } else {
                    assert_eq!(decoded, None);
                }
            }
        }
    }

    #[test]
    fn malformed_caller_fee_events_fail_decoding() {
        let mut multiplier = event_log(
            "BlacklistFeeMultiplierSet",
            &[ParamType::Uint(256)],
            &[],
            &[Token::Uint(100.into())],
        );
        multiplier.data.pop();
        assert!(matches!(
            decode_lunarbase_state_log(&multiplier),
            Err(LogDecodeError::Decode { event: "BlacklistFeeMultiplierSet", .. })
        ));
        let mut whitelist = event_log(
            "WhitelistSet",
            &[ParamType::Address, ParamType::Bool],
            &[Token::Address(QUOTE_CALLER.into())],
            &[Token::Bool(true)],
        );
        whitelist.topics.pop();
        assert!(matches!(
            decode_lunarbase_state_log(&whitelist),
            Err(LogDecodeError::Decode { event: "WhitelistSet", .. })
        ));
    }

    #[test]
    fn state_updated_preserves_the_full_uint160_price() {
        for anchor in [U256::zero(), U256::one(), U256::one() << 128, (U256::one() << 160) - 1] {
            let log = state_log(anchor, 1, 0x00ff_ffff);
            let event = decode_lunarbase_state_log(&log)
                .unwrap()
                .unwrap();
            let attributes = event_attributes(&event, 42);
            let price = &attributes
                .iter()
                .find(|attr| attr.name == attrs::ANCHOR_PRICE_X96)
                .unwrap()
                .value;
            assert_eq!(price.len(), 20);
            assert_eq!(U256::from_big_endian(price), anchor);
        }
    }

    #[test]
    fn integers_outside_the_abi_width_fail_decoding() {
        assert_eq!(
            decode_lunarbase_state_log(&state_log(U256::one() << 160, 1, 2)),
            Err(LogDecodeError::IntegerOverflow("StateUpdated.anchorPrice")),
        );
        assert_eq!(
            decode_lunarbase_state_log(&state_log(U256::one(), 1 << 24, 2)),
            Err(LogDecodeError::IntegerOverflow("StateUpdated.feeAskX24")),
        );
    }

    #[test]
    fn punishment_updates_both_absolute_fees_without_refreshing_the_price() {
        for x_to_y in [true, false] {
            let event = decode_lunarbase_state_log(&punishment_log(x_to_y, 5, 20, 30))
                .unwrap()
                .unwrap();
            assert_eq!(
                event,
                LunarBaseEvent::PunishmentApplied { fee_ask_x24: 20, fee_bid_x24: 30 }
            );
            let attributes = event_attributes(&event, 123);
            assert_eq!(attributes.len(), 2);
            assert!(attributes
                .iter()
                .all(|attr| attr.name != attrs::LATEST_UPDATE_BLOCK));
            assert_eq!(attributes[0].value, 20u32.to_be_bytes());
            assert_eq!(attributes[1].value, 30u32.to_be_bytes());
        }
    }

    #[test]
    fn max_punishment_preserves_disabled_and_full_fee_sentinel_values() {
        for cap in [0u32, 0x00ff_ffff] {
            let log = event_log(
                "MaxPunishmentX24Set",
                &[ParamType::Uint(24)],
                &[],
                &[Token::Uint(cap.into())],
            );
            let event = decode_lunarbase_state_log(&log)
                .unwrap()
                .unwrap();
            assert_eq!(event, LunarBaseEvent::MaxPunishmentX24Set { max_punishment_x24: cap });
            let attributes = event_attributes(&event, 42);
            assert_eq!(attributes[0].name, attrs::MAX_PUNISHMENT_X24);
            assert_eq!(attributes[0].value, cap.to_be_bytes());
        }
    }

    #[test]
    fn malformed_recognized_events_fail_instead_of_being_ignored() {
        let mut truncated_state = state_log(U256::one(), 1, 2);
        truncated_state.data.pop();
        assert!(matches!(
            decode_lunarbase_state_log(&truncated_state),
            Err(LogDecodeError::Decode { event: "StateUpdated", .. })
        ));

        let mut missing_direction = punishment_log(true, 1, 2, 3);
        missing_direction.topics.pop();
        assert!(matches!(
            decode_lunarbase_state_log(&missing_direction),
            Err(LogDecodeError::Decode { event: "PunishmentApplied", .. })
        ));

        let mut paused =
            event_log("Paused", &[ParamType::Address], &[], &[Token::Address(Default::default())]);
        paused.data.clear();
        assert!(matches!(
            decode_lunarbase_state_log(&paused),
            Err(LogDecodeError::Decode { event: "Paused", .. })
        ));
        assert_eq!(decode_lunarbase_state_log(&eth::v2::Log::default()), Ok(None));
        assert_eq!(
            decode_lunarbase_state_log(&eth::v2::Log {
                topics: vec![vec![0; 32]],
                ..Default::default()
            }),
            Ok(None)
        );
    }

    #[test]
    fn unchanged_events_still_update_reserves_staleness_and_pause() {
        let fixtures = [
            (
                event_log(
                    "Sync",
                    &[ParamType::Uint(128), ParamType::Uint(128)],
                    &[],
                    &[Token::Uint(100.into()), Token::Uint(200.into())],
                ),
                LunarBaseEvent::Sync { reserve_x: 100, reserve_y: 200 },
            ),
            (
                event_log("BlockDelaySet", &[ParamType::Uint(48)], &[], &[Token::Uint(7.into())]),
                LunarBaseEvent::BlockDelaySet { block_delay: 7 },
            ),
            (
                event_log(
                    "Paused",
                    &[ParamType::Address],
                    &[],
                    &[Token::Address(Default::default())],
                ),
                LunarBaseEvent::Paused,
            ),
            (
                event_log(
                    "Unpaused",
                    &[ParamType::Address],
                    &[],
                    &[Token::Address(Default::default())],
                ),
                LunarBaseEvent::Unpaused,
            ),
        ];
        for (log, expected) in fixtures {
            assert_eq!(decode_lunarbase_state_log(&log), Ok(Some(expected)));
        }
    }

    #[test]
    fn transaction_event_order_preserves_operator_resets_and_later_punishment() {
        let mut builder = tycho::TransactionChangesBuilder::new(&tycho::Transaction::default());
        builder.add_entity_change(&indexed::initial_entity_change("0xpool"));
        let logs = [
            state_log(U256::one() << 96, 10, 20),
            punishment_log(true, 5, 10, 25),
            punishment_log(false, 2, 12, 25),
            state_log(U256::one() << 129, 1, 2),
            punishment_log(true, 4, 1, 6),
        ];
        for log in logs {
            let event = decode_lunarbase_state_log(&log)
                .unwrap()
                .unwrap();
            builder.add_entity_change(&indexed::entity_change_for_event("0xpool", &event, 100));
        }
        let changes = builder.build().unwrap();
        assert_eq!(changes.entity_changes.len(), 1);
        let attributes = &changes.entity_changes[0].attributes;
        let value = |name: &str| {
            &attributes
                .iter()
                .find(|attr| attr.name == name)
                .unwrap()
                .value
        };
        assert_eq!(*value(attrs::FEE_ASK_X24), 1u32.to_be_bytes());
        assert_eq!(*value(attrs::FEE_BID_X24), 6u32.to_be_bytes());
        assert_eq!(*value(attrs::LATEST_UPDATE_BLOCK), 100u64.to_be_bytes());
        assert_eq!(U256::from_big_endian(value(attrs::ANCHOR_PRICE_X96)), U256::one() << 129);
    }
}

mod topics {
    // BlacklistFeeMultiplierSet(uint256)
    pub const BLACKLIST_FEE_MULTIPLIER_SET: [u8; 32] = [
        0xa1, 0x50, 0x57, 0x88, 0x6e, 0x6e, 0xbc, 0xdf, 0x47, 0x29, 0x4b, 0xcb, 0x09, 0x1d, 0x68,
        0x60, 0x31, 0x12, 0x4d, 0x10, 0x41, 0xca, 0xfe, 0x00, 0x74, 0x0e, 0x93, 0x66, 0x7b, 0xac,
        0xd1, 0x86,
    ];
    // WhitelistSet(address,bool)
    pub const WHITELIST_SET: [u8; 32] = [
        0x0a, 0xa5, 0xec, 0x5f, 0xfd, 0xc7, 0xf6, 0xf9, 0xc4, 0xd0, 0xdd, 0xed, 0x48, 0x9d, 0x74,
        0x50, 0x29, 0x71, 0x55, 0xcb, 0x2f, 0x71, 0xcb, 0x77, 0x1e, 0x02, 0x42, 0x7f, 0x7d, 0xff,
        0x4f, 0x51,
    ];
    // Keccak-256 of the canonical event signatures. Match the signature before validating the
    // payload so malformed state events cannot be silently treated as unrelated events.
    // StateUpdated(uint160,uint24,uint24)
    pub const STATE_UPDATED: [u8; 32] = [
        0x8a, 0xcb, 0x81, 0x1d, 0x2c, 0x51, 0x06, 0x78, 0x5f, 0x84, 0x7f, 0xaf, 0x03, 0xce, 0x16,
        0x0d, 0x2e, 0xb1, 0x24, 0xb8, 0x63, 0x2e, 0xb4, 0x2d, 0x46, 0x6f, 0x46, 0xc0, 0x87, 0x03,
        0x3d, 0x61,
    ];
    // Sync(uint128,uint128)
    pub const SYNC: [u8; 32] = [
        0x99, 0xe9, 0x3f, 0xd9, 0x4a, 0x51, 0xb8, 0x0d, 0x7d, 0xd7, 0xec, 0x3f, 0x69, 0xc4, 0xf0,
        0x9a, 0x43, 0xe7, 0x52, 0x3f, 0x5a, 0x45, 0xca, 0x09, 0xb8, 0x8a, 0x17, 0x8d, 0x9d, 0xaa,
        0xed, 0x1e,
    ];
    // BlockDelaySet(uint48)
    pub const BLOCK_DELAY_SET: [u8; 32] = [
        0x67, 0x3f, 0x92, 0x80, 0x46, 0x7e, 0xf1, 0xd6, 0x77, 0xed, 0xd6, 0xa2, 0x16, 0x30, 0xcf,
        0x32, 0x80, 0x68, 0xa1, 0xdc, 0x8d, 0xa6, 0x42, 0x05, 0xc1, 0xbc, 0x79, 0x85, 0x5c, 0x6b,
        0x23, 0x07,
    ];
    // MaxPunishmentX24Set(uint24)
    pub const MAX_PUNISHMENT_X24_SET: [u8; 32] = [
        0xe0, 0x2e, 0xc3, 0x45, 0x87, 0x0a, 0x39, 0x71, 0xf9, 0x2a, 0x7f, 0x69, 0xac, 0xa9, 0xe0,
        0xf2, 0xd6, 0xe5, 0x56, 0x7a, 0x73, 0x8e, 0x25, 0x78, 0xd5, 0xae, 0x70, 0x60, 0x05, 0x64,
        0x55, 0xcd,
    ];
    // PunishmentApplied(bool,uint24,uint24,uint24)
    pub const PUNISHMENT_APPLIED: [u8; 32] = [
        0x54, 0xd2, 0x92, 0x1c, 0x59, 0xb3, 0x6f, 0xc4, 0xf1, 0x6e, 0xba, 0x1d, 0x38, 0x01, 0x5e,
        0x1b, 0x5e, 0xf8, 0x07, 0x1f, 0x7e, 0xaf, 0xf2, 0xb7, 0xa4, 0x0f, 0x27, 0x63, 0x2e, 0xa3,
        0x88, 0x33,
    ];
    // Paused(address)
    pub const PAUSED: [u8; 32] = [
        0x62, 0xe7, 0x8c, 0xea, 0x01, 0xbe, 0xe3, 0x20, 0xcd, 0x4e, 0x42, 0x02, 0x70, 0xb5, 0xea,
        0x74, 0x00, 0x0d, 0x11, 0xb0, 0xc9, 0xf7, 0x47, 0x54, 0xeb, 0xdb, 0xfc, 0x54, 0x4b, 0x05,
        0xa2, 0x58,
    ];
    // Unpaused(address)
    pub const UNPAUSED: [u8; 32] = [
        0x5d, 0xb9, 0xee, 0x0a, 0x49, 0x5b, 0xf2, 0xe6, 0xff, 0x9c, 0x91, 0xa7, 0x83, 0x4c, 0x1b,
        0xa4, 0xfd, 0xd2, 0x44, 0xa5, 0xe8, 0xaa, 0x4e, 0x53, 0x7b, 0xd3, 0x8a, 0xea, 0xe4, 0xb0,
        0x73, 0xaa,
    ];
}
