pub mod execution;
pub mod rpc_tools;
pub mod validation;
pub use rpc_tools::RPCTools;

pub(crate) fn is_block_not_found(msg: &str) -> bool {
    msg.contains("header not found") || (msg.contains("block #") && msg.contains("not found"))
}
