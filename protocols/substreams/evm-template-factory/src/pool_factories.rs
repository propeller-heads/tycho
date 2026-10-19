use substreams::hex;
use substreams_ethereum::pb::eth::v2::{Call, Log, TransactionTrace};
use tycho_substreams::models::{
    ChangeType, FinancialType, ImplementationType, ProtocolComponent, ProtocolType,
};

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
) -> Option<ProtocolComponent> {
    match *call.address {
        // TODO: replace with your logic
        hex!("0000000000000000000000000000000000000000") => {
            Some(ProtocolComponent {
                id: "".to_string(),
                tokens: vec![
                    // TODO: add the components tokens
                ],
                contracts: vec![
                    // TODO: any contracts required during swapping
                ],
                static_att: vec![
                    // TODO: any additional metadata required, e.g. for swap encoding
                ],
                change: ChangeType::Creation.into(),
                protocol_type: Some(ProtocolType {
                    name: "template".to_string(),
                    financial_type: FinancialType::Swap.into(),
                    attribute_schema: vec![],
                    implementation_type: ImplementationType::Vm.into(),
                }),
            })
        }
        _ => None,
    }
}
