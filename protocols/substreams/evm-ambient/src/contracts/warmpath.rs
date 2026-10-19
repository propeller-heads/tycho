use anyhow::{anyhow, bail};

use crate::utils::{decode_flows_from_output, encode_pool_hash};
use ethabi::{decode, ParamType};
use hex_literal::hex;
use substreams_ethereum::pb::eth::v2::Call;

pub const AMBIENT_WARMPATH_CONTRACT: [u8; 20] = hex!("d268767BE4597151Ce2BB4a70A9E368ff26cB195");
pub const USER_CMD_WARMPATH_FN_SIG: [u8; 4] = hex!("f96dc788");
const USER_CMD_EXTERNAL_ABI: &[ParamType] = &[
    ParamType::Bytes, // userCmd
];

const LIQUIDITY_CHANGE_ABI: &[ParamType] = &[
    ParamType::Uint(8),
    ParamType::Address,   // base
    ParamType::Address,   // quote
    ParamType::Uint(256), // pool index
    ParamType::Int(256),
    ParamType::Uint(128),
    ParamType::Uint(128),
    ParamType::Uint(128),
    ParamType::Uint(8),
    ParamType::Address,
];
pub fn decode_warm_path_user_cmd_call(
    call: &Call,
) -> Result<Option<([u8; 32], ethabi::Int, ethabi::Int)>, anyhow::Error> {
    if let Ok(external_cmd) = decode(USER_CMD_EXTERNAL_ABI, &call.input[4..]) {
        let input_bytes = external_cmd[0]
            .to_owned()
            .into_bytes() // Convert Bytes32 to Vec<u8>
            .ok_or_else(|| anyhow!("Failed to hotproxy userCmd input data.".to_string()))?;

        let code = input_bytes[31];
        let is_mint = [1, 11, 12, 3, 31, 32].contains(&code);
        let is_burn = [2, 21, 22, 4, 41, 42].contains(&code);
        let is_harvest = code == 5;
        if is_mint || is_burn || is_harvest {
            if let Ok(liquidity_change_calldata) = decode(LIQUIDITY_CHANGE_ABI, &input_bytes) {
                let base_token = liquidity_change_calldata[1]
                    .to_owned()
                    .into_address()
                    .ok_or_else(|| {
                        anyhow!(
                        "Failed to convert base token to address for WarmPath userCmd call: {:?}",
                        &liquidity_change_calldata[1]
                    )
                    })?
                    .to_fixed_bytes()
                    .to_vec();
                let quote_token = liquidity_change_calldata[2]
                    .to_owned()
                    .into_address()
                    .ok_or_else(|| {
                        anyhow!(
                        "Failed to convert quote token to address for WarmPath userCmd call: {:?}",
                        &liquidity_change_calldata[2]
                    )
                    })?
                    .to_fixed_bytes()
                    .to_vec();

                let mut pool_index_buf = [0u8; 32];
                liquidity_change_calldata[3]
                    .to_owned()
                    .into_uint()
                    .ok_or_else(|| {
                        anyhow!("Failed to convert pool index to bytes for WarmPath userCmd call"
                            .to_string())
                    })?
                    .to_big_endian(&mut pool_index_buf);
                let pool_index = pool_index_buf.to_vec();

                let (base_flow, quote_flow) = decode_flows_from_output(call)?;
                let pool_hash = encode_pool_hash(base_token, quote_token, pool_index);
                Ok(Some((pool_hash, base_flow, quote_flow)))
            } else {
                bail!("Failed to decode inputs for WarmPath userCmd call.".to_string());
            }
        } else {
            Ok(None)
        }
    } else {
        bail!("Failed to decode WarmPath call external input.".to_string());
    }
}
