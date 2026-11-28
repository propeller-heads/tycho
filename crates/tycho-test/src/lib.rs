pub mod checks;
pub mod execution;
pub mod expectations;
pub mod rpc_tools;
pub mod token_prices;
pub mod validation;
pub use rpc_tools::RPCTools;

/// Returns true if an RPC error indicates the queried block is not yet available on the node,
/// i.e. the node lags behind Tycho. Providers phrase this differently: geth returns
/// "header not found", others return "block not found" (optionally suffixed with the block, e.g.
/// Base's `error code -32001: block not found: 0x...`), "block #<n> not found", or, on the prod
/// Ethereum RPC, `error code -32000: block 0x<hex number> not found`.
pub fn is_block_not_found(msg: &str) -> bool {
    let msg = msg.to_lowercase();
    msg.contains("header not found") ||
        msg.contains("block not found") ||
        (msg.contains("block #") && msg.contains("not found")) ||
        names_hex_block_not_found(&msg)
}

/// Whether lowercase `msg` contains `block 0x<hex digits> not found`.
fn names_hex_block_not_found(msg: &str) -> bool {
    msg.match_indices("block 0x")
        .any(|(start, prefix)| {
            let rest = &msg[start + prefix.len()..];
            let digits = rest.len() -
                rest.trim_start_matches(|c: char| c.is_ascii_hexdigit())
                    .len();
            digits > 0 && rest[digits..].starts_with(" not found")
        })
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
        // block-hash phrasing of the prod Ethereum RPC, as the simulator wraps it
        assert!(is_block_not_found(
            "Transaction failed: server returned an error response: error code -32000: block \
             0x1940520 not found"
        ));
        assert!(is_block_not_found("block 0xAbC not found"));
    }

    #[test]
    fn ignores_genuine_reverts() {
        assert!(!is_block_not_found("execution reverted"));
        assert!(!is_block_not_found("TychoRouter__NegativeSlippage(1000, 990)"));
        assert!(!is_block_not_found("UniswapV2: K"));
        assert!(!is_block_not_found("Component not found"));
        assert!(!is_block_not_found("block 0x not found"));
        assert!(!is_block_not_found("block 0x1940520 reverted: pool not found"));
    }
}
