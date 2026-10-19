use serde::Deserialize;
use substreams_ethereum::pb::eth::v2::{Call, Log, TransactionTrace};
use tycho_substreams::models::{
    Attribute, ChangeType, FinancialType, ImplementationType, ProtocolComponent, ProtocolType,
};

#[derive(Deserialize)]
pub struct DeploymentConfig {
    #[serde(with = "hex::serde")]
    pub vault_address: Vec<u8>,
}

/// Potentially constructs a new ProtocolComponent given a call
///
/// This method is given each individual call within a transaction, the corresponding
/// logs emitted during that call as well as the full transaction trace.
///
/// If this call creates a component in your protocol please contstruct and return it
/// here. Otherwise, simply return None.
pub fn maybe_create_component(
    call: &Call,
    _log: &Log,
    _tx: &TransactionTrace,
    config: &DeploymentConfig,
) -> Option<ProtocolComponent> {
    if call.address == config.vault_address {
        // TODO: replace with your logic
        Some(ProtocolComponent {
            id: "".to_string(),
            tokens: vec![
                // TODO: add the components tokens
            ],
            contracts: vec![
                config.vault_address.clone(),
                // TODO: any additional contracts required during swapping
            ],
            static_att: vec![
                // Singleton components are marked as manual updates since we can't
                // infer component updates from vault storage updates. The template
                // later sets update markers on components that had a balance change.
                Attribute {
                    name: "manual_updates".to_string(),
                    value: vec![1u8],
                    change: ChangeType::Creation.into(),
                }, // TODO: any additional metadata required, e.g. for swap encoding
            ],
            change: ChangeType::Creation.into(),
            protocol_type: Some(ProtocolType {
                name: "template".to_string(),
                financial_type: FinancialType::Swap.into(),
                attribute_schema: vec![],
                implementation_type: ImplementationType::Vm.into(),
            }),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn test_decode_config() {
        let config: DeploymentConfig = serde_qs::from_str("vault_address=0001").unwrap();

        assert_eq!(config.vault_address, [0u8, 1u8]);
    }
}
