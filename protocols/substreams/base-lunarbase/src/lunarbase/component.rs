use tycho_substreams::prelude as tycho;

use crate::lunarbase::Address;

pub const PROTOCOL_TYPE_NAME: &str = "lunarbase_pool";

pub fn component_id(pool: Address) -> String {
    format!("0x{}", hex::encode(pool))
}

pub fn protocol_component(
    pool: Address,
    token_x: Address,
    token_y: Address,
    quote_caller: Address,
) -> tycho::ProtocolComponent {
    tycho::ProtocolComponent::new(&component_id(pool))
        .with_tokens(&[token_x, token_y])
        .with_attributes(&[("quote_caller", quote_caller)])
        .as_swap_type(PROTOCOL_TYPE_NAME, tycho::ImplementationType::Custom)
}
