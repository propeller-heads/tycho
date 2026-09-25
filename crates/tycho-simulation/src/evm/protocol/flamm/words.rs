// Copyright (c) 2026 Everlong Labs Limited

//! The storage words a FLAMM component carries as attributes, as the `base-flamm` substreams
//! emits them (`protocols/substreams/base-flamm`, README "Attributes"): the slot keys of every
//! word the quote reads, derived here exactly as Solidity derives them, and the packed-field
//! reads that turn a 32-byte word into the values `FLAMMStore.S` (`src/core/flamm/FLAMMStore.sol`),
//! `EverlongHook`, `LeverageSpreadHook`, `MMRouterLib.PoolRecord`, `MorphoBlueAccount`,
//! `PriceFeed`, `FLAMMFactory`, Morpho Blue and the `AdaptiveCurveIrm` store in it.
//!
//! Attribute names are `<role>:0x<64 hex digits of the slot key>` for the FLAMM-owned roles
//! (`pool`, `hook`, `spread`, `router`, `account`, `pricefeed`, `factory`), `mm:<v>:market:<k>` /
//! `mm:<v>:position:<k>` / `irm:<v>:rate_at_target` for venue `<v>`'s Morpho words and
//! `feed:<f>:<name>` for the four Chainlink feeds ([`super::feeds`]). A FLAMM-owned word that is
//! absent was never written (the contracts are tracked from their creation, and a write of zero
//! to a zero slot is no storage change) and reads as zero, except the words the pinned code
//! writes non-zero when it constructs the contract, which the decoder requires to be present
//! ([`Words::required_owned`]): the `EverlongHook` `Params` and `Tuning` rows (slots 0, 4, 5, 6),
//! its support, anchor, reservation price and book (10-18 and 20), the `LeverageSpreadHook`'s
//! only word, the `PriceFeed` token pair of each registered token, `FLAMMStore`'s configuration
//! rows and the share supply. An absent Morpho, IRM or feed word is unknown and the decoder
//! refuses to quote.

use std::collections::BTreeMap;

use alloy::primitives::{keccak256, Address, B256, U256};
use tycho_common::Bytes;

/// `FLAMMStore`'s ERC-7201 namespace (`FLAMMStore.sol:315`): `keccak256(abi.encode(uint256(
/// keccak256("everlong.storage.FLAMM")) - 1)) & ~bytes32(uint256(0xff))`.
pub const FLAMM_NS: U256 = U256::from_be_bytes(hex_literal::hex!(
    "5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f4500"
));
/// OpenZeppelin's `openzeppelin.storage.ERC20` namespace (`ERC20Upgradeable.sol:37` holds
/// `_totalSupply` at `+2`).
pub const ERC20_NS: U256 = U256::from_be_bytes(hex_literal::hex!(
    "52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace00"
));

/// Morpho Blue v1.0.0 storage slots (`Morpho.sol`): `position` is slot 2, `market` slot 3.
pub const MORPHO_POSITION_SLOT: u64 = 2;
pub const MORPHO_MARKET_SLOT: u64 = 3;

/// The attribute map of one component, keyed by attribute name.
pub type Attributes = BTreeMap<String, Bytes>;

/// Why a word could not be read: absent where the value is needed, or malformed. Those are the
/// first two cases of the decoder's own [`DecodeError`](super::decoder::DecodeError), and a
/// word read is refused as nothing else, so this module reports them in that type instead of
/// in a second enum a `From` impl has to keep in step with it.
pub use super::decoder::DecodeError as WordError;

/// `<role>:0x<slot key>`, the attribute name of a raw storage word.
pub fn word_name(role: &str, slot: U256) -> String {
    format!("{role}:0x{slot:064x}")
}

/// A Solidity mapping entry's slot: `keccak256(abi.encode(key, slot))`.
pub fn mapping_slot(key: B256, slot: U256) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(key.as_slice());
    buf[32..].copy_from_slice(&slot.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

/// A mapping keyed by an address (left-padded to 32 bytes, as `abi.encode` pads it).
pub fn address_mapping_slot(key: Address, slot: U256) -> U256 {
    mapping_slot(key.into_word(), slot)
}

/// A dynamic array's data base: `keccak256(abi.encode(slot))`.
pub fn array_base(slot: U256) -> U256 {
    U256::from_be_bytes(keccak256(slot.to_be_bytes::<32>()).0)
}

/// A packed field of a storage word: `len` bytes at `byte_off` from the low end, as Solidity packs
/// value types.
pub fn field(w: U256, byte_off: usize, len: usize) -> U256 {
    debug_assert!(byte_off + len <= 32);
    let shifted = w >> (8 * byte_off);
    if len >= 32 {
        return shifted;
    }
    shifted & ((U256::from(1u8) << (8 * len)) - U256::from(1u8))
}

/// A packed `address`.
pub fn field_addr(w: U256, byte_off: usize) -> Address {
    Address::from_slice(&field(w, byte_off, 20).to_be_bytes::<32>()[12..])
}

/// A packed `bool`: any non-zero byte is true (Solidity only ever writes 0 or 1).
pub fn field_bool(w: U256, byte_off: usize) -> bool {
    !field(w, byte_off, 1).is_zero()
}

/// A packed `uint64` (or narrower) as `u64`.
pub fn field_u64(w: U256, byte_off: usize, len: usize) -> u64 {
    debug_assert!(len <= 8);
    field(w, byte_off, len).to::<u64>()
}

/// An attribute value as a 32-byte word: at most 32 bytes, big-endian (the substreams emits the
/// 32 bytes `eth_getStorageAt` returns; a shorter value is left-padded).
pub fn word_of(name: &str, v: &Bytes) -> Result<U256, WordError> {
    if v.len() > 32 {
        return Err(WordError::Malformed(format!("{name}: {} bytes, a word has 32", v.len())));
    }
    Ok(U256::from_be_slice(v))
}

/// An attribute value as an address: 20 bytes, or a 32-byte word whose high 12 bytes are zero.
pub fn address_of(name: &str, v: &Bytes) -> Result<Address, WordError> {
    match v.len() {
        20 => Ok(Address::from_slice(v)),
        32 if v[..12].iter().all(|b| *b == 0) => Ok(Address::from_slice(&v[12..])),
        n => Err(WordError::Malformed(format!("{name}: {n} bytes, an address has 20"))),
    }
}

/// An attribute value as a 32-byte hash (exactly 32 bytes).
pub fn hash_of(name: &str, v: &Bytes) -> Result<B256, WordError> {
    if v.len() != 32 {
        return Err(WordError::Malformed(format!("{name}: {} bytes, a hash has 32", v.len())));
    }
    Ok(B256::from_slice(v))
}

/// The words of one component, read by role and slot key.
pub struct Words<'a> {
    attrs: &'a Attributes,
}

impl<'a> Words<'a> {
    pub fn new(attrs: &'a Attributes) -> Self {
        Self { attrs }
    }

    /// A named attribute as a word, `None` when absent.
    pub fn get_word(&self, name: &str) -> Result<Option<U256>, WordError> {
        match self.attrs.get(name) {
            Some(v) => Ok(Some(word_of(name, v)?)),
            None => Ok(None),
        }
    }

    /// A named attribute that must be present, as a word.
    pub fn required_word(&self, name: &str) -> Result<U256, WordError> {
        self.get_word(name)?
            .ok_or_else(|| WordError::Missing(name.to_owned()))
    }

    /// A FLAMM-owned storage word (`<role>:<slot>`): zero when absent, which is a word the
    /// tracked contract never wrote.
    pub fn owned(&self, role: &str, slot: U256) -> Result<U256, WordError> {
        Ok(self
            .get_word(&word_name(role, slot))?
            .unwrap_or(U256::ZERO))
    }

    /// A FLAMM-owned storage word the pinned code writes non-zero when it constructs the
    /// contract, so that it is in the stream from the component's creation on and its absence
    /// is a lost word, not a zero: `Missing` rather than zero, because zero would decode into a
    /// state that quotes a different amount instead of refusing.
    ///
    /// The guard is on presence, not on value: a present word is decoded whatever it holds,
    /// including 32 zero bytes or an empty `0x`. It is therefore a construction-time
    /// completeness check over the words [`super::decoder::decode_core`] marks required, not a
    /// general lost-word detector — the words the constructor leaves for the first fill or the
    /// first observation (`EverlongHook`'s `idleStable`, `idleVolatile` and `rvWad`) are read
    /// through [`Words::owned`] and a lost one of those still decodes.
    pub fn required_owned(&self, role: &str, slot: U256) -> Result<U256, WordError> {
        self.required_word(&word_name(role, slot))
    }
}

/// `MMRouterLib.PoolRecord` of `pool` in the Router's `_pools` mapping (`MMRouter.sol`, mapping
/// slot 1): `keccak256(abi.encode(pool, 1))`.
pub fn router_record_slot(pool: Address) -> U256 {
    address_mapping_slot(pool, U256::from(1u8))
}

/// `MorphoBlueAccount._markets[id]` (`MorphoBlueAccount.sol:63`, mapping slot 5).
pub fn account_market_slot(id: B256) -> U256 {
    mapping_slot(id, U256::from(5u8))
}

/// `PriceFeed._tokens[token]` (`PriceFeed.sol:14`, mapping slot 0).
pub fn pricefeed_token_slot(token: Address) -> U256 {
    address_mapping_slot(token, U256::ZERO)
}

/// `FLAMMFactory.isPool[pool]` (`FLAMMFactory.sol:59`, mapping slot 4).
pub fn factory_is_pool_slot(pool: Address) -> U256 {
    address_mapping_slot(pool, U256::from(4u8))
}

/// Morpho Blue `market[id]` base slot (`Morpho.sol`, mapping slot 3): three words follow.
pub fn morpho_market_slot(id: B256) -> U256 {
    mapping_slot(id, U256::from(MORPHO_MARKET_SLOT))
}

/// Morpho Blue `position[id][account]` base slot (`Morpho.sol`, mapping slot 2): two words.
pub fn morpho_position_slot(id: B256, account: Address) -> U256 {
    let inner = mapping_slot(id, U256::from(MORPHO_POSITION_SLOT));
    address_mapping_slot(account, inner)
}

/// `AdaptiveCurveIrm.rateAtTarget[id]` (`AdaptiveCurveIrm.sol`, mapping slot 0).
pub fn irm_rate_slot(id: B256) -> U256 {
    mapping_slot(id, U256::ZERO)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn erc7201(id: &str) -> U256 {
        let inner = U256::from_be_bytes(keccak256(id.as_bytes()).0) - U256::from(1u8);
        let outer = U256::from_be_bytes(keccak256(inner.to_be_bytes::<32>()).0);
        outer & !U256::from(0xffu8)
    }

    #[test]
    fn namespaces_are_the_erc7201_derivations() {
        assert_eq!(erc7201("everlong.storage.FLAMM"), FLAMM_NS);
        assert_eq!(erc7201("openzeppelin.storage.ERC20"), ERC20_NS);
    }

    #[test]
    fn schema_slot_keys_reproduce() {
        // schema/SCHEMA.md section 2: the c104 pool's derived keys.
        let pool = Address::from_str("0xc0fdCB1799cCc2CEBaA1fe247157b0dF33D57572").unwrap();
        let rec = router_record_slot(pool);
        assert_eq!(
            word_name("router", rec),
            "router:0x627459f28fd627023883d9310c65240762faa343d3f2429d1746640d8d8a0574"
        );
        assert_eq!(
            word_name("router", array_base(rec + U256::from(4u8))),
            "router:0x0e05292437d38cd75116d8f770eb87608d908595ff06eb5903099b996d076108"
        );
        assert_eq!(
            word_name("router", array_base(rec + U256::from(2u8))),
            "router:0x86a9bd383d29db7a1cff9d9758922f2534f0b6481acf587fe12a79b5d6f72f15"
        );
        assert_eq!(
            word_name("router", array_base(rec + U256::from(3u8))),
            "router:0x001189082010b9dff4cf86574fcb5fe6ac33a2e918767f233fca4e67dd4bba1c"
        );
        assert_eq!(
            word_name("pool", array_base(FLAMM_NS + U256::from(11u8))),
            "pool:0xe27b86aa3e64fe0cf7c9294fb8b6fb20a28e5f01ba99e4bca9e76b647cc44f23"
        );
        assert_eq!(
            word_name("pool", ERC20_NS + U256::from(2u8)),
            "pool:0x52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace02"
        );
        let id =
            B256::from_str("0x9103c3b4e834476c9a62ea009ba2c884ee42e94e6e314a26f04d312434191836")
                .unwrap();
        assert_eq!(
            word_name("account", account_market_slot(id)),
            "account:0x7f73fe763fd70629cadd63d534e4c70682776b4eaeffdf39178b56c0a1bffde4"
        );
        let cbbtc = Address::from_str("0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf").unwrap();
        let usdc = Address::from_str("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913").unwrap();
        assert_eq!(
            word_name("pricefeed", pricefeed_token_slot(cbbtc)),
            "pricefeed:0x1df6378d90dbe801fca9d47d5375a5a229ffa4eb34516b72a9e9ff9483681050"
        );
        assert_eq!(
            word_name("pricefeed", pricefeed_token_slot(usdc)),
            "pricefeed:0x167d7ad8ce5bbf928e114a13d4a925d29e6437f0d5be246a7858d666db460b9d"
        );
        assert_eq!(
            word_name("factory", factory_is_pool_slot(pool)),
            "factory:0xf67576777f99137ee577c518af5f53b3235ac7369b0be96b8b092f51a7007c6a"
        );
        assert_eq!(
            format!("0x{:064x}", morpho_market_slot(id)),
            "0xb37d8d77c527a1e411d2abd81f103dee202b9a350a1fcbf567227a9222316a6a"
        );
        let account = Address::from_str("0x6760E3b032eE2d670Cb684d9076b8f48cb066c48").unwrap();
        assert_eq!(
            format!("0x{:064x}", morpho_position_slot(id, account)),
            "0x1ca893d18673e8d37ef3632fa9ba2c4b035a7e69714e0db5d1d2587da8f1fce9"
        );
        assert_eq!(
            format!("0x{:064x}", irm_rate_slot(id)),
            "0xe6f1c64c0bda05fd8d1c7bdb2840489820c4a2a4313f38c6e61dc429ea02ee12"
        );
    }

    #[test]
    fn packed_fields_read_from_the_low_end() {
        // FLAMM_NS+13 at 51302915: phiWad 1e18 @0, phiMin @8, phiMax @16, ltvWad 0.55e18 @24.
        let w = U256::from_str_radix(
            "07a1fe16027700000de0b6b3a764000006f05b59d3b200000de0b6b3a7640000",
            16,
        )
        .unwrap();
        assert_eq!(field_u64(w, 0, 8), 1_000_000_000_000_000_000);
        assert_eq!(field_u64(w, 16, 8), 1_000_000_000_000_000_000);
        assert_eq!(field_u64(w, 8, 8), 500_000_000_000_000_000);
        assert_eq!(field_u64(w, 24, 8), 550_000_000_000_000_000);
        // FLAMM_NS+10: controllerHook @0, initialized @20, bootstrapped @21, paused @22, levPaused
        // @23.
        let w = U256::from_str_radix(
            "00000000000000000100010165cbd227cbc61248ae77a5fc813a29c54c092134",
            16,
        )
        .unwrap();
        assert_eq!(
            field_addr(w, 0),
            Address::from_str("0x65CBD227cBC61248ae77a5fC813A29C54C092134").unwrap()
        );
        assert!(field_bool(w, 20));
        assert!(field_bool(w, 21));
        assert!(!field_bool(w, 22));
        assert!(field_bool(w, 23));
        assert_eq!(field(w, 0, 32), w);
    }

    #[test]
    fn values_decode_by_width() {
        let b = Bytes::from(vec![0u8; 20]);
        assert_eq!(address_of("a", &b), Ok(Address::ZERO));
        let mut v = vec![0u8; 32];
        v[31] = 7;
        assert_eq!(word_of("w", &Bytes::from(v.clone())), Ok(U256::from(7u8)));
        assert_eq!(address_of("a", &Bytes::from(v.clone())), Ok(Address::with_last_byte(7)));
        v[0] = 1;
        assert!(matches!(address_of("a", &Bytes::from(v)), Err(WordError::Malformed(_))));
        assert_eq!(word_of("w", &Bytes::from(vec![9u8])), Ok(U256::from(9u8)));
        assert!(matches!(word_of("w", &Bytes::from(vec![0u8; 33])), Err(WordError::Malformed(_))));
        assert!(matches!(hash_of("h", &Bytes::from(vec![0u8; 31])), Err(WordError::Malformed(_))));
        let attrs = Attributes::new();
        let w = Words::new(&attrs);
        assert_eq!(w.owned("pool", FLAMM_NS), Ok(U256::ZERO));
        assert_eq!(
            w.required_owned("hook", U256::from(15u8)),
            Err(WordError::Missing(
                "hook:0x000000000000000000000000000000000000000000000000000000000000000f".into()
            ))
        );
        assert_eq!(
            w.required_word("mm:0:market:0"),
            Err(WordError::Missing("mm:0:market:0".into()))
        );
    }
}
