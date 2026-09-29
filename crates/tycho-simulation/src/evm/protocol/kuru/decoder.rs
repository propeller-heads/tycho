//! Tycho attributes (written by `protocols/substreams/monad-kuru`) -> `KuruState`.
//!
//! Keys: `base`, `quote` (addresses), `price_precision`, `size_precision`, optional
//! `base_decimals`/`quote_decimals` (else from Tycho token metadata), `taker_fee_bps`,
//! `maker_fee_bps`, `active` (0/1), vault storage `vault_best_bid`, `vault_bid_partial`,
//! `vault_best_ask`, `vault_ask_partial`, `vault_ask_size`, `vault_bid_size`, `vault_spread`, and
//! one key per book level: `b/<price>` / `a/<price>` (price in pricePrecision units, decimal) ->
//! total size.
use std::collections::{BTreeMap, HashMap};

use alloy::primitives::U256;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::{KuruState, Vault};
use crate::protocol::{
    errors::InvalidSnapshotError,
    models::{DecoderContext, TryFromWithBlock},
};

fn num(v: &Bytes) -> Result<U256, String> {
    if v.len() > 32 {
        return Err(format!("attribute longer than 32 bytes: {}", v.len()));
    }
    Ok(U256::from_be_slice(v.as_ref()))
}

fn pow10(d: U256) -> Result<U256, String> {
    if d > U256::from(77) {
        return Err(format!("decimals {d}"));
    }
    Ok(U256::from(10u64).pow(d))
}

/// Applies one attribute; `None` = deleted (only levels may be deleted).
pub(super) fn apply_attribute(
    s: &mut KuruState,
    key: &str,
    v: Option<&Bytes>,
) -> Result<(), String> {
    if let Some((side, price)) = key.split_once('/') {
        let book = match side {
            "b" => &mut s.bids,
            "a" => &mut s.asks,
            _ => return Err(format!("unknown attribute {key}")),
        };
        let price: u32 = price
            .parse()
            .map_err(|_| format!("bad level key {key}"))?;
        match v.map(num).transpose()? {
            Some(size) if !size.is_zero() => book.insert(price, size),
            _ => book.remove(&price),
        };
        return Ok(());
    }
    let v = v.ok_or_else(|| format!("attribute {key} deleted"))?;
    let x = || num(v);
    match key {
        "base" => s.base = v.clone(),
        "quote" => s.quote = v.clone(),
        "price_precision" => s.price_precision = x()?,
        "size_precision" => s.size_precision = x()?,
        "base_decimals" => s.base_mult = pow10(x()?)?,
        "quote_decimals" => s.quote_mult = pow10(x()?)?,
        "taker_fee_bps" => s.taker_fee_bps = x()?,
        "maker_fee_bps" => s.maker_fee_bps = x()?,
        "active" => s.active = !x()?.is_zero(),
        "vault_best_bid" => s.vault.best_bid = x()?,
        "vault_bid_partial" => s.vault.bid_partial = x()?,
        "vault_best_ask" => s.vault.best_ask = x()?,
        "vault_ask_partial" => s.vault.ask_partial = x()?,
        "vault_ask_size" => s.vault.ask_size = x()?,
        "vault_bid_size" => s.vault.bid_size = x()?,
        "vault_spread" => s.vault.spread = x()?,
        // Tycho-injected, not ours.
        "block_number" | "block_timestamp" => {}
        _ => return Err(format!("unknown attribute {key}")),
    }
    Ok(())
}

const REQUIRED: [&str; 5] = ["base", "quote", "price_precision", "size_precision", "taker_fee_bps"];

/// `tokens` supplies decimals (Tycho resolves token metadata) unless `*_decimals` attributes do.
pub fn state_from_attributes(
    attrs: &HashMap<String, Bytes>,
    tokens: &HashMap<Bytes, Token>,
) -> Result<KuruState, String> {
    if let Some(k) = REQUIRED
        .iter()
        .find(|k| !attrs.contains_key(**k))
    {
        return Err(format!("missing attribute {k}"));
    }
    let mut s = KuruState {
        base: Bytes::default(),
        quote: Bytes::default(),
        price_precision: U256::ZERO,
        size_precision: U256::ZERO,
        base_mult: U256::ZERO,
        quote_mult: U256::ZERO,
        taker_fee_bps: U256::ZERO,
        maker_fee_bps: U256::ZERO,
        active: true,
        bids: BTreeMap::new(),
        asks: BTreeMap::new(),
        // An uninitialised vault: ask = uint256.max, bid = 0 (OrderBook.initialize).
        vault: Vault { best_ask: U256::MAX, ..Default::default() },
    };
    for (k, v) in attrs {
        apply_attribute(&mut s, k, Some(v))?;
    }
    for (addr, mult) in [(s.base.clone(), &mut s.base_mult), (s.quote.clone(), &mut s.quote_mult)] {
        if mult.is_zero() {
            let t = tokens
                .get(&addr)
                .ok_or_else(|| format!("no decimals for token {addr}"))?;
            *mult = pow10(U256::from(t.decimals))?;
        }
    }
    if s.price_precision.is_zero() || s.size_precision.is_zero() {
        return Err("zero precision".into());
    }
    Ok(s)
}

impl TryFromWithBlock<ComponentWithState, BlockHeader> for KuruState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        _decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        // static attributes (tokens, precisions) + state attributes (fees, vault, levels)
        let mut attrs = snapshot
            .component
            .static_attributes
            .clone();
        attrs.extend(snapshot.state.attributes);
        state_from_attributes(&attrs, all_tokens).map_err(InvalidSnapshotError::ValueError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be(x: u128) -> Bytes {
        Bytes::from(x.to_be_bytes().to_vec())
    }

    #[test]
    fn decode_and_delete_level() {
        let mut a: HashMap<String, Bytes> = HashMap::from([
            ("base".into(), Bytes::from([0u8; 20])),
            ("quote".into(), Bytes::from([1u8; 20])),
            ("price_precision".into(), be(100_000_000)),
            ("size_precision".into(), be(10_000_000_000)),
            ("base_decimals".into(), be(18)),
            ("taker_fee_bps".into(), be(0)),
            ("a/2890000".into(), be(7)),
        ]);
        let usdc = Token::new(&Bytes::from([1u8; 20]), "USDC", 6, 0, &[], Default::default(), 100);
        let tokens = HashMap::from([(usdc.address.clone(), usdc)]);
        assert!(state_from_attributes(&a, &HashMap::new()).is_err());
        let mut s = state_from_attributes(&a, &tokens).unwrap();
        assert_eq!(s.asks.get(&2_890_000), Some(&U256::from(7)));
        assert_eq!(s.quote_mult, U256::from(1_000_000));
        apply_attribute(&mut s, "a/2890000", None).unwrap();
        assert!(s.asks.is_empty());
        a.remove("base");
        assert!(state_from_attributes(&a, &tokens).is_err());
    }

    #[tokio::test]
    async fn snapshot_merges_static_and_state() {
        use tycho_client::feed::synchronizer::ComponentWithState;
        use tycho_common::models::protocol::{ProtocolComponent, ProtocolComponentState};

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "0x01".into(),
                attributes: HashMap::from([
                    ("taker_fee_bps".into(), be(3)),
                    ("b/100".into(), be(5)),
                ]),
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                static_attributes: HashMap::from([
                    ("base".into(), Bytes::from([0u8; 20])),
                    ("quote".into(), Bytes::from([1u8; 20])),
                    ("price_precision".into(), be(100)),
                    ("size_precision".into(), be(1000)),
                    ("base_decimals".into(), be(18)),
                    ("quote_decimals".into(), be(6)),
                ]),
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        };
        let s = crate::evm::protocol::test_utils::try_decode_snapshot_with_defaults::<KuruState>(
            snapshot,
        )
        .await
        .unwrap();
        assert_eq!(s.taker_fee_bps, U256::from(3));
        assert_eq!(s.bids.get(&100), Some(&U256::from(5)));
        assert_eq!(s.vault.best_ask, U256::MAX);
    }
}
