pub mod price_level_stream_processor;
pub mod protocol_stream_processor;
pub mod rfq_stream_processor;

use std::{collections::HashMap, fmt, fmt::Display, sync::Arc};

use tycho_simulation::{book::models::Book, protocol::models::Update};

#[derive(Debug)]
pub struct StreamUpdate {
    pub payload: StreamUpdatePayload,
    pub is_first_update: bool,
    pub received_at: std::time::Duration,
}

#[derive(Debug)]
pub enum StreamUpdatePayload {
    Protocol(Update),
    /// A sampled view of one RFQ provider's complete book.
    Rfq {
        protocol_system: String,
        /// When the feed received the snapshot the books are sampled from.
        received_at: chrono::DateTime<chrono::Utc>,
        books: Arc<HashMap<String, Book>>,
    },
    PriceLevelStream(Update),
}

impl Display for StreamUpdatePayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamUpdatePayload::Protocol(_) => write!(f, "Protocol"),
            StreamUpdatePayload::Rfq { .. } => write!(f, "RFQ"),
            StreamUpdatePayload::PriceLevelStream(_) => write!(f, "PriceLevelStream"),
        }
    }
}
