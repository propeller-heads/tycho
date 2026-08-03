pub mod execution;
pub mod rpc_tools;
pub mod token_prices;
pub mod validation;
pub use rpc_tools::RPCTools;

/// Returns true if an RPC error indicates the queried block is not yet available on the node,
/// i.e. the node lags behind Tycho. Providers phrase this differently: geth returns
/// "header not found", others return "block not found" (optionally suffixed with the block, e.g.
/// Base's `error code -32001: block not found: 0x...`) or "block #<n> not found".
pub fn is_block_not_found(msg: &str) -> bool {
    let msg = msg.to_lowercase();
    msg.contains("header not found") ||
        msg.contains("block not found") ||
        (msg.contains("block #") && msg.contains("not found"))
}

#[cfg(test)]
mod tests {
    use super::is_block_not_found;

    #[test]
    fn detects_block_not_found_variants() {
        // Base / load-balanced endpoints (JSON-RPC error code -32001)
        assert!(is_block_not_found(
            "getReserves call reverted: Call reverted: server returned an error response: \
             error code -32001: block not found: 0x2dd647d"
        ));
        // geth
        assert!(is_block_not_found("header not found"));
        // block-number phrasing
        assert!(is_block_not_found("block #12345 not found"));
        // case-insensitive
        assert!(is_block_not_found("Block Not Found"));
    }

    #[test]
    fn ignores_genuine_reverts() {
        assert!(!is_block_not_found("execution reverted"));
        assert!(!is_block_not_found("TychoRouter__NegativeSlippage(1000, 990)"));
        assert!(!is_block_not_found("UniswapV2: K"));
    }
}
