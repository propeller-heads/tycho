//! Storage layout and events of the Biconomy PropAMM executor and venue, and the component
//! attributes `tycho-simulation`'s `biconomy` state decodes.
//!
//! The executor stores every board in `boards[mm][tokenIn][tokenOut]` (slot 0, 24 words per
//! board), every pair anchor in `anchors[mm][token0][token1]` (slot 1) and the self-pause flag in
//! `makerPaused[mm]` (slot 2). Every write to them comes with an event naming the maker and the
//! pair, which is how a storage change is matched to the board it belongs to.
//!
//! The venue keeps `feeBps` in the low 16 bits of slot 0 and `makerList` as a dynamic array at
//! slot 1. Provider inventory comes from the configured tokens' balance and allowance mappings.
//! Nothing is read over RPC.

use tiny_keccak::{Hasher, Keccak};
use tycho_substreams::prelude as tycho;

pub type Address = [u8; 20];
pub type Word = [u8; 32];

pub const PROTOCOL_TYPE_NAME: &str = "biconomy_venue";
pub const BOARD_WORDS: u8 = 24;
/// Makers the venue's list is tracked for (`makerList` element slots precomputed per tx).
pub const MAX_MAKERS: u8 = 64;

const BOARDS_SLOT: u8 = 0;
const ANCHORS_SLOT: u8 = 1;
const PAUSED_SLOT: u8 = 2;
const VENUE_FEE_SLOT: u8 = 0;
const VENUE_MAKERS_SLOT: u8 = 1;

pub mod attrs {
    pub const FEE_BPS: &str = "fee_bps";
    pub const MAKER_COUNT: &str = "maker_count";
    pub const PAMM_ADDRESS: &str = "pamm_address";
    pub const EXECUTOR: &str = "executor";
}

pub fn keccak(data: &[u8]) -> Word {
    let mut hasher = Keccak::v256();
    hasher.update(data);
    let mut out = [0u8; 32];
    hasher.finalize(&mut out);
    out
}

fn mapping_slot(key: &Address, base: &Word) -> Word {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(key);
    buf[32..].copy_from_slice(base);
    keccak(&buf)
}

pub fn root(slot: u8) -> Word {
    let mut word = [0u8; 32];
    word[31] = slot;
    word
}

fn add(word: &Word, n: u8) -> Word {
    let mut out = *word;
    let mut carry = u16::from(n);
    for byte in out.iter_mut().rev() {
        let sum = u16::from(*byte) + carry;
        *byte = sum as u8;
        carry = sum >> 8;
        if carry == 0 {
            break;
        }
    }
    out
}

/// First storage slot of `boards[mm][token_in][token_out]`.
pub fn board_slot(mm: &Address, token_in: &Address, token_out: &Address) -> Word {
    mapping_slot(token_out, &mapping_slot(token_in, &mapping_slot(mm, &root(BOARDS_SLOT))))
}

pub fn anchor_slot(mm: &Address, token0: &Address, token1: &Address) -> Word {
    mapping_slot(token1, &mapping_slot(token0, &mapping_slot(mm, &root(ANCHORS_SLOT))))
}

pub fn paused_slot(mm: &Address) -> Word {
    mapping_slot(mm, &root(PAUSED_SLOT))
}

/// `balanceOf[holder]` for a token keeping balances in a mapping at `base`.
pub fn balance_slot(holder: &Address, base: u8) -> Word {
    mapping_slot(holder, &root(base))
}

/// `allowance[owner][spender]` for a token keeping allowances in a mapping at `base`.
pub fn allowance_slot(owner: &Address, spender: &Address, base: u8) -> Word {
    mapping_slot(spender, &mapping_slot(owner, &root(base)))
}

/// Every venue slot the fee and maker list live in, with its attribute.
pub fn venue_slots() -> Vec<(Word, String)> {
    let mut slots = vec![
        (root(VENUE_FEE_SLOT), attrs::FEE_BPS.to_owned()),
        (root(VENUE_MAKERS_SLOT), attrs::MAKER_COUNT.to_owned()),
    ];
    let first = keccak(&root(VENUE_MAKERS_SLOT));
    for i in 0..MAX_MAKERS {
        slots.push((add(&first, i), maker_attribute(i)));
    }
    slots
}

pub fn hex_address(address: &[u8]) -> String {
    format!("0x{}", hex::encode(address))
}

pub fn board_attribute(mm: &Address, token_in: &Address, token_out: &Address, slot: u8) -> String {
    format!("board/{}/{}/{}/{slot}", hex_address(mm), hex_address(token_in), hex_address(token_out))
}

pub fn anchor_attribute(mm: &Address, token0: &Address, token1: &Address) -> String {
    format!("anchor/{}/{}/{}", hex_address(mm), hex_address(token0), hex_address(token1))
}

pub fn paused_attribute(mm: &Address) -> String {
    format!("paused/{}", hex_address(mm))
}

pub fn maker_attribute(index: u8) -> String {
    format!("maker/{index}")
}

pub fn provider_attribute(provider: &Address) -> String {
    format!("provider/{}", hex_address(provider))
}

pub fn balance_attribute(token: &Address, holder: &Address) -> String {
    format!("balance/{}/{}", hex_address(token), hex_address(holder))
}

pub fn allowance_attribute(token: &Address, owner: &Address, spender: &Address) -> String {
    format!("allowance/{}/{}/{}", hex_address(token), hex_address(owner), hex_address(spender))
}

pub fn attribute(name: String, value: Vec<u8>) -> tycho::Attribute {
    tycho::Attribute { name, value, change: tycho::ChangeType::Update.into() }
}

pub fn to_address(bytes: &[u8]) -> Option<Address> {
    match bytes.len() {
        20 => bytes.try_into().ok(),
        32 if bytes[..12].iter().all(|b| *b == 0) => bytes[12..].try_into().ok(),
        _ => None,
    }
}

/// An executor event naming a maker and a pair: the storage it may have written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Touch {
    pub mm: Address,
    pub token_a: Address,
    pub token_b: Address,
}

/// Executor events that write boards, anchors or the pause flag.
pub mod topics {
    pub const LADDER_COMMITTED: &str =
        "LadderCommitted(address,address,address,uint256,uint256,uint8)";
    pub const OFFSETS_COMMITTED: &str =
        "OffsetsCommitted(address,address,address,uint256,uint256,uint8,uint32)";
    pub const ANCHOR_COMMITTED: &str =
        "AnchorCommitted(address,address,address,uint256,uint256,uint256,uint256,uint256)";
    pub const CONTROLS_COMMITTED: &str =
        "ControlsCommitted(address,address,address,uint256,uint256,uint32,uint32,uint16)";
    pub const MM_FILL_EXECUTED: &str = "MMFillExecuted(address,address,address,address,address,uint256,uint256,uint256,uint128,address,uint256,uint256)";
    pub const MAKER_PAUSED: &str = "MakerPaused(address,bool)";
}

pub fn topic(signature: &str) -> Word {
    keccak(signature.as_bytes())
}

pub enum ExecutorEvent {
    /// A commit or fill on a maker's board for a direction or pair.
    Board(Touch),
    Paused(Address),
}

/// Decodes the executor events that write state. Others are ignored.
pub fn decode_executor_log(topics: &[Vec<u8>], data: &[u8]) -> Option<ExecutorEvent> {
    let first = topics.first()?.as_slice();
    let address_topic = |i: usize| {
        topics
            .get(i)
            .and_then(|t| to_address(t))
    };
    if first == topic(topics::LADDER_COMMITTED) ||
        first == topic(topics::OFFSETS_COMMITTED) ||
        first == topic(topics::CONTROLS_COMMITTED) ||
        first == topic(topics::ANCHOR_COMMITTED)
    {
        return Some(ExecutorEvent::Board(Touch {
            mm: address_topic(1)?,
            token_a: address_topic(2)?,
            token_b: address_topic(3)?,
        }));
    }
    if first == topic(topics::MM_FILL_EXECUTED) {
        // mmProvider, mmSigner and receiver are indexed; tokenIn and tokenOut lead the data.
        return Some(ExecutorEvent::Board(Touch {
            mm: address_topic(2)?,
            token_a: to_address(data.get(0..32)?)?,
            token_b: to_address(data.get(32..64)?)?,
        }));
    }
    if first == topic(topics::MAKER_PAUSED) {
        return address_topic(1).map(ExecutorEvent::Paused);
    }
    None
}

/// Every executor slot an event on `touch` may have written, with its attribute: both
/// directions' boards and the pair anchor.
pub fn touched_slots(touch: &Touch) -> Vec<(Word, String)> {
    let Touch { mm, token_a, token_b } = touch;
    let mut slots = Vec::with_capacity(2 * BOARD_WORDS as usize + 1);
    for (tin, tout) in [(token_a, token_b), (token_b, token_a)] {
        let base = board_slot(mm, tin, tout);
        for i in 0..BOARD_WORDS {
            slots.push((add(&base, i), board_attribute(mm, tin, tout, i)));
        }
    }
    let (t0, t1) = if token_a < token_b { (token_a, token_b) } else { (token_b, token_a) };
    slots.push((anchor_slot(mm, t0, t1), anchor_attribute(mm, t0, t1)));
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(hex_str: &str) -> Address {
        hex::decode(hex_str.trim_start_matches("0x"))
            .unwrap()
            .try_into()
            .unwrap()
    }

    #[test]
    fn board_slot_matches_solidity_mapping_layout() {
        // keccak(tokenOut . keccak(tokenIn . keccak(mm . 0))), checked with `cast index`.
        let mm = address("0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc");
        let weth = address("0x8b414ad7005eefd315af2a16538885eae229bab7");
        let usdc = address("0xabbdbbbd6d56593a9c5656c06cb30d61e4a544df");

        assert_eq!(hex::encode(board_slot(&mm, &weth, &usdc)), BOARD_SLOT_MM3_WETH_USDC);
    }

    // `cast index` of the keys above.
    const BOARD_SLOT_MM3_WETH_USDC: &str =
        "e158976d8c24996dab27c054429bdf514fb3242351849c9cd242736862ef7c51";

    #[test]
    fn token_slots_match_solidity_mapping_layout() {
        // `cast index address <holder> 9` and the nested form for allowances at slot 10.
        let holder = address("0x25e864b4903fd3eebbfd723b905241ff98893595");
        let vault = address("0x862ffacd6b13b4734bd8550c720f115f3f31e6ab");

        assert_eq!(hex::encode(balance_slot(&holder, 9)), BALANCE_SLOT_9);
        assert_eq!(hex::encode(allowance_slot(&vault, &holder, 10)), ALLOWANCE_SLOT_10);
    }

    const BALANCE_SLOT_9: &str = "9917b6e308075675a2ab72a7f97bb48b7392f98f9b0646fdf6f4a610dfb3ba7d";
    const ALLOWANCE_SLOT_10: &str =
        "90aaa7b352dccb8df1ea68ae1b890d55fa600a9a0ab724a6ae9eae71fa4c2778";

    #[test]
    fn venue_maker_slots_start_at_the_array_data() {
        let slots = venue_slots();
        assert_eq!(slots[0], (root(0), attrs::FEE_BPS.to_owned()));
        assert_eq!(slots[1], (root(1), attrs::MAKER_COUNT.to_owned()));
        // keccak(uint256(1)) is where `makerList` elements start.
        assert_eq!(
            hex::encode(slots[2].0),
            "b10e2d527612073b26eecdfd717e6a320cf44b4afac2b0732d9fcbe2b7fa0cf6"
        );
        assert_eq!(slots[3].1, "maker/1");
    }

    #[test]
    fn add_carries_across_bytes() {
        let mut word = [0u8; 32];
        word[31] = 0xff;
        let next = add(&word, 1);
        assert_eq!(next[30], 1);
        assert_eq!(next[31], 0);
    }
}
