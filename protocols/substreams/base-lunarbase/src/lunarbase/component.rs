use crate::lunarbase::Address;
use tycho_substreams::prelude as tycho;

pub const PROTOCOL_TYPE_NAME: &str = "lunarbase_pool";

pub fn component_id(pool: Address) -> String {
    address_to_hex(pool)
}

pub fn protocol_component(
    pool: Address,
    token_x: Address,
    token_y: Address,
) -> tycho::ProtocolComponent {
    tycho::ProtocolComponent::new(&component_id(pool))
        .with_tokens(&[token_x, token_y])
        .with_contracts(&[pool])
        .as_swap_type(PROTOCOL_TYPE_NAME, tycho::ImplementationType::Custom)
}

fn address_to_hex(address: Address) -> String {
    let mut out = String::with_capacity(42);
    out.push_str("0x");
    for byte in address {
        out.push(nibble_to_hex(byte >> 4));
        out.push(nibble_to_hex(byte & 0x0f));
    }
    out
}

fn nibble_to_hex(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'a' + value - 10) as char,
        _ => unreachable!("nibble is always <= 15"),
    }
}
