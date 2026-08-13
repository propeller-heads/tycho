use crate::abi::b_factory::events::{BTokenCreated, PoolCreated};
use substreams_ethereum::{pb::eth::v2::Log, Event};
use tycho_substreams::models::{
    Attribute, ChangeType, FinancialType, ImplementationType, ProtocolComponent, ProtocolType,
};

pub const RELAY_ADDRESS: [u8; 20] = [
    0xc8, 0x1f, 0xd8, 0x94, 0xc0, 0xac, 0xe0, 0x37, 0xd1, 0x33, 0xaf, 0x48, 0x86, 0x55, 0x0a, 0xc8,
    0x13, 0x35, 0x68, 0xe8,
];

/// Returns the bToken address for factory lifecycle events (`BTokenCreated`, `PoolCreated`).
///
/// `BTokenCreated` matters even though it does not create a component: in the two-step
/// `createBToken` -> `createPool` flow, `createBToken` writes `pool.totalSupply` to relay
/// storage, possibly blocks before `PoolCreated`. Substreams only see storage diffs, so
/// that slot must be indexed from the moment the bToken address is first known.
pub fn factory_b_token(log: &Log) -> Option<Vec<u8>> {
    if log.address.as_slice() != RELAY_ADDRESS {
        return None;
    }
    BTokenCreated::match_and_decode(log)
        .map(|event| event.b_token_address)
        .or_else(|| PoolCreated::match_and_decode(log).map(|event| event.b_token_address))
}

/// Potentially constructs a new ProtocolComponent given a call
///
/// This method is given each individual call within a transaction, the corresponding
/// logs emitted during that call as well as the full transaction trace.
///
/// If this call creates a component in your protocol please contstruct and return it
/// here. Otherwise, simply return None.
pub fn maybe_create_component(log: &Log) -> Option<ProtocolComponent> {
    if log.address.as_slice() != RELAY_ADDRESS {
        return None;
    }

    let event = PoolCreated::match_and_decode(log)?;
    let component_id = format!("0x{}", hex::encode(&event.b_token_address));

    Some(ProtocolComponent {
        id: component_id,
        tokens: vec![event.b_token_address, event.reserve_address.clone()],
        contracts: vec![],
        static_att: vec![
            Attribute {
                name: "relay".to_string(),
                value: RELAY_ADDRESS.to_vec(),
                change: ChangeType::Creation.into(),
            },
            Attribute {
                name: "reserve".to_string(),
                value: event.reserve_address,
                change: ChangeType::Creation.into(),
            },
            Attribute {
                name: "manual_updates".to_string(),
                value: vec![1u8],
                change: ChangeType::Creation.into(),
            },
        ],
        change: ChangeType::Creation.into(),
        protocol_type: Some(ProtocolType {
            name: "baseline".to_string(),
            financial_type: FinancialType::Swap.into(),
            attribute_schema: vec![],
            implementation_type: ImplementationType::Custom.into(),
        }),
    })
}

#[cfg(test)]
mod test {
    use super::*;

    const POOL_CREATED_TOPIC: [u8; 32] = [
        40, 223, 178, 227, 204, 62, 41, 94, 255, 129, 153, 40, 77, 121, 190, 109, 220, 73, 23, 27,
        19, 130, 253, 168, 221, 46, 233, 52, 237, 146, 52, 131,
    ];

    #[test]
    fn creates_custom_component_from_pool_created_log() {
        let b_token = address(1);
        let reserve = address(2);
        let log = substreams_ethereum::pb::eth::v2::Log {
            address: RELAY_ADDRESS.to_vec(),
            topics: vec![POOL_CREATED_TOPIC.to_vec()],
            data: pool_created_data(&b_token, &reserve),
            ..Default::default()
        };

        let component = maybe_create_component(&log).expect("component");

        assert_eq!(component.id, format!("0x{}", hex::encode(&b_token)));
        assert_eq!(component.tokens, vec![b_token, reserve.clone()]);
        assert!(component.contracts.is_empty());
        assert_eq!(
            component
                .protocol_type
                .as_ref()
                .expect("protocol type")
                .implementation_type,
            ImplementationType::Custom as i32
        );
        assert_eq!(
            component
                .static_att
                .iter()
                .find(|attr| attr.name == "reserve")
                .expect("reserve attr")
                .value,
            reserve
        );
    }

    fn pool_created_data(b_token: &[u8], reserve: &[u8]) -> Vec<u8> {
        use ethabi::{Address, Token, Uint};

        ethabi::encode(&[
            Token::Address(Address::from_slice(b_token)),
            Token::Address(Address::from_slice(reserve)),
            Token::Address(Address::from_slice(&address(4))),
            Token::Address(Address::from_slice(&address(5))),
            Token::Uint(Uint::from(0)),
            Token::Uint(Uint::from(1)),
            Token::Uint(Uint::from(2)),
            Token::Uint(Uint::from(3)),
            Token::Uint(Uint::from(4)),
            Token::Uint(Uint::from(5)),
            Token::Uint(Uint::from(6)),
            Token::FixedBytes(vec![7u8; 32]),
        ])
    }

    fn address(last_byte: u8) -> Vec<u8> {
        let mut address = vec![0u8; 20];
        address[19] = last_byte;
        address
    }
}
