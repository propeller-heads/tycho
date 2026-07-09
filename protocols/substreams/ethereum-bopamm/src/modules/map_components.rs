use std::collections::HashMap;

use anyhow::Result;
use substreams_ethereum::pb::eth;
use tycho_substreams::prelude::*;

use tycho_substreams::attributes::json_serialize_address_list;

use crate::{
    common::{asset_config_slot, component_id, is_zero, MAX_ASSET_ID},
    config::DeploymentConfig,
};

/// Discovers books from zero→non-zero writes to the module's asset-config slots.
///
/// BopAMM emits no creation event, so a new market is observed as the first write that
/// sets `assetConfig[assetId]` (packed `param | decimals | token`, token in the low 20
/// bytes). Emits one `Creation` component per newly configured book.
#[substreams::handlers::map]
pub fn map_components(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    let config: DeploymentConfig = serde_qs::from_str(&params)?;

    // The asset-config slot map is a pure function of the deployment's base slot, but it is
    // built lazily on the first module storage change: asset-config writes are rare, so the
    // `MAX_ASSET_ID` keccak computations are skipped entirely on the blocks (almost all of
    // them) that never touch the module's storage.
    let mut slot_to_asset: Option<HashMap<Vec<u8>, u64>> = None;

    let mut tx_components = Vec::new();
    for tx in block.transactions() {
        let mut components = Vec::new();
        for call in tx
            .calls
            .iter()
            .filter(|c| !c.state_reverted)
        {
            for change in &call.storage_changes {
                if change.address != config.module {
                    continue;
                }
                let slot_to_asset = slot_to_asset.get_or_insert_with(|| {
                    (0..MAX_ASSET_ID)
                        .map(|i| (asset_config_slot(i, config.asset_config_base_slot), i))
                        .collect()
                });
                let Some(&asset_id) = slot_to_asset.get(&change.key) else { continue };
                // Only a fresh listing (zero -> non-zero) creates a book. Delisting
                // (non-zero -> zero) is intentionally not tracked: components are immutable
                // and the venue has never delisted an asset; revisit if that changes.
                if !is_zero(&change.old_value) || is_zero(&change.new_value) {
                    continue;
                }
                let Some(token) = change.new_value.get(12..32) else { continue };
                let asset_id_bytes = asset_id.to_be_bytes();
                let mut tokens = vec![token.to_vec(), config.usdc.clone()];
                tokens.sort_unstable();
                // BopAMM emits no token-contract storage, so in the shared simulation DB its
                // tokens are self-contained mock proxies. Flag them (ENG-6161) so
                // tycho-simulation resolves their transfers via local proxy bookkeeping
                // instead of delegating to an implementation another VM protocol indexed for
                // the same token. This covers the maker→taker output `transferFrom` (the
                // documented failure); the settlement-intermediary sell-token legs are a
                // separate, bytecode-dependent residual — see HANDOVER §8.
                let self_contained = json_serialize_address_list(&tokens);
                components.push(
                    ProtocolComponent::new(&component_id(&config.settlement, asset_id))
                        .with_tokens(&tokens)
                        .with_contracts(&[
                            config.settlement.clone(),
                            config.module.clone(),
                            config.registry.clone(),
                        ])
                        .with_attributes(&[
                            ("asset_id", &asset_id_bytes[..]),
                            ("manual_updates", &[1u8][..]),
                            ("self_contained_tokens", self_contained.as_slice()),
                        ])
                        .as_swap_type("bopamm_book", ImplementationType::Vm),
                );
            }
        }
        if !components.is_empty() {
            tx_components.push(TransactionProtocolComponents { tx: Some(tx.into()), components });
        }
    }
    Ok(BlockTransactionProtocolComponents { tx_components })
}
