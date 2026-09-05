//! Key namespaces for `store_protocol_components`.
//!
//! The store holds three kinds of entry under one keyspace, so each kind carries a prefix rather
//! than relying on the shape of the value to tell them apart.

/// Key under which a market's `[sy, pt, yt]` token list is stored, given the market component id.
pub fn market_tokens_key(component_id: &str) -> String {
    format!("market:{component_id}")
}

/// Key mapping a yield token address to the id of the market that owns it.
pub fn market_by_yt_key(yt_address: &[u8]) -> String {
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
