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
    registry
        .split(';')
        .filter(|line| !line.is_empty())
        .filter_map(MarketEntry::decode)
        .filter(|entry| block_timestamp < entry.expiry)
        .collect()
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
