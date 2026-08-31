use std::{
    collections::HashMap,
    io::{self, Read},
    str::FromStr,
};

use alloy::{hex, primitives::Keccak256, sol_types::SolValue};
use num_bigint::BigUint;
use serde::{Deserialize, Deserializer, Serialize};
use tycho_common::{
    models::{protocol::ProtocolComponent, token::Token, Chain},
    Bytes,
};
use tycho_execution::encoding::{
    evm::{
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
        utils::{biguint_to_u256, bytes_to_address},
    },
    models::{ClientFeeParams, Solution, Swap},
};

fn deserialize_biguint<'de, D>(deserializer: D) -> Result<BigUint, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    BigUint::from_str(&value).map_err(serde::de::Error::custom)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EncodeRequest {
    chain: Chain,
    router: Bytes,
    filler: Bytes,
    component_id: String,
    pool_manager: Bytes,
    token_in: Bytes,
    token_out: Bytes,
    #[serde(deserialize_with = "deserialize_biguint")]
    amount_in: BigUint,
    #[serde(deserialize_with = "deserialize_biguint")]
    expected_amount_out: BigUint,
    #[serde(deserialize_with = "deserialize_biguint")]
    min_amount_out: BigUint,
    fee: u32,
    tick_spacing: i32,
    hooks: Bytes,
    hook_data: Bytes,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EncodeResponse {
    callback_data: String,
    tycho_calldata: String,
    function_signature: String,
}

fn input_selector(signature: &str, encoded_args: Vec<u8>) -> Vec<u8> {
    let mut hasher = Keccak256::new();
    hasher.update(signature.as_bytes());
    let mut call_data = hasher.finalize()[..4].to_vec();
    call_data.extend(encoded_args);
    call_data
}

fn build_callback_data(request: &EncodeRequest) -> Result<EncodeResponse, String> {
    if request.amount_in == BigUint::ZERO {
        return Err("amountIn must be positive".into());
    }
    if request.expected_amount_out == BigUint::ZERO {
        return Err("expectedAmountOut must be positive".into());
    }
    if request.min_amount_out == BigUint::ZERO
        || request.min_amount_out > request.expected_amount_out
    {
        return Err("minAmountOut must be positive and no greater than expectedAmountOut".into());
    }

    let mut static_attributes = HashMap::new();
    static_attributes.insert(
        "key_lp_fee".into(),
        request
            .fee
            .to_be_bytes()
            .to_vec()
            .into(),
    );
    static_attributes.insert(
        "tick_spacing".into(),
        request
            .tick_spacing
            .to_be_bytes()
            .to_vec()
            .into(),
    );
    static_attributes.insert("hooks".into(), request.hooks.clone());

    let component = ProtocolComponent {
        id: request.component_id.clone(),
        protocol_system: "uniswap_v4".into(),
        protocol_type_name: "uniswap_v4_pool".into(),
        chain: request.chain,
        tokens: vec![request.token_in.clone(), request.token_out.clone()],
        contract_addresses: vec![request.pool_manager.clone(), request.hooks.clone()],
        static_attributes,
        ..Default::default()
    };
    let input = Token::new(&request.token_in, "TOKEN_IN", 0, 0, &[], request.chain, 100);
    let output = Token::new(&request.token_out, "TOKEN_OUT", 0, 0, &[], request.chain, 100);
    let user_data = serde_json::to_vec(&serde_json::json!({
        "hook_data": request.hook_data.to_string(),
    }))
    .map_err(|error| format!("failed to encode V4 hookData: {error}"))?;
    let swap = Swap::new(component, input, output, BigUint::ZERO).with_user_data(user_data.into());
    let solution = Solution::new(
        request.filler.clone(),
        request.filler.clone(),
        request.token_in.clone(),
        request.token_out.clone(),
        request.amount_in.clone(),
        request.expected_amount_out.clone(),
        request.min_amount_out.clone(),
        vec![swap],
    );

    let registry = SwapEncoderRegistry::new(request.chain)
        .add_default_encoders(None)
        .map_err(|error| format!("failed to initialize Tycho encoders: {error}"))?;
    let encoder = TychoRouterEncoderBuilder::new()
        .chain(request.chain)
        .swap_encoder_registry(registry)
        .router_address(request.router.clone())
        .build()
        .map_err(|error| format!("failed to initialize Tycho Router V3 encoder: {error}"))?;
    let encoded = encoder
        .encode_solutions(vec![solution])
        .map_err(|error| format!("failed to encode Aqua0 V4 swap: {error}"))?
        .into_iter()
        .next()
        .ok_or_else(|| "Tycho returned no encoded solution".to_string())?;

    let client_fee_params = ClientFeeParams::default().into_abi_params();
    let method_args = (
        biguint_to_u256(&request.amount_in),
        bytes_to_address(&request.token_in).map_err(|error| format!("invalid tokenIn: {error}"))?,
        bytes_to_address(&request.token_out)
            .map_err(|error| format!("invalid tokenOut: {error}"))?,
        biguint_to_u256(&request.expected_amount_out),
        biguint_to_u256(&request.min_amount_out),
        bytes_to_address(&request.filler).map_err(|error| format!("invalid filler: {error}"))?,
        client_fee_params,
        encoded.swaps().to_vec(),
    )
        .abi_encode();
    let tycho_calldata = input_selector(encoded.function_signature(), method_args);

    // Both approvals are safe to request repeatedly because the upstream filler uses forceApprove.
    // Avoiding two RPC allowance reads keeps the binding quote-to-submit window short.
    let callback_data = (true, true, tycho_calldata.as_slice()).abi_encode_packed();

    Ok(EncodeResponse {
        callback_data: format!("0x{}", hex::encode(callback_data)),
        tycho_calldata: format!("0x{}", hex::encode(tycho_calldata)),
        function_signature: encoded.function_signature().into(),
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    if input.trim().is_empty() {
        return Err("No input provided. Expected JSON on stdin.".into());
    }
    let request: EncodeRequest = serde_json::from_str(&input)?;
    let response = build_callback_data(&request)?;
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn request() -> EncodeRequest {
        EncodeRequest {
            chain: Chain::Base,
            router: Bytes::from_str("0x9bA632d83e9eF57571256Cf4cc951b8aF1158e9C").unwrap(),
            filler: Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap(),
            component_id: format!("0x{}", "22".repeat(32)),
            pool_manager: Bytes::from_str("0x498581fF718922c3f8e6A244956aF099B2652b2b").unwrap(),
            token_in: Bytes::from_str("0x4200000000000000000000000000000000000006").unwrap(),
            token_out: Bytes::from_str("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913").unwrap(),
            amount_in: BigUint::from(1_000u32),
            expected_amount_out: BigUint::from(2_000u32),
            min_amount_out: BigUint::from(1_900u32),
            fee: 3_000,
            tick_spacing: 60,
            hooks: Bytes::from_str("0xACaF2945890AB6caea62bDa459d1922532A500C8").unwrap(),
            hook_data: Bytes::from_str("0xdeadbeef").unwrap(),
        }
    }

    #[test]
    fn encodes_existing_filler_callback_and_forwards_hook_data() {
        let response = build_callback_data(&request()).unwrap();
        let callback = hex::decode(
            response
                .callback_data
                .trim_start_matches("0x"),
        )
        .unwrap();
        let expected_selector = input_selector(&response.function_signature, Vec::new());

        assert_eq!(&callback[..2], &[1, 1]);
        assert_eq!(&callback[2..6], &expected_selector[..4]);
        assert!(callback
            .windows(4)
            .any(|window| window == [0xde, 0xad, 0xbe, 0xef]));
        assert!(response
            .function_signature
            .starts_with("singleSwap("));
    }

    #[test]
    fn rejects_an_invalid_output_floor() {
        let mut invalid = request();
        invalid.min_amount_out = BigUint::from(2_001u32);
        assert!(build_callback_data(&invalid)
            .unwrap_err()
            .contains("minAmountOut"));
    }
}
