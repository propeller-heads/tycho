pub(crate) mod component;
pub(crate) mod events;
pub(crate) mod indexed;
pub(crate) mod state;

pub(crate) type Address = [u8; 20];

pub(crate) use component::{
    component_id, protocol_component, ProtocolComponent, PROTOCOL_TYPE_NAME,
};
pub(crate) use events::EventApplyContext;
pub(crate) use indexed::{
    BlockChanges, BlockChangesBuilder, BootstrapState, IndexedTransaction, TransactionChanges,
};
