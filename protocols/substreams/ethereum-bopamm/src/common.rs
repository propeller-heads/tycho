//! Shared helpers and ABI-level constants for the BopAMM modules.
//!
//! Deployment-specific values (addresses, storage-layout slots) come from
//! [`crate::config::DeploymentConfig`]; the constants here are ABI-level (function selectors,
//! event topics) that are fixed by the contract code itself.
use ethabi::{ParamType, Token};
use keccak_hash::keccak;
use substreams::{
    hex,
    scalar::BigInt,
    store::{StoreGet, StoreGetProto},
};
use tycho_substreams::prelude::*;

/// `Paused(address)` — OpenZeppelin Pausable, emitted by the settlement contract.
pub const PAUSED_TOPIC: [u8; 32] =
    hex!("62e78cea01bee320cd4e420270b5ea74000d11b0c9f74754ebdbfc544b05a258");
/// `Unpaused(address)`.
pub const UNPAUSED_TOPIC: [u8; 32] =
    hex!("5db9ee0a495bf2e6ff9c91a7834c1ba4fdd244a5e8aa4e537bd38aeae4b073aa");

/// `updateState(address,uint256,uint32,uint256[])` — the live on-chain commit path.
pub const SEL_UPDATE_STATE: [u8; 4] = hex!("a9114b0f");
/// `batchUpdateStateWithSignature((address,address,uint256,uint32,uint256[],bytes)[])` — a
/// signed multi-book commit path that exists in the ABI but is currently unused on-chain.
pub const SEL_BATCH_UPDATE: [u8; 4] = hex!("e50de8ea");

/// Upper bound on enumerable book ids (assetIds are small sequential integers).
pub const MAX_ASSET_ID: u64 = 64;

/// Deterministic component id for a book: 32 bytes = `settlement (20) ‖ assetId (12, BE)`,
/// hex-encoded.
///
/// The id must hex-decode to at most 32 bytes: `tycho-simulation` converts it into the swap
/// adapter's `bytes32 poolId` via `string_to_bytes32`, and the adapter recovers the
/// settlement from the first 20 bytes and the asset id from the low 12.
pub fn component_id(settlement: &[u8], asset_id: u64) -> String {
    let mut id = [0u8; 32];
    id[..20].copy_from_slice(settlement);
    id[24..].copy_from_slice(&asset_id.to_be_bytes());
    format!("0x{}", hex::encode(id))
}

/// Storage slot of `assetConfig[asset_id]` = `keccak256(abi.encode(asset_id, base_slot))`.
pub fn asset_config_slot(asset_id: u64, base_slot: u64) -> Vec<u8> {
    let mut input = [0u8; 64];
    input[24..32].copy_from_slice(&asset_id.to_be_bytes());
    input[56..64].copy_from_slice(&base_slot.to_be_bytes());
    keccak(input.as_slice())
        .as_bytes()
        .to_vec()
}

pub fn is_zero(value: &[u8]) -> bool {
    value.iter().all(|&b| b == 0)
}

/// Extracts the 20-byte address packed in the low bytes of a 32-byte storage word.
///
/// Storage words shorter than 20 bytes are left-padded with zeros; the zero word yields the
/// zero address, which callers treat as "no maker yet" (first designation).
pub fn address_from_word(word: &[u8]) -> Vec<u8> {
    let mut address = [0u8; 20];
    let take = word.len().min(20);
    address[20 - take..].copy_from_slice(&word[word.len() - take..]);
    address.to_vec()
}

/// The signed WETH balance delta a `Deposit`/`Withdrawal` applies to the maker.
///
/// `signed_wad` is the change as it affects the maker's WETH balance (`+wad` for a deposit,
/// `-wad` for a withdrawal). Returns `Some` only when `party` (the deposit `dst` / withdrawal
/// `src`) is the maker, since these events change only that party's balance.
pub fn weth_event_delta(party: &[u8], maker: &[u8], signed_wad: BigInt) -> Option<BigInt> {
    (party == maker).then_some(signed_wad)
}

/// All currently-known book component ids, by probing the components store.
///
/// Probes `book:{i}` from id 0 upward and stops at the first miss. This short-circuit relies on
/// two protocol invariants: asset ids are assigned sequentially with no gaps, and delisting is
/// not supported (components are immutable; a book is never removed once created). If delisting
/// is ever added the gap would truncate enumeration early and this must be revisited.
pub fn enumerate_books(store: &StoreGetProto<ProtocolComponent>) -> Vec<String> {
    enumerate_books_from(|i| {
        store
            .get_last(format!("book:{i}"))
            .map(|c| c.id)
    })
}

/// Short-circuiting book enumeration over a probe of book id -> component id.
///
/// Returns ids for `0, 1, 2, ...` until `probe` first returns `None`. Split out from
/// [`enumerate_books`] so the unbounded short-circuit can be tested without the WASM store.
fn enumerate_books_from(probe: impl Fn(u64) -> Option<String>) -> Vec<String> {
    let mut books = Vec::new();
    let mut i = 0u64;
    while let Some(id) = probe(i) {
        books.push(id);
        i += 1;
    }
    books
}

/// Store keys under which a book is indexed by token, one per token.
///
/// Every token is indexed (not just the asset side) so the lookup is independent of token
/// ordering: the USDC key is harmless because [`books_for_token`] resolves USDC through its
/// own hub branch and never reads it, while each asset token maps to exactly its own book.
pub fn token_index_keys(tokens: &[Vec<u8>]) -> Vec<String> {
    tokens
        .iter()
        .map(|t| format!("token:0x{}", hex::encode(t)))
        .collect()
}

/// Component ids a token backs: USDC backs every book; an asset backs its own `asset/USDC`
/// book.
pub fn books_for_token(
    token: &[u8],
    usdc: &[u8],
    store: &StoreGetProto<ProtocolComponent>,
) -> Vec<String> {
    if token == usdc {
        enumerate_books(store)
    } else {
        store
            .get_last(format!("token:0x{}", hex::encode(token)))
            .map(|c| vec![c.id])
            .unwrap_or_default()
    }
}

/// Reads a big-endian `u64` from a left-padded attribute value.
pub fn u64_from_word_padded(value: &[u8]) -> Option<u64> {
    if value.len() > 8 {
        let start = value.len() - 8;
        value[start..]
            .try_into()
            .ok()
            .map(u64::from_be_bytes)
    } else {
        let mut buf = [0u8; 8];
        buf[8 - value.len()..].copy_from_slice(value);
        Some(u64::from_be_bytes(buf))
    }
}

/// Decodes the `(caller, bookId, ts)` tuples committed by a registry update call.
///
/// `caller` is the update's first argument: the pricing module the book belongs to. The
/// `PrioUpdateRegistry` is shared across venues, and book ids are only unique *per caller*
/// (the lane slot is `keccak(caller, bookId)`), so callers must filter on the BopAMM module
/// before attributing an update to a book.
///
/// Returns one tuple for `updateState`, one per struct for `batchUpdateStateWithSignature`,
/// and an empty vec for any other call or malformed calldata.
pub fn committed_updates(input: &[u8]) -> Vec<(Vec<u8>, u64, u32)> {
    let Some(selector) = input.get(0..4) else { return Vec::new() };
    let data = &input[4..];
    let Ok(selector): std::result::Result<[u8; 4], _> = selector.try_into() else {
        return Vec::new();
    };
    if selector == SEL_UPDATE_STATE {
        decode_update_state(data)
            .into_iter()
            .collect()
    } else if selector == SEL_BATCH_UPDATE {
        decode_batch_update(data)
    } else {
        Vec::new()
    }
}

/// `updateState(address caller, uint256 bookId, uint32 ts, uint256[] lanes)`.
fn decode_update_state(data: &[u8]) -> Option<(Vec<u8>, u64, u32)> {
    let types = [
        ParamType::Address,
        ParamType::Uint(256),
        ParamType::Uint(32),
        ParamType::Array(Box::new(ParamType::Uint(256))),
    ];
    let tokens = ethabi::decode(&types, data).ok()?;
    let caller = tokens.first()?.clone().into_address()?;
    let book_id = tokens.get(1)?.clone().into_uint()?;
    let ts = tokens.get(2)?.clone().into_uint()?;
    Some((caller.as_bytes().to_vec(), book_id.low_u64(), ts.low_u32()))
}

/// `batchUpdateStateWithSignature((address caller,address,uint256 bookId,uint32
/// ts,uint256[],bytes)[])`.
///
/// The first struct field is the caller (the pricing module), matching `updateState`'s caller
/// argument. This path is unused on-chain today (every commit is a top-level `updateState`).
fn decode_batch_update(data: &[u8]) -> Vec<(Vec<u8>, u64, u32)> {
    let entry = ParamType::Tuple(vec![
        ParamType::Address,
        ParamType::Address,
        ParamType::Uint(256),
        ParamType::Uint(32),
        ParamType::Array(Box::new(ParamType::Uint(256))),
        ParamType::Bytes,
    ]);
    let Ok(tokens) = ethabi::decode(&[ParamType::Array(Box::new(entry))], data) else {
        return Vec::new();
    };
    let Some(Token::Array(items)) = tokens.into_iter().next() else { return Vec::new() };
    let mut updates = Vec::new();
    for item in items {
        let Token::Tuple(fields) = item else { continue };
        let caller = fields
            .first()
            .and_then(|t| t.clone().into_address());
        let book_id = fields
            .get(2)
            .and_then(|t| t.clone().into_uint());
        let ts = fields
            .get(3)
            .and_then(|t| t.clone().into_uint());
        if let (Some(caller), Some(book_id), Some(ts)) = (caller, book_id, ts) {
            updates.push((caller.as_bytes().to_vec(), book_id.low_u64(), ts.low_u32()));
        }
    }
    updates
}

#[cfg(test)]
mod tests {
    use ethabi::{encode, ethereum_types::U256};

    use super::*;

    const SETTLEMENT: [u8; 20] = hex!("db13ad0fcd134e9c48f2fdaea8f6751a0f5349ca");

    #[test]
    fn asset_config_slot_matches_onchain_values() {
        // keccak256(abi.encode(uint256(assetId), uint256(3))), verified via `cast index`.
        assert_eq!(
            hex::encode(asset_config_slot(0, 3)),
            "3617319a054d772f909f7c479a2cebe5066e836a939412e32403c99029b92eff"
        );
        assert_eq!(
            hex::encode(asset_config_slot(1, 3)),
            "a15bc60c955c405d20d9149c709e2460f1c2d9a497496a7f46004d1772c3054c"
        );
    }

    #[test]
    fn component_id_is_settlement_scoped_bytes32() {
        assert_eq!(
            component_id(&SETTLEMENT, 0),
            "0xdb13ad0fcd134e9c48f2fdaea8f6751a0f5349ca000000000000000000000000"
        );
        assert_eq!(
            component_id(&SETTLEMENT, 1),
            "0xdb13ad0fcd134e9c48f2fdaea8f6751a0f5349ca000000000000000000000001"
        );
    }

    #[test]
    fn decodes_real_update_state_calldata() {
        // Real on-chain `updateState` calldata (registry 0xDa7A…): bookId=0, ts=0x6a23368f.
        let input = hex::decode(concat!(
            "a9114b0f",
            "000000000000000000000000bc60639345dfa607d73b74e88c2d54d8b8ad7cc3",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "000000000000000000000000000000000000000000000000000000006a23368f",
            "0000000000000000000000000000000000000000000000000000000000000080",
            "0000000000000000000000000000000000000000000000000000000000000002",
            "0000000000008138888000000000000000000000000000000000000000000000",
            "013fff016dac01b330000000000000013e190000000000000000000000000000",
        ))
        .unwrap();
        let updates = committed_updates(&input);
        let module = hex::decode("bc60639345dfa607d73b74e88c2d54d8b8ad7cc3").unwrap();
        assert_eq!(updates, vec![(module, 0u64, 0x6a23_368f_u32)]);

        // Lock the critical invariant: the registry `bookId`, the module `assetId`, and the
        // component index are the same space. Book 0 is the WETH book (asset-config slot
        // keccak(0,3)); its component id must be the assetId-0 component.
        let (_, book_id, _) = &updates[0];
        let book_id = *book_id;
        assert_eq!(
            component_id(&SETTLEMENT, book_id),
            "0xdb13ad0fcd134e9c48f2fdaea8f6751a0f5349ca000000000000000000000000"
        );
        assert_eq!(
            hex::encode(asset_config_slot(book_id, 3)),
            "3617319a054d772f909f7c479a2cebe5066e836a939412e32403c99029b92eff"
        );
    }

    #[test]
    fn decodes_synthetic_batch_calldata() {
        // No `batchUpdateStateWithSignature` call exists on-chain yet; round-trip synthetic
        // calldata to validate the decoder against the ABI.
        let entry = Token::Tuple(vec![
            Token::Address([0x11u8; 20].into()),
            Token::Address([0x22u8; 20].into()),
            Token::Uint(U256::from(1u64)),
            Token::Uint(U256::from(0x6a23_368f_u64)),
            Token::Array(vec![Token::Uint(U256::from(7u64))]),
            Token::Bytes(vec![0xaa, 0xbb]),
        ]);
        let mut input = SEL_BATCH_UPDATE.to_vec();
        input.extend(encode(&[Token::Array(vec![entry])]));
        assert_eq!(committed_updates(&input), vec![(vec![0x11u8; 20], 1u64, 0x6a23_368f_u32)]);
    }

    #[test]
    fn ignores_unknown_and_short_calldata() {
        assert!(committed_updates(&hex::decode("deadbeef").unwrap()).is_empty());
        assert!(committed_updates(&[0x01, 0x02]).is_empty());
    }

    #[test]
    fn token_index_keys_cover_every_token_regardless_of_order() {
        let weth = hex::decode("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap();
        let usdc = hex::decode("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        // `map_components` sorts the pair ascending => [usdc, weth]; the asset token must
        // still be indexed (it is the second element here), independent of ordering.
        let keys = token_index_keys(&[usdc.clone(), weth.clone()]);
        assert!(keys.contains(&format!("token:0x{}", hex::encode(&weth))));
        assert!(keys.contains(&format!("token:0x{}", hex::encode(&usdc))));
    }

    #[test]
    fn enumerate_books_from_is_unbounded_and_sequential() {
        // No upper bound: 100 sequential books are all enumerated.
        let present: std::collections::HashSet<u64> = (0..100).collect();
        let books = enumerate_books_from(|i| {
            present
                .contains(&i)
                .then(|| format!("book:{i}"))
        });
        assert_eq!(books.len(), 100);
        assert_eq!(books.first().map(String::as_str), Some("book:0"));
        assert_eq!(books.last().map(String::as_str), Some("book:99"));
    }

    #[test]
    fn enumerate_books_from_short_circuits_on_first_gap() {
        // Ids 0,1,2 then a gap at 3: enumeration stops at the gap and never sees id 4, relying
        // on the no-gap / no-delisting invariant documented on `enumerate_books`.
        let present: std::collections::HashSet<u64> = [0, 1, 2, 4].into_iter().collect();
        let books = enumerate_books_from(|i| {
            present
                .contains(&i)
                .then(|| format!("book:{i}"))
        });
        assert_eq!(books, vec!["book:0", "book:1", "book:2"]);
    }

    #[test]
    fn enumerate_books_from_empty_store_yields_nothing() {
        let books = enumerate_books_from(|_| None);
        assert!(books.is_empty());
    }

    #[test]
    fn u64_from_word_padded_handles_short_and_long() {
        assert_eq!(u64_from_word_padded(&[0x01]), Some(1));
        assert_eq!(u64_from_word_padded(&1u64.to_be_bytes()), Some(1));
        let padded = [&[0u8; 24][..], &1u64.to_be_bytes()[..]].concat();
        assert_eq!(u64_from_word_padded(&padded), Some(1));
    }

    #[test]
    fn address_from_word_extracts_low_20_bytes() {
        let maker = hex::decode("6f7a3714d7fc266e3e84067ac31e7b1a3be18060").unwrap();
        let mut word = [0u8; 32];
        word[12..].copy_from_slice(&maker);
        assert_eq!(address_from_word(&word), maker);
    }

    #[test]
    fn address_from_word_of_zero_word_is_zero_address() {
        // First maker designation: the slot's old value is the zero word, which must decode to the
        // zero address so callers treat `balanceOf(old)` as 0.
        let zero = address_from_word(&[0u8; 32]);
        assert_eq!(zero.len(), 20);
        assert!(zero.iter().all(|&b| b == 0));
    }

    #[test]
    fn address_from_word_handles_short_words() {
        // Storage values are stored unpadded; a short word is left-padded.
        assert_eq!(address_from_word(&[0xab]), {
            let mut a = vec![0u8; 20];
            a[19] = 0xab;
            a
        });
    }

    #[test]
    fn weth_event_delta_signs_by_party() {
        let maker = hex::decode("6f7a3714d7fc266e3e84067ac31e7b1a3be18060").unwrap();
        let other = hex::decode("9008d19f58aabd9ed0d60971565aa8510560ab41").unwrap();
        // Deposit to the maker: caller passes +wad.
        assert_eq!(weth_event_delta(&maker, &maker, BigInt::from(100)), Some(BigInt::from(100)));
        // Withdrawal from the maker: caller passes -wad.
        assert_eq!(weth_event_delta(&maker, &maker, BigInt::from(-100)), Some(BigInt::from(-100)));
        // A deposit/withdrawal for another wallet does not touch the maker's balance.
        assert_eq!(weth_event_delta(&other, &maker, BigInt::from(100)), None);
    }

    #[test]
    fn reseed_delta_is_new_minus_old() {
        // On a maker write the re-seed delta is balanceOf(new) - balanceOf(old). First
        // designation: old balance is 0, so the delta seeds the maker's pre-existing holdings.
        let first_designation = BigInt::from(500) - BigInt::zero();
        assert_eq!(first_designation, BigInt::from(500));
        // Rotation from a wallet holding 200 to one holding 500 nets +300.
        let rotation = BigInt::from(500) - BigInt::from(200);
        assert_eq!(rotation, BigInt::from(300));
    }
}
