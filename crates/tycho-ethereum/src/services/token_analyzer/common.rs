use alloy::{
    primitives::Address,
    rpc::types::{BlockNumberOrTag, TransactionInput, TransactionRequest},
};
use tycho_common::models::blockchain::BlockTag;

/// Converts a tycho BlockTag to an alloy BlockNumberOrTag.
pub fn map_block_tag(block: BlockTag) -> BlockNumberOrTag {
    match block {
        BlockTag::Finalized => BlockNumberOrTag::Finalized,
        BlockTag::Safe => BlockNumberOrTag::Safe,
        BlockTag::Latest => BlockNumberOrTag::Latest,
        BlockTag::Earliest => BlockNumberOrTag::Earliest,
        BlockTag::Pending => BlockNumberOrTag::Pending,
        BlockTag::Number(n) => BlockNumberOrTag::Number(n),
    }
}

/// Builds a `TransactionRequest` for a read-only or impersonated call used in trace simulations.
pub fn call_request(from: Option<Address>, to: Address, calldata: Vec<u8>) -> TransactionRequest {
    let mut req = TransactionRequest::default()
        .to(to)
        .input(TransactionInput::both(calldata.into()));

    if let Some(addr) = from {
        req = req.from(addr);
    }

    req
}

#[cfg(test)]
mod tests {
    use alloy::rpc::types::BlockNumberOrTag;
    use tycho_common::models::blockchain::BlockTag;

    use super::map_block_tag;

    #[test]
    fn test_map_block_tag() {
        assert_eq!(map_block_tag(BlockTag::Finalized), BlockNumberOrTag::Finalized);
        assert_eq!(map_block_tag(BlockTag::Safe), BlockNumberOrTag::Safe);
        assert_eq!(map_block_tag(BlockTag::Latest), BlockNumberOrTag::Latest);
        assert_eq!(map_block_tag(BlockTag::Earliest), BlockNumberOrTag::Earliest);
        assert_eq!(map_block_tag(BlockTag::Pending), BlockNumberOrTag::Pending);
        assert_eq!(map_block_tag(BlockTag::Number(123)), BlockNumberOrTag::Number(123));
    }
}
