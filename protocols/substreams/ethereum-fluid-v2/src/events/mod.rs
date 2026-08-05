use crate::{
    abi::{
        controller_module::events::{
            LogUpdateFeeVersion0, LogUpdateFeeVersion1, LogUpdateFetchDynamicFeeFlag,
        },
        d3_admin_module_idx_1::events::{
            LogStopPerPoolAccounting, LogUpdateProtocolCutFee, LogUpdateProtocolFee,
        },
        d3_swap_module::events::{LogSwapIn, LogSwapOut},
        d3_user_module::events::{LogDeposit, LogInitialize, LogWithdraw},
    },
    pb::tycho::evm::fluid_v2::Pool,
};
use substreams::store::StoreGetProto;
use substreams_ethereum::{
    pb::eth::v2::{Log, StorageChange},
    Event,
};
use tycho_substreams::prelude::*;

pub mod deposit;
pub mod fetch_dynamic_fee_flag;
pub mod initialize;
pub mod stop_per_pool_accounting;
pub mod swap_in;
pub mod swap_out;
pub mod update_fee_version_0;
pub mod update_fee_version_1;
pub mod update_protocol_cut_fee;
pub mod update_protocol_fee;
pub mod withdraw;

/// A trait for extracting changed attributes and balance from an event.
pub trait EventTrait {
    /// Get all relevant changed attributes from the `[StorageChange]`.
    ///
    /// # Arguments
    ///
    /// * `storage_changes` - A slice of `StorageChange` that indicates the changes in storage.
    /// * `dex_v2_address` - Address of the dex storage contract.
    fn get_changed_attributes(
        &self,
        storage_changes: &[StorageChange],
        dex_v2_address: &[u8],
    ) -> (String, Vec<Attribute>);

    /// Get all balance deltas from the event.
    ///
    /// # Arguments
    ///
    /// * `tx` - Reference to the `Transaction`.
    /// * `ordinal` - The ordinal number of the event.
    /// * `pools_store` - Store to lookup pools by dex id.
    fn get_balance_delta(
        &self,
        tx: &Transaction,
        ordinal: u64,
        pools_store: &StoreGetProto<Pool>,
    ) -> Vec<BalanceDelta>;
}

/// Represent every events of a DEX V2 pool.
pub enum EventType {
    Initialize(LogInitialize),
    SwapIn(LogSwapIn),
    SwapOut(LogSwapOut),
    Deposit(LogDeposit),
    Withdraw(LogWithdraw),
    StopPerPoolAccounting(LogStopPerPoolAccounting),
    UpdateProtocolFee(LogUpdateProtocolFee),
    UpdateProtocolCutFee(LogUpdateProtocolCutFee),
    UpdateFeeVersion0(LogUpdateFeeVersion0),
    UpdateFeeVersion1(LogUpdateFeeVersion1),
    UpdateFetchDynamicFeeFlag(LogUpdateFetchDynamicFeeFlag),
}

impl EventType {
    fn as_event_trait(&self) -> &dyn EventTrait {
        match self {
            EventType::Initialize(e) => e,
            EventType::SwapIn(e) => e,
            EventType::SwapOut(e) => e,
            EventType::Deposit(e) => e,
            EventType::Withdraw(e) => e,
            EventType::StopPerPoolAccounting(e) => e,
            EventType::UpdateProtocolFee(e) => e,
            EventType::UpdateProtocolCutFee(e) => e,
            EventType::UpdateFeeVersion0(e) => e,
            EventType::UpdateFeeVersion1(e) => e,
            EventType::UpdateFetchDynamicFeeFlag(e) => e,
        }
    }
}

/// Decodes a given log into an `EventType`.
pub fn decode_event(event: &Log) -> Option<EventType> {
    [
        LogInitialize::match_and_decode(event).map(EventType::Initialize),
        LogSwapIn::match_and_decode(event).map(EventType::SwapIn),
        LogSwapOut::match_and_decode(event).map(EventType::SwapOut),
        LogDeposit::match_and_decode(event).map(EventType::Deposit),
        LogWithdraw::match_and_decode(event).map(EventType::Withdraw),
        LogStopPerPoolAccounting::match_and_decode(event).map(EventType::StopPerPoolAccounting),
        LogUpdateProtocolFee::match_and_decode(event).map(EventType::UpdateProtocolFee),
        LogUpdateProtocolCutFee::match_and_decode(event).map(EventType::UpdateProtocolCutFee),
        LogUpdateFeeVersion0::match_and_decode(event).map(EventType::UpdateFeeVersion0),
        LogUpdateFeeVersion1::match_and_decode(event).map(EventType::UpdateFeeVersion1),
        LogUpdateFetchDynamicFeeFlag::match_and_decode(event)
            .map(EventType::UpdateFetchDynamicFeeFlag),
    ]
    .into_iter()
    .find_map(std::convert::identity)
}

/// Gets the changed attributes from the log.
pub fn get_log_changed_attributes(
    event: &Log,
    storage_changes: &[StorageChange],
    dex_v2_address: &[u8],
) -> Option<(String, Vec<Attribute>)> {
    decode_event(event)
        .map(|e| {
            e.as_event_trait()
                .get_changed_attributes(storage_changes, dex_v2_address)
        })
        .and_then(
            |(component_id, attrs)| {
                if attrs.is_empty() {
                    None
                } else {
                    Some((component_id, attrs))
                }
            },
        )
}

/// Gets the changed balances from the log.
pub fn get_log_changed_balances(
    tx: &Transaction,
    event: &Log,
    dex_v2_address: &[u8],
    pools_store: &StoreGetProto<Pool>,
) -> Vec<BalanceDelta> {
    if event.address != dex_v2_address {
        return vec![];
    }
    decode_event(event)
        .map(|e| {
            e.as_event_trait()
                .get_balance_delta(tx, event.ordinal, pools_store)
        })
        .unwrap_or_default()
}
