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
    encoding::encode_tycho_router_call, eth, eth_chain, get_signer, get_tycho_router_encoder, weth,
};

mod common;
// In this module we test the ability to chain swaps or not. Different protocols are
// tested. The encoded data is used for solidity tests as well

#[test]
fn test_uniswap_v3_uniswap_v2() {
    // Note: This test does not assert anything. It is only used to obtain
    // integration test data for our router solidity test.
    //
    // Performs a sequential swap from WETH to USDC though WBTC using USV3 and USV2
    // pools
    //
    //   WETH ───(USV3)──> WBTC ───(USV2)──> USDC

    let weth = weth();
    let wbtc = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap();
    let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

    let swap_weth_wbtc = Swap::new(
        ProtocolComponent {
            id: "0xCBCdF9626bC03E24f779434178A73a0B4bad62eD".to_string(),
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
        default_token(wbtc.clone()),
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
        vec![swap_weth_wbtc, swap_wbtc_usdc],
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
    write_calldata_to_file("test_uniswap_v3_uniswap_v2", hex_calldata.as_str());
}

#[test]
fn test_uniswap_v3_uniswap_v3() {
    // Note: This test does not assert anything. It is only used to obtain
    // integration test data for our router solidity test.
    //
    // Performs a sequential swap from WETH to USDC though WBTC using USV3 pools
    // There is no optimization between the two USV3 pools, this is currently not
    // supported.
    //
    //   WETH ───(USV3)──> WBTC ───(USV3)──> USDC

    let weth = weth();
    let wbtc = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap();
    let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

    let swap_weth_wbtc = Swap::new(
        ProtocolComponent {
            id: "0xCBCdF9626bC03E24f779434178A73a0B4bad62eD".to_string(),
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
        default_token(wbtc.clone()),
        BigUint::ZERO,
    );
    let swap_wbtc_usdc = Swap::new(
        ProtocolComponent {
            id: "0x99ac8cA7087fA4A2A1FB6357269965A2014ABc35".to_string(),
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
        vec![swap_weth_wbtc, swap_wbtc_usdc],
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
    write_calldata_to_file("test_uniswap_v3_uniswap_v3", hex_calldata.as_str());
}

#[test]
fn test_uniswap_v3_curve() {
    // Note: This test does not assert anything. It is only used to obtain
    // integration test data for our router solidity test.
    //
    // Performs a sequential swap from WETH to USDT though WBTC using USV3 and curve
    // pools
    //
    //   WETH ───(USV3)──> WBTC ───(curve)──> USDT

    let weth = weth();
    let wbtc = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap();
    let usdt = Bytes::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap();

    let swap_weth_wbtc = Swap::new(
        ProtocolComponent {
            id: "0xCBCdF9626bC03E24f779434178A73a0B4bad62eD".to_string(),
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
        default_token(wbtc.clone()),
        BigUint::ZERO,
    );

    let swap_wbtc_usdt = Swap::new(
        ProtocolComponent {
            id: String::from("0xD51a44d3FaE010294C616388b506AcdA1bfAAE46"),
            protocol_system: String::from("vm:curve"),
            static_attributes: {
                let mut attrs: HashMap<String, Bytes> = HashMap::new();
                attrs.insert(
                    "factory".into(),
                    Bytes::from(
                        "0x0000000000000000000000000000000000000000"
                            .as_bytes()
                            .to_vec(),
                    ),
                );
                attrs.insert(
                "coins".into(),
                Bytes::from_str("0x5b22307864616331376639353864326565353233613232303632303639393435393763313364383331656337222c22307832323630666163356535353432613737336161343466626366656466376331393362633263353939222c22307863303261616133396232323366653864306130653563346632376561643930383363373536636332225d")
                    .unwrap(),
            );
                attrs
            },
            ..Default::default()
        },
        default_token(wbtc.clone()),
        default_token(usdt.clone()),
        BigUint::ZERO,
    );
    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        weth,
        usdt,
        BigUint::from_str("1_000000000000000000").unwrap(),
        BigUint::from_str("26173932").unwrap(),
        vec![swap_weth_wbtc, swap_wbtc_usdt],
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
    write_calldata_to_file("test_uniswap_v3_curve", hex_calldata.as_str());
}

#[test]
fn test_balancer_v2_uniswap_v2() {
    // Note: This test does not assert anything. It is only used to obtain
    // integration test data for our router solidity test.
    //
    // Performs a sequential swap from WETH to USDC though WBTC using balancer and
    // USV2 pools
    //
    //   WETH ───(balancer)──> WBTC ───(USV2)──> USDC

    let weth = weth();
    let wbtc = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap();
    let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();

    let swap_weth_wbtc = Swap::new(
        ProtocolComponent {
            id: "0xa6f548df93de924d73be7d25dc02554c6bd66db500020000000000000000000e".to_string(),
            protocol_system: "vm:balancer_v2".to_string(),
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(wbtc.clone()),
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
        vec![swap_weth_wbtc, swap_wbtc_usdc],
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
    write_calldata_to_file("test_balancer_v2_uniswap_v2", hex_calldata.as_str());
}

#[test]
fn test_evm_multi_protocol() {
    // Note: This test does not assert anything. It is only used to obtain
    // integration test data for our router solidity test.
    //
    // Performs the following swap:
    //
    //   DAI ─(USV2)-> WETH ─(bal)─> WBTC ─(curve)─> USDT ─(ekubo)─> USDC ─(USV4)─>
    // ETH

    let weth = weth();
    let eth = eth();
    let wbtc = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap();
    let usdc = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
    let usdt = Bytes::from_str("0xdAC17F958D2ee523a2206206994597C13D831ec7").unwrap();
    let dai = Bytes::from_str("0x6B175474E89094C44Da98b954EedeAC495271d0F").unwrap();

    let usv2_swap_dai_weth = Swap::new(
        ProtocolComponent {
            id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(dai.clone()),
        default_token(weth.clone()),
        BigUint::ZERO,
    );

    let balancer_swap_weth_wbtc = Swap::new(
        ProtocolComponent {
            id: "0xa6f548df93de924d73be7d25dc02554c6bd66db500020000000000000000000e".to_string(),
            protocol_system: "vm:balancer_v2".to_string(),
            ..Default::default()
        },
        default_token(weth.clone()),
        default_token(wbtc.clone()),
        BigUint::ZERO,
    );

    let curve_swap_wbtc_usdt = Swap::new(
        ProtocolComponent {
            id: String::from("0xD51a44d3FaE010294C616388b506AcdA1bfAAE46"),
            protocol_system: String::from("vm:curve"),
            static_attributes: {
                let mut attrs: HashMap<String, Bytes> = HashMap::new();
                attrs.insert(
                    "factory".into(),
                    Bytes::from(
                        "0x0000000000000000000000000000000000000000"
                            .as_bytes()
                            .to_vec(),
                    ),
                );
                attrs.insert(
                "coins".into(),
                Bytes::from_str("0x5b22307864616331376639353864326565353233613232303632303639393435393763313364383331656337222c22307832323630666163356535353432613737336161343466626366656466376331393362633263353939222c22307863303261616133396232323366653864306130653563346632376561643930383363373536636332225d")
                    .unwrap(),
            );
                attrs
            },
            ..Default::default()
        },
        default_token(wbtc.clone()),
        default_token(usdt.clone()),
        BigUint::ZERO,
    );

    // Ekubo

    let component = ProtocolComponent {
        // All Ekubo swaps go through the core contract - not necessary to specify
        // pool id for test
        protocol_system: "ekubo_v2".to_string(),
        // 0.0025% fee & 0.005% base pool
        static_attributes: HashMap::from([
            ("fee".to_string(), Bytes::from(461168601842738_u64)),
            ("tick_spacing".to_string(), Bytes::from(50_u32)),
            ("extension".to_string(), Bytes::zero(20)),
        ]),
        ..Default::default()
    };
    let ekubo_swap_usdt_usdc = Swap::new(
        component,
        default_token(usdt.clone()),
        default_token(usdc.clone()),
        BigUint::ZERO,
    );

    // USV4
    // Fee and tick spacing information for this test is obtained by querying the
    // USV4 Position Manager contract: 0xbd216513d74c8cf14cf4747e6aaa6420ff64ee9e
    // Using the poolKeys function with the first 25 bytes of the pool id
    let pool_fee_usdc_eth = Bytes::from(BigInt::from(3000).to_signed_bytes_be());
    let tick_spacing_usdc_eth = Bytes::from(BigInt::from(60).to_signed_bytes_be());
    let mut static_attributes_usdc_eth: HashMap<String, Bytes> = HashMap::new();
    static_attributes_usdc_eth.insert("key_lp_fee".into(), pool_fee_usdc_eth);
    static_attributes_usdc_eth.insert("tick_spacing".into(), tick_spacing_usdc_eth);

    let usv4_swap_usdc_eth = Swap::new(
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

    // Put all components together
    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        dai,
        eth.clone(),
        BigUint::from_str("1500_000000000000000000").unwrap(),
        BigUint::from_str("732214216964381330").unwrap(),
        vec![
            usv2_swap_dai_weth,
            balancer_swap_weth_wbtc,
            curve_swap_wbtc_usdt,
            ekubo_swap_usdt_usdc,
            usv4_swap_usdc_eth,
        ],
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
    write_calldata_to_file("test_multi_protocol", hex_calldata.as_str());
}

#[test]
fn test_uniswap_v3_balancer_v3() {
    // Note: This test does not assert anything. It is only used to obtain
    // integration test data for our router solidity test.
    //
    //   WETH ───(USV3)──> WBTC ───(balancer v3)──> QNT

    let weth = weth();
    let wbtc = Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap();
    let qnt = Bytes::from_str("0x4a220e6096b25eadb88358cb44068a3248254675").unwrap();

    let swap_weth_wbtc = Swap::new(
        ProtocolComponent {
            id: "0xCBCdF9626bC03E24f779434178A73a0B4bad62eD".to_string(),
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
        default_token(wbtc.clone()),
        BigUint::ZERO,
    );
    let swap_wbtc_qnt = Swap::new(
        ProtocolComponent {
            id: "0x571bea0e99e139cd0b6b7d9352ca872dfe0d72dd".to_string(),
            protocol_system: "vm:balancer_v3".to_string(),
            ..Default::default()
        },
        default_token(wbtc.clone()),
        default_token(qnt.clone()),
        BigUint::ZERO,
    );
    let encoder = get_tycho_router_encoder(Chain::Ethereum);

    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        weth,
        qnt,
        BigUint::from_str("1_0000000000000000").unwrap(),
        BigUint::from_str("26173932").unwrap(),
        vec![swap_weth_wbtc, swap_wbtc_qnt],
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
    write_calldata_to_file("test_uniswap_v3_balancer_v3", hex_calldata.as_str());
}
