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

/// Component id of a contract, matching `ProtocolComponent::at_contract`.
pub fn contract_id(address: &[u8]) -> String {
    format!("0x{}", hex::encode(address))
}
