// Copyright (c) 2026 Everlong Labs Limited

//! The Chainlink feeds a quote reads, as the `base-flamm` substreams carries them
//! (`feed:<f>:*`): the cbBTC/USD and USDC/USD aggregators behind `PriceFeed`
//! (`src/core/PriceFeed.sol`), the L2 sequencer uptime feed (`PriceFeed.SEQUENCER_FEED`) and the
//! BTC/USD `DualAggregator` behind the Morpho market oracle
//! (`MorphoChainlinkOracleV2.BASE_FEED_1`). Each feed is read through a v0.6 `EACAggregatorProxy`
//! whose current aggregator is its slot 2 (`uint16 phaseId | address aggregator`) and whose
//! `latestRoundData` is `checkAccess()`-guarded (slot 5, `accessController`).
//!
//! What `latestRoundData()` answers is a function of the aggregator's words alone for the OCR2
//! aggregators and the uptime feed; for the `DualAggregator` it is a function of its words AND
//! `block.timestamp` (the SVR reveal, [`DualFeed::latest_round`]), which is why the Morpho oracle
//! answer is recomputed at every execution clock rather than decoded once.

use alloy::primitives::{Address, U256};

use super::{
    error::FlammError,
    math::mul_div,
    pricefeed::FeedRound,
    words::{address_of, field, field_u64, word_of, Attributes, WordError},
};

/// The four feed roles of a one-loan-asset pool, in the attribute names' spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FeedRole {
    /// cbBTC/USD, the pool asset's USD feed (`PriceFeed._tokens[poolAsset]`).
    Asset,
    /// USDC/USD, loan asset 0's USD feed.
    Loan0,
    /// The L2 sequencer uptime feed.
    Seq,
    /// BTC/USD behind venue 0's Morpho market oracle.
    Mo0,
}

impl FeedRole {
    pub fn name(self) -> &'static str {
        match self {
            Self::Asset => "asset",
            Self::Loan0 => "loan0",
            Self::Seq => "seq",
            Self::Mo0 => "mo0",
        }
    }
}

/// The aggregator kinds the substreams decodes (its `feed:<f>:kind`): each has its own storage
/// layout and read guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FeedKind {
    /// `AccessControlledOCR2Aggregator`: `latestRoundData` is `checkAccess()`-guarded by its
    /// `SimpleWriteAccessController` pair.
    Ocr2,
    /// `OptimismSequencerUptimeFeed`: guarded the same way.
    Uptime,
    /// Chainlink SVR `DualAggregator`: no read guard of its own.
    Dual,
}

impl FeedKind {
    fn parse(name: &str, v: &[u8]) -> Result<Self, WordError> {
        match v {
            b"ocr2" => Ok(Self::Ocr2),
            b"uptime" => Ok(Self::Uptime),
            b"dual" => Ok(Self::Dual),
            other => Err(WordError::Malformed(format!(
                "{name}: unknown aggregator kind {:?}",
                String::from_utf8_lossy(other)
            ))),
        }
    }

    /// Whether the aggregator's `latestRoundData` is behind `SimpleWriteAccessController`
    /// (schema 2.6.4).
    pub fn guarded(self) -> bool {
        !matches!(self, Self::Dual)
    }
}

/// One `DualAggregator` transmission word as stored (`DualAggregator.sol:451-455`): `int192 answer`
/// at byte 0, `uint32 observationsTimestamp` at 24, `uint32 recordedTimestamp` at 28.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Transmission {
    /// The `int192` answer as its two's-complement `int256` word.
    pub answer: U256,
    pub observations_timestamp: u32,
    pub recorded_timestamp: u32,
}

impl Transmission {
    pub fn unpack(w: U256) -> Self {
        let raw = field(w, 0, 24);
        // int192 -> int256: sign-extend from bit 191.
        let answer = if raw.bit(191) { raw | (U256::MAX << 192) } else { raw };
        Self {
            answer,
            observations_timestamp: field_u64(w, 24, 4) as u32,
            recorded_timestamp: field_u64(w, 28, 4) as u32,
        }
    }
}

/// The `DualAggregator` words the secondary-path reveal reads (`feed:mo0:*`): `s_hotVars`'
/// `latestAggregatorRoundId` / `latestSecondaryRoundId`, `s_cutoffTime`, and `s_transmissions[r]`
/// for every round the reveal can answer with (the substreams carries `latest-20..=latest` plus
/// the secondary round and deletes the rest).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DualFeed {
    pub latest: u32,
    pub secondary: u32,
    pub cutoff: u32,
    /// `(round, word)`, ascending by round.
    pub ring: Vec<(u32, Transmission)>,
}

impl DualFeed {
    /// `s_transmissions[r]`: round 0 is never written and reads as the zero word; any other round
    /// outside the carried ring is unknown.
    fn transmission(&self, r: u32) -> Result<Transmission, FeedError> {
        if r == 0 {
            return Ok(Transmission::default());
        }
        self.ring
            .iter()
            .find(|(k, _)| *k == r)
            .map(|(_, t)| *t)
            .ok_or(FeedError::RingRoundAbsent(r))
    }

    /// `recordedTimestamp + s_cutoffTime < block.timestamp`, the `uint32` sum checked
    /// (`DualAggregator.sol:541`, `:562`).
    fn stale(&self, t: &Transmission, now: u64) -> Result<bool, FeedError> {
        let deadline = t
            .recorded_timestamp
            .checked_add(self.cutoff)
            .ok_or(FeedError::Reverted)?;
        Ok(u64::from(deadline) < now)
    }

    /// `_getLatestRound` on the secondary path (`DualAggregator.sol:552-568`, the caller is the
    /// secondary proxy): the latest secondary round while it is inside the cutoff, else
    /// `_getSyncPrimaryRound` (`:529-548`): the newest of the last `i_maxSyncIterations` primary
    /// rounds that is past the cutoff, else the secondary round.
    pub fn latest_round(&self, now: u64, max_sync_iterations: u32) -> Result<u32, FeedError> {
        let sec = self.transmission(self.secondary)?;
        if !self.stale(&sec, now)? {
            return Ok(self.secondary);
        }
        let mut r = self.latest;
        while r > 0 {
            if self.latest - r == max_sync_iterations {
                break;
            }
            if self.stale(&self.transmission(r)?, now)? {
                return Ok(r);
            }
            r -= 1;
        }
        Ok(self.secondary)
    }

    /// `latestRoundData()` through the secondary proxy at `now` (`DualAggregator.sol:1072-1088`).
    pub fn latest_round_data(
        &self,
        phase: u64,
        now: u64,
        max_sync_iterations: u32,
    ) -> Result<FeedRound, FeedError> {
        let r = self.latest_round(now, max_sync_iterations)?;
        let t = self.transmission(r)?;
        Ok(FeedRound {
            ok: true,
            round_id: proxy_round_id(phase, u64::from(r)),
            answer: t.answer,
            started_at: U256::from(t.observations_timestamp),
            updated_at: U256::from(t.recorded_timestamp),
        })
    }
}

/// `EACAggregatorProxy.addPhase`: `(phaseId << 64) | aggregatorRoundId`.
pub fn proxy_round_id(phase: u64, round: u64) -> U256 {
    (U256::from(phase) << 64) | U256::from(round)
}

/// Why a feed cannot be read at a clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedError {
    /// A ring round the reveal visits is not carried.
    RingRoundAbsent(u32),
    /// The aggregator's own arithmetic reverts (a `uint32` timestamp sum past 2^32).
    Reverted,
}

/// One feed as decoded from its attributes: the proxy's rotation word and read guard, the
/// aggregator's kind and, by kind, its latest round or its reveal ring.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Feed {
    pub role: FeedRole,
    pub aggregator: Address,
    pub phase: u64,
    pub access_controller: Address,
    pub kind: FeedKind,
    /// The `SimpleWriteAccessController` pair of a guarded kind: `checkEnabled` and
    /// `s_accessList[proxy]`.
    pub check_enabled: Option<bool>,
    pub access_list: Option<bool>,
    /// The latest round of an OCR2 aggregator / the uptime feed's `s_feedState`, with the proxy's
    /// round id (`phase << 64 | round`).
    pub round: Option<FeedRound>,
    /// The `DualAggregator`'s reveal inputs.
    pub dual: Option<DualFeed>,
}

impl Feed {
    /// Decodes `feed:<role>:*`. Every attribute the kind needs must be present: an absent one is
    /// unknown (a rotation the aggregator has not transmitted after, a word never seeded), and the
    /// feed then refuses to quote.
    pub fn decode(role: FeedRole, attrs: &Attributes) -> Result<Self, WordError> {
        let name = |n: &str| format!("feed:{}:{n}", role.name());
        let get = |n: &str| -> Result<&tycho_common::Bytes, WordError> {
            attrs
                .get(&name(n))
                .ok_or_else(|| WordError::Missing(name(n)))
        };
        let word = |n: &str| -> Result<U256, WordError> { word_of(&name(n), get(n)?) };
        let u64_word = |n: &str| -> Result<u64, WordError> {
            let w = word(n)?;
            u64::try_from(w)
                .map_err(|_| WordError::Malformed(format!("{}: {w} does not fit u64", name(n))))
        };
        let aggregator = address_of(&name("aggregator"), get("aggregator")?)?;
        let phase = u64_word("phase")?;
        let access_controller = address_of(&name("access_controller"), get("access_controller")?)?;
        let kind = FeedKind::parse(&name("kind"), get("kind")?)?;
        let (check_enabled, access_list) = if kind.guarded() {
            (Some(!word("check_enabled")?.is_zero()), Some(!word("access_list")?.is_zero()))
        } else {
            (None, None)
        };
        let (round, dual) = match kind {
            FeedKind::Ocr2 | FeedKind::Uptime => {
                let round = u64_word("round")?;
                let r = FeedRound {
                    ok: true,
                    round_id: proxy_round_id(phase, round),
                    answer: word("answer")?,
                    started_at: word("started_at")?,
                    updated_at: word("updated_at")?,
                };
                (Some(r), None)
            }
            FeedKind::Dual => {
                let latest = u32::try_from(u64_word("round")?)
                    .map_err(|_| WordError::Malformed(name("round")))?;
                let secondary = u32::try_from(u64_word("secondary_round")?)
                    .map_err(|_| WordError::Malformed(name("secondary_round")))?;
                let cutoff = u32::try_from(u64_word("cutoff")?)
                    .map_err(|_| WordError::Malformed(name("cutoff")))?;
                let prefix = name("tx:");
                let mut ring = Vec::new();
                for (k, v) in attrs.range(prefix.clone()..) {
                    let Some(r) = k.strip_prefix(&prefix) else {
                        break;
                    };
                    let r: u32 = r
                        .parse()
                        .map_err(|_| WordError::Malformed(k.clone()))?;
                    ring.push((r, Transmission::unpack(word_of(k, v)?)));
                }
                ring.sort_by_key(|(r, _)| *r);
                (None, Some(DualFeed { latest, secondary, cutoff, ring }))
            }
        };
        Ok(Self {
            role,
            aggregator,
            phase,
            access_controller,
            kind,
            check_enabled,
            access_list,
            round,
            dual,
        })
    }

    /// Whether a `latestRoundData()` through the proxy returns rather than reverts (schema
    /// 2.6.4): the proxy's `checkAccess` passes only with no controller set (a controller's own
    /// state is not tracked, so a set one fails closed), and a guarded aggregator's
    /// `hasAccess(proxy, data)` is `s_accessList[proxy] || !checkEnabled` (the proxy is a
    /// contract, never `tx.origin`).
    pub fn read_ok(&self) -> bool {
        if self.access_controller != Address::ZERO {
            return false;
        }
        match (self.check_enabled, self.access_list) {
            (Some(enabled), Some(listed)) => listed || !enabled,
            _ => !self.kind.guarded(),
        }
    }

    /// `latestRoundData()` as `PriceFeed._read` / `_requireSequencer` see it: `ok` false when the
    /// call reverts (the read guard).
    pub fn feed_round(&self) -> Result<FeedRound, WordError> {
        let mut r = self
            .round
            .clone()
            .ok_or_else(|| WordError::Missing(format!("feed:{}:round", self.role.name())))?;
        r.ok = self.read_ok();
        Ok(r)
    }

    /// The Morpho market oracle's answer at `now` (`MorphoBlueAccount.oraclePrice`,
    /// `MorphoBlueAccount.sol:351-359`, over `MorphoChainlinkOracleV2.price()`: `SCALE_FACTOR *
    /// BASE_FEED_1.latestRoundData().answer` through `Math.mulDiv` with every other feed and vault
    /// unset, `ChainlinkDataFeedLib.getPrice` requiring `answer >= 0`). Returns `(ok, price,
    /// zero)`: `ok` is a non-reverting, non-zero price; `zero` a non-reverting zero (which Blue's
    /// health check reads as zero where the account reads `ok == false`). A reverting read
    /// (guard, a negative answer, an overflowing product) is `(false, 0, false)`.
    pub fn oracle_price(
        &self,
        scale_factor: U256,
        now: u64,
        max_sync_iterations: u32,
    ) -> Result<(bool, U256, bool), FeedError> {
        let Some(dual) = &self.dual else {
            // A non-dual aggregator behind the Morpho oracle would answer its stored round;
            // only the deployed kind is modelled and the decoder refuses the others.
            return Err(FeedError::Reverted);
        };
        if !self.read_ok() {
            return Ok((false, U256::ZERO, false));
        }
        let r = dual.latest_round_data(self.phase, now, max_sync_iterations)?;
        if r.answer.bit(255) {
            return Ok((false, U256::ZERO, false));
        }
        match mul_div(scale_factor, r.answer, U256::from(1u8)) {
            Ok(p) => Ok((!p.is_zero(), p, p.is_zero())),
            Err(FlammError::MulDivOverflow) => Ok((false, U256::ZERO, false)),
            Err(_) => Ok((false, U256::ZERO, false)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tycho_common::Bytes;

    use super::*;

    fn word(hex: &str) -> U256 {
        U256::from_str_radix(hex, 16).unwrap()
    }

    fn tx(hex: &str) -> Transmission {
        Transmission::unpack(word(hex))
    }

    /// The ring at 51302915 (schema snapshot): rounds 3563..=3583, secondary 3581, cutoff 10.
    fn ring_51302915() -> DualFeed {
        let words = [
            (3579u32, "6aa7fedf6aa7fed100000000000000000000000000000000000007211e1e9d9c"),
            (3580, "6aa7ff576aa7ff49000000000000000000000000000000000000071ef5cbca6a"),
            (3581, "6aa800656aa800580000000000000000000000000000000000000720e303f9f1"),
            (3582, "6aa800836aa800760000000000000000000000000000000000000722b80e9d87"),
            (3583, "6aa800dd6aa800d00000000000000000000000000000000000000720d1de7916"),
        ];
        DualFeed {
            latest: 3583,
            secondary: 3581,
            cutoff: 10,
            ring: words
                .iter()
                .map(|(r, w)| (*r, tx(w)))
                .collect(),
        }
    }

    #[test]
    fn transmission_unpacks_and_sign_extends() {
        let t = tx("6aa800dd6aa800d00000000000000000000000000000000000000720d1de7916");
        assert_eq!(t.answer, U256::from(7_837_541_366_038u64));
        assert_eq!(t.observations_timestamp, 0x6aa800d0);
        assert_eq!(t.recorded_timestamp, 0x6aa800dd);
        // A negative int192 (-2) reads as the int256 -2.
        let mut w = [0xffu8; 32];
        w[..8].copy_from_slice(&[0, 0, 0, 9, 0, 0, 0, 7]);
        w[31] = 0xfe;
        let t = Transmission::unpack(U256::from_be_bytes(w));
        assert_eq!(t.answer, U256::MAX - U256::from(1u8));
        assert_eq!(t.observations_timestamp, 7);
        assert_eq!(t.recorded_timestamp, 9);
    }

    #[test]
    fn reveal_follows_the_clock() {
        let d = ring_51302915();
        // Block 51302915 (ts 1789395177): the secondary (3581, recorded 1789395045) is past the
        // cutoff; 3583 (recorded 1789395165 + 10 = 1789395175) is revealed.
        assert_eq!(d.latest_round(1_789_395_177, 20), Ok(3583));
        // At 1789395175 the sum is not < the clock: 3583 is withheld, 3582 (1789395062 + 10 =
        // 1789395072 < now) is revealed.
        assert_eq!(d.latest_round(1_789_395_175, 20), Ok(3582));
        assert_eq!(d.latest_round(1_789_395_176, 20), Ok(3583));
        // Inside the secondary round's (3581, recorded 1789395045) cutoff the secondary answers;
        // once it is stale the sync visits 3583, 3582 and then 3581 itself, which is stale.
        assert_eq!(d.latest_round(1_789_395_050, 20), Ok(3581));
        assert_eq!(d.latest_round(1_789_395_055, 20), Ok(3581));
        assert_eq!(d.latest_round(1_789_395_056, 20), Ok(3581));
        assert_eq!(d.latest_round(1_789_395_085, 20), Ok(3581));
        assert_eq!(d.latest_round(1_789_395_086, 20), Ok(3582));
        // A visited round outside the ring is unknown.
        let mut short = d.clone();
        short.ring.retain(|(r, _)| *r != 3582);
        assert_eq!(short.latest_round(1_789_395_100, 20), Err(FeedError::RingRoundAbsent(3582)));
        assert_eq!(short.latest_round(1_789_395_177, 20), Ok(3583));
        // Nothing past the cutoff within the sync depth: the secondary round, even when it is
        // the one stale round (past the depth) or absent from the ring.
        let mut fresh = d.clone();
        fresh.secondary = 3579;
        for (r, t) in fresh.ring.iter_mut() {
            t.recorded_timestamp = if *r == 3579 { 1_789_395_000 } else { 1_789_395_170 };
        }
        assert_eq!(fresh.latest_round(1_789_395_177, 2), Ok(3579));
        assert_eq!(fresh.latest_round(1_789_395_177, 20), Ok(3579));
        fresh.secondary = 3570;
        assert_eq!(fresh.latest_round(1_789_395_177, 2), Err(FeedError::RingRoundAbsent(3570)));
        // Round 0 reads as the zero word: a never-posted secondary round is stale at once.
        let mut none = d.clone();
        none.secondary = 0;
        assert_eq!(none.latest_round(1_789_395_177, 20), Ok(3583));
        // The uint32 sum is checked.
        let mut wrap = d.clone();
        wrap.cutoff = u32::MAX;
        assert_eq!(wrap.latest_round(1_789_395_177, 20), Err(FeedError::Reverted));
        let r = d
            .latest_round_data(3, 1_789_395_177, 20)
            .unwrap();
        assert_eq!(r.round_id, proxy_round_id(3, 3583));
        assert_eq!(r.answer, U256::from(7_837_541_366_038u64));
        assert_eq!(r.updated_at, U256::from(0x6aa800ddu64));
        assert_eq!(r.started_at, U256::from(0x6aa800d0u64));
    }

    fn feed_attrs(role: &str, kind: &str) -> Attributes {
        let mut a = Attributes::new();
        let n = |s: &str| format!("feed:{role}:{s}");
        a.insert(n("aggregator"), Bytes::from(vec![0x51u8; 20]));
        a.insert(n("phase"), Bytes::from(U256::from(2u8).to_be_bytes::<32>()));
        a.insert(n("access_controller"), Bytes::from(vec![0u8; 20]));
        a.insert(n("kind"), Bytes::from(kind.as_bytes().to_vec()));
        a
    }

    #[test]
    fn ocr2_feed_decodes_with_its_guard() {
        let mut a = feed_attrs("asset", "ocr2");
        let n = |s: &str| format!("feed:asset:{s}");
        let w = |x: u64| Bytes::from(U256::from(x).to_be_bytes::<32>());
        assert!(matches!(Feed::decode(FeedRole::Asset, &a), Err(WordError::Missing(_))));
        a.insert(n("check_enabled"), w(1));
        a.insert(n("access_list"), w(1));
        a.insert(n("round"), w(14785));
        a.insert(n("answer"), w(7_835_610_638_287));
        a.insert(n("started_at"), w(1_789_394_629));
        a.insert(n("updated_at"), w(1_789_394_643));
        let f = Feed::decode(FeedRole::Asset, &a).unwrap();
        assert!(f.read_ok());
        let r = f.feed_round().unwrap();
        assert!(r.ok);
        assert_eq!(r.round_id, U256::from_str("36893488147419118017").unwrap());
        assert_eq!(r.answer, U256::from(7_835_610_638_287u64));
        // The guard: listed || !checkEnabled; a set controller fails closed.
        a.insert(n("access_list"), w(0));
        assert!(!Feed::decode(FeedRole::Asset, &a)
            .unwrap()
            .read_ok());
        a.insert(n("check_enabled"), w(0));
        assert!(Feed::decode(FeedRole::Asset, &a)
            .unwrap()
            .read_ok());
        a.insert(n("access_controller"), Bytes::from(vec![1u8; 20]));
        let f = Feed::decode(FeedRole::Asset, &a).unwrap();
        assert!(!f.read_ok());
        assert!(!f.feed_round().unwrap().ok);
        a.insert(n("kind"), Bytes::from(b"other".to_vec()));
        assert!(matches!(Feed::decode(FeedRole::Asset, &a), Err(WordError::Malformed(_))));
    }

    #[test]
    fn dual_feed_prices_the_oracle_at_the_clock() {
        let mut a = feed_attrs("mo0", "dual");
        let n = |s: &str| format!("feed:mo0:{s}");
        let w = |x: u64| Bytes::from(U256::from(x).to_be_bytes::<32>());
        a.insert(n("round"), w(3583));
        a.insert(n("secondary_round"), w(3581));
        a.insert(n("cutoff"), w(10));
        for (r, t) in ring_51302915().ring {
            let mut word: U256 = U256::from(t.recorded_timestamp) << 224;
            word |= U256::from(t.observations_timestamp) << 192;
            word |= t.answer;
            a.insert(n(&format!("tx:{r}")), Bytes::from(word.to_be_bytes::<32>()));
        }
        let f = Feed::decode(FeedRole::Mo0, &a).unwrap();
        assert_eq!(f.check_enabled, None);
        assert!(f.read_ok());
        assert_eq!(f.dual.as_ref().unwrap().ring.len(), 5);
        let scale = U256::from(10u8).pow(U256::from(26u8));
        // 1e26 * 7837541366038 at block 51302915, the e2e dump's oraclePrice.
        assert_eq!(
            f.oracle_price(scale, 1_789_395_177, 20),
            Ok((true, U256::from_str("783754136603800000000000000000000000000").unwrap(), false))
        );
        // A negative answer is NEGATIVE_ANSWER: not ok.
        let mut neg = f.clone();
        for (_, t) in neg
            .dual
            .as_mut()
            .unwrap()
            .ring
            .iter_mut()
        {
            t.answer = U256::MAX;
        }
        assert_eq!(neg.oracle_price(scale, 1_789_395_177, 20), Ok((false, U256::ZERO, false)));
        // A zero answer is `(false, 0)` at the account and zero at Blue.
        let mut zero = f.clone();
        for (_, t) in zero
            .dual
            .as_mut()
            .unwrap()
            .ring
            .iter_mut()
        {
            t.answer = U256::ZERO;
        }
        assert_eq!(zero.oracle_price(scale, 1_789_395_177, 20), Ok((false, U256::ZERO, true)));
        // A product past 2^256 reverts Math.mulDiv: not ok.
        assert_eq!(f.oracle_price(U256::MAX, 1_789_395_177, 20), Ok((false, U256::ZERO, false)));
        // The guard applies to the proxy the oracle reads through.
        let mut guarded = f.clone();
        guarded.access_controller = Address::with_last_byte(1);
        assert_eq!(guarded.oracle_price(scale, 1_789_395_177, 20), Ok((false, U256::ZERO, false)));
        // A ring round the reveal visits but the attributes lack is unknown; one it does not
        // visit at this clock is not needed.
        a.remove(&n("tx:3582"));
        let f = Feed::decode(FeedRole::Mo0, &a).unwrap();
        assert_eq!(f.oracle_price(scale, 1_789_395_100, 20), Err(FeedError::RingRoundAbsent(3582)));
        assert_eq!(
            f.oracle_price(scale, 1_789_395_177, 20),
            Ok((true, U256::from_str("783754136603800000000000000000000000000").unwrap(), false))
        );
    }
}
