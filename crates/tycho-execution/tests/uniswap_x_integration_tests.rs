use std::{collections::HashMap, str::FromStr};

use alloy::{hex::encode, primitives::Address, sol_types::SolValue};
use num_bigint::{BigInt, BigUint};
use tycho_common::{
    models::{protocol::ProtocolComponent, Chain},
    Bytes,
};
use tycho_execution::encoding::{
    evm::{
        approvals::protocol_approvals_manager::ProtocolApprovalsManager,
        utils::{bytes_to_address, write_calldata_to_file},
    },
    models::{default_token, Solution, Swap},
};

use crate::common::{
    dai, encoding::encode_tycho_router_call, eth, eth_chain, get_tycho_router_encoder,
    router_address, usdc, usdt,
};

mod common;

#[test]
fn test_evm_sequential_swap_usx() {
    // Replicates real uniswap X order settled in tx:
    // 0x005d7b150017ba1b59d2f99395ccae7bda9b739938ade4e509817e32760aaf9d
    // Performs a sequential
    // swap from DAI to USDT though USDC using USV3 pools
    //
    //   DAI ───(USV3)──> USDC ───(USV2)──> USDT
    // Creates all the calldata needed for the uniswap X callbackData

    let filler = Bytes::from_str("0x1b435D322a41287C2e5b3b2E1469709e3ab13867").unwrap();
    let usx_reactor = Address::from_str("0x00000011F84B9aa48e5f8aA8B9897600006289Be").unwrap();

    let dai = dai();
    let usdc = usdc();
    let usdt = usdt();

    let swap_dai_usdc = Swap::new(
        ProtocolComponent {
            id: "0x5777d92f208679DB4b9778590Fa3CAB3aC9e2168".to_string(),
            protocol_system: "uniswap_v3".to_string(),
            static_attributes: {
                let mut attrs = HashMap::new();
                attrs
                    .insert("fee".to_string(), Bytes::from(BigInt::from(100).to_signed_bytes_be()));
                attrs
            },
            ..Default::default()
        },
        default_token(dai.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    );
    let swap_usdc_usdt = Swap::new(
        ProtocolComponent {
            id: "0x3416cF6C708Da44DB2624D63ea0AAef7113527C6".to_string(),
            protocol_system: "uniswap_v3".to_string(),
            static_attributes: {
                let mut attrs = HashMap::new();
                attrs
                    .insert("fee".to_string(), Bytes::from(BigInt::from(100).to_signed_bytes_be()));
                attrs
            },
            ..Default::default()
        },
        default_token(usdc.clone()),
        default_token(usdt.clone()),
        BigUint::ZERO,
    );
    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        filler.clone(),
        filler.clone(),
        dai.clone(),
        usdt.clone(),
        BigUint::from_str("2000000000000000000000").unwrap(),
        BigUint::from_str("1990000000").unwrap(),
        vec![swap_dai_usdc, swap_usdc_usdt],
    );

    let encoded_solution = encoder
        .encode_solutions(vec![solution.clone()])
        .unwrap()[0]
        .clone();

    let tycho_calldata = encode_tycho_router_call(
        eth_chain().id(),
        encoded_solution,
        &solution,
        &eth(),
        None,
        0,
        Bytes::zero(20),
        BigUint::ZERO,
    )
    .unwrap()
    .data;

    // Uniswap X specific part
    let filler_address = bytes_to_address(&filler).unwrap();
    let token_approvals_manager = ProtocolApprovalsManager::new().unwrap();

    let token_in_approval_needed = token_approvals_manager
        .approval_needed(
            bytes_to_address(&dai).unwrap(),
            filler_address,
            bytes_to_address(&router_address()).unwrap(),
        )
        .unwrap();

    let token_out_approval_needed = token_approvals_manager
        .approval_needed(bytes_to_address(&usdc).unwrap(), filler_address, usx_reactor)
        .unwrap();

    let full_calldata =
        (token_in_approval_needed, token_out_approval_needed, tycho_calldata).abi_encode_packed();

    let hex_calldata = encode(&full_calldata);
    write_calldata_to_file("test_sequential_swap_usx", hex_calldata.as_str());
}
