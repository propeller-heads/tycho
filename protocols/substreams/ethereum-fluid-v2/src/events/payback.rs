use crate::{
    abi::d4_user_module::events::LogPayback,
    events::{position::PositionChange, EventTrait},
    pb::tycho::evm::fluid_v2::Pool,
};
use substreams::store::StoreGetProto;
use substreams_ethereum::pb::eth::v2::StorageChange;
use tycho_substreams::prelude::*;

impl LogPayback {
    fn position_change(&self) -> PositionChange<'_> {
        PositionChange {
            dex_type: &self.dex_type,
            dex_id: &self.dex_id,
            tick_lower: &self.tick_lower,
            tick_upper: &self.tick_upper,
            amount0: &self.amount0,
            amount1: &self.amount1,
            adds_reserves: false,
        }
    }
}

impl EventTrait for LogPayback {
    fn get_changed_attributes(
        &self,
        storage_changes: &[StorageChange],
        dex_v2_address: &[u8],
    ) -> (String, Vec<Attribute>) {
        self.position_change()
            .changed_attributes(storage_changes, dex_v2_address)
    }

    fn get_balance_delta(
        &self,
        tx: &Transaction,
        ordinal: u64,
        pools_store: &StoreGetProto<Pool>,
    ) -> Vec<BalanceDelta> {
        self.position_change()
            .balance_deltas(tx, ordinal, pools_store)
    }
}
