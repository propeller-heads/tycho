mod common;

use std::str::FromStr;

use alloy::hex::encode;
use num_bigint::BigUint;
use tycho_common::{
    models::{protocol::ProtocolComponent, Chain},
    Bytes,
};
use tycho_execution::encoding::{
    evm::utils::write_calldata_to_file,
    models::{default_token, Solution, Swap},
};

use crate::common::{
    client_fee_forwarder_address, dai, encoding::encode_client_fee_forwarder_call, eth,
    get_client_fee_forwarder_encoder, weth,
};

#[test]
fn test_client_fee_forwarder_single_swap() {
    // 1 WETH -> DAI on USV2 through the forwarder
    let expected_amount_out = BigUint::from_str("2018817438608734439722").unwrap();
    // 2% below the quote
    let min_amount_out = &expected_amount_out * BigUint::from(9800u64) / BigUint::from(10_000u64);

    let swap = Swap::new(
        ProtocolComponent {
            id: "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11".to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        default_token(weth()),
        default_token(dai()),
        BigUint::ZERO,
    );
    let solution = Solution::new(
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        Bytes::from_str("0xcd09f75E2BF2A4d11F3AB23f1389FcC1621c0cc2").unwrap(),
        weth(),
        dai(),
        BigUint::from_str("1_000000000000000000").unwrap(),
        expected_amount_out,
        min_amount_out,
        vec![swap],
    );

    let encoded_solution = get_client_fee_forwarder_encoder(Chain::Ethereum)
        .encode_solutions(vec![solution.clone()])
        .unwrap()
        .remove(0);
    // makeAddr("clientFeeWallet") in ClientFeeForwarder.t.sol
    let fee_wallet = Bytes::from_str("0x683C82e4B6796e3d733dcbFac2841e101ad2b8fD").unwrap();
    // 1%
    let client_fee_bps = 1_000_000;
    let transaction = encode_client_fee_forwarder_call(
        encoded_solution,
        &solution,
        &eth(),
        &fee_wallet,
        client_fee_bps,
    )
    .unwrap();

    assert_eq!(transaction.to, client_fee_forwarder_address());
    assert_eq!(transaction.value, BigUint::ZERO);
    write_calldata_to_file("test_client_fee_forwarder_single_swap", &encode(&transaction.data));
}
