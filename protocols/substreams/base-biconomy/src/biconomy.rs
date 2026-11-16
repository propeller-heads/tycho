//! Storage layout and events of the Biconomy PropAMM executor and venue, and the component
//! attributes `tycho-simulation`'s `biconomy` state decodes.
//!
//! The executor stores every board in `boards[mm][tokenIn][tokenOut]` (slot 0, 24 words per
//! board), every pair anchor in `anchors[mm][token0][token1]` (slot 1) and the self-pause flag in
//! `makerPaused[mm]` (slot 2). Every write to them comes with an event naming the maker and the
//! pair, which is how a storage change is matched to the board it belongs to.
//!
//! The venue keeps `feeBps` in the low 16 bits of slot 0 and `makerList` as a dynamic array at
//! slot 1. Provider inventory comes from the configured tokens' balance and allowance mappings,
//! and whether a provider serves the venue from its `signer` and `approvedExecutor` slots and, for
//! a vault provider, the vault's `router()`. Nothing is read over RPC.

use tiny_keccak::{Hasher, Keccak};
use tycho_substreams::prelude as tycho;

pub type Address = [u8; 20];
pub type Word = [u8; 32];

pub const PROTOCOL_TYPE_NAME: &str = "biconomy_venue";
pub const BOARD_WORDS: u8 = 24;
/// Makers the venue's list is tracked for: the venue caps `makerList` at 16 (`MAX_MAKERS` in
/// `PropAMMVenue.sol`). Their element slots are precomputed once per block.
pub const MAX_MAKERS: u8 = 16;

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

/// A 20-byte address as the 32-byte storage word holding it.
pub fn address_word(address: &Address) -> Vec<u8> {
    let mut word = vec![0u8; 32];
    word[12..].copy_from_slice(address);
    word
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

/// Solady `ERC20`'s `_BALANCE_SLOT_SEED`.
const SOLADY_BALANCE_SLOT_SEED: [u8; 4] = [0x87, 0xa2, 0x11, 0xa2];
/// Solady `ERC20`'s `_ALLOWANCE_SLOT_SEED`.
const SOLADY_ALLOWANCE_SLOT_SEED: [u8; 4] = [0x7f, 0x5e, 0x9f, 0x20];

/// `balanceOf[holder]` for Solady's `ERC20`: keccak256(holder . uint96(_BALANCE_SLOT_SEED)).
pub fn solady_balance_slot(holder: &Address) -> Word {
    let mut buf = [0u8; 32];
    buf[..20].copy_from_slice(holder);
    buf[28..].copy_from_slice(&SOLADY_BALANCE_SLOT_SEED);
    keccak(&buf)
}

/// Root of the `base.b20` ERC-7201 namespace that B20 tokens (Base's node-native token standard,
/// e.g. the Coinbase tokenized stocks) keep their core storage in:
/// keccak256(abi.encode(uint256(keccak256("base.b20")) - 1)) & ~bytes32(uint256(0xff)).
/// `balances` is the mapping at offset 4 and `allowances` the one at offset 5.
const B20_ROOT: Word = [
    0xc7, 0x8b, 0x71, 0xfe, 0xe7, 0x95, 0xdd, 0xd7, 0x4a, 0xff, 0x64, 0xea, 0x9b, 0x24, 0x74, 0x19,
    0x4c, 0x93, 0x8c, 0x31, 0x96, 0x43, 0x0e, 0x10, 0xbb, 0x5f, 0x01, 0xed, 0x48, 0x43, 0x40, 0x00,
];

/// `B20_ROOT` plus `offset`, which only touches the low byte the namespace mask cleared.
fn b20_slot(offset: u8) -> Word {
    let mut slot = B20_ROOT;
    slot[31] = offset;
    slot
}

/// `balanceOf[holder]` for a B20 token: the `balances` mapping at `base.b20` offset 4.
pub fn b20_balance_slot(holder: &Address) -> Word {
    mapping_slot(holder, &b20_slot(4))
}

/// `allowance[owner][spender]` for a B20 token: the `allowances` mapping at `base.b20` offset 5.
pub fn b20_allowance_slot(owner: &Address, spender: &Address) -> Word {
    mapping_slot(spender, &mapping_slot(owner, &b20_slot(5)))
}

/// `allowance[owner][spender]` for Solady's `ERC20`:
/// keccak256(owner . uint96(_ALLOWANCE_SLOT_SEED) . spender).
pub fn solady_allowance_slot(owner: &Address, spender: &Address) -> Word {
    let mut buf = [0u8; 52];
    buf[..20].copy_from_slice(owner);
    buf[28..32].copy_from_slice(&SOLADY_ALLOWANCE_SLOT_SEED);
    buf[32..].copy_from_slice(spender);
    keccak(&buf)
}

/// The length slot of a dynamic array at `slot` and the slot of its first element.
pub fn array_slots(slot: u8) -> (Word, Word) {
    (root(slot), keccak(&root(slot)))
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

pub fn signer_attribute(provider: &Address) -> String {
    format!("signer/{}", hex_address(provider))
}

pub fn approved_executor_attribute(provider: &Address) -> String {
    format!("approved_executor/{}", hex_address(provider))
}

pub fn router_count_attribute(vault: &Address) -> String {
    format!("router_count/{}", hex_address(vault))
}

pub fn router_attribute(vault: &Address) -> String {
    format!("router/{}", hex_address(vault))
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

    #[test]
    fn b20_root_is_the_erc7201_namespace() {
        let mut id = keccak(b"base.b20");
        // uint256(keccak256("base.b20")) - 1
        let mut i = 31;
        loop {
            let (value, borrow) = id[i].overflowing_sub(1);
            id[i] = value;
            if !borrow {
                break;
            }
            i -= 1;
        }
        let mut root = keccak(&id);
        root[31] = 0;
        assert_eq!(root, B20_ROOT);
    }

    #[test]
    fn b20_slots_match_nvdac_on_base() {
        // Read on Base from NVDAc 0xb20000000000000000000078ee7ce2fe4908108c: `balanceOf` of
        // 0x4446…a5ef loads the first slot, and `allowance(0x4d6a…6ca6, 0x4857…c60b)` reads the
        // second (prestate trace and `eth_getStorageAt` against `allowance`).
        let holder = address("0x4446adc0b8136ffc55ddb7a488ba5509ace2a5ef");
        let owner = address("0x4d6ad10afb81a14268e632dd15c30c28b9266ca6");
        let spender = address("0x4857db576ee0e45f10525ec59527bfd10beec60b");
        assert_eq!(
            hex::encode(b20_balance_slot(&holder)),
            "7d89f3ea355dcd1268991cff176e733dab5fa1578fc4ca8eb82dca61caad04ea"
        );
        assert_eq!(
            hex::encode(b20_allowance_slot(&owner, &spender)),
            "89ec070e7d5441faa9115dd1461a1467a764f2ca1980761182ef4b16d67309b6"
        );
    }

    #[test]
    fn solady_slots_match_aero_on_base() {
        // Read on Base from AERO 0x3f5bf3edac59d650d8fdf9657a71f2f904a3a4ee (Solady ERC20):
        // `balanceOf` of 0x865d…b1d3 loads the first, and `allowance(vault, provider)` is
        // keccak256(vault . uint96(seed) . provider).
        let holder = address("0x865d8cab29e4003b9b512e2121fdc1660d66b1d3");
        let vault = address("0x862ffacd6b13b4734bd8550c720f115f3f31e6ab");
        let provider = address("0x25e864b4903fd3eebbfd723b905241ff98893595");
        assert_eq!(
            hex::encode(solady_balance_slot(&holder)),
            "4fecda5ab456cad623dc2c466c63279b3eddcf2ada8101f0050c7300d8c7e5e4"
        );
        assert_eq!(
            hex::encode(solady_allowance_slot(&vault, &provider)),
            "bae7d51fb55e91efbfe5ad479b5941f70a49699b0342ca09df25bd6f7970530c"
        );
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
