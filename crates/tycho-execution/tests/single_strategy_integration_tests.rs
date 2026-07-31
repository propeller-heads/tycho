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
    let checked_amount = BigUint::from_str("2018817438608734439720").unwrap();
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
        checked_amount.clone(),
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
    let expected_min_amount_encoded = encode(U256::abi_encode(&biguint_to_u256(&checked_amount)));
    let expected_input = [
        "e7a307b0", // Function selector (singleSwapPermit2)
        "0000000000000000000000000000000000000000000000000de0b6b3a7640000", // amount in
        "000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token in
        "0000000000000000000000006b175474e89094c44da98b954eedeac495271d0f", // token out
        &expected_min_amount_encoded, // min amount out
        "000000000000000000000000cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2", // receiver
        "00000000000000000000000000000000000000000000000000000000000001c0", /* clientFeeParams
                     * offset = 448 */
    ]
    .join("");

    // After this there is the permit and because of the deadlines (that depend on block
    // time) it's hard to assert back

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

    assert_eq!(hex_calldata[..392], expected_input);
    assert_eq!(hex_calldata[1544..], expected_swap);
    write_calldata_to_file("test_single_swap_strategy_encoder", &hex_calldata.to_string());
}

#[test]
fn test_single_swap_strategy_encoder_transfer_from() {
    // Performs a single swap from WETH to DAI on a USV2 pool, using transfer from and no
    // grouping optimizations.
    let weth = weth();
    let dai = dai();

    let checked_amount = BigUint::from_str("1_640_000000000000000000").unwrap();
    let expected_min_amount = U256::from_str("1_640_000000000000000000").unwrap();

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
        checked_amount,
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
    let expected_min_amount_encoded = encode(U256::abi_encode(&expected_min_amount));
    let expected_input = [
        "ce25e49e", // Function selector (singleSwap)
        "0000000000000000000000000000000000000000000000000de0b6b3a7640000", // amount in
        "000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token in
        "0000000000000000000000006b175474e89094c44da98b954eedeac495271d0f", // token out
        &expected_min_amount_encoded, // min amount out
        "000000000000000000000000cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2", // receiver
        "00000000000000000000000000000000000000000000000000000000000000e0", // clientFeeParams offset = 224
        "00000000000000000000000000000000000000000000000000000000000001a0", // swapData offset = 416
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
    // Swap is 1 WETH for 2018.8 DAI
    // Client takes 1% -> 20.18 DAI (20188174386087344397)
    let checked_amount = BigUint::from_str("1995_000000000000000000").unwrap();
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
        checked_amount.clone(),
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
        100,
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
    // Performs a single swap from WETH to DAI on a USV2 pool, with fees
    // Swap is 1 WETH for 2018.8 DAI
    // Tycho Router takes 1% -> 20.18 DAI (20188174386087344397)
    // Client takes 1% -> 20.18 DAI (20188174386087344397)
    // But (for some reason) the client contributes with at most 22 DAI
    let checked_amount = BigUint::from_str("2000_000000000000000000").unwrap();
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
        checked_amount.clone(),
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
        100,
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
