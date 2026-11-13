//! Aqua0's Tycho RFQ API, as it answers on the wire.

use serde::{Deserialize, Serialize};

/// `GET /markets`: every pool on one chain where Aqua0 quotes, with its strategy id.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Aqua0MarketsResponse {
    pub markets: Vec<Aqua0Market>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Aqua0Market {
    pub chain_id: u64,
    pub pool_id: String,
    pub class_id: String,
    pub tokens: [String; 2],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Aqua0Range {
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity: String,
}

/// One sampled exact-input fill. `amount_in` is what the binding quote for that size signs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Aqua0Level {
    pub amount_in: String,
    pub amount_out: String,
    pub fully_supported: bool,
    /// The JIT ranges the fill draws on; the swap's gas grows with their count.
    pub ranges: Vec<Aqua0Range>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Aqua0Directions {
    pub zero_for_one: Vec<Aqua0Level>,
    pub one_for_zero: Vec<Aqua0Level>,
}

/// `GET /state`: indicative levels for one pool, plus the V4 pool key the fill goes through.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Aqua0StateResponse {
    pub schema_version: String,
    pub component_id: String,
    pub protocol_system: String,
    pub protocol_type_name: String,
    pub chain_id: u64,
    pub pool_id: String,
    pub class_id: String,
    pub pool_manager: String,
    pub tokens: [String; 2],
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: String,
    pub directions: Aqua0Directions,
    pub generated_at: u64,
    pub expires_at: u64,
    pub state_version: String,
}

/// The body of an Aqua0 error answer. Every refusal carries a machine `code`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Aqua0ErrorBody {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub retry_after_seconds: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Aqua0QuoteRequest {
    pub request_id: String,
    pub component_id: String,
    pub chain_id: u64,
    pub pool_id: String,
    pub class_id: String,
    pub token_in: String,
    pub token_out: String,
    pub amount_in: String,
    pub expected_router: String,
}

/// `POST /quote`: a binding approval, signed V4 hook data for one exact-input fill.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Aqua0QuoteResponse {
    pub schema_version: String,
    pub request_id: String,
    pub component_id: String,
    pub chain_id: u64,
    pub token_in: String,
    pub token_out: String,
    pub amount_in: String,
    pub amount_out: String,
    pub router: String,
    pub executor: String,
    pub hook_data: String,
    pub deadline: String,
}
