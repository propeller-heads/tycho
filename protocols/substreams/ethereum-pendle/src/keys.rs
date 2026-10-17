//! Store keys, each kind carrying a prefix so keys sharing a store cannot collide.

/// Key under which a market's `[sy, pt, yt]` token list is stored, given the market component id.
pub fn market_tokens_key(component_id: &str) -> String {
    format!("market:{component_id}")
}

/// Key under which `store_markets_by_yt` lists every market a yield token backs.
pub fn markets_by_yt_key(yt_address: &[u8]) -> String {
    format!("yt:0x{}", hex::encode(yt_address))
}

/// Component id of a contract: the address, hex-encoded and `0x`-prefixed.
pub fn contract_id(address: &[u8]) -> String {
    format!("0x{}", hex::encode(address))
}

/// The single key under which `store_market_registry` keeps every market it has seen.
///
/// Substreams stores are key-addressed and cannot be enumerated — `StoreGet` offers only
/// `get_at` / `get_last` / `get_first` on one key — so the set of markets has to live as a
/// *value* rather than as a range of keys. See `crate::registry`.
pub const MARKET_REGISTRY: &str = "markets";

/// Key under which a market's last known `pyIndexStored` is kept, given its component id.
pub fn py_index_key(component_id: &str) -> String {
    format!("py_index:{component_id}")
}

/// Whether an SY has a token converted at its exchange rate, and so needs that rate read.
pub fn sy_rate_needed_key(sy_id: &str) -> String {
    format!("rate_needed:{sy_id}")
}
