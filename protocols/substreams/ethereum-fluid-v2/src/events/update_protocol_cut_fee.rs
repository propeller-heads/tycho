use crate::{
    abi::d3_admin_module_idx_1::events::LogUpdateProtocolCutFee,
    events::EventTrait,
    modules::utils,
    pb::tycho::evm::fluid_v2::Pool,
    storage::{dex_v2, storage_view::StorageChangesView},
};
use substreams::store::StoreGetProto;
use substreams_ethereum::pb::eth::v2::StorageChange;
use tycho_substreams::prelude::*;

impl EventTrait for LogUpdateProtocolCutFee {
    fn get_changed_attributes(
        &self,
        storage_changes: &[StorageChange],
        dex_v2_address: &[u8],
    ) -> (String, Vec<Attribute>) {
        let storage_view = StorageChangesView::new_filtered(dex_v2_address, storage_changes);
        let dex_type = self.dex_type.to_u64();
        let attrs = dex_v2::dex_variables2_attributes(&storage_view, &self.dex_id, dex_type);
        (utils::component_id(&self.dex_type, &self.dex_id), attrs)
    }

    fn get_balance_delta(
        &self,
        _tx: &Transaction,
        _ordinal: u64,
        _pools_store: &StoreGetProto<Pool>,
    ) -> Vec<BalanceDelta> {
        vec![]
    }
}
