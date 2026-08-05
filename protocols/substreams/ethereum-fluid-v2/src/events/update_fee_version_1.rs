use crate::{
    abi::controller_module::events::LogUpdateFeeVersion1,
    events::EventTrait,
    modules::utils,
    pb::tycho::evm::fluid_v2::Pool,
    storage::{dex_v2, storage_view::StorageChangesView},
};
use substreams::store::StoreGetProto;
use substreams_ethereum::pb::eth::v2::StorageChange;
use tycho_substreams::prelude::*;

impl EventTrait for LogUpdateFeeVersion1 {
    fn get_changed_attributes(
        &self,
        storage_changes: &[StorageChange],
        dex_v2_address: &[u8],
    ) -> (String, Vec<Attribute>) {
        let storage_view = StorageChangesView::new_filtered(dex_v2_address, storage_changes);
        let dex_type = self.dex_type.to_u64();
        let mut attrs = Vec::new();
        attrs.extend(dex_v2::dex_variables2_attributes(&storage_view, &self.dex_id, dex_type));
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
