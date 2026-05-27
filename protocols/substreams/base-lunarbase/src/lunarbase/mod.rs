pub mod attributes;
pub mod component;
pub mod decoder;
pub mod events;
pub mod evm_log;
pub mod indexed;

pub type Address = [u8; 20];

pub use component::{component_id, protocol_component, ProtocolComponent, PROTOCOL_TYPE_NAME};
pub use events::EventApplyContext;
pub use evm_log::EvmLog;
pub use indexed::{BlockChanges, BlockChangesBuilder, IndexedTransaction, TransactionChanges};
