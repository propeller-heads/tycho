use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Aqua0Market {
    pub pool_id: String,
    pub class_id: String,
    pub amount0_samples: Vec<String>,
    pub amount1_samples: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Aqua0Range {
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Aqua0Level {
    pub requested_amount_in: String,
    pub amount_in: String,
    pub amount_out: String,
    pub fully_supported: bool,
    pub current_tick: i32,
    pub sqrt_price_x96: String,
    pub ranges: Vec<Aqua0Range>,
    pub route_plan: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Aqua0Directions {
    pub zero_for_one: Vec<Aqua0Level>,
    pub one_for_zero: Vec<Aqua0Level>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Aqua0StateResponse {
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

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Aqua0QuoteRequest {
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

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Aqua0QuoteResponse {
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
    pub swap_id: String,
    pub nonce: String,
    pub deadline: String,
    pub ranges: Vec<Aqua0Range>,
}
