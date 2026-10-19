use anyhow::{anyhow, bail};
use ethabi::{decode, ethereum_types::U256, ParamType, Token, Uint};
use substreams_ethereum::pb::eth::v2::Call;
use tiny_keccak::{Hasher, Keccak};

pub fn encode_pool_hash(token_x: Vec<u8>, token_y: Vec<u8>, pool_idx: Vec<u8>) -> [u8; 32] {
    let base_address = ethabi::Address::from_slice(&token_x);
    let quote_address = ethabi::Address::from_slice(&token_y);
    let pool_idx_uint = Uint::from_big_endian(&pool_idx);

    let encoded = ethabi::encode(&[
        Token::Address(base_address),
        Token::Address(quote_address),
        Token::Uint(pool_idx_uint),
    ]);

    let mut hasher = Keccak::v256();
    hasher.update(&encoded);
    let mut output = [0u8; 32];
    hasher.finalize(&mut output);

    output
}

pub fn decode_flows_from_output(call: &Call) -> Result<(ethabi::Int, ethabi::Int), anyhow::Error> {
    if let Ok(external_outputs) = decode(BASE_QUOTE_FLOW_OUTPUT, &call.return_data) {
        let base_flow = external_outputs[0]
            .to_owned()
            .into_int() // Needs conversion into bytes for next step
            .ok_or_else(|| anyhow!("Failed to convert base flow to i128".to_string()))?;

        let quote_flow = external_outputs[1]
            .to_owned()
            .into_int() // Needs conversion into bytes for next step
            .ok_or_else(|| anyhow!("Failed to convert quote flow to i128".to_string()))?;
        Ok((base_flow, quote_flow))
    } else {
        bail!("Failed to decode swap call outputs.".to_string());
    }
}

const BASE_QUOTE_FLOW_OUTPUT: &[ParamType] = &[
    // The token base and quote token flows associated with this swap action.
    // Negative indicates a credit paid to the user (token balance of pool
    // decreases), positive a debit collected from the user (token balance of pool
    // increases).
    ParamType::Int(128), // baseFlow
    ParamType::Int(128), // quoteFlow
];

pub fn from_u256_to_vec(src: U256) -> Vec<u8> {
    let mut buf = [0u8; 32];
    src.to_big_endian(&mut buf);
    buf.to_vec()
}
