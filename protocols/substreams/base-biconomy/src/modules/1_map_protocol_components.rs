use anyhow::Result;
use substreams_ethereum::pb::eth;
use tycho_substreams::prelude as tycho;

use crate::{
    biconomy::{attrs, PROTOCOL_TYPE_NAME},
    modules::config::Config,
};

/// Creates the venue's one component at `bootstrap_block`, in the venue's deployment
/// transaction (or the block's first transaction touching it).
#[substreams::handlers::map]
pub fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<tycho::BlockTransactionProtocolComponents> {
    let config = Config::parse(&params)?;
    if block.number != config.bootstrap_block {
        return Ok(tycho::BlockTransactionProtocolComponents { tx_components: vec![] });
    }
    let Some(tx) = block.transactions().find(|tx| {
        tx.calls
            .iter()
            .any(|call| call.address == config.venue)
    }) else {
        return Ok(tycho::BlockTransactionProtocolComponents { tx_components: vec![] });
    };

    let component = tycho::ProtocolComponent::at_contract(&config.venue)
        .with_contracts(&[config.venue, config.executor])
        .with_tokens(&config.tokens)
        .with_attributes(&[
            (attrs::PAMM_ADDRESS, config.venue.to_vec()),
            (attrs::EXECUTOR, config.executor.to_vec()),
        ])
        .as_swap_type(PROTOCOL_TYPE_NAME, tycho::ImplementationType::Custom);

    Ok(tycho::BlockTransactionProtocolComponents {
        tx_components: vec![tycho::TransactionProtocolComponents {
            tx: Some(tx.into()),
            components: vec![component],
        }],
    })
}
