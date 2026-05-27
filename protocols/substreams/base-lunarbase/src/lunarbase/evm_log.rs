use ethabi::ethereum_types::U256;

use crate::lunarbase::{
    events::{topics, LunarBaseEvent},
    Address,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvmLog {
    pub address: Address,
    pub topics: Vec<[u8; 32]>,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogDecodeError {
    MissingTopic0,
    InvalidTopicConstant(&'static str),
    InvalidDataLength { event: &'static str, expected_words: usize, actual_bytes: usize },
    MissingIndexedTopic { event: &'static str, index: usize },
    IntegerOverflow(&'static str),
    InvalidBool(&'static str),
}

pub fn decode_lunarbase_state_log(log: &EvmLog) -> Result<Option<LunarBaseEvent>, LogDecodeError> {
    let topic0 = log
        .topics
        .first()
        .ok_or(LogDecodeError::MissingTopic0)?;

    if topic_matches(topic0, topics::STATE_UPDATED)? {
        return Ok(Some(LunarBaseEvent::StateUpdated {
            anchor_price_x96: word_u128(
                "StateUpdated.anchorPrice",
                data_word(log, "StateUpdated", 0, 3)?,
            )?,
            fee_ask_x24: word_u32("StateUpdated.feeAskX24", data_word(log, "StateUpdated", 1, 3)?)?,
            fee_bid_x24: word_u32("StateUpdated.feeBidX24", data_word(log, "StateUpdated", 2, 3)?)?,
        }));
    }

    if topic_matches(topic0, topics::SYNC)? {
        return Ok(Some(LunarBaseEvent::Sync {
            reserve_x: word_u128("Sync.reserveX", data_word(log, "Sync", 0, 2)?)?,
            reserve_y: word_u128("Sync.reserveY", data_word(log, "Sync", 1, 2)?)?,
        }));
    }

    if topic_matches(topic0, topics::BLOCK_DELAY_SET)? {
        return Ok(Some(LunarBaseEvent::BlockDelaySet {
            block_delay: word_u64(
                "BlockDelaySet.blockDelay",
                data_word(log, "BlockDelaySet", 0, 1)?,
            )?,
        }));
    }

    if topic_matches(topic0, topics::CONCENTRATION_K_SET)? {
        return Ok(Some(LunarBaseEvent::ConcentrationKSet {
            concentration_k: word_u32(
                "ConcentrationKSet.concentrationK",
                data_word(log, "ConcentrationKSet", 0, 1)?,
            )?,
        }));
    }

    if topic_matches(topic0, topics::WHITELIST_SET)? {
        return Ok(Some(LunarBaseEvent::WhitelistSet {
            account: topic_address(indexed_topic(log, "WhitelistSet", 1)?),
            whitelisted: word_bool(
                "WhitelistSet.whitelisted",
                data_word(log, "WhitelistSet", 0, 1)?,
            )?,
        }));
    }

    if topic_matches(topic0, topics::BLACKLIST_FEE_MULTIPLIER_SET)? {
        return Ok(Some(LunarBaseEvent::BlacklistFeeMultiplierSet {
            multiplier: word_u256(data_word(log, "BlacklistFeeMultiplierSet", 0, 1)?),
        }));
    }

    if topic_matches(topic0, topics::PAUSED)? {
        return Ok(Some(LunarBaseEvent::Paused));
    }

    if topic_matches(topic0, topics::UNPAUSED)? {
        return Ok(Some(LunarBaseEvent::Unpaused));
    }

    if topic_matches(topic0, topics::SWAP_EXECUTED)? {
        ensure_data_words(log, "SwapExecuted", 5)?;
        return Ok(None);
    }

    Ok(None)
}

fn data_word<'a>(
    log: &'a EvmLog,
    event: &'static str,
    index: usize,
    expected_words: usize,
) -> Result<&'a [u8], LogDecodeError> {
    ensure_data_words(log, event, expected_words)?;
    let start = index * 32;
    Ok(&log.data[start..start + 32])
}

fn ensure_data_words(
    log: &EvmLog,
    event: &'static str,
    expected_words: usize,
) -> Result<(), LogDecodeError> {
    if log.data.len() != expected_words * 32 {
        return Err(LogDecodeError::InvalidDataLength {
            event,
            expected_words,
            actual_bytes: log.data.len(),
        });
    }
    Ok(())
}

fn indexed_topic<'a>(
    log: &'a EvmLog,
    event: &'static str,
    index: usize,
) -> Result<&'a [u8; 32], LogDecodeError> {
    log.topics
        .get(index)
        .ok_or(LogDecodeError::MissingIndexedTopic { event, index })
}

fn topic_matches(topic: &[u8; 32], hex: &'static str) -> Result<bool, LogDecodeError> {
    Ok(*topic == parse_topic(hex)?)
}

fn parse_topic(hex: &'static str) -> Result<[u8; 32], LogDecodeError> {
    let bytes = hex
        .strip_prefix("0x")
        .unwrap_or(hex)
        .as_bytes();
    if bytes.len() != 64 {
        return Err(LogDecodeError::InvalidTopicConstant(hex));
    }

    let mut out = [0u8; 32];
    for (idx, chunk) in bytes.chunks_exact(2).enumerate() {
        out[idx] = (hex_nibble(chunk[0], hex)? << 4) | hex_nibble(chunk[1], hex)?;
    }
    Ok(out)
}

fn hex_nibble(byte: u8, constant: &'static str) -> Result<u8, LogDecodeError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(LogDecodeError::InvalidTopicConstant(constant)),
    }
}

fn topic_address(topic: &[u8; 32]) -> Address {
    let mut out = [0u8; 20];
    out.copy_from_slice(&topic[12..32]);
    out
}

fn word_bool(name: &'static str, word: &[u8]) -> Result<bool, LogDecodeError> {
    match word_u8(name, word)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(LogDecodeError::InvalidBool(name)),
    }
}

fn word_u8(name: &'static str, word: &[u8]) -> Result<u8, LogDecodeError> {
    if word[..31].iter().any(|byte| *byte != 0) {
        return Err(LogDecodeError::IntegerOverflow(name));
    }
    Ok(word[31])
}

fn word_u32(name: &'static str, word: &[u8]) -> Result<u32, LogDecodeError> {
    if word[..28].iter().any(|byte| *byte != 0) {
        return Err(LogDecodeError::IntegerOverflow(name));
    }
    let mut out = [0u8; 4];
    out.copy_from_slice(&word[28..32]);
    Ok(u32::from_be_bytes(out))
}

fn word_u64(name: &'static str, word: &[u8]) -> Result<u64, LogDecodeError> {
    if word[..24].iter().any(|byte| *byte != 0) {
        return Err(LogDecodeError::IntegerOverflow(name));
    }
    let mut out = [0u8; 8];
    out.copy_from_slice(&word[24..32]);
    Ok(u64::from_be_bytes(out))
}

fn word_u128(name: &'static str, word: &[u8]) -> Result<u128, LogDecodeError> {
    if word[..16].iter().any(|byte| *byte != 0) {
        return Err(LogDecodeError::IntegerOverflow(name));
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&word[16..32]);
    Ok(u128::from_be_bytes(out))
}

fn word_u256(word: &[u8]) -> U256 {
    U256::from_big_endian(word)
}
