mod db_out;
mod fee_config;
mod trades;

pub use db_out::db_out;
pub use fee_config::{map_fee_config_events, store_fee_config};
pub use trades::map_trades;

/// Store key prefixes shared between the fee-config store writer and the trades reader.
pub(crate) mod keys {
    pub fn router_fee_calculator(router: &[u8]) -> String {
        format!("router:{}:fc", hex::encode(router))
    }
    pub fn fee_on_output(fc: &[u8]) -> String {
        format!("fc:{}:output", hex::encode(fc))
    }
    pub fn fee_on_client_fee(fc: &[u8]) -> String {
        format!("fc:{}:client", hex::encode(fc))
    }
    pub fn custom_fee_on_output(fc: &[u8], client: &[u8]) -> String {
        format!("fc:{}:custom_output:{}", hex::encode(fc), hex::encode(client))
    }
    pub fn custom_fee_on_client_fee(fc: &[u8], client: &[u8]) -> String {
        format!("fc:{}:custom_client:{}", hex::encode(fc), hex::encode(client))
    }
    pub fn positive_slippage(fc: &[u8]) -> String {
        format!("fc:{}:pos_slip", hex::encode(fc))
    }
}

/// Event names emitted in `FeeConfigEvent.event`.
pub(crate) mod events {
    pub const ROUTER_FEE_ON_OUTPUT_UPDATED: &str = "RouterFeeOnOutputUpdated";
    pub const ROUTER_FEE_ON_CLIENT_FEE_UPDATED: &str = "RouterFeeOnClientFeeUpdated";
    pub const CUSTOM_FEE_ON_OUTPUT_UPDATED: &str = "CustomRouterFeeOnOutputUpdated";
    pub const CUSTOM_FEE_ON_CLIENT_FEE_UPDATED: &str = "CustomRouterFeeOnClientFeeUpdated";
    pub const CUSTOM_FEE_ON_OUTPUT_REMOVED: &str = "CustomRouterFeeOnOutputRemoved";
    pub const CUSTOM_FEE_ON_CLIENT_FEE_REMOVED: &str = "CustomRouterFeeOnClientFeeRemoved";
    pub const ROUTER_FEE_RECEIVER_UPDATED: &str = "RouterFeeReceiverUpdated";
    pub const POSITIVE_SLIPPAGE_TOGGLED: &str = "PositiveSlippageToggled";
    pub const FEE_CALCULATOR_SET: &str = "FeeCalculatorSet";
    pub const FEE_CALCULATOR_ACTIVATED: &str = "FeeCalculatorActivated";
    pub const FEE_CALCULATOR_UPDATED: &str = "FeeCalculatorUpdated";
}

pub(crate) fn hex_addr(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

pub(crate) fn block_timestamp(block: &substreams_ethereum::pb::eth::v2::Block) -> u64 {
    block
        .header
        .as_ref()
        .and_then(|h| h.timestamp.as_ref())
        .map(|t| t.seconds as u64)
        .unwrap_or_default()
}
