use tycho_substreams::prelude as tycho;

pub mod attrs {
    pub const ANCHOR_PRICE_X96: &str = "anchor_price_x96";
    pub const FEE_ASK_X24: &str = "fee_ask_x24";
    pub const FEE_BID_X24: &str = "fee_bid_x24";
    pub const LATEST_UPDATE_BLOCK: &str = "latest_update_block";
    pub const RESERVE_X: &str = "reserve_x";
    pub const RESERVE_Y: &str = "reserve_y";
    pub const CONCENTRATION_K: &str = "concentration_k";
    pub const BLOCK_DELAY: &str = "block_delay";
    pub const PAUSED: &str = "paused";
}

pub fn attribute(name: &'static str, value: Vec<u8>) -> tycho::Attribute {
    tycho::Attribute { name: name.to_owned(), value, change: tycho::ChangeType::Update.into() }
}

pub fn creation_attribute(name: &'static str, value: Vec<u8>) -> tycho::Attribute {
    tycho::Attribute { name: name.to_owned(), value, change: tycho::ChangeType::Creation.into() }
}
