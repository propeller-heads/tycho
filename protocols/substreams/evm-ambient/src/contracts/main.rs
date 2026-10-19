use anyhow::{anyhow, bail};
use ethabi::{decode, ParamType};
use hex_literal::hex;
use substreams_ethereum::pb::eth::v2::Call;

use tycho_substreams::models::{
    Attribute, ChangeType, FinancialType, ImplementationType, ProtocolComponent, ProtocolType,
};

use crate::{
    pb::tycho::ambient::v1::AmbientProtocolComponent,
    utils::{decode_flows_from_output, encode_pool_hash},
};

pub const AMBIENT_CONTRACT: [u8; 20] = hex!("aaaaaaaaa24eeeb8d57d431224f73832bc34f688");
pub const USER_CMD_FN_SIG: [u8; 4] = hex!("a15112f9");

const USER_CMD_EXTERNAL_ABI: &[ParamType] = &[
    // index of the proxy sidecar the command is being called on
    ParamType::Uint(16),
    // call data for internal UserCmd method
    ParamType::Bytes,
];
const USER_CMD_INTERNAL_ABI: &[ParamType] = &[
    ParamType::Uint(8),   // command
    ParamType::Address,   // base
    ParamType::Address,   // quote
    ParamType::Uint(256), // pool index
    ParamType::Uint(128), // price
];

pub const INIT_POOL_CODE: u8 = 71;

pub const SWAP_ABI_INPUT: &[ParamType] = &[
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

// MicroPaths fn sigs
pub const SWAP_FN_SIG: [u8; 4] = hex!("3d719cd9");

pub fn decode_direct_swap_call(
    call: &Call,
) -> Result<([u8; 32], ethabi::Int, ethabi::Int), anyhow::Error> {
    if let Ok(external_input_params) = decode(SWAP_ABI_INPUT, &call.input[4..]) {
        let base_token = external_input_params[0]
            .to_owned()
            .into_address()
            .ok_or_else(|| {
                anyhow!(
                    "Failed to convert base token to address for direct swap call: {:?}",
                    &external_input_params[0]
                )
            })?
            .to_fixed_bytes()
            .to_vec();

        let quote_token = external_input_params[1]
            .to_owned()
            .into_address()
            .ok_or_else(|| {
                anyhow!(
                    "Failed to convert quote token to address for direct swap call: {:?}",
                    &external_input_params[1]
                )
            })?
            .to_fixed_bytes()
            .to_vec();

        let mut pool_index_buf = [0u8; 32];
        external_input_params[2]
            .to_owned()
            .into_uint()
            .ok_or_else(|| {
                anyhow!("Failed to convert pool index to u32 for direct swap call".to_string())
            })?
            .to_big_endian(&mut pool_index_buf);
        let pool_index = pool_index_buf.to_vec();

        let (base_flow, quote_flow) = decode_flows_from_output(call)?;
        let pool_hash = encode_pool_hash(base_token, quote_token, pool_index);
        Ok((pool_hash, base_flow, quote_flow))
    } else {
        bail!("Failed to decode swap call inputs.".to_string());
    }
}

pub fn decode_pool_init(
    call: &Call,
    tx_index: u64,
) -> Result<Option<AmbientProtocolComponent>, anyhow::Error> {
    // Decode external call to UserCmd
    if let Ok(external_params) = decode(USER_CMD_EXTERNAL_ABI, &call.input[4..]) {
        let cmd_bytes = external_params[1]
            .to_owned()
            .into_bytes()
            .ok_or_else(|| anyhow!("Failed to convert to bytes: {:?}", &external_params[1]))?;

        // Call data is structured differently depending on the cmd code, so only
        // decode if this is an init pool code.
        if cmd_bytes[31] == INIT_POOL_CODE {
            // Decode internal call to UserCmd
            if let Ok(internal_params) = decode(USER_CMD_INTERNAL_ABI, &cmd_bytes) {
                let base = internal_params[1]
                    .to_owned()
                    .into_address()
                    .ok_or_else(|| {
                        anyhow!("Failed to convert to address: {:?}", &internal_params[1])
                    })?
                    .to_fixed_bytes()
                    .to_vec();

                let quote = internal_params[2]
                    .to_owned()
                    .into_address()
                    .ok_or_else(|| {
                        anyhow!("Failed to convert to address: {:?}", &internal_params[2])
                    })?
                    .to_fixed_bytes()
                    .to_vec();

                let mut pool_index_buf = [0u8; 32];
                internal_params[3]
                    .to_owned()
                    .into_uint()
                    .ok_or_else(|| anyhow!("Failed to convert to u32".to_string()))?
                    .to_big_endian(&mut pool_index_buf);
                let pool_index = pool_index_buf.to_vec();
                let pool_hash = encode_pool_hash(base.clone(), quote.clone(), pool_index.clone());

                let mut tokens: Vec<Vec<u8>> = vec![base.clone(), quote.clone()];
                tokens.sort();

                let new_component = AmbientProtocolComponent {
                    id: hex::encode(pool_hash),
                    tokens,
                    pool_index,
                    tx_index,
                };
                Ok(Some(new_component))
            } else {
                bail!("Failed to decode ABI internal call.".to_string());
            }
        } else {
            Ok(None)
        }
    } else {
        bail!("Failed to decode ABI external call.".to_string());
    }
}

impl From<AmbientProtocolComponent> for ProtocolComponent {
    fn from(component: AmbientProtocolComponent) -> Self {
        ProtocolComponent {
            id: component.id,
            tokens: component.tokens,
            contracts: vec![AMBIENT_CONTRACT.to_vec()],
            static_att: vec![Attribute {
                name: "pool_index".to_string(),
                value: component.pool_index,
                change: ChangeType::Creation.into(),
            }],
            change: ChangeType::Creation.into(),
            protocol_type: Some(ProtocolType {
                name: "ambient_pool".to_string(),
                attribute_schema: vec![],
                financial_type: FinancialType::Swap.into(),
                implementation_type: ImplementationType::Vm.into(),
            }),
        }
    }
}
