use std::{collections::HashMap, str::FromStr};

use alloy::primitives::Address as AlloyAddress;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use crate::{
    evm::{
        engine_db::{create_engine, SHARED_TYCHO_DB},
        protocol::curve::{state::CurveState, variant, vm},
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

/// Curve's substreams encodes native ETH as `0xEee…EeE`; Tycho's token map and component tokens
/// use the zero address. Both are normalized to the zero address so coin addresses align with the
/// token map and with swap inputs.
const ETH_SENTINEL: [u8; 20] = [0xEE; 20];

impl TryFromWithBlock<ComponentWithState, BlockHeader> for CurveState {
    type Error = InvalidSnapshotError;

    /// Decodes a `vm:curve` snapshot into a `CurveState`.
    ///
    /// Coin order is taken from the `coins` static attribute (the pool's on-chain coin order),
    /// not `component.tokens` (which Tycho sorts by address) — the two differ in general, and the
    /// VM getters (`balances(i)`, `price_scale(i)`, …) are indexed in on-chain order. Decimals and
    /// the swap-token→index mapping therefore also follow the `coins` order.
    async fn try_from_with_header(
        value: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let pool_address = Bytes::from_str(value.component.id.as_str()).map_err(|e| {
            InvalidSnapshotError::ValueError(format!(
                "Expected curve component id to be the pool address: {e}"
            ))
        })?;

        let coins = parse_coins(&value.component.static_attributes)?;
        if coins.len() < 2 {
            return Err(InvalidSnapshotError::ValueError(format!(
                "Curve pool {pool_address} has fewer than 2 coins"
            )));
        }
        let decimals = coins
            .iter()
            .map(|coin| coin_decimals(coin, all_tokens, &pool_address))
            .collect::<Result<Vec<u8>, _>>()?;

        let engine = create_engine(
            SHARED_TYCHO_DB.clone(),
            decoder_context
                .vm_traces
                .unwrap_or_default(),
        )
        .expect("Infallible");

        // Load proxy/implementation contracts so getter delegatecalls resolve (persists into the
        // shared DB for later delta_transition rebuilds).
        vm::load_stateless_contracts(&engine, &value.state.attributes).await?;

        let pool_alloy = AlloyAddress::from_slice(pool_address.as_ref());
        // Ensure the pool's actual MATH() contract is loaded — the indexed math address can be
        // stale, which otherwise breaks TwoCrypto NG-vs-Stable detection. Fail decoding if a pool
        // that exposes MATH() cannot load it, rather than build a pool with unresolved math.
        vm::load_math_contract(&engine, &pool_alloy).await?;

        let resolved = variant::resolve_variant(
            &value.component.static_attributes,
            &pool_alloy,
            coins.len(),
            &engine,
        )?;
        let pool = vm::decode_from_vm(&engine, &pool_alloy, resolved, &decimals)?;

        Ok(CurveState::new(pool_address, coins, decimals, resolved, pool))
    }
}

/// Parse the `coins` static attribute (a JSON array of `"0x…"` addresses in on-chain order) into
/// normalized coin addresses. The ETH sentinel is rewritten to the zero address.
fn parse_coins(
    static_attributes: &HashMap<String, Bytes>,
) -> Result<Vec<Bytes>, InvalidSnapshotError> {
    let raw = static_attributes
        .get("coins")
        .ok_or_else(|| {
            InvalidSnapshotError::ValueError("Missing `coins` static attribute".to_string())
        })?;
    let text = std::str::from_utf8(raw.as_ref()).map_err(|e| {
        InvalidSnapshotError::ValueError(format!("`coins` attribute is not valid UTF-8: {e}"))
    })?;
    let addresses: Vec<String> = serde_json::from_str(text).map_err(|e| {
        InvalidSnapshotError::ValueError(format!("Failed to parse `coins` attribute: {e}"))
    })?;
    addresses
        .iter()
        .map(|address| {
            Bytes::from_str(address)
                .map(normalize_eth)
                .map_err(|e| {
                    InvalidSnapshotError::ValueError(format!("Invalid coin address {address}: {e}"))
                })
        })
        .collect()
}

/// Rewrite the Curve ETH sentinel to the zero address; leave other addresses unchanged.
fn normalize_eth(address: Bytes) -> Bytes {
    if address.as_ref() == ETH_SENTINEL {
        Bytes::from(vec![0u8; 20])
    } else {
        address
    }
}

/// Look up a coin's decimals from the token map. Native ETH (zero address) defaults to 18 when
/// absent; any other unknown coin is an error (the pool is dropped).
fn coin_decimals(
    coin: &Bytes,
    all_tokens: &HashMap<Bytes, Token>,
    pool_address: &Bytes,
) -> Result<u8, InvalidSnapshotError> {
    if let Some(token) = all_tokens.get(coin) {
        return Ok(token.decimals as u8);
    }
    if coin.iter().all(|b| *b == 0) {
        return Ok(18);
    }
    Err(InvalidSnapshotError::ValueError(format!(
        "Missing token {coin} in state for curve pool {pool_address}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs_with_coins(json: &str) -> HashMap<String, Bytes> {
        let mut m = HashMap::new();
        m.insert("coins".to_string(), Bytes::from(json.as_bytes().to_vec()));
        m
    }

    #[test]
    fn parse_coins_preserves_on_chain_order_and_normalizes_eth() {
        // On-chain order [USDC, WBTC, ETH-sentinel]; ETH sentinel must map to the zero address.
        let json = r#"["0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48","0x2260fac5e5542a773aa44fbcfedf7c193bc2c599","0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"]"#;
        let coins = parse_coins(&attrs_with_coins(json)).unwrap();
        assert_eq!(coins.len(), 3);
        assert_eq!(
            coins[0],
            Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap()
        );
        assert_eq!(
            coins[1],
            Bytes::from_str("0x2260fac5e5542a773aa44fbcfedf7c193bc2c599").unwrap()
        );
        assert_eq!(coins[2], Bytes::from(vec![0u8; 20]), "ETH sentinel -> zero address");
    }

    #[test]
    fn parse_coins_missing_attribute_errors() {
        let err = parse_coins(&HashMap::new()).unwrap_err();
        assert!(matches!(err, InvalidSnapshotError::ValueError(_)));
    }
}
