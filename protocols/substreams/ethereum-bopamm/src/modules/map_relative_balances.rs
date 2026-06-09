use anyhow::Result;
use substreams::store::{StoreGet, StoreGetProto, StoreGetString};
use substreams_ethereum::{pb::eth, Event};
use tycho_substreams::{abi::erc20::events::Transfer, prelude::*};

use crate::{common::books_for_token, config::DeploymentConfig};

/// Emits the maker's inventory balance deltas as per-book TVL.
///
/// Tracks every ERC20 `Transfer` touching the maker for tokens that belong to a known book.
/// USDC deltas are emitted under every book (the shared quote inventory is duplicated, not
/// split).
#[substreams::handlers::map]
pub fn map_relative_balances(
    params: String,
    block: eth::v2::Block,
    components_store: StoreGetProto<ProtocolComponent>,
    maker_store: StoreGetString,
) -> Result<BlockBalanceDeltas> {
    let config: DeploymentConfig = serde_qs::from_str(&params)?;
    let Some(maker_hex) = maker_store.get_last("maker") else {
        return Ok(BlockBalanceDeltas::default());
    };
    let maker = hex::decode(maker_hex)?;

    let mut balance_deltas = Vec::new();
    for log in block.logs() {
        let Some(Transfer { from, to, value }) = Transfer::match_and_decode(log.log) else {
            continue;
        };
        let to_maker = to == maker;
        let from_maker = from == maker;
        if !to_maker && !from_maker {
            continue;
        }
        let token = log.address().to_vec();
        let books = books_for_token(&token, &config.usdc, &components_store);
        if books.is_empty() {
            continue;
        }
        for comp_id in books {
            let delta = if to_maker { value.clone() } else { value.clone().neg() };
            balance_deltas.push(BalanceDelta {
                ord: log.ordinal(),
                tx: Some(log.receipt.transaction.into()),
                token: token.clone(),
                delta: delta.to_signed_bytes_be(),
                component_id: comp_id.into_bytes(),
            });
        }
    }
    Ok(BlockBalanceDeltas { balance_deltas })
}
