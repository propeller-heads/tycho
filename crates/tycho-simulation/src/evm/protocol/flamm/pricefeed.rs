// Copyright (c) 2026 Everlong Labs Limited

//! `PriceFeed` (c104 @ `80abd43`, `src/core/PriceFeed.sol`;
//! `0xbED275459578C87a63F2f50A0b077C720e838816` on Base): the checked USD quote of each registered
//! token from its Chainlink USD aggregator behind the L2 sequencer uptime feed, and the cross of
//! two quotes. Only the paths the swap and leverage routes reach are ported: `cross`, `peekCross`,
//! `usd`, `peekUsd` and `pegOk`. The state is each aggregator's `latestRoundData` as last seen plus
//! the immutable per-token config; every check is evaluated at the call's timestamp, so a snapshot
//! ages exactly as the chain does (`StalePrice` past the heartbeat, `SequencerGrace` inside the
//! grace) until a new round lands.

use alloy::primitives::U256;

use super::{
    error::FlammError,
    math::{checked_mul, mul_div, UINT48_MAX, WAD},
};

/// `PriceFeed.MAX_PRICE_WAD = 1 << 200` (`PriceFeed.sol:22`).
pub const FEED_MAX_PRICE_WAD: U256 = U256::from_limbs([0, 0, 0, 1 << 8]);

/// One aggregator's `latestRoundData` (`roundId`, `answer`, `startedAt`, `updatedAt`); `ok` is
/// false when the call reverted. `answer` is the `int256` as its two's-complement word.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct FeedRound {
    pub ok: bool,
    pub round_id: U256,
    pub answer: U256,
    pub started_at: U256,
    pub updated_at: U256,
}

/// `PriceFeed.Token` (`config(token)`, `PriceFeed.sol:13-19`) with its aggregator's round. `known`
/// is false for a token the feed never registered (`UnknownToken`).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct FeedToken {
    pub known: bool,
    pub heartbeat: U256,
    /// `10**(18 - feed decimals)`.
    pub scale: U256,
    /// `10**token decimals`.
    pub unit: U256,
    pub peg_band_wad: U256,
    pub round: FeedRound,
}

/// The `PriceFeed` as the pool's paths read it: the sequencer feed (absent when `SEQUENCER_FEED ==
/// 0`) with `SEQUENCER_GRACE`, the pool asset's token and each loan asset's, in loan order.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct PriceFeedState {
    pub has_sequencer: bool,
    pub sequencer_grace: U256,
    pub sequencer: FeedRound,
    pub asset: FeedToken,
    pub loans: Vec<FeedToken>,
}

impl PriceFeedState {
    /// `PriceFeed._requireSequencer` (`PriceFeed.sol:157-162`): a non-zero answer or a zero
    /// `startedAt` is down, and `block.timestamp - startedAt <= SEQUENCER_GRACE` (a checked
    /// subtraction) is inside the grace.
    pub fn require_sequencer(&self, now: u64) -> Result<(), FlammError> {
        if !self.has_sequencer {
            return Ok(());
        }
        let r = &self.sequencer;
        if !r.ok {
            return Err(FlammError::FeedReverted);
        }
        if !r.answer.is_zero() || r.started_at.is_zero() {
            return Err(FlammError::SequencerDown);
        }
        let up = U256::from(now);
        if up < r.started_at {
            return Err(FlammError::PanicArithmetic);
        }
        if up - r.started_at <= self.sequencer_grace {
            return Err(FlammError::SequencerGrace);
        }
        Ok(())
    }

    /// `PriceFeed.usd` (`PriceFeed.sol:68-71`): the sequencer first, then the registration, then
    /// the round. Returns `(usdWad, observedAt)`.
    pub fn usd(&self, t: &FeedToken, now: u64) -> Result<(U256, u64), FlammError> {
        self.require_sequencer(now)?;
        if !t.known {
            return Err(FlammError::UnknownToken);
        }
        feed_read(t, now)
    }

    /// `PriceFeed.peekUsd` (`PriceFeed.sol:74-80`): any revert of `usd` reads as `(false, 0, 0)`.
    pub fn peek_usd(&self, t: &FeedToken, now: u64) -> (bool, U256, u64) {
        match self.usd(t, now) {
            Ok((v, ts)) => (true, v, ts),
            Err(_) => (false, U256::ZERO, 0),
        }
    }

    /// `PriceFeed.cross` (`PriceFeed.sol:108-116`): `priceWad = mulDiv(baseUsd, WAD, quoteUsd) /
    /// base.unit`, N18 per base unit, which must lie in `(0, 2^200)`; `observedAt` is the older
    /// of the two rounds.
    pub fn cross(
        &self,
        base: &FeedToken,
        quote: &FeedToken,
        now: u64,
    ) -> Result<(U256, u64), FlammError> {
        self.require_sequencer(now)?;
        if !base.known {
            return Err(FlammError::UnknownToken);
        }
        let (base_usd, base_ts) = feed_read(base, now)?;
        if !quote.known {
            return Err(FlammError::UnknownToken);
        }
        let (quote_usd, quote_ts) = feed_read(quote, now)?;
        let p = mul_div(base_usd, WAD, quote_usd)?;
        if base.unit.is_zero() {
            return Err(FlammError::PanicDivZero);
        }
        let p = p / base.unit;
        if p.is_zero() || p >= FEED_MAX_PRICE_WAD {
            return Err(FlammError::InvalidPrice);
        }
        Ok((p, base_ts.min(quote_ts)))
    }

    /// `PriceFeed.peekCross` (`PriceFeed.sol:119-125`): any revert of `cross` reads as `(false, 0,
    /// 0)`.
    pub fn peek_cross(&self, base: &FeedToken, quote: &FeedToken, now: u64) -> (bool, U256, u64) {
        match self.cross(base, quote, now) {
            Ok((p, ts)) => (true, p, ts),
            Err(_) => (false, U256::ZERO, 0),
        }
    }

    /// `PriceFeed.pegOk` (`PriceFeed.sol:83-92`): an unregistered token reverts `UnknownToken`
    /// (outside the try); a zero band is always ok; otherwise a reverting `usd` is not ok, and
    /// `|usd - WAD|` must be within the band.
    pub fn peg_ok(&self, t: &FeedToken, now: u64) -> Result<bool, FlammError> {
        if !t.known {
            return Err(FlammError::UnknownToken);
        }
        if t.peg_band_wad.is_zero() {
            return Ok(true);
        }
        let Ok((v, _)) = self.usd(t, now) else {
            return Ok(false);
        };
        let dev = if v > WAD { v - WAD } else { WAD - v };
        Ok(dev <= t.peg_band_wad)
    }
}

/// `PriceFeed._read` (`PriceFeed.sol:164-174`): the round must be a positive answer with a set,
/// non-future timestamp no older than the heartbeat; `usdWad = answer * scale` (checked). Returns
/// the round's `updatedAt` as the `uint48` `observedAt` of `_usd` (`PriceFeed.sol:154`).
pub fn feed_read(t: &FeedToken, now: u64) -> Result<(U256, u64), FlammError> {
    let r = &t.round;
    if !r.ok {
        return Err(FlammError::FeedReverted);
    }
    let now_u = U256::from(now);
    // `answer <= 0`: an int256 whose top bit is set is negative.
    let negative = r.answer.bit(255);
    if r.round_id.is_zero() ||
        r.answer.is_zero() ||
        negative ||
        r.updated_at.is_zero() ||
        r.updated_at > now_u
    {
        return Err(FlammError::InvalidPrice);
    }
    if now_u - r.updated_at > t.heartbeat {
        return Err(FlammError::StalePrice);
    }
    let usd = checked_mul(r.answer, t.scale)?;
    // updatedAt <= now < 2^64, so the uint48 cast only matters past the year 8.9M; kept for
    // exactness.
    let ts = u64::try_from(r.updated_at).map_or(UINT48_MAX, |v| v & UINT48_MAX);
    Ok((usd, ts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(answer: u64, updated_at: u64) -> FeedToken {
        FeedToken {
            known: true,
            heartbeat: U256::from(3600u64),
            scale: U256::from(10_000_000_000u64),
            unit: U256::from(100_000_000u64),
            peg_band_wad: U256::ZERO,
            round: FeedRound {
                ok: true,
                round_id: U256::from(7u64),
                answer: U256::from(answer),
                started_at: U256::from(updated_at),
                updated_at: U256::from(updated_at),
            },
        }
    }

    fn feed() -> PriceFeedState {
        PriceFeedState {
            has_sequencer: true,
            sequencer_grace: U256::from(3600u64),
            sequencer: FeedRound {
                ok: true,
                round_id: U256::from(1u64),
                answer: U256::ZERO,
                started_at: U256::from(1_000_000u64),
                updated_at: U256::from(1_000_000u64),
            },
            asset: token(7_835_610_638_287, 2_000_000),
            loans: vec![FeedToken {
                unit: U256::from(1_000_000u64),
                heartbeat: U256::from(90_000u64),
                peg_band_wad: U256::from(10_000_000_000_000_000u64),
                ..token(99_983_415, 2_000_000)
            }],
        }
    }

    #[test]
    fn heartbeat_boundary_is_inclusive() {
        let f = feed();
        let now = 2_000_000 + 3600;
        assert!(f
            .cross(&f.asset, &f.loans[0], now)
            .is_ok());
        assert_eq!(f.cross(&f.asset, &f.loans[0], now + 1), Err(FlammError::StalePrice));
        assert_eq!(f.peek_cross(&f.asset, &f.loans[0], now + 1), (false, U256::ZERO, 0));
    }

    #[test]
    fn sequencer_grace_boundary_is_inclusive() {
        let mut f = feed();
        let started = 1_000_000u64;
        assert_eq!(f.require_sequencer(started + 3600), Err(FlammError::SequencerGrace));
        assert!(f
            .require_sequencer(started + 3601)
            .is_ok());
        assert_eq!(f.require_sequencer(started - 1), Err(FlammError::PanicArithmetic));
        f.sequencer.answer = U256::from(1u64);
        assert_eq!(f.require_sequencer(started + 3601), Err(FlammError::SequencerDown));
        f.sequencer.answer = U256::ZERO;
        f.sequencer.started_at = U256::ZERO;
        assert_eq!(f.require_sequencer(started + 3601), Err(FlammError::SequencerDown));
        f.sequencer.ok = false;
        assert_eq!(f.require_sequencer(started + 3601), Err(FlammError::FeedReverted));
        f.has_sequencer = false;
        assert!(f
            .require_sequencer(started + 3601)
            .is_ok());
    }

    #[test]
    fn read_rejects_invalid_rounds_in_order() {
        let now = 2_000_001u64;
        let mut t = token(1, 2_000_000);
        assert!(feed_read(&t, now).is_ok());
        t.round.round_id = U256::ZERO;
        assert_eq!(feed_read(&t, now), Err(FlammError::InvalidPrice));
        let mut t = token(0, 2_000_000);
        assert_eq!(feed_read(&t, now), Err(FlammError::InvalidPrice));
        t.round.answer = U256::MAX; // int256 -1
        assert_eq!(feed_read(&t, now), Err(FlammError::InvalidPrice));
        let t = token(1, 0);
        assert_eq!(feed_read(&t, now), Err(FlammError::InvalidPrice));
        let t = token(1, now + 1);
        assert_eq!(feed_read(&t, now), Err(FlammError::InvalidPrice));
        let mut t = token(1, now);
        t.round.ok = false;
        assert_eq!(feed_read(&t, now), Err(FlammError::FeedReverted));
        // answer * scale is a checked product.
        let mut t = token(1, now);
        t.round.answer = U256::from(1u64) << 254;
        t.scale = U256::from(4u64);
        assert_eq!(feed_read(&t, now), Err(FlammError::PanicArithmetic));
    }

    #[test]
    fn cross_bounds_and_observed_at() {
        let f = feed();
        let now = 2_000_100u64;
        let (p, ts) = f
            .cross(&f.asset, &f.loans[0], now)
            .unwrap();
        // 7835610638287e10 * 1e18 / 99983415e10 / 1e8
        assert_eq!(p, U256::from(783_691_038_987_516u64));
        assert_eq!(ts, 2_000_000);
        let mut g = f.clone();
        g.loans[0].round.updated_at = U256::from(1_999_999u64);
        assert_eq!(
            g.cross(&g.asset, &g.loans[0], now)
                .unwrap()
                .1,
            1_999_999
        );
        // priceWad == 0 and priceWad >= 2^200 are InvalidPrice.
        let mut g = f.clone();
        g.asset.round.answer = U256::from(1u64);
        g.asset.scale = U256::from(1u64);
        assert_eq!(g.cross(&g.asset, &g.loans[0], now), Err(FlammError::InvalidPrice));
        // baseUsd = 2^200, quoteUsd = 1e18, unit 1: priceWad == 2^200 exactly, one under is
        // accepted.
        let mut g = f.clone();
        g.asset.unit = U256::from(1u64);
        g.asset.scale = U256::from(1u64);
        g.asset.round.answer = U256::from(1u64) << 200;
        g.loans[0].round.answer = U256::from(100_000_000u64);
        assert_eq!(g.cross(&g.asset, &g.loans[0], now), Err(FlammError::InvalidPrice));
        g.asset.round.answer = (U256::from(1u64) << 200) - U256::from(1u64);
        assert_eq!(
            g.cross(&g.asset, &g.loans[0], now)
                .unwrap()
                .0,
            FEED_MAX_PRICE_WAD - U256::from(1u64)
        );
        let mut g = f.clone();
        g.asset.known = false;
        assert_eq!(g.cross(&g.asset, &g.loans[0], now), Err(FlammError::UnknownToken));
        let mut g = f.clone();
        g.loans[0].known = false;
        assert_eq!(g.cross(&g.asset, &g.loans[0], now), Err(FlammError::UnknownToken));
        // The base round is read before the quote's registration is checked.
        let mut g = f.clone();
        g.loans[0].known = false;
        g.asset.round.round_id = U256::ZERO;
        assert_eq!(g.cross(&g.asset, &g.loans[0], now), Err(FlammError::InvalidPrice));
    }

    #[test]
    fn peg_band_is_inclusive_and_catches_reverts() {
        let f = feed();
        let now = 2_000_100u64;
        assert_eq!(f.peg_ok(&f.loans[0], now), Ok(true));
        let mut g = f.clone();
        g.loans[0].round.answer = U256::from(99_000_000u64); // exactly 1% under
        assert_eq!(g.peg_ok(&g.loans[0], now), Ok(true));
        g.loans[0].round.answer = U256::from(98_999_999u64);
        assert_eq!(g.peg_ok(&g.loans[0], now), Ok(false));
        g.loans[0].round.answer = U256::from(101_000_000u64);
        assert_eq!(g.peg_ok(&g.loans[0], now), Ok(true));
        g.loans[0].round.answer = U256::from(101_000_001u64);
        assert_eq!(g.peg_ok(&g.loans[0], now), Ok(false));
        // A stale round or a sequencer refusal is caught: not ok, no revert.
        assert_eq!(f.peg_ok(&f.loans[0], 2_000_000 + 90_001), Ok(false));
        assert_eq!(f.peg_ok(&f.loans[0], 1_000_000 + 3600), Ok(false));
        // A zero band never reads the round; an unknown token reverts outside the try.
        assert_eq!(f.peg_ok(&f.asset, 0), Ok(true));
        let mut g = f.clone();
        g.loans[0].known = false;
        assert_eq!(g.peg_ok(&g.loans[0], now), Err(FlammError::UnknownToken));
    }
}
