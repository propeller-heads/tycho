pub(crate) mod component;
pub(crate) mod events;
pub(crate) mod indexed;
pub(crate) mod state;

pub(crate) type Address = [u8; 20];

pub(crate) use component::{component_id, protocol_component};
