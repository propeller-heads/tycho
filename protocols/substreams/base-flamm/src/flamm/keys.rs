// Copyright (c) 2026 Everlong Labs Limited
//! Storage-slot derivations for every contract the package tracks.
//!
//! The only layout knowledge the substreams carries: where each contract keeps the words the
//! simulator reads. Everything is a pure function of addresses, market ids and the pinned ERC-7201
//! namespace, so a word is always verifiable with one `eth_getStorageAt`. The Rust decoder
//! (tycho-simulation) unpacks the words; this module only knows the keys.
use keccak_hash::keccak;

pub type Word = [u8; 32];
pub type Address = [u8; 20];

/// `FLAMMStore.sol:315`: `keccak256(abi.encode(uint256(keccak256("everlong.storage.FLAMM")) - 1)) &
/// ~0xff`.
pub const FLAMM_NS: Word =
    hex_literal("5b7e76949cacd5346234367c3806fe494a22f183af782d834d5fc4ee5b0f4500");
/// OpenZeppelin `ERC20Upgradeable.sol:37` namespace `openzeppelin.storage.ERC20`; `_totalSupply` is
/// word +2.
pub const ERC20_NS: Word =
    hex_literal("52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace00");

/// Words of the pool namespace forwarded as `pool:<slot>`: `FLAMMStore.S` spans `FLAMM_NS+0..+33`
/// (`FLAMMStore.sol:246-311`); the range is forwarded whole so a field the schema does not list
/// still reaches the decoder.
pub const POOL_NAMESPACE_WORDS: u64 = 34;
/// `FLAMMStore.S.loans` (`FLAMMStore.sol:265`): six words per `LoanCfg` (`:209-216`) from
/// `keccak256(FLAMM_NS+11)`; forwarded for up to this many loans.
pub const POOL_LOAN_WORDS: u64 = 6;
pub const MAX_POOL_LOANS: u64 = 8;
/// `EverlongHook` keeps every quote input in fixed slots 0..25 (`EverlongHook.sol` state, schema
/// 2.2).
pub const HOOK_SLOTS: u64 = 32;
/// `LeverageSpreadHook` packs its state into slot 0 (`LeverageSpreadHook.sol:30-35`).
pub const SPREAD_SLOTS: u64 = 2;
/// `MorphoBlueAccount` slots 0..4 are the bound addresses, 5 the `_markets` mapping base, 6
/// `_armedContext` (`MorphoBlueAccount.sol:58-64`).
pub const ACCOUNT_SLOTS: u64 = 7;
pub const ACCOUNT_MARKETS_BASE: u64 = 5;
/// `MMRouterLib.PoolRecord` (`MMRouterLib.sol:66-77`): eight head words from `keccak256(pool . 1)`,
/// then the `loans` (7 words each), `venues` (6 words each) and four `uint16[]` order arrays.
pub const ROUTER_RECORD_WORDS: u64 = 8;
pub const ROUTER_LOAN_WORDS: u64 = 7;
pub const MAX_ROUTER_LOANS: u64 = 8;
pub const ROUTER_VENUE_WORDS: u64 = 6;
pub const MAX_ROUTER_VENUES: u64 = 16;
pub const ROUTER_ORDERS: u64 = 4;
/// 16 `uint16` per data word; more than 16 venues in one order adds words.
pub const MAX_ORDER_WORDS: u64 = 4;
/// `FLAMMFactory.sol:52-59`: `_implementation` 0,
/// `pendingImplementation|implementationExecutableAt` 1, `pendingImplementationCodehash` 2,
/// `_pools` 3, `isPool` 4.
pub const FACTORY_SLOTS: u64 = 3;
pub const FACTORY_ISPOOL_BASE: u64 = 4;
/// `PriceFeed.sol:14-18`: `_tokens` mapping at slot 0, two words per token.
pub const PRICEFEED_TOKENS_BASE: u64 = 0;
/// Morpho Blue v1.0.0: `position` 2, `market` 3 (`Morpho.sol` state).
pub const MORPHO_POSITION_BASE: u64 = 2;
pub const MORPHO_MARKET_BASE: u64 = 3;
/// `AdaptiveCurveIrm.rateAtTarget` mapping at slot 0.
pub const IRM_RATE_BASE: u64 = 0;
/// `EACAggregatorProxy` (v0.6): `currentPhase` slot 2 (`uint16 id | address aggregator @2`),
/// `accessController` slot 5.
pub const PROXY_PHASE_SLOT: u64 = 2;
pub const PROXY_ACCESS_CONTROLLER_SLOT: u64 = 5;

const fn hex_literal(s: &str) -> Word {
    let bytes = s.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = (nibble(bytes[2 * i]) << 4) | nibble(bytes[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("not a hex digit"),
    }
}

pub fn word_from_u64(n: u64) -> Word {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&n.to_be_bytes());
    w
}

pub fn word_from_address(a: &[u8]) -> Word {
    let mut w = [0u8; 32];
    let take = a.len().min(20);
    w[32 - take..].copy_from_slice(&a[a.len() - take..]);
    w
}

/// Big-endian `slot + n` (wrapping, as the EVM does).
pub fn add(slot: &Word, n: u64) -> Word {
    let mut out = *slot;
    let mut carry = n as u128;
    let mut i = 32;
    while carry != 0 && i > 0 {
        i -= 1;
        let sum = out[i] as u128 + (carry & 0xff);
        out[i] = (sum & 0xff) as u8;
        carry = (carry >> 8) + (sum >> 8);
    }
    out
}

/// Solidity mapping slot `keccak256(key . base)`, key left-padded to 32 bytes.
pub fn map_slot(key: &Word, base: &Word) -> Word {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(key);
    buf[32..].copy_from_slice(base);
    keccak(buf).0
}

/// Dynamic array data base `keccak256(slot)`.
pub fn array_base(slot: &Word) -> Word {
    keccak(slot).0
}

pub fn keccak256(data: &[u8]) -> Word {
    keccak(data).0
}

/// A slot number below 2^64, as a word.
pub fn slot(n: u64) -> Word {
    word_from_u64(n)
}

/// The pool words: the namespace range, the loans array and the ERC-20 total supply.
pub fn pool_keys() -> Vec<Word> {
    let mut keys: Vec<Word> = (0..POOL_NAMESPACE_WORDS)
        .map(|i| add(&FLAMM_NS, i))
        .collect();
    let loans = array_base(&add(&FLAMM_NS, 11));
    keys.extend((0..POOL_LOAN_WORDS * MAX_POOL_LOANS).map(|i| add(&loans, i)));
    keys.push(add(&ERC20_NS, 2));
    keys
}

pub fn hook_keys() -> Vec<Word> {
    (0..HOOK_SLOTS).map(slot).collect()
}

pub fn spread_keys() -> Vec<Word> {
    (0..SPREAD_SLOTS).map(slot).collect()
}

pub fn account_keys(market_ids: &[Word]) -> Vec<Word> {
    let mut keys: Vec<Word> = (0..ACCOUNT_SLOTS).map(slot).collect();
    for id in market_ids {
        let base = map_slot(id, &slot(ACCOUNT_MARKETS_BASE));
        keys.push(base);
        keys.push(add(&base, 1));
    }
    keys
}

pub fn pricefeed_keys(tokens: &[Address]) -> Vec<Word> {
    let mut keys = Vec::with_capacity(2 * tokens.len());
    for token in tokens {
        let base = map_slot(&word_from_address(token), &slot(PRICEFEED_TOKENS_BASE));
        keys.push(base);
        keys.push(add(&base, 1));
    }
    keys
}

pub fn factory_keys(pool: &Address) -> Vec<Word> {
    let mut keys: Vec<Word> = (0..FACTORY_SLOTS).map(slot).collect();
    keys.push(map_slot(&word_from_address(pool), &slot(FACTORY_ISPOOL_BASE)));
    keys
}

/// `keccak256(pool . 1)`: the pool's `PoolRecord` in `MMRouter.pools` (`MMRouter.sol` slot 1).
pub fn router_record(pool: &Address) -> Word {
    map_slot(&word_from_address(pool), &slot(1))
}

pub fn router_keys(pool: &Address) -> Vec<Word> {
    let record = router_record(pool);
    let mut keys = vec![slot(0)];
    keys.extend((0..ROUTER_RECORD_WORDS).map(|i| add(&record, i)));
    let loans = array_base(&add(&record, 2));
    keys.extend((0..ROUTER_LOAN_WORDS * MAX_ROUTER_LOANS).map(|i| add(&loans, i)));
    let venues = array_base(&add(&record, 3));
    keys.extend((0..ROUTER_VENUE_WORDS * MAX_ROUTER_VENUES).map(|i| add(&venues, i)));
    for j in 0..ROUTER_ORDERS {
        let order = array_base(&add(&record, 4 + j));
        keys.extend((0..MAX_ORDER_WORDS).map(|k| add(&order, k)));
    }
    keys
}

/// `venues[i]` head word of the pool's router record.
pub fn router_venue(pool: &Address, venue: u64) -> Word {
    let venues = array_base(&add(&router_record(pool), 3));
    add(&venues, ROUTER_VENUE_WORDS * venue)
}

/// `loans[i]` head word of the pool's router record.
pub fn router_loan(pool: &Address, loan: u64) -> Word {
    let loans = array_base(&add(&router_record(pool), 2));
    add(&loans, ROUTER_LOAN_WORDS * loan)
}

/// `FLAMMStore.S.loans[i]` head word.
pub fn pool_loan(loan: u64) -> Word {
    add(&array_base(&add(&FLAMM_NS, 11)), POOL_LOAN_WORDS * loan)
}

/// Morpho `market[id]` words +0..+2 (`totalSupplyAssets|totalSupplyShares`,
/// `totalBorrowAssets|totalBorrowShares`, `lastUpdate|fee`).
pub fn morpho_market_keys(market: &Word) -> [Word; 3] {
    let base = map_slot(market, &slot(MORPHO_MARKET_BASE));
    [base, add(&base, 1), add(&base, 2)]
}

/// Morpho `position[id][account]` words +0 (`supplyShares`) and +1 (`borrowShares|collateral`).
pub fn morpho_position_keys(market: &Word, account: &Address) -> [Word; 2] {
    let inner = map_slot(market, &slot(MORPHO_POSITION_BASE));
    let base = map_slot(&word_from_address(account), &inner);
    [base, add(&base, 1)]
}

/// `AdaptiveCurveIrm.rateAtTarget[id]`.
pub fn irm_rate_key(market: &Word) -> Word {
    map_slot(market, &slot(IRM_RATE_BASE))
}

/// `_markets[id]` of a `MorphoBlueAccount` (`MorphoBlueAccount.sol:63`), words +0 (`oracle|lltv`)
/// and +1 (`irm`).
pub fn account_market_keys(market: &Word) -> [Word; 2] {
    let base = map_slot(market, &slot(ACCOUNT_MARKETS_BASE));
    [base, add(&base, 1)]
}

/// `PriceFeed._tokens[token]` words +0 (`aggregator|heartbeat|scale`) and +1 (`unit|pegBandWad`).
pub fn pricefeed_token_keys(token: &Address) -> [Word; 2] {
    let base = map_slot(&word_from_address(token), &slot(PRICEFEED_TOKENS_BASE));
    [base, add(&base, 1)]
}

pub fn factory_is_pool_key(pool: &Address) -> Word {
    map_slot(&word_from_address(pool), &slot(FACTORY_ISPOOL_BASE))
}

/// `s_accessList[proxy]` of a `SimpleWriteAccessController` with the mapping at `base`.
pub fn access_list_key(proxy: &Address, base: u64) -> Word {
    map_slot(&word_from_address(proxy), &slot(base))
}

/// `s_transmissions[round]` of an OCR2 / dual aggregator with the mapping at `base`.
pub fn transmission_key(round: u32, base: u64) -> Word {
    map_slot(&word_from_u64(round as u64), &slot(base))
}

pub fn hex_word(w: &Word) -> String {
    format!("0x{}", hex::encode(w))
}

pub fn hex_address(a: &[u8]) -> String {
    format!("0x{}", hex::encode(a))
}

pub fn parse_word(s: &str) -> anyhow::Result<Word> {
    let raw = hex::decode(s.strip_prefix("0x").unwrap_or(s))?;
    if raw.len() > 32 {
        anyhow::bail!("`{s}` is longer than 32 bytes");
    }
    let mut w = [0u8; 32];
    w[32 - raw.len()..].copy_from_slice(&raw);
    Ok(w)
}

pub fn parse_address(s: &str) -> anyhow::Result<Address> {
    let raw = hex::decode(s.strip_prefix("0x").unwrap_or(s))?;
    raw.as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("`{s}` is not a 20-byte address"))
}

/// The 20-byte address packed in the low bytes of a word.
pub fn address_in_word(w: &[u8]) -> Address {
    let mut a = [0u8; 20];
    let take = w.len().min(20);
    a[20 - take..].copy_from_slice(&w[w.len() - take..]);
    a
}

/// A packed field: `byte_offset` from the low end, `width` bytes, as in Solidity packing.
pub fn field(w: &Word, byte_offset: usize, width: usize) -> u128 {
    let end = 32 - byte_offset;
    let start = end - width;
    let mut v: u128 = 0;
    for b in &w[start..end] {
        v = (v << 8) | *b as u128;
    }
    v
}

pub fn is_zero(w: &[u8]) -> bool {
    w.iter().all(|b| *b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_derived_bases() {
        // slots.json `derived_bases`, all for the live pool 0xc0fd…7572 and market 0x9103…1836.
        let pool = parse_address("0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572").unwrap();
        let account = parse_address("0x6760e3b032ee2d670cb684d9076b8f48cb066c48").unwrap();
        let market =
            parse_word("0x9103c3b4e834476c9a62ea009ba2c884ee42e94e6e314a26f04d312434191836")
                .unwrap();
        assert_eq!(
            hex_word(&router_record(&pool)),
            "0x627459f28fd627023883d9310c65240762faa343d3f2429d1746640d8d8a0574"
        );
        assert_eq!(
            hex_word(&router_loan(&pool, 0)),
            "0x86a9bd383d29db7a1cff9d9758922f2534f0b6481acf587fe12a79b5d6f72f15"
        );
        assert_eq!(
            hex_word(&router_venue(&pool, 0)),
            "0x001189082010b9dff4cf86574fcb5fe6ac33a2e918767f233fca4e67dd4bba1c"
        );
        assert_eq!(
            hex_word(&array_base(&add(&router_record(&pool), 4))),
            "0x0e05292437d38cd75116d8f770eb87608d908595ff06eb5903099b996d076108"
        );
        assert_eq!(
            hex_word(&array_base(&add(&router_record(&pool), 7))),
            "0x173431a0f2ef71671a05b1cb20f99ceb92e153e9cc9eccab201c1f96cc0ee907"
        );
        assert_eq!(
            hex_word(&pool_loan(0)),
            "0xe27b86aa3e64fe0cf7c9294fb8b6fb20a28e5f01ba99e4bca9e76b647cc44f23"
        );
        assert_eq!(
            hex_word(&account_market_keys(&market)[0]),
            "0x7f73fe763fd70629cadd63d534e4c70682776b4eaeffdf39178b56c0a1bffde4"
        );
        let cbbtc = parse_address("0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf").unwrap();
        let usdc = parse_address("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap();
        assert_eq!(
            hex_word(&pricefeed_token_keys(&cbbtc)[0]),
            "0x1df6378d90dbe801fca9d47d5375a5a229ffa4eb34516b72a9e9ff9483681050"
        );
        assert_eq!(
            hex_word(&pricefeed_token_keys(&usdc)[1]),
            "0x167d7ad8ce5bbf928e114a13d4a925d29e6437f0d5be246a7858d666db460b9e"
        );
        assert_eq!(
            hex_word(&factory_is_pool_key(&pool)),
            "0xf67576777f99137ee577c518af5f53b3235ac7369b0be96b8b092f51a7007c6a"
        );
        assert_eq!(
            hex_word(&morpho_market_keys(&market)[0]),
            "0xb37d8d77c527a1e411d2abd81f103dee202b9a350a1fcbf567227a9222316a6a"
        );
        assert_eq!(
            hex_word(&morpho_position_keys(&market, &account)[1]),
            "0x1ca893d18673e8d37ef3632fa9ba2c4b035a7e69714e0db5d1d2587da8f1fcea"
        );
        assert_eq!(
            hex_word(&irm_rate_key(&market)),
            "0xe6f1c64c0bda05fd8d1c7bdb2840489820c4a2a4313f38c6e61dc429ea02ee12"
        );
        // Access-list keys of the guarded aggregators (schema 2.6.4).
        let asset_proxy = parse_address("0x07da0e54543a844a80abe69c8a12f22b3aa59f9d").unwrap();
        assert_eq!(
            hex_word(&access_list_key(&asset_proxy, 22)),
            "0x41680701ce92aeb803cdc51efa589e1cb7f85e01e147ba821db917cf259b0822"
        );
        let seq_proxy = parse_address("0xbcf85224fc0756b9fa45aa7892530b47e10b6433").unwrap();
        assert_eq!(
            hex_word(&access_list_key(&seq_proxy, 2)),
            "0xcc566c2d764e73e7eb8ae03477eede3173fda9e087085e4a6ba9d0449524cc2e"
        );
        assert_eq!(
            hex_word(&add(&ERC20_NS, 2)),
            "0x52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace02"
        );
    }

    #[test]
    fn add_carries() {
        let mut w = [0u8; 32];
        w[31] = 0xff;
        w[30] = 0xff;
        assert_eq!(add(&w, 1)[29..], [1, 0, 0]);
        assert_eq!(field(&add(&w, 1), 0, 4), 0x10000);
    }
}
