use crate::{
    modules::utils,
    pb::tycho::evm::fluid_v2::Pool,
    storage::{dex_v2, storage_view::StorageChangesView},
};
use substreams::{
    scalar::BigInt,
    store::{StoreGet, StoreGetProto},
};
use substreams_ethereum::pb::eth::v2::StorageChange;
use tycho_substreams::prelude::*;

/// A liquidity position change: a D3 deposit or withdrawal, or a D4 borrow or payback.
pub struct PositionChange<'a> {
    pub dex_type: &'a BigInt,
    pub dex_id: &'a [u8; 32],
    pub tick_lower: &'a BigInt,
    pub tick_upper: &'a BigInt,
    pub amount0: &'a BigInt,
    pub amount1: &'a BigInt,
    /// Whether the change adds to the pool's reserves. A D4 borrow adds to them like a D3
    /// deposit does.
    pub adds_reserves: bool,
}

impl PositionChange<'_> {
    pub fn changed_attributes(
        &self,
        storage_changes: &[StorageChange],
        dex_v2_address: &[u8],
    ) -> (String, Vec<Attribute>) {
        let storage_view = StorageChangesView::new_filtered(dex_v2_address, storage_changes);
        let dex_type = self.dex_type.to_u64();
        let mut attrs = dex_v2::tick_data_attributes(
            &storage_view,
            self.dex_id,
            dex_type,
            vec![self.tick_lower, self.tick_upper],
        );
        attrs.extend(dex_v2::dex_variables2_attributes(&storage_view, self.dex_id, dex_type));
        attrs.extend(dex_v2::token_reserves_attributes(&storage_view, self.dex_id, dex_type));
        (utils::component_id(self.dex_type, self.dex_id), attrs)
    }

    pub fn balance_deltas(
        &self,
        tx: &Transaction,
        ordinal: u64,
        pools_store: &StoreGetProto<Pool>,
    ) -> Vec<BalanceDelta> {
        let Some(pool) = pools_store.get_last(utils::pool_store_key(self.dex_type, self.dex_id))
        else {
            return vec![];
        };
        let component_id = utils::component_id(self.dex_type, self.dex_id);
        [(pool.token0, self.amount0), (pool.token1, self.amount1)]
            .into_iter()
            .map(|(token, amount)| {
                let delta = if self.adds_reserves { amount.clone() } else { amount.neg() };
                BalanceDelta {
                    ord: ordinal,
                    tx: Some(tx.clone()),
                    token,
                    delta: delta.to_signed_bytes_be(),
                    component_id: component_id.clone().into_bytes(),
                }
            })
            .collect()
    }
}
