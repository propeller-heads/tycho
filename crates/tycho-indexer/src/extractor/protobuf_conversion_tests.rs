//! Tests for the protobuf → domain model conversions provided by
//! [`tycho_protobuf::convert`].
//!
//! The fixtures live in this crate (`crate::testing::fixtures`) and depend on
//! tycho-indexer-internal helpers, so these tests stay here rather than moving into
//! the `tycho-protobuf` crate alongside the implementation.

use std::{collections::HashMap, str::FromStr};

use rstest::rstest;
use tycho_common::{
    models::{
        blockchain::{Block, RPCTracerParams, TracingParams, Transaction, TxWithContractChanges},
        contract::{ContractChanges, ContractStorageChange},
        protocol::{ComponentBalance, ProtocolComponent, ProtocolComponentStateDelta},
        Address, Chain, ProtocolType,
    },
    Bytes,
};
use tycho_protobuf::{convert::TryFromMessage, pb::tycho::evm::v1 as pb};

use crate::{extractor::models::fixtures::create_transaction, testing::fixtures};

fn transaction() -> Transaction {
    create_transaction(
        "0000000000000000000000000000000000000000000000000000000011121314",
        "0000000000000000000000000000000000000000000000000000000031323334",
        2,
    )
}

#[test]
fn test_parse_protocol_state_update() {
    let msg = fixtures::pb_state_changes();

    let res = ProtocolComponentStateDelta::try_from_message(msg).unwrap();

    assert_eq!(res, fixtures::protocol_state_delta());
}

#[test]
fn test_parse_tx_with_storage_changes() {
    let msg = fixtures::pb_transaction_storage_changes(0);
    let tx = Transaction::new(
        Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000001")
            .unwrap(),
        Bytes::default(),
        Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
        Some(Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap()),
        1,
    );
    let exp = TxWithContractChanges {
        tx,
        contract_changes: HashMap::from([
            (
                Bytes::from_str("0000000000000000000000000000000000000001").unwrap(),
                ContractChanges::new(
                    Bytes::from_str("0000000000000000000000000000000000000001").unwrap(),
                    HashMap::from([
                        (
                            Bytes::from_str("0x01").unwrap(),
                            ContractStorageChange::initial(Bytes::from_str("0x01").unwrap()),
                        ),
                        (
                            Bytes::from_str("0x02").unwrap(),
                            ContractStorageChange::initial(Bytes::from_str("0x02").unwrap()),
                        ),
                    ]),
                    None,
                ),
            ),
            (
                Bytes::from_str("0000000000000000000000000000000000000002").unwrap(),
                ContractChanges::new(
                    Bytes::from_str("0000000000000000000000000000000000000002").unwrap(),
                    HashMap::from([(
                        Bytes::from_str("0x03").unwrap(),
                        ContractStorageChange::initial(Bytes::from_str("0x03").unwrap()),
                    )]),
                    Some(Bytes::from(1000u64)),
                ),
            ),
        ]),
    };

    let res = TxWithContractChanges::try_from_message((msg, &Block::default())).unwrap();

    assert_eq!(res, exp);
}

#[test]
fn test_parse_protocol_component() {
    let msg = fixtures::pb_protocol_component();

    let expected_chain = Chain::Ethereum;
    let expected_protocol_system = "ambient".to_string();
    let expected_attribute_map: HashMap<String, Bytes> = vec![
        ("balance".to_string(), Bytes::from(100u64).lpad(32, 0)),
        ("factory_address".to_string(), Bytes::from(b"0x0fwe0g240g20".to_vec())),
    ]
    .into_iter()
    .collect();

    let protocol_type_id = "WeightedPool".to_string();
    let protocol_types: HashMap<String, ProtocolType> =
        HashMap::from([(protocol_type_id.clone(), ProtocolType::default())]);

    let result = ProtocolComponent::try_from_message((
        msg,
        expected_chain,
        &expected_protocol_system,
        &protocol_types,
        Bytes::from_str("0x0e22048af8040c102d96d14b0988c6195ffda24021de4d856801553aa468bcac")
            .unwrap(),
        Default::default(),
    ));

    assert!(result.is_ok());

    let protocol_component = result.unwrap();

    assert_eq!(
        protocol_component.id,
        "d417ff54652c09bd9f31f216b1a2e5d1e28c1dce1ba840c40d16f2b4d09b5902".to_string()
    );
    assert_eq!(protocol_component.protocol_system, expected_protocol_system);
    assert_eq!(protocol_component.protocol_type_name, protocol_type_id);
    assert_eq!(protocol_component.chain, expected_chain);
    assert_eq!(
        protocol_component.tokens,
        vec![
            Bytes::from_str("6B175474E89094C44Da98b954EedeAC495271d0F").unwrap(),
            Bytes::from_str("6B175474E89094C44Da98b954EedeAC495271d0F").unwrap(),
        ]
    );
    assert_eq!(
        protocol_component.contract_addresses,
        vec![
            Bytes::from_str("31fF2589Ee5275a2038beB855F44b9Be993aA804").unwrap(),
            Bytes::from_str("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
        ]
    );
    assert_eq!(protocol_component.static_attributes, expected_attribute_map);
}

#[test]
fn test_parse_component_balance() {
    let tx = transaction();
    let expected_balance: f64 = 3000.0;
    let msg_balance = expected_balance.to_be_bytes().to_vec();

    let expected_token = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
    let msg_token = expected_token.0.to_vec();
    let expected_component_id = "d417ff54652c09bd9f31f216b1a2e5d1e28c1dce1ba840c40d16f2b4d09b5902";
    let msg_component_id = expected_component_id
        .as_bytes()
        .to_vec();
    let msg = pb::BalanceChange {
        balance: msg_balance.to_vec(),
        token: msg_token,
        component_id: msg_component_id,
    };
    let from_message = ComponentBalance::try_from_message((msg, &tx)).unwrap();

    assert_eq!(from_message.balance, msg_balance);
    assert_eq!(from_message.modify_tx, tx.hash);
    assert_eq!(from_message.token, expected_token);
    assert_eq!(from_message.component_id, expected_component_id);
}

#[rstest]
#[case::rpc_trace_data(
    pb::entry_point_params::TraceData::Rpc(
        pb::RpcTraceData {
            caller: Some(
                Bytes::from_str("0x1234567890123456789012345678901234567890")
                    .unwrap()
                    .to_vec(),
            ),
            calldata: Bytes::from_str("0xabcdef")
                .unwrap()
                .to_vec(),
        },
    ),
    TracingParams::RPCTracer(RPCTracerParams {
        caller: Some(Address::from_str("0x1234567890123456789012345678901234567890").unwrap()),
        calldata: Bytes::from_str("0xabcdef").unwrap(),
        state_overrides: None,
        prune_addresses: None,
    })
)]
fn test_parse_entrypoint_params(
    #[case] trace_data: pb::entry_point_params::TraceData,
    #[case] expected: TracingParams,
) {
    let msg = pb::EntryPointParams {
        entrypoint_id: "test_entrypoint".to_string(),
        component_id: Some("test_component".to_string()),
        trace_data: Some(trace_data),
    };

    let result = TracingParams::try_from_message(msg).unwrap();

    assert_eq!(result, expected);
}
