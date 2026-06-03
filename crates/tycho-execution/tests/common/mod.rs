#![allow(dead_code)]
pub mod encoding;

use std::{fs, str::FromStr};

use alloy::{
    primitives::{B256, U256},
    signers::local::PrivateKeySigner,
};
use tycho_common::{models::Chain, Bytes};
use tycho_execution::encoding::{
    evm::{
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
    },
    tycho_encoder::TychoEncoder,
};

pub fn router_address() -> Bytes {
    Bytes::from_str("0x6bc529DC7B81A031828dDCE2BC419d01FF268C66").unwrap()
}

pub fn bob_address() -> Bytes {
    Bytes::from_str("0x9964bFf29BAa37B47604F3F3F51F3B3C5149d6DE").unwrap()
}

pub fn alice_address() -> Bytes {
    Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap()
}

pub fn client_fee_receiver() -> Bytes {
    // Address derived from CLIENT_FEE_RECEIVER_PK in foundry/test/Constants.sol
    Bytes::from_str("0xe1B1380a85Cd5D76F328A184B070E2821593D2a8").unwrap()
}

pub fn eth_chain() -> Chain {
    Chain::Ethereum
}

pub fn eth() -> Bytes {
    Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap()
}

pub fn weth() -> Bytes {
    Bytes::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap()
}

pub fn baseline_btoken() -> Bytes {
    Bytes::from_str("0x9fDbDE76236998Dc2836FE67A9954eDE456A1D63").unwrap()
}

pub fn usdc() -> Bytes {
    Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap()
}

pub fn dai() -> Bytes {
    Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap()
}

pub fn wbtc() -> Bytes {
    Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap()
}

pub fn pepe() -> Bytes {
    Bytes::from_str("0x6982508145454Ce325dDbE47a25d4ec3d2311933").unwrap()
}

pub fn usdt() -> Bytes {
    Bytes::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap()
}

pub fn ondo() -> Bytes {
    Bytes::from_str("0xfAbA6f8e4a5E8Ab82F62fe7C39859FA577269BE3").unwrap()
}

pub fn get_signer() -> PrivateKeySigner {
    // Set up a mock private key for signing (Alice's pk in our contract tests)
    let private_key =
        "0x123456789abcdef123456789abcdef123456789abcdef123456789abcdef1234".to_string();

    let pk = B256::from_str(&private_key).unwrap();
    PrivateKeySigner::from_bytes(&pk).unwrap()
}

pub fn get_tycho_router_encoder(chain: Chain) -> Box<dyn TychoEncoder> {
    let executors_addresses = fs::read_to_string("config/test_executor_addresses.json").unwrap();
    let swap_encoder_registry = SwapEncoderRegistry::new(chain)
        .add_default_encoders(Some(executors_addresses))
        .unwrap();
    TychoRouterEncoderBuilder::new()
        .chain(chain)
        .swap_encoder_registry(swap_encoder_registry)
        .router_address(router_address())
        .build()
        .expect("Failed to build encoder")
}

/// Builds the complete Bebop calldata in the format expected by the encoder
/// Returns: [ partial_fill_offset (u8) | original_taker_amount (U256) | calldata (bytes (selector +
/// ABI encoded params)) ]
pub fn build_bebop_calldata(
    calldata: &[u8],
    partial_fill_offset: u8,
    original_taker_amount: U256,
) -> Bytes {
    let mut user_data = Vec::with_capacity(1 + 32 + calldata.len());
    user_data.push(partial_fill_offset);
    user_data.extend_from_slice(&original_taker_amount.to_be_bytes::<32>());
    user_data.extend_from_slice(calldata);

    Bytes::from(user_data)
}
