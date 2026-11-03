mod common;

use std::str::FromStr;

use alloy::{hex::encode, primitives::U256, sol_types::SolValue};
use num_bigint::BigUint;
use tycho_common::{
    models::{protocol::ProtocolComponent, Chain},
    Bytes,
};
use tycho_execution::encoding::{
    evm::utils::{biguint_to_u256, write_calldata_to_file},
    models::{default_token, Solution, Swap, UserTransferType},
};

use crate::common::{
    client_fee_receiver, dai, encoding::encode_tycho_router_call, eth, eth_chain, get_signer,
    get_tycho_router_encoder, weth,
};

#[test]
fn test_evm_single_swap_strategy_encoder() {
    // Performs a single swap from WETH to DAI on a USV2 pool, with no grouping
    // optimizations.
    let expected_amount_out = BigUint::from_str("2018817438608734439720").unwrap();
    // 2% below the quote
    let min_amount_out = &expected_amount_out * BigUint::from(9800u64) / BigUint::from(10_000u64);
    let weth = weth();
    let dai = dai();

    let swap = Swap::new(
        ProtocolComponent {
            id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(dai.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        weth,
        dai,
        BigUint::from_str("1_000000000000000000").unwrap(),
        expected_amount_out.clone(),
        min_amount_out.clone(),
        vec![swap],
    )
    .with_user_transfer_type(UserTransferType::TransferFromPermit2);

    let encoded_solutions = encoder
        .encode_solutions(vec![solution.clone()])
        .unwrap();

    let calldata = encode_tycho_router_call(
        eth_chain().id(),
        encoded_solutions[0].clone(),
        &solution,
        &eth(),
        Some(get_signer()),
        0,
        Bytes::zero(20),
        BigUint::ZERO,
    )
    .unwrap()
    .data;
    let expected_amount_out_encoded =
        encode(U256::abi_encode(&biguint_to_u256(&expected_amount_out)));
    let min_amount_out_encoded = encode(U256::abi_encode(&biguint_to_u256(&min_amount_out)));
    let expected_input = [
        "ca931073", // selector (singleSwapPermit2)
        "0000000000000000000000000000000000000000000000000de0b6b3a7640000", // amount in
        "000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token in
        "0000000000000000000000006b175474e89094c44da98b954eedeac495271d0f", // token out
        &expected_amount_out_encoded, // expectedAmountOut
        &min_amount_out_encoded, // minAmountOut (2% below expected)
        "000000000000000000000000cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2", // receiver
        // clientFeeParams offset = 480
        "00000000000000000000000000000000000000000000000000000000000001e0",
    ]
    .join("");

    // After this there is the permit2 struct (with time-dependent deadline) and swap data.
    // The permit is hard to assert against due to block time.

    let expected_swap = String::from(concat!(
        // length of encoded swap (80 bytes: 20 pool + 20 tokenIn + 20 tokenOut)
        "0000000000000000000000000000000000000000000000000000000000000050",
        // Swap data
        "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address
        "a478c2975ab1ea89e8196811f51a7b7ade33eb11", // component id (pool address)
        "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // tokenIn
        "6b175474e89094c44da98b954eedeac495271d0f", // tokenOut
        "00000000000000000000000000000000",         // padding to 32-byte boundary
    ));
    let hex_calldata = encode(&calldata);

    assert_eq!(hex_calldata[..456], expected_input);
    assert_eq!(hex_calldata[1608..], expected_swap);
    write_calldata_to_file("test_single_swap_strategy_encoder", &hex_calldata.to_string());
}

#[test]
fn test_single_swap_strategy_encoder_transfer_from() {
    // Performs a single swap from WETH to DAI on a USV2 pool, using transfer from and no
    // grouping optimizations.
    let weth = weth();
    let dai = dai();

    let expected_amount_out = BigUint::from_str("1_640_000000000000000000").unwrap();
    // 2% below the quote
    let min_amount_out = &expected_amount_out * BigUint::from(9800u64) / BigUint::from(10_000u64);

    let swap = Swap::new(
        ProtocolComponent {
            id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(dai.clone()),
        BigUint::ZERO,
    );
    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        weth,
        dai,
        BigUint::from_str("1_000000000000000000").unwrap(),
        expected_amount_out.clone(),
        min_amount_out.clone(),
        vec![swap],
    );

    let encoded_solution = encoder
        .encode_solutions(vec![solution.clone()])
        .unwrap()[0]
        .clone();
    let calldata = encode_tycho_router_call(
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
    let expected_amount_out_encoded =
        encode(U256::abi_encode(&biguint_to_u256(&expected_amount_out)));
    let min_amount_out_encoded = encode(U256::abi_encode(&biguint_to_u256(&min_amount_out)));
    let expected_input = [
        "0c1a0ee7", // Function selector (singleSwap)
        "0000000000000000000000000000000000000000000000000de0b6b3a7640000", // amount in
        "000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token in
        "0000000000000000000000006b175474e89094c44da98b954eedeac495271d0f", // token out
        &expected_amount_out_encoded, // expectedAmountOut
        &min_amount_out_encoded,      // minAmountOut (2% below expected)
        "000000000000000000000000cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2", // receiver
        "0000000000000000000000000000000000000000000000000000000000000100", // clientFeeParams offset = 256
        "00000000000000000000000000000000000000000000000000000000000001c0", // swapData offset = 448
        // clientFeeParams tail (6 words):
        "0000000000000000000000000000000000000000000000000000000000000000", // clientFeeBps = 0
        "0000000000000000000000000000000000000000000000000000000000000000", // clientFeeReceiver = 0
        "0000000000000000000000000000000000000000000000000000000000000000", // maxClientContribution = 0
        "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff", // deadline = U256::MAX
        "00000000000000000000000000000000000000000000000000000000000000a0", // clientSignature offset in struct = 160
        "0000000000000000000000000000000000000000000000000000000000000000", // clientSignature length = 0
        // swapData:
        "0000000000000000000000000000000000000000000000000000000000000050", // len swap = 80 bytes
        // Swap data
        "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address
        "a478c2975ab1ea89e8196811f51a7b7ade33eb11", // component id (pool address)
        "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // tokenIn
        "6b175474e89094c44da98b954eedeac495271d0f", // tokenOut
        "00000000000000000000000000000000",         // padding to 32-byte boundary
    ]
    .join("");

    let hex_calldata = encode(&calldata);

    assert_eq!(hex_calldata, expected_input);
    write_calldata_to_file(
        "test_single_swap_strategy_encoder_transfer_from",
        hex_calldata.as_str(),
    );
}

#[test]
fn test_single_swap_with_client_fees() {
    // Performs a single swap from WETH to DAI on a USV2 pool, with fees
    // Swap is 1 WETH for 2018.8 DAI (2018817438608734439722)
    // Client takes 1% -> 20.18 DAI (20188174386087344397)
    let expected_amount_out = BigUint::from_str("2018817438608734439722").unwrap();
    // 2% below the quote
    let min_amount_out = &expected_amount_out * BigUint::from(9800u64) / BigUint::from(10_000u64);
    let weth = weth();
    let dai = dai();

    let swap = Swap::new(
        ProtocolComponent {
            id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(dai.clone()),
        BigUint::ZERO,
    );
    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        weth,
        dai,
        BigUint::from_str("1_000000000000000000").unwrap(),
        expected_amount_out,
        min_amount_out,
        vec![swap],
    )
    .with_user_transfer_type(UserTransferType::TransferFrom);

    let encoded_solutions = encoder
        .encode_solutions(vec![solution.clone()])
        .unwrap();

    let calldata = encode_tycho_router_call(
        eth_chain().id(),
        encoded_solutions[0].clone(),
        &solution,
        &eth(),
        None,
        1_000_000,
        client_fee_receiver(),
        BigUint::ZERO,
    )
    .unwrap()
    .data;

    let hex_calldata = encode(&calldata);

    write_calldata_to_file("test_single_swap_with_client_fees", &hex_calldata.to_string());
}

#[test]
fn test_single_swap_with_fees_and_client_contribution() {
    // Performs a single swap from WETH to DAI on a USV2 pool, with fees and client contribution
    // Swap is 1 WETH for 2018.8 DAI; quotedAmountOut = 2000e18
    // Tycho Router takes 1% of 2000e18 -> 20 DAI
    // Client takes 1% of 2000e18 -> 20 DAI
    // Client contributes up to 22 DAI to cover shortfall (actual 1978.8 < quoted 2000)
    let expected_amount_out = BigUint::from_str("2000_000000000000000000").unwrap();
    let weth = weth();
    let dai = dai();

    let swap = Swap::new(
        ProtocolComponent {
            id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(dai.clone()),
        BigUint::ZERO,
    );
    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        weth,
        dai,
        BigUint::from_str("1_000000000000000000").unwrap(),
        expected_amount_out.clone(),
        // No tolerance, so the client contribution has to cover the shortfall
        expected_amount_out,
        vec![swap],
    )
    .with_user_transfer_type(UserTransferType::TransferFrom);

    let encoded_solutions = encoder
        .encode_solutions(vec![solution.clone()])
        .unwrap();

    let calldata = encode_tycho_router_call(
        eth_chain().id(),
        encoded_solutions[0].clone(),
        &solution,
        &eth(),
        None,
        1_000_000,
        client_fee_receiver(),
        BigUint::from_str("22_000000000000000000").unwrap(),
    )
    .unwrap()
    .data;

    let hex_calldata = encode(&calldata);

    write_calldata_to_file(
        "test_single_swap_with_fees_and_client_contribution",
        &hex_calldata.to_string(),
    );
}
