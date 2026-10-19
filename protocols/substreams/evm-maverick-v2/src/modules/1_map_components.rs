use anyhow::{Ok, Result};
use ethabi::ethereum_types::Address;
use substreams_ethereum::pb::eth::v2::{Block, Log, TransactionTrace};
use tycho_substreams::prelude::*;

use crate::{abi::factory::events::PoolCreated, modules::utils::Params};
use substreams_helper::{event_handler::EventHandler, hex::Hexable};

#[substreams::handlers::map]
pub fn map_components(params: String, block: Block) -> Result<BlockTransactionProtocolComponents> {
    let mut new_pools: Vec<TransactionProtocolComponents> = vec![];
    let params = Params::parse_from_query(&params)?;
    get_new_pools(params, &block, &mut new_pools);

    Ok(BlockTransactionProtocolComponents { tx_components: new_pools })
}

fn get_new_pools(
    params: Params,
    block: &Block,
    new_pools: &mut Vec<TransactionProtocolComponents>,
) {
    let (factory_address, quoter_address) = params.decode_addresses().unwrap();

    let mut on_pool_created = |event: PoolCreated, _tx: &TransactionTrace, _log: &Log| {
        let tycho_tx: Transaction = _tx.into();
        let contracts = vec![
            event.pool_address.as_slice(),
            factory_address.as_slice(),
            quoter_address.as_slice(),
        ];
        let new_pool_component = ProtocolComponent::new(&event.pool_address.to_hex())
            .with_tokens(&[event.token_a.as_slice(), event.token_b.as_slice()])
            .with_contracts(&contracts)
            .with_attributes(&[
                ("fee_a_in", &event.fee_a_in.to_signed_bytes_be()),
                ("fee_b_in", &event.fee_b_in.to_signed_bytes_be()),
                ("tick_spacing", &event.tick_spacing.to_signed_bytes_be()),
                ("kinds", &event.kinds.to_signed_bytes_be()),
            ])
            .as_swap_type("maverick_v2_pool", ImplementationType::Vm);

        new_pools.push(TransactionProtocolComponents {
            tx: Some(tycho_tx.clone()),
            components: vec![new_pool_component],
        });
    };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(vec![Address::from_slice(&factory_address)]);

    eh.on::<PoolCreated, _>(&mut on_pool_created);
    eh.handle_events();
}
