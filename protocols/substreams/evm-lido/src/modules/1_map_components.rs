use anyhow::Result;
use substreams_ethereum::pb::eth;
use tycho_substreams::{
    models::{ChangeType, FinancialType, ImplementationType, ProtocolComponent, ProtocolType},
    prelude::*,
};

use crate::{
    ETH_ADDRESS, ST_ETH_ADDRESS_PROXY, ST_ETH_ADDRESS_PROXY_COMPONENT_ID, WST_ETH_ADDRESS,
    WST_ETH_ADDRESS_COMPONENT_ID,
};

/// Create all relevant protocol components
///
/// This method instantiates hardcoded ProtocolComponents for the first block,
///  with a unique ids as well as all necessary metadata for routing and encoding.
#[substreams::handlers::map]
fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    if block.number !=
        params
            .parse::<u64>()
            .map_err(|e| anyhow::anyhow!("Failed to parse block number from params: {}", e))?
    {
        return Ok(BlockTransactionProtocolComponents { tx_components: vec![] })
    }

    let tx: &eth::v2::TransactionTrace = block.transactions().next().unwrap();
    let components = create_components();

    Ok(BlockTransactionProtocolComponents {
        tx_components: vec![TransactionProtocolComponents { tx: Some(tx.into()), components }],
    })
}

/// Constructs ProtocolComponents
///
/// This method is used only for the first block and first transaction in that block, the
/// corresponding logs emitted during that call as well as the full transaction trace.
pub fn create_components() -> Vec<ProtocolComponent> {
    vec![
        ProtocolComponent {
            id: ST_ETH_ADDRESS_PROXY_COMPONENT_ID.to_owned(),
            tokens: vec![ST_ETH_ADDRESS_PROXY.into(), ETH_ADDRESS.into()],
            contracts: vec![],
            static_att: vec![
                Attribute {
                    name: "protocol_type_name".into(),
                    value: "stETH".into(),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "token_to_track_total_pooled_eth".into(),
                    value: ETH_ADDRESS.into(),
                    change: ChangeType::Creation.into(),
                },
            ],
            change: ChangeType::Creation.into(),
            protocol_type: Some(ProtocolType {
                name: "stETH".to_string(),
                financial_type: FinancialType::Swap.into(),
                attribute_schema: vec![],
                implementation_type: ImplementationType::Custom.into(),
            }),
        },
        ProtocolComponent {
            id: WST_ETH_ADDRESS_COMPONENT_ID.to_owned(),
            tokens: vec![ST_ETH_ADDRESS_PROXY.into(), WST_ETH_ADDRESS.into()],
            contracts: vec![],
            static_att: vec![
                Attribute {
                    name: "protocol_type_name".into(),
                    value: "wstETH".into(),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "token_to_track_total_pooled_eth".into(),
                    value: ST_ETH_ADDRESS_PROXY.into(),
                    change: ChangeType::Creation.into(),
                },
            ],
            change: ChangeType::Creation.into(),
            protocol_type: Some(ProtocolType {
                name: "wstETH".to_string(),
                financial_type: FinancialType::Swap.into(),
                attribute_schema: vec![],
                implementation_type: ImplementationType::Custom.into(),
            }),
        },
    ]
}
