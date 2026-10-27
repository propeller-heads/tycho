// Copyright (c) 2026 Everlong Labs Limited
//! The events the aggregators emit beside the words they write, decoded by the tests only: the
//! package derives every feed attribute from storage words (`feeds::feed_state`), and the tests
//! check that decoding against the event fields of real rounds, which the aggregators compute
//! from the same values (`OCR2Aggregator._report`, `DualAggregator.sol:911-965`,
//! `OptimismSequencerUptimeFeed._recordRound` / `_updateRound`).
use anyhow::{anyhow, bail, Result};
use substreams::hex;
use substreams_ethereum::pb::eth::v2::Log;

use crate::flamm::keys::Word;

/// `NewTransmission(uint32 indexed aggregatorRoundId, int192 answer, address transmitter, uint32
/// observationsTimestamp, int192[] observations, bytes observers, int192 juelsPerFeeCoin, bytes32
/// configDigest, uint40 epochAndRound)` (`OCR2Aggregator._report`, `DualAggregator.sol:946`).
pub const NEW_TRANSMISSION_TOPIC: [u8; 32] =
    hex!("c797025feeeaf2cd924c99e9205acb8ec04d5cad21c41ce637a38fb6dee6016a");
/// `AnswerUpdated(int256 indexed current, uint256 indexed roundId, uint256 updatedAt)`.
pub const ANSWER_UPDATED_TOPIC: [u8; 32] =
    hex!("0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f");
/// `NewRound(uint256 indexed roundId, address indexed startedBy, uint256 startedAt)`.
pub const NEW_ROUND_TOPIC: [u8; 32] =
    hex!("0109fc6f55cf40689f02fbaad7af7fe7bbac8a3d2186600afc7d3e10cac60271");
/// `SecondaryRoundIdUpdated(uint32 indexed secondaryRoundId)` (`DualAggregator.sol:459`).
pub const SECONDARY_ROUND_TOPIC: [u8; 32] =
    hex!("8d530b9ddc4b318d28fdd4c3a21fcfecece54c1a72a824f262985b99afef009b");
/// `CutoffTimeSet(uint32 cutoffTime)` (`DualAggregator.sol:467`).
pub const CUTOFF_TIME_SET_TOPIC: [u8; 32] =
    hex!("b24a681ce3399a408a89fd0c2b59dfc24bdad592b1c7ec7671cf060596c1c4d1");
/// `RoundUpdated(int256 status, uint64 updatedAt)` (`OptimismSequencerUptimeFeed._updateRound`).
pub const ROUND_UPDATED_TOPIC: [u8; 32] =
    hex!("297642343ed2faefb1a411b39fc449eae700e54223d5d0499a9421eb6f68f66a");

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewTransmission {
    pub round: u32,
    pub answer: Word,
    pub observations_ts: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerUpdated {
    pub current: Word,
    pub round: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundUpdated {
    pub status: Word,
    pub updated_at: u64,
}

fn topic(log: &Log, i: usize) -> Option<&[u8]> {
    log.topics.get(i).map(|t| t.as_slice())
}

fn data_word(data: &[u8], i: usize) -> Result<Word> {
    data.get(32 * i..32 * (i + 1))
        .ok_or_else(|| anyhow!("log data has no word {i}"))?
        .try_into()
        .map_err(|_| anyhow!("word {i}"))
}

fn u64_word(w: &Word) -> Result<u64> {
    if !w[..24].iter().all(|b| *b == 0) {
        bail!("word does not fit in u64");
    }
    Ok(u64::from_be_bytes(w[24..].try_into().expect("8 bytes")))
}

pub fn decode_new_transmission(log: &Log) -> Result<NewTransmission> {
    if topic(log, 0) != Some(&NEW_TRANSMISSION_TOPIC) {
        bail!("not a NewTransmission log");
    }
    let round = topic(log, 1).ok_or_else(|| anyhow!("NewTransmission without round topic"))?;
    let round: Word = round
        .try_into()
        .map_err(|_| anyhow!("topic"))?;
    let observations_ts = u64_word(&data_word(&log.data, 2)?)?;
    if observations_ts > u32::MAX as u64 {
        bail!("observationsTimestamp does not fit in u32");
    }
    Ok(NewTransmission {
        round: u64_word(&round)? as u32,
        answer: data_word(&log.data, 0)?,
        observations_ts: observations_ts as u32,
    })
}

pub fn decode_answer_updated(log: &Log) -> Result<AnswerUpdated> {
    if topic(log, 0) != Some(&ANSWER_UPDATED_TOPIC) {
        bail!("not an AnswerUpdated log");
    }
    let current: Word = topic(log, 1)
        .ok_or_else(|| anyhow!("AnswerUpdated without current"))?
        .try_into()
        .map_err(|_| anyhow!("topic"))?;
    let round: Word = topic(log, 2)
        .ok_or_else(|| anyhow!("AnswerUpdated without roundId"))?
        .try_into()
        .map_err(|_| anyhow!("topic"))?;
    Ok(AnswerUpdated {
        current,
        round: u64_word(&round)?,
        updated_at: u64_word(&data_word(&log.data, 0)?)?,
    })
}

pub fn decode_secondary_round(log: &Log) -> Result<u32> {
    if topic(log, 0) != Some(&SECONDARY_ROUND_TOPIC) {
        bail!("not a SecondaryRoundIdUpdated log");
    }
    let round: Word = topic(log, 1)
        .ok_or_else(|| anyhow!("SecondaryRoundIdUpdated without round"))?
        .try_into()
        .map_err(|_| anyhow!("topic"))?;
    Ok(u64_word(&round)? as u32)
}

pub fn decode_cutoff_time_set(log: &Log) -> Result<u32> {
    if topic(log, 0) != Some(&CUTOFF_TIME_SET_TOPIC) {
        bail!("not a CutoffTimeSet log");
    }
    Ok(u64_word(&data_word(&log.data, 0)?)? as u32)
}

pub fn decode_round_updated(log: &Log) -> Result<RoundUpdated> {
    if topic(log, 0) != Some(&ROUND_UPDATED_TOPIC) {
        bail!("not a RoundUpdated log");
    }
    Ok(RoundUpdated {
        status: data_word(&log.data, 0)?,
        updated_at: u64_word(&data_word(&log.data, 1)?)?,
    })
}

/// The logs of one aggregator round decoded into the values the words carry: `(round, answer,
/// startedAt/observationsTimestamp, updatedAt/recordedTimestamp)` from `NewTransmission` +
/// `AnswerUpdated` (OCR2 and DualAggregator), or from `AnswerUpdated` alone (the uptime feed's
/// `_recordRound`: `startedAt` is the event's timestamp, `updatedAt` the block's).
pub fn round_from_logs(logs: &[Log], block_ts: u64) -> Option<(u64, Word, u64, u64)> {
    let nt = logs
        .iter()
        .find_map(|l| decode_new_transmission(l).ok());
    let au = logs
        .iter()
        .find_map(|l| decode_answer_updated(l).ok())?;
    match nt {
        Some(nt) => {
            assert_eq!(nt.round as u64, au.round);
            assert_eq!(nt.answer, au.current);
            Some((au.round, nt.answer, nt.observations_ts as u64, au.updated_at))
        }
        None => Some((au.round, au.current, au.updated_at, block_ts)),
    }
}
