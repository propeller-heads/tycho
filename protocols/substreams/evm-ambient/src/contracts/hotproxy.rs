use anyhow::{anyhow, bail};

use crate::utils::{decode_flows_from_output, encode_pool_hash};
use ethabi::{decode, ParamType};
use hex_literal::hex;
use substreams_ethereum::pb::eth::v2::Call;

pub const AMBIENT_HOTPROXY_CONTRACT: [u8; 20] = hex!("37e00522Ce66507239d59b541940F99eA19fF81F");
pub const USER_CMD_HOTPROXY_FN_SIG: [u8; 4] = hex!("f96dc788");

pub const SWAP_ABI_HOTPROXY_INPUT: &[ParamType] = &[
    ParamType::Address,   // base
    ParamType::Address,   // quote
    ParamType::Uint(256), // pool index
    // isBuy - if true the direction of the swap is for the user to send base
    // tokens and receive back quote tokens.
    ParamType::Bool,
    ParamType::Bool,      // inBaseQty
    ParamType::Uint(128), //qty
    ParamType::Uint(16),  // poolTip
    ParamType::Uint(128), // limitPrice
    ParamType::Uint(128), // minOut
    ParamType::Uint(8),   // reserveFlags
];
const USER_CMD_EXTERNAL_ABI: &[ParamType] = &[
    ParamType::Bytes, // userCmd
];

pub fn decode_direct_swap_hotproxy_call(
    call: &Call,
) -> Result<([u8; 32], ethabi::Int, ethabi::Int), anyhow::Error> {
    if let Ok(external_cmd) = decode(USER_CMD_EXTERNAL_ABI, &call.input[4..]) {
        let input_bytes = external_cmd[0]
            .to_owned()
            .into_bytes() // Convert Bytes32 to Vec<u8>
            .ok_or_else(|| anyhow!("Failed to hotproxy userCmd input data.".to_string()))?;

        if let Ok(input_params) = decode(SWAP_ABI_HOTPROXY_INPUT, &input_bytes) {
            let base_token = input_params[0]
                .to_owned()
                .into_address()
                .ok_or_else(|| {
                    anyhow!(
                        "Failed to convert base token to address for direct swap hotproxy call: {:?}",
                        &input_params[0]
                    )
                })?
                .to_fixed_bytes()
                .to_vec();

            let quote_token = input_params[1]
                .to_owned()
                .into_address()
                .ok_or_else(|| {
                    anyhow!(
                        "Failed to convert quote token to address for direct swap hotproxy call: {:?}",
                        &input_params[1]
                    )
                })?
                .to_fixed_bytes()
                .to_vec();

            let mut pool_index_buf = [0u8; 32];
            input_params[2]
                .to_owned()
                .into_uint()
                .ok_or_else(|| {
                    anyhow!("Failed to convert pool index to u32 for direct swap hotproxy call"
                        .to_string())
                })?
                .to_big_endian(&mut pool_index_buf);
            let pool_index = pool_index_buf.to_vec();

            let (base_flow, quote_flow) = decode_flows_from_output(call)?;
            let pool_hash = encode_pool_hash(base_token, quote_token, pool_index);
            Ok((pool_hash, base_flow, quote_flow))
        } else {
            bail!("Failed to decode hotproxy swap call internap inputs.".to_string());
        }
    } else {
        bail!("Failed to decode hotproxy swap call external input.".to_string());
    }
}
