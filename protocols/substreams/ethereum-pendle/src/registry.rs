//! The set of markets, materialised under one store key.
//!
//! Refreshing every live market's PY index once per block means asking "which markets are live
//! right now?" on a block where nothing happened — there is no log to start from. Substreams
//! stores cannot answer that: reads are `get_at` / `get_last` / `get_first` against a single key,
//! with no iterator and no prefix scan. So the set is kept as the *value* of one key in an
//! `append` store, and parsed back here.
//!
//! One line per market, `;`-separated by `StoreAppend`, fields `|`-separated:
//! `<market component id>|<SY component id>|<expiry>|<factory address>|<creation fee rate>`. At
//! ~140 bytes per line
//! and 484 markets created on Ethereum since 2023, the whole registry is under 60 KB.
//!
//! The factory is here because the fee events fan out by factory: the original factory's
//! `NewMarketConfig` changes the fee for every market it ever deployed, and V3+'s
//! `NewTreasuryAndFeeReserve` does the same per factory.

use std::fmt::Write as _;

/// A market as the registry remembers it: enough to refresh its state without re-reading chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketEntry {
    /// Component id of the market.
    pub id: String,
    /// Component id of the SY the market prices PT against.
    pub sy: String,
    /// Unix timestamp at which the market stops trading.
    pub expiry: u64,
    /// Hex address of the factory that deployed it, without the `0x` prefix.
    pub factory: String,
    /// `lnFeeRateRoot` as the creation event reported it, decimal. Zero for original-factory
    /// markets, whose event does not carry it — their fee comes from `getMarketConfig` instead.
    pub ln_fee_rate_root_at_creation: String,
}

impl MarketEntry {
    /// Renders the entry as one registry line.
    pub fn encode(&self) -> String {
        let mut line =
            String::with_capacity(self.id.len() + self.sy.len() + self.factory.len() + 48);
        let _ = write!(
            line,
            "{}|{}|{}|{}|{}",
            self.id, self.sy, self.expiry, self.factory, self.ln_fee_rate_root_at_creation
        );
        line
    }

    fn decode(line: &str) -> Option<Self> {
        let mut fields = line.split('|');
        let id = fields.next()?;
        let sy = fields.next()?;
        let expiry = fields.next()?.parse().ok()?;
        let factory = fields.next()?;
        let ln_fee_rate_root_at_creation = fields.next()?;
        if fields.next().is_some() ||
            id.is_empty() ||
            sy.is_empty() ||
            factory.is_empty() ||
            ln_fee_rate_root_at_creation.is_empty()
        {
            return None;
        }
        Some(MarketEntry {
            id: id.to_string(),
            sy: sy.to_string(),
            expiry,
            factory: factory.to_string(),
            ln_fee_rate_root_at_creation: ln_fee_rate_root_at_creation.to_string(),
        })
    }

    /// Whether this market was deployed by the factory at `address`.
    pub fn is_from(&self, address: &[u8]) -> bool {
        self.factory == hex::encode(address)
    }
}

/// Returns every market in the registry that still trades at `block_timestamp`.
///
/// A market is dead from its expiry onwards — the contract reverts with `MarketExpired` at
/// `block.timestamp >= expiry`, not after it — so the comparison is strict.
///
/// Lines that do not parse are skipped rather than failing the block: the registry is
/// append-only and a malformed line would otherwise poison every block after it.
pub fn live_markets(registry: &str, block_timestamp: u64) -> Vec<MarketEntry> {
    entries(registry)
        .filter(|entry| block_timestamp < entry.expiry)
        .collect()
}

/// How long past its expiry a market keeps having its clock republished.
///
/// The republication is what tells a consumer the market is dead (see `expired_markets`), and it
/// can only happen on a block the package refreshes state on. So the window has to be wider than
/// the wall-clock gap between two consecutive refresh blocks: a day covers an
/// `sy_rate_refresh_blocks` of several thousand, orders of magnitude above anything deployed.
/// Being generous costs one attribute per expired market per refresh block; being too tight costs
/// a market that never dies.
pub const EXPIRY_GRACE_SECONDS: u64 = 86_400;

/// Returns every market that expired within `EXPIRY_GRACE_SECONDS` before `block_timestamp`.
///
/// These are the markets whose clock still has to be published even though they no longer trade.
/// A consumer decides a market is expired by comparing its `expiry` against the last timestamp it
/// was given, and `live_markets` stops strictly below expiry by construction — so without this
/// the last timestamp a market ever receives is one it was still alive at, and it goes on quoting
/// off a frozen clock forever.
pub fn expired_markets(registry: &str, block_timestamp: u64) -> Vec<MarketEntry> {
    entries(registry)
        .filter(|entry| {
            entry.expiry <= block_timestamp && block_timestamp < entry.expiry + EXPIRY_GRACE_SECONDS
        })
        .collect()
}

/// Returns every distinct SY in the registry, in the order the markets first named them.
///
/// Not filtered by expiry, and deliberately so: an SY is an ERC-5115 wrapper with no maturity of
/// its own. It goes on wrapping and unwrapping after every market that ever priced against it has
/// expired, so its rate has to keep being refreshed for as long as it is a component. Tying that
/// to the markets would quietly retire a wrapper that still trades.
pub fn sy_components(registry: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for entry in entries(registry) {
        if !seen.contains(&entry.sy) {
            seen.push(entry.sy);
        }
    }
    seen
}

/// Parses the registry, skipping lines that do not decode.
fn entries(registry: &str) -> impl Iterator<Item = MarketEntry> + '_ {
    registry
        .split(';')
        .filter(|line| !line.is_empty())
        .filter_map(MarketEntry::decode)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FACTORY_V1: &str = "27b1dacd74688af24a64bd3c9c1b143118740784";

    fn entry(id: &str, sy: &str, expiry: u64) -> MarketEntry {
        MarketEntry {
            id: id.to_string(),
            sy: sy.to_string(),
            expiry,
            factory: FACTORY_V1.to_string(),
            ln_fee_rate_root_at_creation: "0".to_string(),
        }
    }

    /// The wstETH market and its SY, with the expiry the brief quotes.
    fn wsteth() -> MarketEntry {
        entry(
            "0x34280882267ffa6383b363e278b027be083bbe3b",
            "0xcbc72d92b2dc8187414f6734718563898740c0bc",
            1830124800,
        )
    }

    #[test]
    fn a_market_survives_the_round_trip() {
        let registry = format!("{};", wsteth().encode());
        assert_eq!(live_markets(&registry, 0), vec![wsteth()]);
    }

    /// `expiry` itself is already dead: the market reverts at `block.timestamp >= expiry`.
    #[test]
    fn expiry_is_exclusive() {
        let registry = format!("{};", wsteth().encode());
        assert_eq!(live_markets(&registry, 1830124799).len(), 1);
        assert!(live_markets(&registry, 1830124800).is_empty());
    }

    /// The tombstone: the first refresh at or after expiry republishes the clock, which is the
    /// only emission a market ever gets carrying a timestamp it is dead at.
    #[test]
    fn the_clock_outlives_the_market() {
        let registry = format!("{};", wsteth().encode());
        assert!(expired_markets(&registry, 1830124799).is_empty());
        assert_eq!(expired_markets(&registry, 1830124800), vec![wsteth()]);
    }

    /// A market is either quoted or buried, never both — the two sets partition the registry.
    #[test]
    fn live_and_expired_never_overlap() {
        let registry = format!("{};", wsteth().encode());
        for timestamp in [0, 1830124799, 1830124800, 1830124800 + EXPIRY_GRACE_SECONDS] {
            assert!(
                live_markets(&registry, timestamp).is_empty() ||
                    expired_markets(&registry, timestamp).is_empty(),
                "both sets are non-empty at {timestamp}"
            );
        }
    }

    /// The clock stops once the grace window closes: a consumer that has not seen the tombstone
    /// by then was not listening at all.
    #[test]
    fn the_grace_window_closes() {
        let registry = format!("{};", wsteth().encode());
        let last = 1830124800 + EXPIRY_GRACE_SECONDS - 1;
        assert_eq!(expired_markets(&registry, last).len(), 1);
        assert!(expired_markets(&registry, last + 1).is_empty());
    }

    /// The registry is append-only, so one bad line must not take out every later block.
    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let registry =
            format!("garbage;{}|{}|nan|{}|0;{};", "0xaa", "0xbb", FACTORY_V1, wsteth().encode());
        assert_eq!(live_markets(&registry, 0), vec![wsteth()]);
    }

    #[test]
    fn an_empty_registry_yields_nothing() {
        assert!(live_markets("", 0).is_empty());
        assert!(live_markets(";;", 0).is_empty());
    }

    /// One SY backs several expiries, and it outlives all of them.
    #[test]
    fn every_sy_is_listed_once_however_many_markets_it_backs() {
        let second = entry("0xdead", &wsteth().sy, 2_000_000_000);
        let third = entry("0xbeef", "0xfeed", 1_000);
        let registry = format!("{};{};{};", wsteth().encode(), second.encode(), third.encode());
        assert_eq!(sy_components(&registry), vec![wsteth().sy, "0xfeed".to_string()]);
    }

    /// The fan-out for a factory-wide fee change selects on this.
    #[test]
    fn a_market_knows_its_factory() {
        let market = wsteth();
        assert!(market.is_from(&hex::decode(FACTORY_V1).unwrap()));
        assert!(!market.is_from(&hex::decode("6d247b1c044fa1e22e6b04fa9f71baf99eb29a9f").unwrap()));
    }

    #[test]
    fn markets_are_returned_in_registry_order() {
        let second = entry("0xdead", "0xbeef", 2_000_000_000);
        let registry = format!("{};{};", wsteth().encode(), second.encode());
        assert_eq!(live_markets(&registry, 0), vec![wsteth(), second]);
    }
}
