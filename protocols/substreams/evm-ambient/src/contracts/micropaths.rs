use anyhow::{anyhow, bail};

use ethabi::{decode, ParamType};
use hex_literal::hex;
use substreams_ethereum::pb::eth::v2::Call;

pub const AMBIENT_MICROPATHS_CONTRACT: [u8; 20] = hex!("f241bEf0Ea64020655C70963ef81Fea333752367");

pub const SWEEP_SWAP_FN_SIG: [u8; 4] = hex!("7b370fc2");

pub const MINT_AMBIENT_FN_SIG: [u8; 4] = hex!("2ee11587");
pub const MINT_RANGE_FN_SIG: [u8; 4] = hex!("2370632b");
pub const BURN_AMBIENT_FN_SIG: [u8; 4] = hex!("2a6f0864");
pub const BURN_RANGE_FN_SIG: [u8; 4] = hex!("7c6dfe3d");

// ABI for the mintAmbient function with return values
pub const MINT_AMBIENT_RETURN_ABI: &[ParamType] = &[
    ParamType::Int(128),  // int128 baseFlow
    ParamType::Int(128),  // int128 quoteFlow
    ParamType::Uint(128), // uint128 seedOut
];

// ABI for the mintAmbient function parameters
const MINT_AMBIENT_ABI: &[ParamType] = &[
    ParamType::Uint(128),      // uint128 price
    ParamType::Uint(128),      // uint128 seed
    ParamType::Uint(128),      // uint128 conc
    ParamType::Uint(64),       // uint64 seedGrowth
    ParamType::Uint(64),       // uint64 concGrowth
    ParamType::Uint(128),      // uint128 liq
    ParamType::FixedBytes(32), // bytes32 poolHash
];

// ABI for the burnRange function
const BURN_RANGE_ABI: &[ParamType] = &[
    ParamType::Uint(128),      // price
    ParamType::Int(24),        // priceTick
    ParamType::Uint(128),      // seed
    ParamType::Uint(128),      // conc
    ParamType::Uint(64),       // seedGrowth
    ParamType::Uint(64),       // concGrowth
    ParamType::Int(24),        // lowTick
    ParamType::Int(24),        // highTick
    ParamType::Uint(128),      // liq
    ParamType::FixedBytes(32), // poolHash
];

const BURN_RANGE_RETURN_ABI: &[ParamType] = &[
    ParamType::Int(128),  // baseFlow
    ParamType::Int(128),  // quoteFlow
    ParamType::Uint(128), // seedOut
    ParamType::Uint(128), // concOut
];

// ABI for the burnAmbient function with return values
const BURN_AMBIENT_RETURN_ABI: &[ParamType] = &[
    ParamType::Int(128),  // int128 baseFlow
    ParamType::Int(128),  // int128 quoteFlow
    ParamType::Uint(128), // uint128 seedOut
];

// ABI for the burnAmbient function
const BURN_AMBIENT_ABI: &[ParamType] = &[
    ParamType::Uint(128),      // uint128 price
    ParamType::Uint(128),      // uint128 seed
    ParamType::Uint(128),      // uint128 conc
    ParamType::Uint(64),       // uint64 seedGrowth
    ParamType::Uint(64),       // uint64 concGrowth
    ParamType::Uint(128),      // uint128 liq
    ParamType::FixedBytes(32), // bytes32 poolHash
];

// ABI for the mintRange function parameters
const MINT_RANGE_ABI: &[ParamType] = &[
    ParamType::Uint(128),      //  price
    ParamType::Int(24),        //  priceTick
    ParamType::Uint(128),      //  seed
    ParamType::Uint(128),      //  conc
    ParamType::Uint(64),       //  seedGrowth
    ParamType::Uint(64),       //  concGrowth
    ParamType::Int(24),        //  lowTick
    ParamType::Int(24),        //  highTick
    ParamType::Uint(128),      //  liq
    ParamType::FixedBytes(32), //  poolHash
];

// ABI for the mintRange function with return values
const MINT_RANGE_RETURN_ABI: &[ParamType] = &[
    ParamType::Int(128),  //  baseFlow
    ParamType::Int(128),  //  quoteFlow
    ParamType::Uint(128), //  seedOut
    ParamType::Uint(128), //  concOut
];
pub fn decode_mint_range_call(
    call: &Call,
) -> Result<([u8; 32], ethabi::Int, ethabi::Int), anyhow::Error> {
    if let Ok(mint_range) = decode(MINT_RANGE_ABI, &call.input[4..]) {
        let pool_hash: [u8; 32] = mint_range[9]
            .to_owned()
            .into_fixed_bytes()
            .ok_or_else(|| anyhow!("Failed to convert pool hash to fixed bytes".to_string()))?
            .try_into()
            .unwrap();

        if let Ok(external_outputs) = decode(MINT_RANGE_RETURN_ABI, &call.return_data) {
            let base_flow = external_outputs[0]
                .to_owned()
                .into_int() // Needs conversion into bytes for next step
                .ok_or_else(|| anyhow!("Failed to convert base flow to i128".to_string()))?;

            let quote_flow = external_outputs[1]
                .to_owned()
                .into_int() // Needs conversion into bytes for next step
                .ok_or_else(|| anyhow!("Failed to convert quote flow to i128".to_string()))?;
            Ok((pool_hash, base_flow, quote_flow))
        } else {
            bail!("Failed to decode swap call outputs.".to_string());
        }
    } else {
        bail!("Failed to decode inputs for WarmPath userCmd call.".to_string());
    }
}

pub fn decode_burn_ambient_call(
    call: &Call,
) -> Result<([u8; 32], ethabi::Int, ethabi::Int), anyhow::Error> {
    if let Ok(burn_ambient) = decode(BURN_AMBIENT_ABI, &call.input[4..]) {
        let pool_hash: [u8; 32] = burn_ambient[6]
            .to_owned()
            .into_fixed_bytes()
            .ok_or_else(|| anyhow!("Failed to convert pool hash to bytes".to_string()))?
            .try_into()
            .unwrap();

        if let Ok(external_outputs) = decode(BURN_AMBIENT_RETURN_ABI, &call.return_data) {
            let base_flow = external_outputs[0]
                .to_owned()
                .into_int()
                .ok_or_else(|| anyhow!("Failed to convert base flow to i128".to_string()))?;

            let quote_flow = external_outputs[1]
                .to_owned()
                .into_int()
                .ok_or_else(|| anyhow!("Failed to convert quote flow to i128".to_string()))?;

            Ok((pool_hash, base_flow, quote_flow))
        } else {
            bail!("Failed to decode burnAmbient call outputs.".to_string());
        }
    } else {
        bail!("Failed to decode inputs for burnAmbient call.".to_string());
    }
}

pub fn decode_mint_ambient_call(
    call: &Call,
) -> Result<([u8; 32], ethabi::Int, ethabi::Int), anyhow::Error> {
    if let Ok(mint_ambient) = decode(MINT_AMBIENT_ABI, &call.input[4..]) {
        let pool_hash: [u8; 32] = mint_ambient[6]
            .to_owned()
            .into_fixed_bytes()
            .ok_or_else(|| anyhow!("Failed to convert pool hash to bytes".to_string()))?
            .try_into()
            .unwrap();

        if let Ok(external_outputs) = decode(MINT_AMBIENT_RETURN_ABI, &call.return_data) {
            let base_flow = external_outputs[0]
                .to_owned()
                .into_int()
                .ok_or_else(|| anyhow!("Failed to convert base flow to i128".to_string()))?;

            let quote_flow = external_outputs[1]
                .to_owned()
                .into_int()
                .ok_or_else(|| anyhow!("Failed to convert quote flow to i128".to_string()))?;

            Ok((pool_hash, base_flow, quote_flow))
        } else {
            bail!("Failed to decode mintAmbient call outputs.".to_string());
        }
    } else {
        bail!("Failed to decode inputs for mintAmbient call.".to_string());
    }
}

pub fn decode_burn_range_call(
    call: &Call,
) -> Result<([u8; 32], ethabi::Int, ethabi::Int), anyhow::Error> {
    if let Ok(burn_range) = decode(BURN_RANGE_ABI, &call.input[4..]) {
        let pool_hash: [u8; 32] = burn_range[9]
            .to_owned()
            .into_fixed_bytes() // Convert Bytes32 to Vec<u8>
            .ok_or_else(|| anyhow!("Failed to convert pool hash to bytes".to_string()))?
            .try_into()
            .unwrap();

        if let Ok(external_outputs) = decode(BURN_RANGE_RETURN_ABI, &call.return_data) {
            let base_flow = external_outputs[0]
                .to_owned()
                .into_int()
                .ok_or_else(|| anyhow!("Failed to convert base flow to i128".to_string()))?;

            let quote_flow = external_outputs[1]
                .to_owned()
                .into_int()
                .ok_or_else(|| anyhow!("Failed to convert quote flow to i128".to_string()))?;

            Ok((pool_hash, base_flow, quote_flow))
        } else {
            bail!("Failed to decode burnRange call outputs.".to_string());
        }
    } else {
        bail!("Failed to decode inputs for burnRange call.".to_string());
    }
}

pub fn decode_sweep_swap_call(
    call: &Call,
) -> Result<([u8; 32], ethabi::Int, ethabi::Int), anyhow::Error> {
    let sweep_swap_abi: &[ParamType] = &[
        ParamType::Tuple(vec![
            ParamType::Uint(128),
            ParamType::Uint(128),
            ParamType::Uint(128),
            ParamType::Uint(64),
            ParamType::Uint(64),
        ]), // CurveState
        ParamType::Int(24), // midTick
        ParamType::Tuple(vec![
            ParamType::Bool,
            ParamType::Bool,
            ParamType::Uint(8),
            ParamType::Uint(128),
            ParamType::Uint(128),
        ]), // SwapDirective
        ParamType::Tuple(vec![
            ParamType::Tuple(vec![
                ParamType::Uint(8),  // schema
                ParamType::Uint(16), // feeRate
                ParamType::Uint(8),  // protocolTake
                ParamType::Uint(16), // tickSize
                ParamType::Uint(8),  // jitThresh
                ParamType::Uint(8),  // knockoutBits
                ParamType::Uint(8),  // oracleFlags
            ]),
            ParamType::FixedBytes(32), // poolHash
            ParamType::Address,
        ]), // PoolCursor
    ];
    let sweep_swap_abi_output: &[ParamType] = &[
        ParamType::Tuple(vec![
            ParamType::Int(128), // baseFlow
            ParamType::Int(128), // quoteFlow
            ParamType::Uint(128),
            ParamType::Uint(128),
        ]), // Chaining.PairFlow memory accum
        ParamType::Uint(128), // priceOut
        ParamType::Uint(128), // seedOut
        ParamType::Uint(128), // concOut
        ParamType::Uint(64),  // ambientOut
        ParamType::Uint(64),  // concGrowthOut
    ];
    if let Ok(sweep_swap_input) = decode(sweep_swap_abi, &call.input[4..]) {
        let pool_cursor = sweep_swap_input[3]
            .to_owned()
            .into_tuple()
            .ok_or_else(|| {
                anyhow!("Failed to convert pool cursor to tuple for sweepSwap call".to_string())
            })?;
        let pool_hash: [u8; 32] = pool_cursor[1]
            .to_owned()
            .into_fixed_bytes()
            .ok_or_else(|| {
                anyhow!("Failed to convert pool hash to fixed bytes for sweepSwap call".to_string())
            })?
            .try_into()
            .unwrap();
        if let Ok(sweep_swap_output) = decode(sweep_swap_abi_output, &call.return_data) {
            let pair_flow = sweep_swap_output[0]
                .to_owned()
                .into_tuple()
                .ok_or_else(|| {
                    anyhow!("Failed to convert pair flow to tuple for sweepSwap call".to_string())
                })?;

            let base_flow = pair_flow[0]
                .to_owned()
                .into_int() // Needs conversion into bytes for next step
                .ok_or_else(|| {
                    anyhow!("Failed to convert base flow to i128 for sweepSwap call".to_string())
                })?;

            let quote_flow = pair_flow[1]
                .to_owned()
                .into_int() // Needs conversion into bytes for next step
                .ok_or_else(|| {
                    anyhow!("Failed to convert quote flow to i128 for sweepSwap call".to_string())
                })?;

            Ok((pool_hash, base_flow, quote_flow))
        } else {
            bail!("Failed to decode sweepSwap outputs.".to_string());
        }
    } else {
        bail!("Failed to decode sweepSwap inputs.".to_string());
    }
}
