use crate::abi::b_factory::events::PoolCreated;
use serde::Deserialize;
use substreams_ethereum::pb::eth::v2::Log;
use substreams_ethereum::Event;
use tycho_substreams::models::{
    Attribute, ChangeType, FinancialType, ImplementationType, ProtocolComponent, ProtocolType,
};

#[derive(Deserialize)]
pub struct DeploymentConfig {
    #[serde(with = "hex::serde")]
    pub relay_address: Vec<u8>,
    pub protocol_type_name: String,
}

/// Potentially constructs a new ProtocolComponent given a call
///
/// This method is given each individual call within a transaction, the corresponding
/// logs emitted during that call as well as the full transaction trace.
///
/// If this call creates a component in your protocol please contstruct and return it
/// here. Otherwise, simply return None.
pub fn maybe_create_component(log: &Log, config: &DeploymentConfig) -> Option<ProtocolComponent> {
    if log.address != config.relay_address {
        return None;
    }

    let event = PoolCreated::match_and_decode(log)?;
    let component_id = format!("0x{}", hex::encode(&event.b_token_address));

    Some(ProtocolComponent {
        id: component_id,
        tokens: vec![event.b_token_address, event.reserve_address.clone()],
        contracts: vec![config.relay_address.clone()],
        static_att: vec![
            Attribute {
                name: "relay".to_string(),
                value: config.relay_address.clone(),
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
            name: config.protocol_type_name.clone(),
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
        40, 223, 178, 227, 204, 62, 41, 94, 255, 129, 153, 40, 77, 121, 190, 109, 220, 73,
        23, 27, 19, 130, 253, 168, 221, 46, 233, 52, 237, 146, 52, 131,
    ];

    #[test]
    fn test_decode_config() {
        let config: DeploymentConfig =
            serde_qs::from_str("relay_address=0001&protocol_type_name=baseline").unwrap();

        assert_eq!(config.relay_address, [0u8, 1u8]);
        assert_eq!(config.protocol_type_name, "baseline");
    }

    #[test]
    fn creates_custom_component_from_pool_created_log() {
        let relay = address(3);
        let b_token = address(1);
        let reserve = address(2);
        let log = substreams_ethereum::pb::eth::v2::Log {
            address: relay.clone(),
            topics: vec![POOL_CREATED_TOPIC.to_vec()],
            data: pool_created_data(&b_token, &reserve),
            ..Default::default()
        };
        let config = DeploymentConfig {
            relay_address: relay.clone(),
            protocol_type_name: "baseline".to_string(),
        };

        let component = maybe_create_component(&log, &config).expect("component");

        assert_eq!(component.id, format!("0x{}", hex::encode(&b_token)));
        assert_eq!(component.tokens, vec![b_token, reserve.clone()]);
        assert_eq!(component.contracts, vec![relay.clone()]);
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
