//! The set of markets, materialised under one store key.
//!
//! Refreshing every live market's PY index once per block means asking "which markets are live
//! right now?" on a block where nothing happened — there is no log to start from. Substreams
//! stores cannot answer that: reads are `get_at` / `get_last` / `get_first` against a single key,
//! with no iterator and no prefix scan. So the set is kept as the *value* of one key in an
//! `append` store, and parsed back here.
//!
//! One line per market, `;`-separated by `StoreAppend`, fields `|`-separated:
//! `<market component id>|<SY component id>|<expiry>`. At ~75 bytes per line and 484 markets
//! created on Ethereum since 2023, the whole registry is under 40 KB.

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
}

impl MarketEntry {
    /// Renders the entry as one registry line.
    pub fn encode(&self) -> String {
        let mut line = String::with_capacity(self.id.len() + self.sy.len() + 16);
        let _ = write!(line, "{}|{}|{}", self.id, self.sy, self.expiry);
        line
    }

    fn decode(line: &str) -> Option<Self> {
        let mut fields = line.split('|');
        let id = fields.next()?;
        let sy = fields.next()?;
        let expiry = fields.next()?.parse().ok()?;
        if fields.next().is_some() || id.is_empty() || sy.is_empty() {
            return None;
        }
        Some(MarketEntry { id: id.to_string(), sy: sy.to_string(), expiry })
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

    fn entry(id: &str, sy: &str, expiry: u64) -> MarketEntry {
        MarketEntry { id: id.to_string(), sy: sy.to_string(), expiry }
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
        let registry = format!("garbage;{}|{}|nan;{};", "0xaa", "0xbb", wsteth().encode());
        assert_eq!(live_markets(&registry, 0), vec![wsteth()]);
    }

    #[test]
    fn an_empty_registry_yields_nothing() {
        assert!(live_markets("", 0).is_empty());
        assert!(live_markets(";;", 0).is_empty());
    }

    #[test]
    fn markets_are_returned_in_registry_order() {
        let second = entry("0xdead", "0xbeef", 2_000_000_000);
        let registry = format!("{};{};", wsteth().encode(), second.encode());
        assert_eq!(live_markets(&registry, 0), vec![wsteth(), second]);
    }
}
