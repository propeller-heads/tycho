pub mod price_level_stream_processor;
pub mod protocol_stream_processor;
pub mod rfq_stream_processor;

use std::{fmt, fmt::Display};

use tycho_simulation::protocol::models::Update;

#[derive(Debug)]
pub struct StreamUpdate {
    pub update_type: UpdateType,
    pub update: Update,
    pub is_first_update: bool,
    pub received_at: std::time::Duration,
}

#[derive(Debug, PartialEq)]
pub enum UpdateType {
    Protocol,
    Rfq,
    PriceLevelStream,
}

impl Display for UpdateType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpdateType::Protocol => write!(f, "Protocol"),
            UpdateType::Rfq => write!(f, "RFQ"),
            UpdateType::PriceLevelStream => write!(f, "PriceLevelStream"),
        }
    }
}
