pub(crate) mod attributes;
pub(crate) mod component;
pub(crate) mod decoder;
pub(crate) mod events;
pub(crate) mod evm_log;
pub(crate) mod indexed;

pub(crate) type Address = [u8; 20];

pub(crate) use component::{
    component_id, protocol_component, ProtocolComponent, PROTOCOL_TYPE_NAME,
};
pub(crate) use events::EventApplyContext;
pub(crate) use indexed::{
    BlockChanges, BlockChangesBuilder, IndexedTransaction, TransactionChanges,
};
