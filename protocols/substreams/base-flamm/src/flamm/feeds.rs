// Copyright (c) 2026 Everlong Labs Limited
//! Chainlink feeds behind the `PriceFeed` and the Morpho oracle: the proxies' rotation word, the
//! aggregators' read-access pair and the aggregator storage layouts their rounds are decoded from
//! (schema section 2.6). Every feed attribute is a function of these words; the aggregators' events
//! are not read (the tests decode them as an independent check of the layouts, `feed_events`).
use std::collections::{BTreeMap, HashMap};

use anyhow::{anyhow, Result};

use crate::flamm::keys::{
    access_list_key, address_in_word, field, slot, transmission_key, word_from_u64, Address, Word,
    PROXY_PHASE_SLOT,
};

/// The aggregator contract kinds the package can decode. A proxy that rotates to an aggregator of
/// another kind carries only `aggregator` / `phase` until a package update lists it, so the feed
/// fails closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedKind {
    /// `AccessControlledOCR2Aggregator`: `HotVars` slot 11 (`latestAggregatorRoundId @6`),
    /// `s_transmissions` base 12 (`int192 answer | uint32 observationsTimestamp @24 | uint32
    /// transmissionTimestamp @28`), `checkEnabled` slot 21 @0, `s_accessList` base 22.
    Ocr2,
    /// `OptimismSequencerUptimeFeed`: `s_feedState` slot 4 (`uint80 latestRoundId | bool
    /// latestStatus @10 | uint64 startedAt @11 | uint64 updatedAt @19`), `checkEnabled` slot 1
    /// @20, `s_accessList` base 2.
    Uptime,
    /// Chainlink SVR `DualAggregator`: `HotVars` slot 13 (`latestAggregatorRoundId @6 |
    /// latestSecondaryRoundId @10`), `s_transmissions` base 17 (`answer |
    /// observationsTimestamp @24 | recordedTimestamp @28`), `s_cutoffTime` slot 18;
    /// `latestRoundData` has no access check.
    Dual,
}

impl FeedKind {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "ocr2" => Ok(Self::Ocr2),
            "uptime" => Ok(Self::Uptime),
            "dual" => Ok(Self::Dual),
            other => Err(anyhow!("unknown aggregator kind `{other}`")),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Ocr2 => "ocr2",
            Self::Uptime => "uptime",
            Self::Dual => "dual",
        }
    }

    /// `(slot, byte offset)` of the `checkEnabled` bool, for the kinds whose `latestRoundData` is
    /// guarded.
    pub fn check_enabled(self) -> Option<(Word, usize)> {
        match self {
            Self::Ocr2 => Some((slot(21), 0)),
            Self::Uptime => Some((slot(1), 20)),
            Self::Dual => None,
        }
    }

    /// `s_accessList[proxy]` key, for the guarded kinds.
    pub fn access_list(self, proxy: &Address) -> Option<Word> {
        match self {
            Self::Ocr2 => Some(access_list_key(proxy, 22)),
            Self::Uptime => Some(access_list_key(proxy, 2)),
            Self::Dual => None,
        }
    }

    pub fn transmissions_base(self) -> Option<u64> {
        match self {
            Self::Ocr2 => Some(12),
            Self::Dual => Some(17),
            Self::Uptime => None,
        }
    }

    pub fn transmission(self, round: u32) -> Option<Word> {
        self.transmissions_base()
            .map(|base| transmission_key(round, base))
    }
}

/// `EACAggregatorProxy.currentPhase`: `uint16 id @0 | address aggregator @2`.
pub fn phase_and_aggregator(word: &Word) -> (u16, Address) {
    (field(word, 0, 2) as u16, address_in_word(&word[10..30]))
}

/// One aggregator round as `latestRoundData` reports it behind the proxy (round id without the
/// phase).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Round {
    pub round: u64,
    pub answer: Word,
    pub started_at: u64,
    pub updated_at: u64,
}

/// `Transmission { int192 answer; uint32 observationsTimestamp; uint32 recordedTimestamp }` packed
/// as the aggregator stores it: answer in the low 24 bytes, the two timestamps above it.
pub fn pack_transmission(answer: &Word, observations_ts: u32, recorded_ts: u32) -> Word {
    let mut w = [0u8; 32];
    w[..4].copy_from_slice(&recorded_ts.to_be_bytes());
    w[4..8].copy_from_slice(&observations_ts.to_be_bytes());
    w[8..].copy_from_slice(&answer[8..]);
    w
}

/// The inverse of [`pack_transmission`]: `(answer sign-extended to 256 bits, observationsTimestamp,
/// recordedTimestamp)`.
pub fn unpack_transmission(w: &Word) -> (Word, u32, u32) {
    let recorded = u32::from_be_bytes(w[..4].try_into().expect("4 bytes"));
    let observations = u32::from_be_bytes(w[4..8].try_into().expect("4 bytes"));
    let mut answer = [0u8; 32];
    answer[8..].copy_from_slice(&w[8..]);
    if w[8] & 0x80 != 0 {
        for b in answer.iter_mut().take(8) {
            *b = 0xff;
        }
    }
    (answer, observations, recorded)
}

/// The OCR2 round `latestRoundData` reports, from the `HotVars` word and a `s_transmissions`
/// lookup.
pub fn ocr2_round(hotvars: &Word, transmission: impl Fn(u32) -> Option<Word>) -> Option<Round> {
    let round = field(hotvars, 6, 4) as u32;
    if round == 0 {
        return None;
    }
    let (answer, observations, recorded) = unpack_transmission(&transmission(round)?);
    Some(Round {
        round: round as u64,
        answer,
        started_at: observations as u64,
        updated_at: recorded as u64,
    })
}

/// The sequencer round from `s_feedState` (schema 2.6.2): `answer` is the status (0 up, 1 down),
/// `started_at` the L1 timestamp of the status change, `updated_at` the last refresh.
pub fn uptime_round(feedstate: &Word) -> Option<Round> {
    let round = field(feedstate, 0, 10);
    if round == 0 {
        return None;
    }
    Some(Round {
        round: round as u64,
        answer: word_from_u64(field(feedstate, 10, 1) as u64),
        started_at: field(feedstate, 11, 8) as u64,
        updated_at: field(feedstate, 19, 8) as u64,
    })
}

/// `DualAggregator.HotVars`: `(latestAggregatorRoundId, latestSecondaryRoundId)`.
pub fn dual_hotvars(hotvars: &Word) -> (u32, u32) {
    (field(hotvars, 6, 4) as u32, field(hotvars, 10, 4) as u32)
}

/// The rounds the package keeps for a `DualAggregator`: the window `latest-DUAL_RING..=latest`,
/// plus the secondary round when it has fallen below the window. A deliberate superset of the
/// rounds a reveal can answer with, not that set itself: `_getSyncPrimaryRound`
/// (`DualAggregator.sol:530-548`) visits `latest` down to `latest-19` (it breaks when
/// `latest - round == i_maxSyncIterations = 20`), and the secondary-proxy branch answers with
/// `latestSecondaryRoundId`, so `latest-20` is carried although nothing but the secondary round
/// reaches it. The window is 21 rounds and the set is 22 while the secondary round is outside it;
/// the seeds and the schema snapshot are both taken where it is inside, and carry 21.
pub const DUAL_RING: u32 = 20;

pub fn dual_ring(latest: u32, secondary: u32) -> Vec<u32> {
    let mut rounds: Vec<u32> = (latest.saturating_sub(DUAL_RING).max(1)..=latest).collect();
    if secondary != 0 && !rounds.contains(&secondary) {
        rounds.push(secondary);
        rounds.sort_unstable();
    }
    rounds
}

/// The attributes of one feed (`feed:<role>:*`, schema 3.2) as `latestRoundData` through the
/// proxy would read them, from the words `word` answers: the proxy's phase word gives
/// `aggregator` and `phase`; an aggregator whose layout the manifest lists adds its `kind`, its
/// read-access pair (`check_enabled`, `access_list`, the guarded kinds) and its rounds (`round`,
/// `answer`, `started_at`, `updated_at` for OCR2 and the uptime feed; `round`, `secondary_round`,
/// `cutoff` and the ring `tx:<r>` for the `DualAggregator`). A word that is unknown leaves its
/// attributes absent, and an aggregator the manifest does not list contributes nothing beyond
/// `aggregator` / `phase`: absent is unknown, and the decoder refuses to quote.
///
/// Every feed attribute the package emits comes from this function, either whole (the creation
/// snapshot) or as the difference between its value before and after a transaction, so the
/// attributes the indexer holds for a feed are exactly `feed_state` of the words as of the last
/// transaction. That is what makes a `Deletion` safe: it only ever names an attribute the previous
/// state had (the indexer fails the block's write on a deletion of a row it does not hold).
pub fn feed_state(
    role: &str,
    proxy: &Address,
    aggregators: &HashMap<Address, FeedKind>,
    word: impl Fn(&Address, &Word) -> Option<Word>,
) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let name = |n: &str| format!("feed:{role}:{n}");
    let u64_bytes = |n: u64| word_from_u64(n).to_vec();
    let Some(phase_word) = word(proxy, &slot(PROXY_PHASE_SLOT)) else {
        return out;
    };
    let (phase, aggregator) = phase_and_aggregator(&phase_word);
    out.insert(name("aggregator"), aggregator.to_vec());
    out.insert(name("phase"), u64_bytes(phase as u64));
    let Some(kind) = aggregators.get(&aggregator).copied() else {
        return out;
    };
    out.insert(name("kind"), kind.name().as_bytes().to_vec());
    if let Some((slot, offset)) = kind.check_enabled() {
        if let Some(w) = word(&aggregator, &slot) {
            out.insert(name("check_enabled"), u64_bytes((field(&w, offset, 1) != 0) as u64));
        }
    }
    if let Some(key) = kind.access_list(proxy) {
        if let Some(w) = word(&aggregator, &key) {
            out.insert(name("access_list"), w.to_vec());
        }
    }
    let insert_round = |out: &mut BTreeMap<String, Vec<u8>>, round: &Round| {
        out.insert(name("round"), u64_bytes(round.round));
        out.insert(name("answer"), round.answer.to_vec());
        out.insert(name("started_at"), u64_bytes(round.started_at));
        out.insert(name("updated_at"), u64_bytes(round.updated_at));
    };
    match kind {
        FeedKind::Ocr2 => {
            // `OCR2Aggregator.latestRoundData`:
            // `s_transmissions[s_hotVars.latestAggregatorRoundId]`.
            if let Some(hotvars) = word(&aggregator, &slot(11)) {
                let round = ocr2_round(&hotvars, |r| {
                    kind.transmission(r)
                        .and_then(|k| word(&aggregator, &k))
                });
                if let Some(round) = round {
                    insert_round(&mut out, &round);
                }
            }
        }
        FeedKind::Uptime => {
            // `OptimismSequencerUptimeFeed.latestRoundData`: `s_feedState`.
            if let Some(state) = word(&aggregator, &slot(4)) {
                if let Some(round) = uptime_round(&state) {
                    insert_round(&mut out, &round);
                }
            }
        }
        FeedKind::Dual => {
            // `DualAggregator._getLatestRound` (`DualAggregator.sol:552-568`): the hot words, the
            // cutoff and the transmissions of the rounds the secondary path can reveal.
            if let Some(hotvars) = word(&aggregator, &slot(13)) {
                let (latest, secondary) = dual_hotvars(&hotvars);
                out.insert(name("round"), u64_bytes(latest as u64));
                out.insert(name("secondary_round"), u64_bytes(secondary as u64));
                for r in dual_ring(latest, secondary) {
                    if let Some(w) = kind
                        .transmission(r)
                        .and_then(|k| word(&aggregator, &k))
                    {
                        out.insert(name(&format!("tx:{r}")), w.to_vec());
                    }
                }
            }
            if let Some(cutoff) = word(&aggregator, &slot(18)) {
                out.insert(name("cutoff"), u64_bytes(field(&cutoff, 0, 4) as u64));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flamm::keys::parse_word;

    #[test]
    fn transmission_round_trips_negative_answers() {
        let mut answer = [0xffu8; 32];
        answer[31] = 0xfe; // -2
        let packed = pack_transmission(&answer, 7, 9);
        assert_eq!(packed[..4], 9u32.to_be_bytes());
        assert_eq!(packed[4..8], 7u32.to_be_bytes());
        assert_eq!(unpack_transmission(&packed), (answer, 7, 9));
    }

    #[test]
    fn seed_words_decode_like_the_views() {
        // hotVars / transmission / feedState words at block 51154965 (fixtures/seeds), against the
        // `latestRoundData()` views read at the same block.
        let hotvars =
            parse_word("0x00000000000000000000000000000000000000000000000038ad0000af060203")
                .unwrap();
        let tx = parse_word("0x6aa37bff6aa37bf100000000000000000000000000000000000006fcb45cd8a5")
            .unwrap();
        let round = ocr2_round(&hotvars, |r| (r == 0x38ad).then_some(tx)).unwrap();
        assert_eq!(round.round, 0x38ad);
        assert_eq!(round.answer, parse_word("0x06fcb45cd8a5").unwrap());
        assert_eq!(round.started_at, 0x6aa37bf1);
        assert_eq!(round.updated_at, 0x6aa37bff);

        let feedstate =
            parse_word("0x0000000000000000006aa2e345000000006a3ea9730000000000000000000014")
                .unwrap();
        let seq = uptime_round(&feedstate).unwrap();
        assert_eq!(seq.round, 0x14);
        assert_eq!(seq.answer, [0u8; 32]);
        assert_eq!(seq.started_at, 0x6a3ea973);
        assert_eq!(seq.updated_at, 0x6aa2e345);

        let dual = parse_word("0x00000000000000000000000000000000000000000bc900000bcb00000bdb0603")
            .unwrap();
        assert_eq!(dual_hotvars(&dual), (0xbcb, 0xbc9));
        assert_eq!(dual_ring(0xbcb, 0xbc9).len(), 21);
        assert_eq!(dual_ring(0xbcb, 0xbc9)[0], 0xbcb - 20);
        // A secondary round that has fallen below the window is carried beside it: 22 rounds, and
        // `latest - 20` stays in the set whether or not a reveal can reach it.
        let below = dual_ring(0xbcb, 0xbcb - 21);
        assert_eq!(below.len(), 22);
        assert_eq!(below[..2], [0xbcb - 21, 0xbcb - 20]);
        assert_eq!(dual_ring(5, 0), vec![1, 2, 3, 4, 5]);

        let phase =
            parse_word("0x0000000000000000000051ce3091cf646587e02cad83b580992f8723e7180002")
                .unwrap();
        let (id, agg) = phase_and_aggregator(&phase);
        assert_eq!(id, 2);
        assert_eq!(hex::encode(agg), "51ce3091cf646587e02cad83b580992f8723e718");
    }
}
