mod common;
use std::{collections::HashMap, default::Default, str::FromStr, sync::Arc};

use alloy::{hex, hex::encode};
use num_bigint::{BigInt, BigUint};
use tycho_common::{
    models::{protocol::ProtocolComponent, token::Token, Chain},
    Bytes,
};
use tycho_execution::encoding::{
    evm::{
        testing_utils::MockRFQState,
        utils::{biguint_to_u256, write_calldata_to_file},
    },
    models::{default_token, Solution, Swap, UserTransferType},
};

use crate::common::{
    alice_address, dai, encoding::encode_tycho_router_call, eth, eth_chain, get_signer,
    get_tycho_router_encoder, ondo, pepe, usdc, usdt, wbtc, weth,
};

#[test]
fn test_single_encoding_strategy_ekubo() {
    //   ETH ──(EKUBO)──> USDC

    let token_in = eth();
    let token_out = usdc(); // USDC

    let static_attributes = HashMap::from([
        ("fee".to_string(), Bytes::from(0_u64)),
        ("tick_spacing".to_string(), Bytes::from(0_u32)),
        ("extension".to_string(), Bytes::from("0x51d02a5948496a67827242eabc5725531342527c")), /* Oracle */
    ]);

    let component = ProtocolComponent {
        // All Ekubo swaps go through the core contract - not necessary to specify pool
        // id for test
        protocol_system: "ekubo_v2".to_string(),
        static_attributes,
        ..Default::default()
    };

    let swap = Swap::new(
        component,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_ekubo", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_ekubo_erc20() {
    //   USDC_ADDR ──(EKUBO)──> ETH

    let token_in = usdc();
    let token_out = eth();

    let static_attributes = HashMap::from([
        ("fee".to_string(), Bytes::from(0_u64)),
        ("tick_spacing".to_string(), Bytes::from(0_u32)),
        ("extension".to_string(), Bytes::from("0x51d02a5948496a67827242eabc5725531342527c")), /* Oracle */
    ]);

    let component = ProtocolComponent {
        // All Ekubo swaps go through the core contract - not necessary to specify pool
        // id for test
        protocol_system: "ekubo_v2".to_string(),
        static_attributes,
        ..Default::default()
    };

    let swap = Swap::new(
        component,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("1_000_000_000").unwrap(),
        BigUint::from_str("1000").unwrap(),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_ekubo_erc20", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_ekubo_mev_resist() {
    //   USDC_ADDR ──(EKUBO)──> ETH

    let token_in = usdc();
    let token_out = eth();

    let static_attributes = HashMap::from([
        ("fee".to_string(), Bytes::from(0x00068db8bac710cb_u64)),
        ("tick_spacing".to_string(), Bytes::from(200_u32)),
        ("extension".to_string(), Bytes::from("0x553a2EFc570c9e104942cEC6aC1c18118e54C091")), /* mev resist pool */
    ]);

    let component = ProtocolComponent {
        // All Ekubo swaps go through the core contract - not necessary to specify pool
        // id for test
        protocol_system: "ekubo_v2".to_string(),
        static_attributes,
        ..Default::default()
    };

    let swap = Swap::new(
        component,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("1_000_000_000").unwrap(),
        BigUint::from_str("1000").unwrap(),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_ekubo_mev_resist", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_maverick() {
    // GHO -> (maverick) -> USDC
    let maverick_pool = ProtocolComponent {
        id: String::from("0x14Cf6D2Fe3E1B326114b07d22A6F6bb59e346c67"),
        protocol_system: String::from("vm:maverick_v2"),
        ..Default::default()
    };
    let token_in = Bytes::from("0x40D16FC0246aD3160Ccc09B8D0D3A2cD28aE6C2f");
    let token_out = usdc();
    let swap = Swap::new(
        maverick_pool,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_maverick", hex_calldata.as_str());
}

#[test]
fn test_evm_single_encoding_strategy_usv4_eth_in() {
    // Performs a single swap from ETH to PEPE using a USV4 pool
    // Note: This test does not assert anything. It is only used to obtain integration
    // test data for our router solidity test.
    //
    //   ETH ───(USV4)──> PEPE
    //
    let eth = eth();
    let pepe = pepe();

    let pool_fee_eth_pepe = Bytes::from(BigInt::from(25000).to_signed_bytes_be());
    let tick_spacing_eth_pepe = Bytes::from(BigInt::from(500).to_signed_bytes_be());
    let mut static_attributes_eth_pepe: HashMap<String, Bytes> = HashMap::new();
    static_attributes_eth_pepe.insert("key_lp_fee".into(), pool_fee_eth_pepe);
    static_attributes_eth_pepe.insert("tick_spacing".into(), tick_spacing_eth_pepe);

    let swap_eth_pepe = Swap::new(
        ProtocolComponent {
            id: "0xecd73ecbf77219f21f129c8836d5d686bbc27d264742ddad620500e3e548e2c9".to_string(),
            protocol_system: "uniswap_v4".to_string(),
            static_attributes: static_attributes_eth_pepe,
            ..Default::default()
        },
        default_token(eth.clone()),
        default_token(pepe.clone()),
        BigUint::ZERO,
    );
    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        eth.clone(),
        pepe,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("152373460199848577067005852").unwrap(),
        vec![swap_eth_pepe],
    );

    let encoded_solution = encoder
        .encode_solutions(vec![solution.clone()])
        .unwrap()[0]
        .clone();

    let calldata = encode_tycho_router_call(
        eth_chain().id(),
        encoded_solution,
        &solution,
        &eth,
        Some(get_signer()),
        0,
        Bytes::zero(20),
        BigUint::ZERO,
    )
    .unwrap()
    .data;
    let hex_calldata = encode(&calldata);

    write_calldata_to_file("test_single_encoding_strategy_usv4_eth_in", hex_calldata.as_str());
}

#[test]
fn test_evm_single_encoding_strategy_usv4_eth_out() {
    // Performs a single swap from USDC to ETH using a USV4 pool
    // Note: This test does not assert anything. It is only used to obtain integration
    // test data for our router solidity test.
    //
    //   USDC ───(USV4)──> ETH
    //
    let eth = eth();
    let usdc = usdc();

    // Fee and tick spacing information for this test is obtained by querying the
    // USV4 Position Manager contract: 0xbd216513d74c8cf14cf4747e6aaa6420ff64ee9e
    // Using the poolKeys function with the first 25 bytes of the pool id
    let pool_fee_usdc_eth = Bytes::from(BigInt::from(3000).to_signed_bytes_be());
    let tick_spacing_usdc_eth = Bytes::from(BigInt::from(60).to_signed_bytes_be());
    let mut static_attributes_usdc_eth: HashMap<String, Bytes> = HashMap::new();
    static_attributes_usdc_eth.insert("key_lp_fee".into(), pool_fee_usdc_eth);
    static_attributes_usdc_eth.insert("tick_spacing".into(), tick_spacing_usdc_eth);

    let swap_usdc_eth = Swap::new(
        ProtocolComponent {
            id: "0xdce6394339af00981949f5f3baf27e3610c76326a700af57e4b3e3ae4977f78d".to_string(),
            protocol_system: "uniswap_v4".to_string(),
            static_attributes: static_attributes_usdc_eth,
            ..Default::default()
        },
        default_token(usdc.clone()),
        default_token(eth.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        usdc,
        eth.clone(),
        BigUint::from_str("3000_000000").unwrap(),
        BigUint::from_str("1117254495486192350").unwrap(),
        vec![swap_usdc_eth],
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
        &eth,
        Some(get_signer()),
        0,
        Bytes::zero(20),
        BigUint::ZERO,
    )
    .unwrap()
    .data;

    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_usv4_eth_out", hex_calldata.as_str());
}

#[test]
fn test_evm_single_encoding_strategy_usv4_grouped_swap() {
    // Performs a sequential swap from USDC to PEPE though ETH using two consecutive
    // USV4 pools
    //
    //   USDC ──(USV4)──> ETH ───(USV4)──> PEPE
    //

    let eth = eth();
    let usdc = usdc();
    let pepe = pepe();

    // Fee and tick spacing information for this test is obtained by querying the
    // USV4 Position Manager contract: 0xbd216513d74c8cf14cf4747e6aaa6420ff64ee9e
    // Using the poolKeys function with the first 25 bytes of the pool id
    let pool_fee_usdc_eth = Bytes::from(BigInt::from(3000).to_signed_bytes_be());
    let tick_spacing_usdc_eth = Bytes::from(BigInt::from(60).to_signed_bytes_be());
    let mut static_attributes_usdc_eth: HashMap<String, Bytes> = HashMap::new();
    static_attributes_usdc_eth.insert("key_lp_fee".into(), pool_fee_usdc_eth);
    static_attributes_usdc_eth.insert("tick_spacing".into(), tick_spacing_usdc_eth);

    let pool_fee_eth_pepe = Bytes::from(BigInt::from(25000).to_signed_bytes_be());
    let tick_spacing_eth_pepe = Bytes::from(BigInt::from(500).to_signed_bytes_be());
    let mut static_attributes_eth_pepe: HashMap<String, Bytes> = HashMap::new();
    static_attributes_eth_pepe.insert("key_lp_fee".into(), pool_fee_eth_pepe);
    static_attributes_eth_pepe.insert("tick_spacing".into(), tick_spacing_eth_pepe);

    let swap_usdc_eth = Swap::new(
        ProtocolComponent {
            id: "0xdce6394339af00981949f5f3baf27e3610c76326a700af57e4b3e3ae4977f78d".to_string(),
            protocol_system: "uniswap_v4".to_string(),
            static_attributes: static_attributes_usdc_eth,
            ..Default::default()
        },
        default_token(usdc.clone()),
        default_token(eth.clone()),
        BigUint::ZERO,
    );

    let swap_eth_pepe = Swap::new(
        ProtocolComponent {
            id: "0xecd73ecbf77219f21f129c8836d5d686bbc27d264742ddad620500e3e548e2c9".to_string(),
            protocol_system: "uniswap_v4".to_string(),
            static_attributes: static_attributes_eth_pepe,
            ..Default::default()
        },
        default_token(eth.clone()),
        default_token(pepe.clone()),
        BigUint::ZERO,
    );
    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        usdc,
        pepe,
        BigUint::from_str("1000_000000").unwrap(),
        BigUint::from_str("97191013220606467325121599").unwrap(),
        vec![swap_usdc_eth, swap_eth_pepe],
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
        &eth,
        Some(get_signer()),
        0,
        Bytes::zero(20),
        BigUint::ZERO,
    )
    .unwrap()
    .data;

    let expected_input = [
        "e7a307b0", // Function selector (singleSwapPermit2)
        "000000000000000000000000000000000000000000000000000000003b9aca00", // amount in
        "000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // token in
        "0000000000000000000000006982508145454ce325ddbe47a25d4ec3d2311933", // token out
        "0000000000000000000000000000000000000000005064ff624d54346285543f", // min amount out
        "000000000000000000000000cd09f75e2bf2a4d11f3ab23f1389fcc1621c0cc2", // receiver
        "00000000000000000000000000000000000000000000000000000000000001c0", /* clientFeeParams
                     * offset */
    ]
    .join("");

    // After this there is the permit and because of the deadlines (that depend on block
    // time) it's hard to assert back

    let expected_swaps = String::from(concat!(
        // length of ple encoded swaps without padding
        "00000000000000000000000000000000000000000000000000000000000000a0",
        // Swap data header
        "f62849f9a0b5bf2913b396098f7c7019b51a820a", // executor address
        // Protocol data
        "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48", // group token in
        "6982508145454ce325ddbe47a25d4ec3d2311933", // group token out
        "00",                                       // zero2one
        "00",                                       // skip unlock
        // First pool params
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee", // intermediary token (ETH_ADDRESS)
        "000bb8",                                   // fee
        "00003c",                                   // tick spacing
        "0000000000000000000000000000000000000000", // hook address
        "0000",                                     // hook data length
        // ple encoding
        "0030",
        // Second pool params
        "6982508145454ce325ddbe47a25d4ec3d2311933", // intermediary token (PEPE)
        "0061a8",                                   // fee
        "0001f4",                                   // tick spacing
        "0000000000000000000000000000000000000000", // hook address
        "0000",                                     // hook data length
    ));

    let hex_calldata = encode(&calldata);

    assert_eq!(hex_calldata[..392], expected_input);
    assert_eq!(hex_calldata[1544..], expected_swaps);
    write_calldata_to_file(
        "test_single_encoding_strategy_usv4_grouped_swap",
        hex_calldata.as_str(),
    );
}

#[test]
fn test_evm_single_encoding_strategy_usv4_and_hooks_grouped_swap() {
    // Performs a sequential swap from WETH to USDC through ETH using
    // a USV4 pool with Euler hooks followed by a USV4 pool with no hooks
    //
    //   WETH  ───(USV4 Euler Hook)──> USDC ──(USV4)──> ETH

    let usdc = usdc();
    let eth = eth();
    let weth = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();

    // First pool: WETH -> USDC (USV4 with Euler)
    let pool_fee_weth_usdt = Bytes::from(BigInt::from(500).to_signed_bytes_be());
    let tick_spacing_weth_usdt = Bytes::from(BigInt::from(1).to_signed_bytes_be());
    let euler_hook = Bytes::from_str("0x69058613588536167BA0AA94F0CC1Fe420eF28a8").unwrap();
    let mut static_attributes_weth_usdt: HashMap<String, Bytes> = HashMap::new();
    static_attributes_weth_usdt.insert("key_lp_fee".into(), pool_fee_weth_usdt);
    static_attributes_weth_usdt.insert("tick_spacing".into(), tick_spacing_weth_usdt);
    static_attributes_weth_usdt.insert("hooks".into(), euler_hook);

    // Second pool: USDC -> ETH (USV4 no hooks)
    let pool_fee_usdc_eth = Bytes::from(BigInt::from(3000).to_signed_bytes_be());
    let tick_spacing_usdc_eth = Bytes::from(BigInt::from(60).to_signed_bytes_be());
    let mut static_attributes_usdc_eth: HashMap<String, Bytes> = HashMap::new();
    static_attributes_usdc_eth.insert("key_lp_fee".into(), pool_fee_usdc_eth);
    static_attributes_usdc_eth.insert("tick_spacing".into(), tick_spacing_usdc_eth);

    let swap_weth_usdc = Swap::new(
        ProtocolComponent {
            protocol_system: "uniswap_v4_hooks".to_string(),
            static_attributes: static_attributes_weth_usdt,
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    );

    let swap_usdc_eth = Swap::new(
        ProtocolComponent {
            id: "0xdce6394339af00981949f5f3baf27e3610c76326a700af57e4b3e3ae4977f78d".to_string(),
            protocol_system: "uniswap_v4".to_string(),
            static_attributes: static_attributes_usdc_eth,
            ..Default::default()
        },
        default_token(usdc.clone()),
        default_token(eth.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        weth,
        eth.clone(),
        BigUint::from_str("1000000000000000000").unwrap(), // 1 WETH
        BigUint::from_str("900000000000000000").unwrap(),  // 0.9 ETH
        vec![swap_weth_usdc, swap_usdc_eth],
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
        &eth,
        Some(get_signer()),
        0,
        Bytes::zero(20),
        BigUint::ZERO,
    )
    .unwrap()
    .data;

    let hex_calldata = encode(&calldata);
    write_calldata_to_file(
        "test_single_encoding_strategy_usv4_and_hooks_grouped_swap",
        hex_calldata.as_str(),
    );
}

#[test]
fn test_single_encoding_strategy_ekubo_grouped_swap() {
    // Test multi-hop Ekubo swap (grouped swaps)
    //
    //   USDE ──(EKUBO)──> USDC ──(EKUBO)──> USDT

    let usde = Bytes::from_str("0x4c9edd5852cd905f086c759e8383e09bff1e68b3").unwrap();
    let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
    let usdt = Bytes::from_str("0xdac17f958d2ee523a2206206994597c13d831ec7").unwrap();

    // First swap: USDE -> USDC
    let swap1 = Swap::new(
        ProtocolComponent {
            id: "a419f0ebb019eb85fdccd0200843752dd9cc31d0cb3127f3adb4ba37a092788f".to_string(),
            protocol_system: "ekubo_v2".to_string(),
            static_attributes: HashMap::from([
                ("fee".to_string(), Bytes::from(922337203685478_u64)),
                ("tick_spacing".to_string(), Bytes::from(100_u32)),
                (
                    "extension".to_string(),
                    Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
                ),
            ]),
            ..Default::default()
        },
        default_token(usde.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    );

    // Second swap: USDC -> USDT
    let swap2 = Swap::new(
        ProtocolComponent {
            id: "ca5b3ef9770bb95940bd4e0bff5ead70a5973d904a8b370b52147820e61a2ff6".to_string(),
            protocol_system: "ekubo_v2".to_string(),
            static_attributes: HashMap::from([
                ("fee".to_string(), Bytes::from(92233720368547_u64)),
                ("tick_spacing".to_string(), Bytes::from(50_u32)),
                (
                    "extension".to_string(),
                    Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
                ),
            ]),
            ..Default::default()
        },
        default_token(usdc.clone()),
        default_token(usdt.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        usde,
        usdt,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
        vec![swap1, swap2],
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

    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_ekubo_grouped_swap", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_curve() {
    //   UWU ──(curve 2 crypto pool)──> WETH

    let token_in = Bytes::from("0x55C08ca52497e2f1534B59E2917BF524D4765257"); // UWU
    let token_out = weth();

    let static_attributes = HashMap::from([(
        "factory".to_string(),
        Bytes::from(
            "0x98ee851a00abee0d95d08cf4ca2bdce32aeaaf7f"
                .as_bytes()
                .to_vec(),
        )),
        ("coins".to_string(), Bytes::from_str("0x5b22307863303261616133396232323366653864306130653563346632376561643930383363373536636332222c22307835356330386361353234393765326631353334623539653239313762663532346434373635323537225d").unwrap()),
    ]);

    let component = ProtocolComponent {
        id: String::from("0x77146B0a1d08B6844376dF6d9da99bA7F1b19e71"),
        protocol_system: String::from("vm:curve"),
        static_attributes,
        ..Default::default()
    };

    let swap = Swap::new(
        component,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1").unwrap(),
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

    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_curve", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_curve_st_eth() {
    //   ETH ──(curve stETH pool)──> STETH

    let token_in = eth();
    let token_out = Bytes::from("0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84"); // STETH

    let static_attributes = HashMap::from([(
        "factory".to_string(),
        Bytes::from(
            "0x0000000000000000000000000000000000000000"
                .as_bytes()
                .to_vec(),
        ),
    ),
        ("coins".to_string(), Bytes::from_str("0x5b22307865656565656565656565656565656565656565656565656565656565656565656565656565656565222c22307861653761623936353230646533613138653565313131623565616162303935333132643766653834225d").unwrap()),]);

    let component = ProtocolComponent {
        id: String::from("0xDC24316b9AE028F1497c275EB9192a3Ea0f67022"),
        protocol_system: String::from("vm:curve"),
        static_attributes,
        ..Default::default()
    };

    let swap = Swap::new(
        component,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1").unwrap(),
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

    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_curve_st_eth", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_curve_protocol_will_debit_from_vault() {
    // Test ProtocolWillDebit with Curve where funds are taken from user's vault
    // First swap: DAI from vault -> (Curve TriPool) -> USDC
    //
    // This tests the case where:
    // 1. User has DAI in their vault
    // 2. First swap uses the vault (via singleSwapUsingVault)
    // 3. Curve uses ProtocolWillDebit to pull funds from router
    // 4. Funds are successfully taken from the vault balance

    let dai = Bytes::from_str("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap();
    let usdc = Bytes::from_str("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap();

    let static_attributes = HashMap::from([
        (
            "factory".to_string(),
            Bytes::from(
                "0x0000000000000000000000000000000000000000"
                    .as_bytes()
                    .to_vec(),
            ),
        ),
        ("coins".to_string(), Bytes::from_str("0x5b22307836623137353437346538393039346334346461393862393534656564656163343935323731643066222c22307861306238363939316336323138623336633164313964346132653965623063653336303665623438222c22307864616331376639353864326565353233613232303632303639393435393763313364383331656337225d").unwrap()),
    ]);

    let curve_tripool = ProtocolComponent {
        id: String::from("0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7"),
        protocol_system: String::from("vm:curve"),
        static_attributes,
        ..Default::default()
    };

    let swap = Swap::new(
        curve_tripool,
        default_token(dai.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        alice_address(),
        alice_address(),
        dai,
        usdc,
        BigUint::from_str("1000_000000000000000000").unwrap(), // 1000 DAI
        BigUint::from_str("1").unwrap(),
        vec![swap],
    )
    .with_user_transfer_type(UserTransferType::UseVaultsFunds);

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

    let hex_calldata = encode(&calldata);
    write_calldata_to_file(
        "test_single_encoding_strategy_curve_protocol_will_debit_from_vault",
        hex_calldata.as_str(),
    );
}

#[test]
fn test_single_encoding_strategy_balancer_v3() {
    // steakUSDTlite -> (balancer v3) -> steakUSDR
    let balancer_pool = ProtocolComponent {
        id: String::from("0xf028ac624074d6793c36dc8a06ecec0f5a39a718"),
        protocol_system: String::from("vm:balancer_v3"),
        ..Default::default()
    };
    let token_in = Bytes::from("0x097ffedb80d4b2ca6105a07a4d90eb739c45a666");
    let token_out = Bytes::from("0x30881baa943777f92dc934d53d3bfdf33382cab3");
    let swap = Swap::new(
        balancer_pool,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_balancer_v3", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_bebop() {
    // The quote was done separately where the sender is the router and the receiver is a random
    // user
    let _router = Bytes::from_str("0x6bc529DC7B81A031828dDCE2BC419d01FF268C66").unwrap();
    let user = Bytes::from_str("0xd2068e04cf586f76eece7ba5beb779d7bb1474a1").unwrap();

    let token_in = usdc();
    let token_out = ondo();
    let amount_in = BigUint::from_str("200000000").unwrap(); // 200 USDC
    let amount_out = BigUint::from_str("582464275842264783022").unwrap();

    let partial_fill_offset = 12u64;
    let bebop_calldata = Bytes::from_str("0x4dcebcba00000000000000000000000000000000000000000000000000000000697215240000000000000000000000006bc529dc7b81a031828ddce2bc419d01ff268c6600000000000000000000000051c72848c68a965f66fa7a88855f9f7784502a7f00000000000000000000000000000000000000000000000027d0330b6f18fece000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48000000000000000000000000faba6f8e4a5e8ab82f62fe7c39859fa577269be3000000000000000000000000000000000000000000000000000000000bebc20000000000000000000000000000000000000000000000001f9350ccd9bd54d8ae0000000000000000000000006bc529dc7b81a031828ddce2bc419d01ff268c66000000000000000000000000000000000000000000000000000000000000000020dd178be2d265ea34d555878d0a826c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001a00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000041df80ac607bd068150dd110509b97a01177035f4d9d27c00cbcbc184d2bfad9e6410fe402152a7a7298fac95b8c3338055e23905034a09b91cff87ef4b88249101c00000000000000000000000000000000000000000000000000000000000000").unwrap();
    let bebop_state = MockRFQState {
        quote_amount_out: amount_out.clone(),
        quote_data: HashMap::from([
            ("calldata".to_string(), bebop_calldata),
            (
                "partial_fill_offset".to_string(),
                Bytes::from(
                    partial_fill_offset
                        .to_be_bytes()
                        .to_vec(),
                ),
            ),
        ]),
    };

    let bebop_component = ProtocolComponent {
        id: String::from("bebop-rfq"),
        protocol_system: String::from("rfq:bebop"),
        static_attributes: HashMap::new(), // No static attributes needed
        ..Default::default()
    };

    let swap = Swap::new(
        bebop_component,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    )
    .with_estimated_amount_in(BigUint::from_str("200000000").unwrap())
    .with_protocol_state(Arc::new(bebop_state));

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        user.clone(),
        user,
        token_in,
        token_out,
        amount_in,
        amount_out, // Expected output amount
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
    let hex_calldata = hex::encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_bebop", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_bebop_aggregate() {
    // The quote was done separately where the sender is the router and the receiver is a random
    // user
    let _router = Bytes::from_str("0x6bc529DC7B81A031828dDCE2BC419d01FF268C66").unwrap();
    let user = Bytes::from_str("0xd2068e04cf586f76eece7ba5beb779d7bb1474a1").unwrap();

    let token_in = usdc();
    let token_out = ondo();
    let amount_in = BigUint::from_str("20000000000").unwrap(); // 20k USDC
    let amount_out = BigUint::from_str("58302581300158475047842").unwrap();
    let partial_fill_offset = 2u64;

    let bebop_calldata = Bytes::from_str("0xa2f7489300000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000640000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000697215cf0000000000000000000000006bc529dc7b81a031828ddce2bc419d01ff268c66000000000000000000000000000000000000000000000000000000000000016000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000220000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000000000000000000000000000003e000000000000000000000000000000000000000000000000000000000000004c00000000000000000000000006bc529dc7b81a031828ddce2bc419d01ff268c6600000000000000000000000000000000000000000000000000000000000005a08dd0c54e8e2d4918ad2ae17adcad316c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000000000000067336cec42645f55059eff241cb02ea5cc52ff8600000000000000000000000051c72848c68a965f66fa7a88855f9f7784502a7f000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000027d0330b6f19040400000000000000000000000000000000000000000000000027d0330b6f1904050000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000001000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb480000000000000000000000000000000000000000000000000000000000000001000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb480000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000001000000000000000000000000faba6f8e4a5e8ab82f62fe7c39859fa577269be30000000000000000000000000000000000000000000000000000000000000001000000000000000000000000faba6f8e4a5e8ab82f62fe7c39859fa577269be300000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000224cce24e000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000002834ae5b200000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000005af1d63497a5be7d1b500000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000006a97976ceb39a18c1ed00000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000041c41d5149290fc301bf2d936e80adc1a857ae230351b28d4c6d905d5cd4fcce5a333bff59c2130df099e8886b1101ed761adcc3510531aab3b40b40e17fd917991b000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000418df1f9cbfea2aea60a7e34a361e2521b2fd607373197784d8131f857a5888b48380a33803e87e32a7cb1f1ac3845d4a3229d61f74663c2547341e04f08759ce31b00000000000000000000000000000000000000000000000000000000000000").unwrap();
    let bebop_state = MockRFQState {
        quote_amount_out: amount_out.clone(),
        quote_data: HashMap::from([
            ("calldata".to_string(), bebop_calldata),
            (
                "partial_fill_offset".to_string(),
                Bytes::from(
                    partial_fill_offset
                        .to_be_bytes()
                        .to_vec(),
                ),
            ),
        ]),
    };

    let bebop_component = ProtocolComponent {
        id: String::from("bebop-rfq"),
        protocol_system: String::from("rfq:bebop"),
        static_attributes: HashMap::new(),
        ..Default::default()
    };

    let swap = Swap::new(
        bebop_component,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    )
    .with_estimated_amount_in(BigUint::from_str("20000000000").unwrap())
    .with_protocol_state(Arc::new(bebop_state));

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution =
        Solution::new(user.clone(), user, token_in, token_out, amount_in, amount_out, vec![swap]);

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
    let hex_calldata = hex::encode(&calldata);

    write_calldata_to_file("test_single_encoding_strategy_bebop_aggregate", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_hashflow() {
    // Note: This test does not assert anything. It is only used to obtain
    // integration test data for our router solidity test.
    //
    // Performs a swap from USDC to WBTC using Hashflow RFQ
    //
    //   USDC ───(Hashflow RFQ)──> WBTC

    let usdc = usdc();
    let wbtc = wbtc();

    // USDC -> WBTC via Hashflow RFQ using real order data
    let quote_amount_out = BigUint::from_str("3714751").unwrap();

    let hashflow_state = MockRFQState {
        quote_amount_out,
        quote_data: HashMap::from([
            (
                "pool".to_string(),
                Bytes::from_str("0x4c4c3e005c6cb9ce249a267f28299293a628cf38").unwrap(),
            ),
            (
                "external_account".to_string(),
                Bytes::from_str("0x6047b384d58dc7f8f6fef85d75754e6928f06484").unwrap(),
            ),
            (
                "trader".to_string(),
                Bytes::from_str("0x6bc529dc7b81a031828ddce2bc419d01ff268c66").unwrap(),
            ),
            (
                "base_token".to_string(),
                Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            ),
            (
                "quote_token".to_string(),
                Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap(),
            ),
            (
                "base_token_amount".to_string(),
                Bytes::from(biguint_to_u256(&BigUint::from(4308094737_u64)).to_be_bytes::<32>().to_vec()),
            ),
            (
                "quote_token_amount".to_string(),
                Bytes::from(biguint_to_u256(&BigUint::from(4795673_u64)).to_be_bytes::<32>().to_vec()),
            ),
            ("quote_expiry".to_string(), Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000069721877").unwrap()),
            ("nonce".to_string(), Bytes::from_str("0x0000000000000000000000000000000000000000000000000000019be5aea5f8").unwrap()),
            (
                "tx_id".to_string(),
                Bytes::from_str(
                    "0x1250000640006400000017471dc488ffffffffffffff002c8747bfe4f0440000",
                )
                    .unwrap(),
            ),
            ("signature".to_string(), Bytes::from_str("0xdedc8a21a00afdac18e0a62b3f0d641d21de75e1fa0bb8f402ccf047923274fe40df9e249f693d88be4a005f4217d21ed920eac7373fd23d8329d3c6b0c873f71c").unwrap()),
        ]),
    };

    let hashflow_component = ProtocolComponent {
        id: String::from("hashflow-rfq"),
        protocol_system: String::from("rfq:hashflow"),
        ..Default::default()
    };

    let swap_usdc_wbtc = Swap::new(
        hashflow_component,
        default_token(usdc.clone()),
        default_token(wbtc.clone()),
        BigUint::ZERO,
    )
    .with_estimated_amount_in(BigUint::from_str("4308094737").unwrap())
    .with_protocol_state(Arc::new(hashflow_state));
    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        alice_address(),
        alice_address(),
        usdc,
        wbtc,
        BigUint::from_str("4308094737").unwrap(),
        BigUint::from_str("3714751").unwrap(),
        vec![swap_usdc_wbtc],
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

    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_hashflow", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_fluid() {
    let fluid_dex = ProtocolComponent {
        id: String::from("0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b"),
        protocol_system: String::from("fluid_v1"),
        ..Default::default()
    };
    let token_in = Bytes::from("0x9d39a5de30e57443bff2a8307a4256c8797a3497");
    let token_out = Bytes::from("0xdac17f958d2ee523a2206206994597c13d831ec7");
    let alice = Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap();
    let swap = Swap::new(
        fluid_dex,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        alice.clone(),
        alice,
        token_in,
        token_out,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_fluid_v1", hex_calldata.as_str());
}

#[test]
fn test_sequential_encoding_strategy_fluid() {
    let fluid_dex_1 = ProtocolComponent {
        id: String::from("0x1DD125C32e4B5086c63CC13B3cA02C4A2a61Fa9b"),
        protocol_system: String::from("fluid_v1"),
        ..Default::default()
    };
    let fluid_dex_2 = ProtocolComponent {
        id: String::from("0xea734B615888c669667038D11950f44b177F15C0"),
        protocol_system: String::from("fluid_v1"),
        ..Default::default()
    };
    let token_in = Bytes::from("0x9d39a5de30e57443bff2a8307a4256c8797a3497");
    let usdt = Bytes::from("0xdac17f958d2ee523a2206206994597c13d831ec7");
    let token_out = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
    let alice = Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap();
    let swap_1 = Swap::new(
        fluid_dex_1,
        default_token(token_in.clone()),
        default_token(usdt.clone()),
        BigUint::ZERO,
    );
    let swap_2 = Swap::new(
        fluid_dex_2,
        default_token(usdt.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        alice.clone(),
        alice,
        token_in,
        token_out,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
        vec![swap_1, swap_2],
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_sequential_encoding_strategy_fluid_v1", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_rocketpool_deposit() {
    // ETH -> (rocketpool) -> rETH
    // Based on real tx 0xe0f1db165b621cb1e50b629af9d47e064be464fbcc7f2bcba3df1d27dbb916be
    // at block 24480105 where 85 ETH was deposited for 73382345660413064855 rETH
    let rocketpool_pool = ProtocolComponent {
        id: String::from("0xae78736Cd615f374D3085123A210448E74Fc6393"),
        protocol_system: String::from("rocketpool"),
        ..Default::default()
    };
    let token_in = eth();
    let token_out = Bytes::from("0xae78736Cd615f374D3085123A210448E74Fc6393");
    let swap = Swap::new(
        rocketpool_pool,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from(85_000_000_000_000_000_000_u128),
        BigUint::from(73_382_345_660_413_064_855_u128),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file(
        "test_single_encoding_strategy_rocketpool_deposit",
        hex_calldata.as_str(),
    );
}

#[test]
fn test_single_encoding_strategy_rocketpool_burn() {
    // rETH -> (rocketpool) -> ETH
    // Block 24481338: user burned 2515686112138065226 rETH and received 2912504376202664754 ETH
    // We use `bob*` address as sender/receiver as Alice's address has a drainer deployed that
    // would interfere with the test when we send ETH back to her.
    let rocketpool_pool = ProtocolComponent {
        id: String::from("0xae78736Cd615f374D3085123A210448E74Fc6393"),
        protocol_system: String::from("rocketpool"),
        ..Default::default()
    };
    let token_in = Bytes::from("0xae78736Cd615f374D3085123A210448E74Fc6393");
    let token_out = eth();
    let swap = Swap::new(
        rocketpool_pool,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0x9964bff29baa37b47604f3f3f51f3b3c5149d6de").unwrap(),
        Bytes::from_str("0x9964bff29baa37b47604f3f3f51f3b3c5149d6de").unwrap(),
        token_in,
        token_out,
        BigUint::from(2_515_686_112_138_065_226_u128),
        BigUint::from(2_912_504_376_202_664_754_u128),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_rocketpool_burn", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_slipstreams() {
    // WETH -> (Slipstreams) -> USDC
    let static_attributes = HashMap::from([(
        "tick_spacing".to_string(),
        Bytes::from(BigInt::from(100).to_signed_bytes_be()),
    )]);

    let slipstreams_pool = ProtocolComponent {
        id: String::from("0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59"),
        protocol_system: String::from("aerodrome_slipstreams"),
        static_attributes,
        ..Default::default()
    };
    let token_in = Bytes::from("0x4200000000000000000000000000000000000006");
    let token_out = Bytes::from("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    let swap = Swap::new(
        slipstreams_pool,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Base);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_slipstreams", hex_calldata.as_str());
}

#[test]
fn test_sequential_encoding_strategy_slipstreams() {
    // WETH -> (Slipstreams) -> USDC -> (Slipstreams) -> cbBTC
    let slipstreams_weth_usdc_pool = ProtocolComponent {
        id: String::from("0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59"),
        protocol_system: String::from("aerodrome_slipstreams"),
        static_attributes: HashMap::from([(
            "tick_spacing".to_string(),
            Bytes::from(BigInt::from(100).to_signed_bytes_be()),
        )]),
        ..Default::default()
    };
    let weth = Bytes::from("0x4200000000000000000000000000000000000006");
    let usdc = Bytes::from("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    let swap1 = Swap::new(
        slipstreams_weth_usdc_pool,
        default_token(weth.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    );
    let slipstreams_cbbtc_usdc_pool = ProtocolComponent {
        id: String::from("0x4e962BB3889Bf030368F56810A9c96B83CB3E778"),
        protocol_system: String::from("aerodrome_slipstreams"),
        static_attributes: HashMap::from([(
            "tick_spacing".to_string(),
            Bytes::from(BigInt::from(100).to_signed_bytes_be()),
        )]),
        ..Default::default()
    };
    let btc = Bytes::from("0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf");
    let swap2 = Swap::new(
        slipstreams_cbbtc_usdc_pool,
        default_token(usdc.clone()),
        default_token(btc.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Base);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        weth.clone(),
        btc.clone(),
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
        vec![swap1, swap2],
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_sequential_encoding_strategy_slipstreams", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_aerodrome_v1() {
    // tBTC -> (Aerodrome V1) -> USDbC
    let aerodrome_v1_volatile_pool = ProtocolComponent {
        id: String::from("0x723aef6543aece026a15662be4d3fb3424d502a9"),
        protocol_system: String::from("aerodrome_v1"),
        ..Default::default()
    };
    let token_in = Bytes::from("0x236aa50979d5f3de3bd1eeb40e81137f22ab794b");
    let token_out = Bytes::from("0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca");
    let swap = Swap::new(
        aerodrome_v1_volatile_pool,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Base);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("10000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_aerodrome_v1", hex_calldata.as_str());
}

#[test]
fn test_sequential_encoding_strategy_aerodrome_v1() {
    // DOLA -> (Aerodrome V1) -> USDbC -> (Aerodrome V1) -> tBTC
    let aerodrome_v1_stable_pool = ProtocolComponent {
        id: String::from("0x0b25c51637c43decd6cc1c1e3da4518d54ddb528"),
        protocol_system: String::from("aerodrome_v1"),
        ..Default::default()
    };
    let aerodrome_v1_volatile_pool = ProtocolComponent {
        id: String::from("0x723aef6543aece026a15662be4d3fb3424d502a9"),
        protocol_system: String::from("aerodrome_v1"),
        ..Default::default()
    };
    let dola = Bytes::from("0x4621b7a9c75199271f773ebd9a499dbd165c3191");
    let usdbc = Bytes::from("0xd9aaec86b65d86f6a7b5b1b0c42ffa531710b6ca");
    let tbtc = Bytes::from("0x236aa50979d5f3de3bd1eeb40e81137f22ab794b");
    let swap1 = Swap::new(
        aerodrome_v1_stable_pool,
        default_token(dola.clone()),
        default_token(usdbc.clone()),
        BigUint::ZERO,
    );
    let swap2 = Swap::new(
        aerodrome_v1_volatile_pool,
        default_token(usdbc.clone()),
        default_token(tbtc.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Base);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        dola,
        tbtc,
        BigUint::from_str("10000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
        vec![swap1, swap2],
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_sequential_encoding_strategy_aerodrome_v1", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_erc4626() {
    // WETH -> (ERC4626) -> spETH
    let erc4626_pool = ProtocolComponent {
        id: String::from("0xfE6eb3b609a7C8352A241f7F3A21CEA4e9209B8f"),
        protocol_system: String::from("erc4626"),
        ..Default::default()
    };
    let token_in = Bytes::from("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    let token_out = Bytes::from("0xfE6eb3b609a7C8352A241f7F3A21CEA4e9209B8f");
    let swap = Swap::new(
        erc4626_pool,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_erc4626", hex_calldata.as_str());
}

#[test]
fn test_sequential_encoding_strategy_erc4626() {
    // spUSDC -> (ERC4626) -> USDC -> (ERC4626) -> sUSDC
    let spusdc_pool = ProtocolComponent {
        id: String::from("0x28b3a8fb53b741a8fd78c0fb9a6b2393d896a43d"),
        protocol_system: String::from("erc4626"),
        ..Default::default()
    };
    let sp_usdc = Bytes::from("0x28b3a8fb53b741a8fd78c0fb9a6b2393d896a43d");
    let usdc = Bytes::from("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    let swap1 = Swap::new(
        spusdc_pool,
        default_token(sp_usdc.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    );
    let susdc_pool = ProtocolComponent {
        id: String::from("0xbc65ad17c5c0a2a4d159fa5a503f4992c7b545fe"),
        protocol_system: String::from("erc4626"),
        ..Default::default()
    };
    let susdc = Bytes::from("0xbc65ad17c5c0a2a4d159fa5a503f4992c7b545fe");
    let swap2 = Swap::new(
        susdc_pool,
        default_token(usdc.clone()),
        default_token(susdc.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        sp_usdc.clone(),
        susdc.clone(),
        BigUint::from_str("100_000_000").unwrap(),
        BigUint::from_str("90_000000000000000000").unwrap(),
        vec![swap1, swap2],
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_sequential_encoding_strategy_erc4626", hex_calldata.as_str());
}

#[test]
#[ignore] // Performs real Angstrom API call
fn test_single_swap_with_univ4_angstrom() {
    //  USDC ─── (USV4-angstrom) ──> WETH

    dotenvy::dotenv().ok();

    let weth = weth();
    let usdc = usdc();

    // USDC -> WETH (Uniswap v4 with Angstrom hook)
    let angstrom_hook = Bytes::from("0x0000000aa232009084Bd71A5797d089AA4Edfad4");
    let mut usdc_weth_attributes: HashMap<String, Bytes> = HashMap::new();
    usdc_weth_attributes.insert("key_lp_fee".into(), Bytes::from("0x800000")); // 8388608
    usdc_weth_attributes.insert("tick_spacing".into(), Bytes::from("0x0a")); // 10
    usdc_weth_attributes.insert("hooks".into(), angstrom_hook.clone());
    let swap = Swap::new(
        ProtocolComponent {
            id: "0x000000000004444c5dc75cB358380D2e3dE08A90".to_string(),
            protocol_system: "uniswap_v4_hooks".to_string(),
            static_attributes: usdc_weth_attributes,
            ..Default::default()
        },
        default_token(usdc.clone()),
        default_token(weth.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        alice_address(),
        alice_address(),
        usdc.clone(),
        weth.clone(),
        BigUint::from_str("100000000").unwrap(), // 100 USDC (6 decimals)
        BigUint::from_str("99574171").unwrap(),
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
        Some(get_signer()),
        0,
        Bytes::zero(20),
        BigUint::ZERO,
    )
    .unwrap()
    .data;

    let hex_calldata = encode(&calldata);

    // The angstrom attestation adds calldata at the end. If they are not being encoded the
    // following assert would fail
    assert_eq!(hex_calldata[904..].len(), 1152);
}

#[test]
fn test_single_encoding_strategy_weth_wrap() {
    let wrap_executor =
        ProtocolComponent { protocol_system: String::from("native_wrap"), ..Default::default() };
    let token_in = eth();
    let token_out = weth();
    let swap = Swap::new(
        wrap_executor,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        alice_address(),
        alice_address(),
        token_in,
        token_out,
        BigUint::from(1_000_000_000_000_000_000_u128),
        BigUint::from(1_000_000_000_000_000_000_u128),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_wrap_wrapping", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_weth_unwrap() {
    let wrap_executor =
        ProtocolComponent { protocol_system: String::from("native_wrap"), ..Default::default() };
    let token_in = weth();
    let token_out = eth();
    let swap = Swap::new(
        wrap_executor,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0x9964bff29baa37b47604f3f3f51f3b3c5149d6de").unwrap(),
        Bytes::from_str("0x9964bff29baa37b47604f3f3f51f3b3c5149d6de").unwrap(),
        token_in,
        token_out,
        BigUint::from(1_000_000_000_000_000_000_u128),
        BigUint::from(1_000_000_000_000_000_000_u128),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_wrap_unwrapping", hex_calldata.as_str());
}

#[test]
fn test_sequential_encoding_strategy_wrap_added() {
    // The solution is initially a single swap. The wrapping step is inserted automatically.
    // Final execution flow:
    // ETH → (wrap to WETH) → WETH → (Uniswap V2 swap) → DAI

    let swap_weth_dai = Swap::new(
        ProtocolComponent {
            id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(weth().clone()),
        default_token(dai().clone()),
        BigUint::ZERO,
    );
    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0x9964bff29baa37b47604f3f3f51f3b3c5149d6de").unwrap(),
        Bytes::from_str("0x9964bff29baa37b47604f3f3f51f3b3c5149d6de").unwrap(),
        eth(),
        dai(),
        BigUint::from(1_000_000_000_000_000_000_u128),
        BigUint::from(1_000_000_000_000_000_000_u128),
        vec![swap_weth_dai],
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_sequential_encoding_strategy_wrap_added", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_ekubo_v3() {
    //   ETH ──(EKUBO V3)──> USDC

    let token_in = eth();
    let token_out = usdc(); // USDC

    let static_attributes = HashMap::from([
        ("fee".to_string(), Bytes::from(0_u64)),
        ("pool_type_config".to_string(), Bytes::from(0_u32)),
        ("extension".to_string(), Bytes::from("0x517E506700271AEa091b02f42756F5E174Af5230")), /* Oracle */
    ]);

    let component = ProtocolComponent {
        // All Ekubo swaps go through the core contract - not necessary to specify pool
        // id for test
        protocol_system: "ekubo_v3".to_string(),
        static_attributes,
        ..Default::default()
    };

    let swap = Swap::new(
        component,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_ekubo_v3", hex_calldata.as_str());
}

#[test]
fn test_single_ekubo_v3_grouped_swap() {
    // Test multi-hop Ekubo V3 swap (grouped swaps)
    //
    //   USDT ──(EKUBO V3)──> USDC ──(EKUBO V3)──> ETH

    // First swap: USDT -> USDC
    let swap1 = Swap::new(
        ProtocolComponent {
            id: "4a619b24ff31bbeae86503d0321898c9cb3f07bc32097749ee0622d5e9b78d6f".to_string(),
            protocol_system: "ekubo_v3".to_string(),
            static_attributes: HashMap::from([
                (
                    "extension".to_string(),
                    Bytes::from_str("0x0000000000000000000000000000000000000000").unwrap(),
                ),
                ("fee".to_string(), Bytes::from(184467440737096_u64)),
                ("pool_type_config".to_string(), Bytes::from_str("0x80000032").unwrap()), /* tick spacing = 50 */
            ]),
            ..Default::default()
        },
        default_token(usdt()),
        default_token(usdc()),
        BigUint::ZERO,
    );

    // Second swap: USDC -> ETH
    let swap2 = Swap::new(
        ProtocolComponent {
            id: "40f28acb8adc041aa51c8db8f21a9ccac0ee359075b01e1b432c238bb4e6c7eb".to_string(),
            protocol_system: "ekubo_v3".to_string(),
            static_attributes: HashMap::from([
                (
                    "extension".to_string(),
                    Bytes::from_str("0x517e506700271aea091b02f42756f5e174af5230").unwrap(), /* Oracle */
                ),
                ("fee".to_string(), Bytes::from(0_u64)),
                ("pool_type_config".to_string(), Bytes::from(0_u32)),
            ]),
            ..Default::default()
        },
        default_token(usdc()),
        default_token(eth()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        usdt(),
        eth(),
        BigUint::from_str("10000_000000").unwrap(),
        BigUint::from_str("1_000000000000000000").unwrap(),
        vec![swap1, swap2],
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

    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_ekubo_v3_grouped_swap", hex_calldata.as_str());
}

#[test]
fn test_sequential_encoding_strategy_etherfi_unwrap_weeth() {
    // weeth -> (unwrap) -> eeth -> (RedemptionManager) -> eth
    let weeth_pool = ProtocolComponent {
        id: String::from("0xCd5fE23C85820F7B72D0926FC9b05b43E359b7ee"),
        protocol_system: String::from("etherfi"),
        ..Default::default()
    };
    let weeth = Bytes::from("0xCd5fE23C85820F7B72D0926FC9b05b43E359b7ee");
    let eeth = Bytes::from("0x35fA164735182de50811E8e2E824cFb9B6118ac2");
    let swap1 = Swap::new(
        weeth_pool,
        default_token(weeth.clone()),
        default_token(eeth.clone()),
        BigUint::ZERO,
    );
    let eeth_pool = ProtocolComponent {
        id: String::from("0x35fA164735182de50811E8e2E824cFb9B6118ac2"),
        protocol_system: String::from("etherfi"),
        ..Default::default()
    };
    let swap2 =
        Swap::new(eeth_pool, default_token(eeth.clone()), default_token(eth()), BigUint::ZERO);

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        // Bob
        // Avoid ALICE (0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2):
        // it's an EIP-7702 address and RedemptionManager only forwards 10k gas for ETH sends.
        Bytes::from_str("0x9964bFf29BAa37B47604F3F3F51F3B3C5149d6DE").unwrap(),
        Bytes::from_str("0x9964bFf29BAa37B47604F3F3F51F3B3C5149d6DE").unwrap(),
        weeth.clone(),
        eth(),
        BigUint::from_str("1000000000000000000").unwrap(),
        BigUint::from_str("1000000000000000000").unwrap(),
        vec![swap1, swap2],
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file(
        "test_sequential_encoding_strategy_etherfi_unwrap_weeth",
        hex_calldata.as_str(),
    );
}

#[test]
fn test_sequential_encoding_strategy_etherfi_wrap_eeth() {
    // eth -> (deposit) -> eeth -> (wrap) -> weeth
    let eeth = Bytes::from("0x35fA164735182de50811E8e2E824cFb9B6118ac2");
    let eeth_pool = ProtocolComponent {
        id: String::from("0x35fA164735182de50811E8e2E824cFb9B6118ac2"),
        protocol_system: String::from("etherfi"),
        ..Default::default()
    };
    let swap1 =
        Swap::new(eeth_pool, default_token(eth()), default_token(eeth.clone()), BigUint::ZERO);

    let weeth_pool = ProtocolComponent {
        id: String::from("0xCd5fE23C85820F7B72D0926FC9b05b43E359b7ee"),
        protocol_system: String::from("etherfi"),
        ..Default::default()
    };
    let weeth = Bytes::from("0xCd5fE23C85820F7B72D0926FC9b05b43E359b7ee");
    let swap2 = Swap::new(
        weeth_pool,
        default_token(eeth.clone()),
        default_token(weeth.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0x9964bFf29BAa37B47604F3F3F51F3B3C5149d6DE").unwrap(),
        Bytes::from_str("0x9964bFf29BAa37B47604F3F3F51F3B3C5149d6DE").unwrap(),
        eth(),
        weeth.clone(),
        BigUint::from_str("1000000000000000000").unwrap(),
        BigUint::from_str("900000000000000000").unwrap(),
        vec![swap1, swap2],
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file(
        "test_sequential_encoding_strategy_etherfi_wrap_eeth",
        hex_calldata.as_str(),
    );
}

#[test]
fn test_evm_single_encoding_strategy_usv4_twif_fee_token() {
    // Encodes a single swap of TWIF (a fee-on-transfer token that
    // actually charges 6% on every transfer) to USDC through a
    // real UniswapV4 pool on mainnet.
    //
    //   TWIF ───(USV4)──> USDC
    //
    // Pool key: fee=10000, tickSpacing=200, hooks=0x0
    let twif = Bytes::from_str("0x2dd636c514bb4705c756d161585ff9ec665f18a2").unwrap();
    let usdc = usdc();

    let pool_fee = Bytes::from(BigInt::from(10000).to_signed_bytes_be());
    let tick_spacing = Bytes::from(BigInt::from(200).to_signed_bytes_be());
    let mut static_attributes: HashMap<String, Bytes> = HashMap::new();
    static_attributes.insert("key_lp_fee".into(), pool_fee);
    static_attributes.insert("tick_spacing".into(), tick_spacing);

    let twif_token = Token::new(&twif, "TWIF", 18, 600, &[], Chain::Ethereum, 50);
    let usdc_token = Token::new(&usdc, "USDC", 6, 0, &[], Chain::Ethereum, 100);

    let swap = Swap::new(
        ProtocolComponent {
            id: "0x66315f75b2071302fa143f44ae0ec79c0c98f837693fa7150f8ac7ed1fa7576e".to_string(),
            protocol_system: "uniswap_v4".to_string(),
            static_attributes,
            ..Default::default()
        },
        twif_token,
        usdc_token,
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        alice_address(),
        alice_address(),
        twif,
        usdc,
        // TWIF is nearly worthless (~7.6e-10 USDC per TWIF).
        // Use a large amount so the swap produces >=1 USDC.
        BigUint::from_str("10000000000000000000000000000000000").unwrap(),
        BigUint::from(1u64),
        vec![swap],
    )
    .with_user_transfer_type(UserTransferType::TransferFromPermit2);

    let encoded_solution = encoder
        .encode_solutions(vec![solution.clone()])
        .expect("encoding failed for TWIF fee-on-transfer token")[0]
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
    .expect("calldata generation failed for TWIF fee token")
    .data;

    let hex_calldata = encode(&calldata);
    assert!(!hex_calldata.is_empty(), "calldata should not be empty for TWIF fee-token V4 swap");
    write_calldata_to_file(
        "test_single_encoding_strategy_usv4_twif_fee_token",
        hex_calldata.as_str(),
    );
}

#[test]
fn test_evm_single_encoding_strategy_usv4_twif_fee_token_output() {
    // Encodes a swap of USDC to TWIF (a fee-on-transfer token that
    // actually charges 6% on every transfer) through a real
    // UniswapV4 pool on mainnet.
    //
    //   USDC ───(USV4)──> TWIF
    //
    // Tests the output-side FoT fix: the executor must report
    // the actual amount received by the user after the fee,
    // not the pre-fee amount from the V4 pool delta.
    //
    // Pool key: fee=10000, tickSpacing=200, hooks=0x0
    let twif = Bytes::from_str("0x2dd636c514bb4705c756d161585ff9ec665f18a2").unwrap();
    let usdc = usdc();

    let pool_fee = Bytes::from(BigInt::from(10000).to_signed_bytes_be());
    let tick_spacing = Bytes::from(BigInt::from(200).to_signed_bytes_be());
    let mut static_attributes: HashMap<String, Bytes> = HashMap::new();
    static_attributes.insert("key_lp_fee".into(), pool_fee);
    static_attributes.insert("tick_spacing".into(), tick_spacing);

    let usdc_token = Token::new(&usdc, "USDC", 6, 0, &[], Chain::Ethereum, 100);
    let twif_token = Token::new(&twif, "TWIF", 18, 600, &[], Chain::Ethereum, 50);

    let swap = Swap::new(
        ProtocolComponent {
            id: "0x66315f75b2071302fa143f44ae0ec79c0c98f837693fa7150f8ac7ed1fa7576e".to_string(),
            protocol_system: "uniswap_v4".to_string(),
            static_attributes,
            ..Default::default()
        },
        usdc_token,
        twif_token,
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        alice_address(),
        alice_address(),
        usdc,
        twif,
        BigUint::from_str("100000000").unwrap(), // 100 USDC
        BigUint::from(1u64),
        vec![swap],
    )
    .with_user_transfer_type(UserTransferType::TransferFromPermit2);

    let encoded_solution = encoder
        .encode_solutions(vec![solution.clone()])
        .expect("encoding failed for TWIF output fee-on-transfer token")[0]
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
    .expect("calldata generation failed for TWIF output fee token")
    .data;

    let hex_calldata = encode(&calldata);
    assert!(!hex_calldata.is_empty(), "calldata should not be empty for TWIF output V4 swap");
    write_calldata_to_file(
        "test_single_encoding_strategy_usv4_twif_fee_token_output",
        hex_calldata.as_str(),
    );
}

#[test]
fn test_single_encoding_strategy_liquorice_settle_single() {
    // Generates calldata for TychoRouterForLiquoriceTest Solidity integration test.
    // USDC -> WETH via Liquorice RFQ settleSingleOrder
    // Real calldata captured at block 24,392,845
    let user = Bytes::from_str("0xd2068e04cf586f76eece7ba5beb779d7bb1474a1").unwrap();

    let usdc = usdc();
    let weth = weth();

    let quote_amount_out = BigUint::from_str("1000000000000000000").unwrap();

    let liquorice_calldata = Bytes::from(
        hex::decode(concat!(
            "9935c868",
            "00000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f687058",
            "00000000000000000000000000000000000000000000000000000000000000a0",
            "0000000000000000000000000000000000000000000000000000000000000240",
            "00000000000000000000000000000000000000000000000000000000b2d05e00",
            "0000000000000000000000000000000000000000000000000000000000000320",
            "0000000000000000000000000000000000000000000000000000000000000160",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000006bc529dc7b81a031828ddce2bc419d01ff268c66",
            "0000000000000000000000006bc529dc7b81a031828ddce2bc419d01ff268c66",
            "000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            "000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            "00000000000000000000000000000000000000000000000000000000b2d05e00",
            "0000000000000000000000000000000000000000000000000de0b6b3a7640000",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000069850367",
            "00000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f687058",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "3100000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000060",
            "0000000000000000000000000000000000000000000000000000000000000041",
            "9ab2af25941edb594c4c33388649c35f7bbc545ce0a745f9e6272c0ea0e8b251",
            "7939f2747b980419ca5f22129742f65755464928ac9592aff9d16ac4446b18df",
            "1c00000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000060",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ))
        .unwrap(),
    );

    let liquorice_state = MockRFQState {
        quote_amount_out,
        quote_data: HashMap::from([
            ("calldata".to_string(), liquorice_calldata),
            (
                "base_token_amount".to_string(),
                Bytes::from(
                    biguint_to_u256(&BigUint::from(3_000_000_000_u64))
                        .to_be_bytes::<32>()
                        .to_vec(),
                ),
            ),
            ("partial_fill_offset".to_string(), Bytes::from(vec![96u8])),
            (
                "min_base_token_amount".to_string(),
                Bytes::from(
                    biguint_to_u256(&BigUint::from(3_000_000_000_u64))
                        .to_be_bytes::<32>()
                        .to_vec(),
                ),
            ),
        ]),
    };

    let liquorice_component = ProtocolComponent {
        id: String::from("liquorice-rfq"),
        protocol_system: String::from("rfq:liquorice"),
        ..Default::default()
    };

    let swap_usdc_weth = Swap::new(
        liquorice_component,
        default_token(usdc.clone()),
        default_token(weth.clone()),
        BigUint::ZERO,
    )
    .with_estimated_amount_in(BigUint::from_str("3000000000").unwrap())
    .with_protocol_state(Arc::new(liquorice_state));

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        user.clone(),
        user,
        usdc,
        weth,
        BigUint::from_str("3000000000").unwrap(),
        BigUint::from_str("1000000000000000000").unwrap(),
        vec![swap_usdc_weth],
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

    let hex_calldata = encode(&calldata);
    write_calldata_to_file(
        "test_single_encoding_strategy_liquorice_settle_single",
        hex_calldata.as_str(),
    );
}

#[test]
fn test_evm_two_hop_usv4_twif_intermediary() {
    // UniswapV4 cyclical swaps encoded as two separate hops with TWIF (6%
    // fee-on-transfer) as intermediary. Same pool used in both directions to
    // isolate the effect of the transfer tax on the intermediate hop.
    //
    //   USDC ──(USV4)──> TWIF ──(USV4)──> USDC
    //
    // Pool key: fee=10000, tickSpacing=200, hooks=0x0
    let twif = Bytes::from_str("0x2dd636c514bb4705c756d161585ff9ec665f18a2").unwrap();
    let usdc = usdc();

    let pool_fee = Bytes::from(BigInt::from(10000).to_signed_bytes_be());
    let tick_spacing = Bytes::from(BigInt::from(200).to_signed_bytes_be());

    let mut static_attributes_1: HashMap<String, Bytes> = HashMap::new();
    static_attributes_1.insert("key_lp_fee".into(), pool_fee.clone());
    static_attributes_1.insert("tick_spacing".into(), tick_spacing.clone());

    let mut static_attributes_2: HashMap<String, Bytes> = HashMap::new();
    static_attributes_2.insert("key_lp_fee".into(), pool_fee);
    static_attributes_2.insert("tick_spacing".into(), tick_spacing);

    let usdc_token = Token::new(&usdc, "USDC", 6, 0, &[], Chain::Ethereum, 100);
    let twif_token = Token::new(&twif, "TWIF", 18, 600, &[], Chain::Ethereum, 50);

    let pool_id = "0x66315f75b2071302fa143f44ae0ec79c0c98f837693fa7150f8ac7ed1fa7576e";

    // First swap: USDC -> TWIF
    let swap1 = Swap::new(
        ProtocolComponent {
            id: pool_id.to_string(),
            protocol_system: "uniswap_v4".to_string(),
            static_attributes: static_attributes_1,
            ..Default::default()
        },
        usdc_token.clone(),
        twif_token.clone(),
        BigUint::ZERO,
    );

    // Second swap: TWIF -> USDC (same pool, reversed)
    let swap2 = Swap::new(
        ProtocolComponent {
            id: pool_id.to_string(),
            protocol_system: "uniswap_v4".to_string(),
            static_attributes: static_attributes_2,
            ..Default::default()
        },
        twif_token,
        usdc_token,
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        alice_address(),
        alice_address(),
        usdc.clone(),
        usdc,
        BigUint::from_str("100000000").unwrap(), // 100 USDC
        BigUint::from(1u64),
        vec![swap1, swap2],
    )
    .with_user_transfer_type(UserTransferType::TransferFromPermit2);

    let encoded_solution = encoder
        .encode_solutions(vec![solution.clone()])
        .expect("encoding failed for grouped TWIF intermediary swap")[0]
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
    .expect("calldata generation failed for grouped TWIF intermediary swap")
    .data;

    let hex_calldata = encode(&calldata);
    assert!(
        !hex_calldata.is_empty(),
        "calldata should not be empty for grouped TWIF intermediary swap"
    );
    write_calldata_to_file("test_two_hop_usv4_twif_intermediary", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_liquorice_settle() {
    // Generates calldata for TychoRouterForLiquoriceTest Solidity integration test.
    // USDC -> WETH via Liquorice RFQ settle
    // Real calldata captured at block 24,392,845
    let user = Bytes::from_str("0xd2068e04cf586f76eece7ba5beb779d7bb1474a1").unwrap();

    let usdc = usdc();
    let weth = weth();

    let quote_amount_out = BigUint::from_str("1000000000000000000").unwrap();

    let liquorice_calldata = Bytes::from(
        hex::decode(concat!(
            "cba673a7",
            "00000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f687058",
            "00000000000000000000000000000000000000000000000000000000b2d05e00",
            "00000000000000000000000000000000000000000000000000000000000000e0",
            "0000000000000000000000000000000000000000000000000000000000000380",
            "00000000000000000000000000000000000000000000000000000000000003a0",
            "0000000000000000000000000000000000000000000000000000000000000420",
            "0000000000000000000000000000000000000000000000000000000000000500",
            "0000000000000000000000000448633eb8b0a42efed924c42069e0dcf08fb552",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000260",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000006bc529dc7b81a031828ddce2bc419d01ff268c66",
            "0000000000000000000000006bc529dc7b81a031828ddce2bc419d01ff268c66",
            "0000000000000000000000000000000000000000000000000000000069850367",
            "00000000000000000000000006465bceeaef280bb7340a58d75dfc5e1f687058",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            "00000000000000000000000000000000000000000000000000000000b2d05e00",
            "00000000000000000000000000000000000000000000000000000000b2d05e00",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            "0000000000000000000000000000000000000000000000000de0b6b3a7640000",
            "0000000000000000000000000000000000000000000000000de0b6b3a7640000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "3100000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000040",
            "0000000000000000000000000000000000000000000000000000000000000060",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000060",
            "0000000000000000000000000000000000000000000000000000000000000041",
            "6e4084d38e2d4e334057a124ce1ed667947b4fe6a0b44d6cbd62baa5dd384ff9",
            "3eba5c03f8ea1f4fec128c1c76ce49eb1aad71f053adf3a4d860ef9fe973374d",
            "1b00000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000100",
            "0000000000000000000000000000000000000000000000000000000000000100",
            "0000000000000000000000000000000000000000000000000000000000000060",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ))
        .unwrap(),
    );

    let liquorice_state = MockRFQState {
        quote_amount_out,
        quote_data: HashMap::from([
            ("calldata".to_string(), liquorice_calldata),
            (
                "base_token_amount".to_string(),
                Bytes::from(
                    biguint_to_u256(&BigUint::from(3_000_000_000_u64))
                        .to_be_bytes::<32>()
                        .to_vec(),
                ),
            ),
            ("partial_fill_offset".to_string(), Bytes::from(vec![32u8])),
            (
                "min_base_token_amount".to_string(),
                Bytes::from(
                    biguint_to_u256(&BigUint::from(3_000_000_000_u64))
                        .to_be_bytes::<32>()
                        .to_vec(),
                ),
            ),
        ]),
    };

    let liquorice_component = ProtocolComponent {
        id: String::from("liquorice-rfq"),
        protocol_system: String::from("rfq:liquorice"),
        ..Default::default()
    };

    let swap_usdc_weth = Swap::new(
        liquorice_component,
        default_token(usdc.clone()),
        default_token(weth.clone()),
        BigUint::ZERO,
    )
    .with_estimated_amount_in(BigUint::from_str("3000000000").unwrap())
    .with_protocol_state(Arc::new(liquorice_state));

    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        user.clone(),
        user,
        usdc,
        weth,
        BigUint::from_str("3000000000").unwrap(),
        BigUint::from_str("1000000000000000000").unwrap(),
        vec![swap_usdc_weth],
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

    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_liquorice_settle", hex_calldata.as_str());
}

#[test]
fn test_single_encoding_strategy_uniswap_v3_arbitrum() {
    // WETH -> (UniswapV3 0.05% WETH/USDC) -> USDC on Arbitrum
    let usv3_pool = ProtocolComponent {
        id: String::from("0xC6962004f452bE9203591991D15f6b388e09E8D0"),
        protocol_system: String::from("uniswap_v3"),
        static_attributes: HashMap::from([(
            "fee".to_string(),
            Bytes::from(BigInt::from(500).to_signed_bytes_be()),
        )]),
        ..Default::default()
    };
    let token_in = Bytes::from("0x82aF49447D8a07e3bd95BD0d56f35241523fBab1"); // WETH
    let token_out = Bytes::from("0xaf88d065e77c8cC2239327C5EDb3A432268e5831"); // USDC
    let swap = Swap::new(
        usv3_pool,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Arbitrum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file(
        "test_single_encoding_strategy_uniswap_v3_arbitrum",
        hex_calldata.as_str(),
    );
}

#[test]
fn test_single_encoding_strategy_uniswap_v3_polygon() {
    // WETH -> (UniswapV3 0.05% WETH/USDC) -> USDC on Polygon
    let usv3_pool = ProtocolComponent {
        id: String::from("0xA4D8c89f0c20efbe54cBa9e7e7a7E509056228D9"),
        protocol_system: String::from("uniswap_v3"),
        static_attributes: HashMap::from([(
            "fee".to_string(),
            Bytes::from(BigInt::from(500).to_signed_bytes_be()),
        )]),
        ..Default::default()
    };
    let token_in = Bytes::from("0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619"); // WETH
    let token_out = Bytes::from("0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359"); // USDC
    let swap = Swap::new(
        usv3_pool,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Polygon);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file(
        "test_single_encoding_strategy_uniswap_v3_polygon",
        hex_calldata.as_str(),
    );
}

#[test]
fn test_single_encoding_strategy_uniswap_v3_bsc() {
    // WBNB -> (UniswapV3 0.05% WBNB/WETH) -> WETH on BNB Chain
    let pool = ProtocolComponent {
        id: String::from("0x0f338Ec12d3f7C3D77A4B9fcC1f95F3FB6AD0EA6"),
        protocol_system: String::from("uniswap_v3"),
        static_attributes: HashMap::from([(
            "fee".to_string(),
            Bytes::from(BigInt::from(500).to_signed_bytes_be()),
        )]),
        ..Default::default()
    };
    let token_in = Bytes::from("0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c"); // WBNB
    let token_out = Bytes::from("0x2170Ed0880ac9A755fd29B2688956BD959F933F8"); // WETH
    let swap = Swap::new(
        pool,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    );

    let encoder = get_tycho_router_encoder(Chain::Bsc);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        token_in,
        token_out,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("1000").unwrap(),
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
    let hex_calldata = encode(&calldata);
    write_calldata_to_file("test_single_encoding_strategy_uniswap_v3_bsc", hex_calldata.as_str());
}
