use crate::abi::b_factory::events::PoolCreated;
use serde::Deserialize;
use substreams_ethereum::pb::eth::v2::{Call, Log, TransactionTrace};
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
pub fn maybe_create_component(
    _call: &Call,
    log: &Log,
    _tx: &TransactionTrace,
    config: &DeploymentConfig,
) -> Option<ProtocolComponent> {
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
            implementation_type: ImplementationType::Vm.into(),
        }),
    })
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test_decode_config() {
        let config: DeploymentConfig =
            serde_qs::from_str("relay_address=0001&protocol_type_name=baseline").unwrap();

        assert_eq!(config.relay_address, [0u8, 1u8]);
        assert_eq!(config.protocol_type_name, "baseline");
    }
}
