use anyhow::Result;
use substreams_ethereum::pb::eth;
use tycho_substreams::prelude as tycho;

use crate::{lunarbase, modules::config::Config, tycho_mapper::to_tycho_protocol_component};

#[substreams::handlers::map]
pub fn map_protocol_components(
    params: String,
    block: eth::v2::Block,
) -> Result<tycho::BlockTransactionProtocolComponents> {
    let config = Config::parse(&params)?;
    if config.bootstrap_block != Some(block.number) {
        return Ok(tycho::BlockTransactionProtocolComponents::default());
    }

    let component = to_tycho_protocol_component(lunarbase::protocol_component(
        config.pool,
        config.token_x,
        config.token_y,
    ));

    let tx_components = block
        .transactions()
        .find(|tx| {
            tx.logs_with_calls()
                .any(|(log, _)| log.address == config.pool)
        })
        .map(|tx| tycho::TransactionProtocolComponents {
            tx: Some(tx.into()),
            components: vec![component],
        })
        .into_iter()
        .collect();

    Ok(tycho::BlockTransactionProtocolComponents { tx_components })
}
