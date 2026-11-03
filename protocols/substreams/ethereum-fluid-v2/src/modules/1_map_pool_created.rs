use ethabi::ethereum_types::Address;
use substreams_ethereum::pb::eth::v2::{self as eth};

use substreams_helper::event_handler::EventHandler;

use crate::{
    abi::d3_user_module::events::LogInitialize,
    modules::utils::{self, Params, SUPPORTED_DEX_TYPES},
};

use tycho_substreams::prelude::*;

#[substreams::handlers::map]
pub fn map_pools_created(
    params: String,
    block: eth::Block,
) -> Result<BlockChanges, substreams::errors::Error> {
    let params = Params::parse_from_query(&params)?;
    let dex_v2_address = Address::from_slice(&params.dex_v2_address()?);
    let resolver_address = params.resolver_address()?;
    let mut new_pools: Vec<TransactionChanges> = vec![];
    get_new_pools(&block, &mut new_pools, dex_v2_address, &resolver_address);
    Ok(BlockChanges { block: Some((&block).into()), changes: new_pools, ..Default::default() })
}

fn get_new_pools(
    block: &eth::Block,
    new_pools: &mut Vec<TransactionChanges>,
    dex_v2_address: Address,
    resolver_address: &[u8],
) {
    // Every DEX v2 pool, D3 and D4 alike, emits LogInitialize from the proxy. D1 and D2 pools are
    // a different pool model and are not indexed.
    let mut on_pool_initialize =
        |event: LogInitialize, tx: &eth::TransactionTrace, _: &eth::Log| {
            if !SUPPORTED_DEX_TYPES.contains(&event.dex_type.to_u64()) {
                return;
            }
            let component_id = utils::component_id(&event.dex_type, &event.dex_id);
            new_pools.push(TransactionChanges {
                tx: Some(tx.into()),
                entity_changes: vec![EntityChanges {
                    component_id: component_id.clone(),
                    attributes: initial_attributes(),
                }],
                component_changes: vec![ProtocolComponent {
                    id: component_id,
                    tokens: vec![
                        utils::to_tycho_token(&event.dex_key.0),
                        utils::to_tycho_token(&event.dex_key.1),
                    ],
                    contracts: vec![],
                    static_att: vec![
                        static_attribute("dex_type", event.dex_type.to_signed_bytes_be()),
                        static_attribute("fee", event.dex_key.2.to_signed_bytes_be()),
                        static_attribute("tick_spacing", event.dex_key.3.to_signed_bytes_be()),
                        static_attribute("controller", event.dex_key.4),
                        static_attribute("resolver", resolver_address.to_vec()),
                    ],
                    change: i32::from(ChangeType::Creation),
                    protocol_type: Some(ProtocolType {
                        name: "fluid_v2_pool".to_string(),
                        financial_type: FinancialType::Swap.into(),
                        attribute_schema: vec![],
                        implementation_type: ImplementationType::Custom.into(),
                    }),
                }],
                ..Default::default()
            });
        };
    let mut eh = EventHandler::new(block);

    eh.filter_by_address(dex_v2_address);

    eh.on::<LogInitialize, _>(&mut on_pool_initialize);
    eh.handle_events();
}

/// Zeroed pool state; LogInitialize's storage changes in the same transaction fill it in.
fn initial_attributes() -> Vec<Attribute> {
    [
        ("dex_variables", 32),
        ("dex_variables2", 32),
        ("token0/token_reserves", 16),
        ("token1/token_reserves", 16),
    ]
    .into_iter()
    .map(|(name, size)| Attribute {
        name: name.to_string(),
        value: vec![0u8; size],
        change: ChangeType::Creation.into(),
    })
    .collect()
}

fn static_attribute(name: &str, value: Vec<u8>) -> Attribute {
    Attribute { name: name.to_string(), value, change: ChangeType::Creation.into() }
}
