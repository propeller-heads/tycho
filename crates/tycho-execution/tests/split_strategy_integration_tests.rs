mod common;

use std::{collections::HashMap, str::FromStr};

use alloy::hex::encode;
use num_bigint::{BigInt, BigUint};
use tycho_common::{
    models::{protocol::ProtocolComponent, Chain},
    Bytes,
};
use tycho_execution::encoding::{
    evm::utils::write_calldata_to_file,
    models::{default_token, Solution, Swap, UserTransferType},
};

use crate::common::{
    client_fee_receiver, dai, encoding::encode_tycho_router_call, eth, eth_chain, get_signer,
    get_tycho_router_encoder, usdc, wbtc, weth,
};

#[test]
fn test_evm_split_swap_strategy_encoder() {
    // Note: This test does not assert anything. It is only used to obtain integration
    // test data for our router solidity test.
    //
    // Performs a split swap from WETH to USDC though WBTC and DAI using USV2 pools
    //
    //         ┌──(USV2)──> WBTC ───(USV2)──> USDC
    //   WETH ─┤
    //         └──(USV2)──> DAI  ───(USV2)──> USDC
    //

    let weth = weth();
    let dai = dai();
    let wbtc = wbtc();
    let usdc = usdc();

    let swap_weth_dai = Swap::new(
        ProtocolComponent {
            id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(dai.clone()),
        BigUint::ZERO,
    )
    .with_split(0.5f64);
    let swap_weth_wbtc = Swap::new(
        ProtocolComponent {
            id: "0xBb2b8038a1640196FbE3e38816F3e67Cba72D940".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(wbtc.clone()),
        BigUint::ZERO,
    );
    let swap_dai_usdc = Swap::new(
        ProtocolComponent {
            id: "0xAE461cA67B15dc8dc81CE7615e0320dA1A9aB8D5".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(dai.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    );
    let swap_wbtc_usdc = Swap::new(
        ProtocolComponent {
            id: "0x004375Dff511095CC5A197A54140a24eFEF3A416".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(wbtc.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    );
    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        weth,
        usdc,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("26173932").unwrap(),
        vec![swap_weth_dai, swap_weth_wbtc, swap_dai_usdc, swap_wbtc_usdc],
    )
    .with_user_transfer_type(UserTransferType::TransferFromPermit2);

    let encoded_solution = encoder
        .encode_solutions(vec![solution.clone()])
        .unwrap()[0]
        .clone();

    let calldata = encode_tycho_router_call(
        eth_chain().id(),
        encoded_solution,
        &solution,
        &eth(),
        Some(get_signer()),
        0,
        Bytes::zero(20),
        BigUint::ZERO,
    )
    .unwrap()
    .data;

    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_split_swap_strategy_encoder", hex_calldata.as_str());
}

#[test]
fn test_evm_split_input_cyclic_swap() {
    // This test has start and end tokens that are the same
    // The flow is:
    //            ┌─ (USV3, 60% split) ──> WETH ─┐
    //            │                              │
    // USDC ──────┤                              ├──(USV2)──> USDC
    //            │                              │
    //            └─ (USV3, 40% split) ──> WETH ─┘

    let weth = weth();
    let usdc = usdc();

    // USDC -> WETH (Pool 1) - 60% of input
    let swap_usdc_weth_pool1 = Swap::new(
        ProtocolComponent {
            id: "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640".to_string(), /* USDC-WETH USV3
                                                                           * Pool 1 */
            protocol_system: "uniswap_v3".to_string(),
            static_attributes: {
                let mut attrs = HashMap::new();
                attrs
                    .insert("fee".to_string(), Bytes::from(BigInt::from(500).to_signed_bytes_be()));
                attrs
            },
            ..Default::default()
        },
        default_token(usdc.clone()),
        default_token(weth.clone()),
        BigUint::ZERO,
    )
    .with_split(0.6f64) // 60% of input
    ;

    // USDC -> WETH (Pool 2) - 40% of input (remaining)
    let swap_usdc_weth_pool2 = Swap::new(
        ProtocolComponent {
            id: "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8".to_string(), /* USDC-WETH USV3
                                                                           * Pool 2 */
            protocol_system: "uniswap_v3".to_string(),
            static_attributes: {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "fee".to_string(),
                    Bytes::from(BigInt::from(3000).to_signed_bytes_be()),
                );
                attrs
            },
            ..Default::default()
        },
        default_token(usdc.clone()),
        default_token(weth.clone()),
        BigUint::ZERO,
    );

    // WETH -> USDC (Pool 2)
    let swap_weth_usdc_pool2 = Swap::new(
        ProtocolComponent {
            id: "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc".to_string(), /* USDC-WETH USV2
                                                                           * Pool 2 */
            protocol_system: "uniswap_v2".to_string(),
            static_attributes: {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "fee".to_string(),
                    Bytes::from(BigInt::from(3000).to_signed_bytes_be()),
                );
                attrs
            },
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        usdc.clone(),
        usdc.clone(),
        BigUint::from_str("100000000").unwrap(), // 100 USDC (6 decimals)
        BigUint::from_str("99574171").unwrap(),
        vec![swap_usdc_weth_pool1, swap_usdc_weth_pool2, swap_weth_usdc_pool2],
    )
    .with_user_transfer_type(UserTransferType::TransferFromPermit2);

    let encoded_solution = encoder
        .encode_solutions(vec![solution.clone()])
        .unwrap()[0]
        .clone();

    let calldata = encode_tycho_router_call(
        eth_chain().id(),
        encoded_solution,
        &solution,
        &eth(),
        Some(get_signer()),
        0,
        Bytes::zero(20),
        BigUint::ZERO,
    )
    .unwrap()
    .data;

    let hex_calldata = alloy::hex::encode(&calldata);
    let expected_input = [
        "e6675f5c", // selector (splitSwapPermit2)
        "0000000000000000000000000000000000000000000000000000000005f5e100", // given amount
        "000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // given token
        "000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // checked token
        "0000000000000000000000000000000000000000000000000000000005ef619b", // min amount out
        "0000000000000000000000000000000000000000000000000000000000000002", // tokens length
        "000000000000000000000000cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2", // receiver
        "00000000000000000000000000000000000000000000000000000000000001e0", /* clientFeeParams
                     * offset = 480 */
    ]
    .join("");

    // After this there is the permit and because of the deadlines (that depend on block
    // time) it's hard to assert back

    let expected_swaps = [
        "000000000000000000000000000000000000000000000000000000000000010d", /* length of ple
                                                                             * encoded swaps
                                                                             * without padding
                                                                             * (269 bytes) */
        "0059",                                     // ple encoded swaps (89 bytes)
        "00",                                       // token in index
        "01",                                       // token out index
        "999999",                                   // split
        "2e234dae75c793f67a35089c9d99245e1c58470b", // executor address
        "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token in
        "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token out
        "0001f4",                                   // pool fee
        "88e6a0c2ddd26feeb64f039a2c41296fcb3f5640", // component id
        "01",                                       // zero2one
        "0059",                                     // ple encoded swaps (89 bytes)
        "00",                                       // token in index
        "01",                                       // token out index
        "000000",                                   // split
        "2e234dae75c793f67a35089c9d99245e1c58470b", // executor address
        "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token in
        "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token out
        "000bb8",                                   // pool fee
        "8ad599c3a0ff1de082011efddc58f1908eb6e6d8", // component id
        "01",                                       // zero2one
        "0055",                                     // ple encoded swaps (85 bytes)
        "01",                                       // token in index
        "00",                                       // token out index
        "000000",                                   // split
        "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address
        "b4e16d0168e52d35cacd2c6185b44281ec28c9dc", // component id (pool address)
        "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token in (WETH)
        "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token out (USDC)
        "00000000000000000000000000000000000000",   // padding (19 bytes)
    ]
    .join("");
    assert_eq!(hex_calldata[..456], expected_input);
    assert_eq!(hex_calldata[1608..], expected_swaps);
    write_calldata_to_file("test_split_input_cyclic_swap", hex_calldata.as_str());
}

#[test]
fn test_evm_split_output_cyclic_swap() {
    // This test has start and end tokens that are the same
    // The flow is:
    //                        ┌─── (USV3, 60% split) ───┐
    //                        │                         │
    // USDC ──(USV2) ── WETH──|                         ├─> USDC
    //                        │                         │
    //                        └─── (USV3, 40% split) ───┘

    let weth = weth();
    let usdc = usdc();

    let swap_usdc_weth_v2 = Swap::new(
        ProtocolComponent {
            id: "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc".to_string(), /* USDC-WETH USV2 */
            protocol_system: "uniswap_v2".to_string(),
            static_attributes: {
                let mut attrs = HashMap::new();
                attrs
                    .insert("fee".to_string(), Bytes::from(BigInt::from(500).to_signed_bytes_be()));
                attrs
            },
            ..Default::default()
        },
        default_token(usdc.clone()),
        default_token(weth.clone()),
        BigUint::ZERO,
    );

    let swap_weth_usdc_v3_pool1 = Swap::new(
        ProtocolComponent {
            id: "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640".to_string(), /* USDC-WETH USV3
                                                                           * Pool 1 */
            protocol_system: "uniswap_v3".to_string(),
            static_attributes: {
                let mut attrs = HashMap::new();
                attrs
                    .insert("fee".to_string(), Bytes::from(BigInt::from(500).to_signed_bytes_be()));
                attrs
            },
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    )
    .with_split(0.6f64);

    let swap_weth_usdc_v3_pool2 = Swap::new(
        ProtocolComponent {
            id: "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8".to_string(), /* USDC-WETH USV3
                                                                           * Pool 2 */
            protocol_system: "uniswap_v3".to_string(),
            static_attributes: {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "fee".to_string(),
                    Bytes::from(BigInt::from(3000).to_signed_bytes_be()),
                );
                attrs
            },
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        usdc.clone(),
        usdc.clone(),
        BigUint::from_str("100000000").unwrap(), // 100 USDC (6 decimals)
        BigUint::from_str("99025908").unwrap(),
        vec![swap_usdc_weth_v2, swap_weth_usdc_v3_pool1, swap_weth_usdc_v3_pool2],
    )
    .with_user_transfer_type(UserTransferType::TransferFromPermit2);

    let encoded_solution = encoder
        .encode_solutions(vec![solution.clone()])
        .unwrap()[0]
        .clone();

    let calldata = encode_tycho_router_call(
        eth_chain().id(),
        encoded_solution,
        &solution,
        &eth(),
        Some(get_signer()),
        0,
        Bytes::zero(20),
        BigUint::ZERO,
    )
    .unwrap()
    .data;

    let hex_calldata = alloy::hex::encode(&calldata);
    let expected_input = [
        "e6675f5c", // selector (splitSwapPermit2)
        "0000000000000000000000000000000000000000000000000000000005f5e100", // given amount
        "000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // given token
        "000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // checked token
        "0000000000000000000000000000000000000000000000000000000005e703f4", // min amount out
        "0000000000000000000000000000000000000000000000000000000000000002", // tokens length
        "000000000000000000000000cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2", // receiver
        "00000000000000000000000000000000000000000000000000000000000001e0", /* clientFeeParams
                     * offset = 480 */
    ]
    .join("");

    // After this there is the permit and because of the deadlines (that depend on block
    // time) it's hard to assert back

    let expected_swaps = [
        "000000000000000000000000000000000000000000000000000000000000010d", /* length of ple
                                                                             * encoded swaps
                                                                             * without padding
                                                                             * (269 bytes) */
        "0055",                                     // ple encoded swaps (85 bytes)
        "00",                                       // token in index
        "01",                                       // token out index
        "000000",                                   // split
        "5615deb798bb3e4dfa0139dfa1b3d433cc23b72f", // executor address
        "b4e16d0168e52d35cacd2c6185b44281ec28c9dc", // component id (pool address)
        "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token in (USDC)
        "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token out (WETH)
        "0059",                                     // ple encoded swaps (89 bytes)
        "01",                                       // token in index
        "00",                                       // token out index
        "999999",                                   // split
        "2e234dae75c793f67a35089c9d99245e1c58470b", // executor address
        "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token in
        "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token out
        "0001f4",                                   // pool fee
        "88e6a0c2ddd26feeb64f039a2c41296fcb3f5640", // component id
        "00",                                       // zero2one
        "0059",                                     // ple encoded swaps (89 bytes)
        "01",                                       // token in index
        "00",                                       // token out index
        "000000",                                   // split
        "2e234dae75c793f67a35089c9d99245e1c58470b", // executor address
        "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", // token in
        "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token out
        "000bb8",                                   // pool fee
        "8ad599c3a0ff1de082011efddc58f1908eb6e6d8", // component id
        "00",                                       // zero2one
        "00000000000000000000000000000000000000",   // padding (19 bytes)
    ]
    .join("");

    assert_eq!(hex_calldata[..456], expected_input);
    assert_eq!(hex_calldata[1608..], expected_swaps);
    write_calldata_to_file("test_split_output_cyclic_swap", hex_calldata.as_str());
}

#[test]
fn test_evm_split_swap_strategy_with_fees() {
    // Performs a split swap from WETH to USDC though WBTC and DAI using USV2 pools
    //
    //         ┌──(USV2)──> WBTC ───(USV2)──> USDC
    //   WETH ─┤
    //         └──(USV2)──> DAI  ───(USV2)──> USDC
    //
    // Client takes 1%

    let weth = weth();
    let dai = dai();
    let wbtc = wbtc();
    let usdc = usdc();

    let swap_weth_dai = Swap::new(
        ProtocolComponent {
            id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(dai.clone()),
        BigUint::ZERO,
    )
    .with_split(0.5f64);
    let swap_weth_wbtc = Swap::new(
        ProtocolComponent {
            id: "0xBb2b8038a1640196FbE3e38816F3e67Cba72D940".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(wbtc.clone()),
        BigUint::ZERO,
    );
    let swap_dai_usdc = Swap::new(
        ProtocolComponent {
            id: "0xAE461cA67B15dc8dc81CE7615e0320dA1A9aB8D5".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(dai.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    );
    let swap_wbtc_usdc = Swap::new(
        ProtocolComponent {
            id: "0x004375Dff511095CC5A197A54140a24eFEF3A416".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(wbtc.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    );
    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        weth,
        usdc,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("26173932").unwrap(),
        vec![swap_weth_dai, swap_weth_wbtc, swap_dai_usdc, swap_wbtc_usdc],
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
        Some(get_signer()),
        100,
        client_fee_receiver(),
        BigUint::ZERO,
    )
    .unwrap()
    .data;

    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_split_swap_strategy_with_fees", hex_calldata.as_str());
}
