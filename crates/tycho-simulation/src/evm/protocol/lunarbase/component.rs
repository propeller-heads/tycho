use super::{state::Address, PROTOCOL_SYSTEM};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolComponent {
    pub id: String,
    pub protocol_system: &'static str,
    pub tokens: [Address; 2],
    pub contract_addresses: Vec<Address>,
}

pub fn component_id(pool: Address) -> String {
    address_to_hex(pool)
}

pub fn protocol_component(pool: Address, token_x: Address, token_y: Address) -> ProtocolComponent {
    ProtocolComponent {
        id: component_id(pool),
        protocol_system: PROTOCOL_SYSTEM,
        tokens: [token_x, token_y],
        contract_addresses: vec![pool],
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_stable_component_id() {
        let pool = [0xab; 20];
        assert_eq!(component_id(pool), "0xabababababababababababababababababababab");
    }
}
