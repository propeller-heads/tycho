use crate::{
    abi::d3_swap_module::events::LogSwapIn,
    events::EventTrait,
    modules::utils,
    pb::tycho::evm::fluid_v2::Pool,
    storage::{dex_v2, storage_view::StorageChangesView},
};
use substreams::store::{StoreGet, StoreGetProto};
use substreams_ethereum::pb::eth::v2::StorageChange;
use tycho_substreams::prelude::*;

impl EventTrait for LogSwapIn {
    fn get_changed_attributes(
        &self,
        storage_changes: &[StorageChange],
        dex_v2_address: &[u8],
    ) -> (String, Vec<Attribute>) {
        let storage_view = StorageChangesView::new_filtered(dex_v2_address, storage_changes);
        let dex_type = self.dex_type.to_u64();
        let mut attrs = Vec::new();
        attrs.extend(dex_v2::dex_variables_attributes(&storage_view, &self.dex_id, dex_type));
        attrs.extend(dex_v2::token_reserves_attributes(&storage_view, &self.dex_id, dex_type));
        (utils::component_id(&self.dex_type, &self.dex_id), attrs)
    }

    fn get_balance_delta(
        &self,
        tx: &Transaction,
        ordinal: u64,
        pools_store: &StoreGetProto<Pool>,
    ) -> Vec<BalanceDelta> {
        let component_id = utils::component_id(&self.dex_type, &self.dex_id);
        let pool_key = utils::pool_store_key(&self.dex_type, &self.dex_id);
        let pool = match pools_store.get_last(pool_key) {
            Some(pool) => pool,
            None => return vec![],
        };
        let (token_in, token_out) =
            if self.is0to1 { (pool.token0, pool.token1) } else { (pool.token1, pool.token0) };
        vec![
            BalanceDelta {
                ord: ordinal,
                tx: Some(tx.clone()),
                token: token_in.clone(),
                delta: self
                    .amount_in
                    .clone()
                    .to_signed_bytes_be(),
                component_id: component_id.clone().into_bytes(),
            },
            BalanceDelta {
                ord: ordinal,
                tx: Some(tx.clone()),
                token: token_out.clone(),
                delta: self
                    .amount_out
                    .neg()
                    .clone()
                    .to_signed_bytes_be(),
                component_id: component_id.into_bytes(),
            },
        ]
    }
}
