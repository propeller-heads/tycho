use anyhow::Result;
use substreams_ethereum::pb::eth;
use tycho_substreams::prelude as tycho;

use crate::{lunarbase, modules::config::Config, tycho_mapper::to_tycho_block_changes};

#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    new_components: tycho::BlockTransactionProtocolComponents,
) -> Result<tycho::BlockChanges> {
    let config = Config::parse(&params)?;
    let component_id = lunarbase::component_id(config.pool);
    let component_is_known = config
        .bootstrap_block
        .map(|bootstrap_block| block.number >= bootstrap_block)
        .unwrap_or(true) ||
        new_components
            .tx_components
            .iter()
            .flat_map(|tx| tx.components.iter())
            .any(|component| component.id == component_id);

    let known_components = component_is_known
        .then(|| component_id.clone())
        .into_iter()
        .collect::<Vec<_>>();
    let mut builder = lunarbase::BlockChangesBuilder::new(known_components);

    for tx_components in new_components.tx_components.iter() {
        let Some(tx) = tx_components
            .tx
            .as_ref()
            .and_then(indexed_tx_from_tycho_tx)
        else {
            continue;
        };
        for component in tx_components.components.iter() {
            builder.register_component(
                tx,
                lunarbase::protocol_component(config.pool, config.token_x, config.token_y),
            );
            if component.id != component_id {
                substreams::log::info!("ignoring unexpected LunarBase component {}", component.id);
            }
        }
    }

    for tx in block.transactions() {
        for (log, _) in tx.logs_with_calls() {
            if log.address != config.pool {
                continue;
            }

            let Some(indexed_tx) = indexed_tx_from_eth_tx(tx) else {
                continue;
            };
            let Some(evm_log) = evm_log_from_eth_log(log) else {
                continue;
            };

            if let Err(err) = builder.apply_log(
                indexed_tx,
                &evm_log,
                lunarbase::EventApplyContext {
                    block_number: block.number,
                    tycho_executor: config.tycho_executor,
                },
                config.token_x,
                config.token_y,
            ) {
                substreams::log::info!("failed to apply LunarBase log: {:?}", err);
            }
        }
    }

    Ok(to_tycho_block_changes((&block).into(), builder.finish()))
}

fn indexed_tx_from_eth_tx(tx: &eth::v2::TransactionTrace) -> Option<lunarbase::IndexedTransaction> {
    Some(lunarbase::IndexedTransaction {
        hash: fixed_32(&tx.hash)?,
        from: fixed_20(&tx.from)?,
        to: fixed_20(&tx.to)?,
        index: tx.index.into(),
    })
}

fn indexed_tx_from_tycho_tx(tx: &tycho::Transaction) -> Option<lunarbase::IndexedTransaction> {
    Some(lunarbase::IndexedTransaction {
        hash: fixed_32(&tx.hash)?,
        from: fixed_20(&tx.from)?,
        to: fixed_20(&tx.to)?,
        index: tx.index,
    })
}

fn evm_log_from_eth_log(log: &eth::v2::Log) -> Option<lunarbase::EvmLog> {
    Some(lunarbase::EvmLog {
        address: fixed_20(&log.address)?,
        topics: log
            .topics
            .iter()
            .filter_map(|topic| fixed_32(topic))
            .collect(),
        data: log.data.clone(),
    })
}

fn fixed_20(value: &[u8]) -> Option<[u8; 20]> {
    value.try_into().ok()
}

fn fixed_32(value: &[u8]) -> Option<[u8; 32]> {
    value.try_into().ok()
}
