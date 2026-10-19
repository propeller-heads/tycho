use anyhow::{anyhow, bail};

use crate::utils::{decode_flows_from_output, encode_pool_hash};
use ethabi::{decode, ParamType};
use hex_literal::hex;
use substreams_ethereum::pb::eth::v2::Call;

pub const AMBIENT_KNOCKOUT_CONTRACT: [u8; 20] = hex!("7F5D75AdE75646919c923C98D53E9Cc7Be7ea794");
pub const USER_CMD_KNOCKOUT_FN_SIG: [u8; 4] = hex!("f96dc788");

// Represents the ABI of any cmd which is not mint or burn
const KNOCKOUT_INTERNAL_OTHER_CMD_ABI: &[ParamType] = &[
    ParamType::Uint(8),
    ParamType::Address,   // base
    ParamType::Address,   // quote
    ParamType::Uint(256), // poolIdx
    ParamType::Int(24),
    ParamType::Int(24),
    ParamType::Bool,
    ParamType::Uint(8),
    ParamType::Uint(256),
    ParamType::Uint(256),
    ParamType::Uint(32),
];
const KNOCKOUT_INTERNAL_MINT_BURN_ABI: &[ParamType] = &[
    ParamType::Uint(8),
    ParamType::Address,   // base
    ParamType::Address,   // quote
    ParamType::Uint(256), // poolIdx
    ParamType::Int(24),
    ParamType::Int(24),
    ParamType::Bool,
    ParamType::Uint(8),
    ParamType::Uint(256),
    ParamType::Uint(256),
    ParamType::Uint(128),
    ParamType::Bool,
];

const KNOCKOUT_EXTERNAL_ABI: &[ParamType] = &[
    ParamType::Bytes, // userCmd
];

pub fn decode_knockout_call(
    call: &Call,
) -> Result<([u8; 32], ethabi::Int, ethabi::Int), anyhow::Error> {
    if let Ok(external_cmd) = decode(KNOCKOUT_EXTERNAL_ABI, &call.input[4..]) {
        let input_data = external_cmd[0]
            .to_owned()
            .into_bytes() // Convert Bytes32 to Vec<u8>
            .ok_or_else(|| anyhow!("Failed to Knockout userCmd input data.".to_string()))?;

        let code = input_data[31];
        let is_mint = code == 91;
        let is_burn = code == 92;

        let abi = if is_mint || is_burn {
            KNOCKOUT_INTERNAL_MINT_BURN_ABI
        } else {
            KNOCKOUT_INTERNAL_OTHER_CMD_ABI
        };

        if let Ok(mint_burn_inputs) = decode(abi, &input_data) {
            let base_token = mint_burn_inputs[1]
                .to_owned()
                .into_address()
                .ok_or_else(|| {
                    anyhow!(
                        "Failed to convert base token to address for knockout call: {:?}",
                        &mint_burn_inputs[1]
                    )
                })?
                .to_fixed_bytes()
                .to_vec();
            let quote_token = mint_burn_inputs[2]
                .to_owned()
                .into_address()
                .ok_or_else(|| {
                    anyhow!(
                        "Failed to convert quote token to address for knockout call: {:?}",
                        &mint_burn_inputs[2]
                    )
                })?
                .to_fixed_bytes()
                .to_vec();

            let mut pool_index_buf = [0u8; 32];
            mint_burn_inputs[3]
                .to_owned()
                .into_uint()
                .ok_or_else(|| {
                    anyhow!("Failed to convert pool index to bytes for knockout call".to_string())
                })?
                .to_big_endian(&mut pool_index_buf);
            let pool_index = pool_index_buf.to_vec();

            let (base_flow, quote_flow) = decode_flows_from_output(call)?;
            let pool_hash = encode_pool_hash(base_token, quote_token, pool_index);
            Ok((pool_hash, base_flow, quote_flow))
        } else {
            bail!("Failed to decode knockout call outputs.".to_string());
        }
    } else {
        bail!("Failed to decode inputs for knockout call.".to_string());
    }
}
