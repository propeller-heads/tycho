//! Hanji's aggregator market list: which proxy orderbooks to read, and the tokens of each.

use std::time::Duration;

use alloy::primitives::Address;
use serde::Deserialize;
use serde_with::serde_as;
use tracing::warn;
use tycho_common::Bytes;

use crate::{
    book::wire::SkipInvalidEntries,
    snapshot_feed::{
        errors::FeedError,
        http::{fetch_json, nothing_refused},
    },
};

/// The `/aggregators/markets` response: its entries, less those that do not parse.
#[serde_as]
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
struct ApiMarkets(#[serde_as(deserialize_as = "SkipInvalidEntries")] Vec<ApiMarket>);

/// One entry of `/aggregators/markets`, as the API sends it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ApiMarket {
    pub orderbook_address: Address,
    pub base_token: ApiToken,
    pub quote_token: ApiToken,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ApiToken {
    /// The token's contract; for the native token, the wrapped token the contracts use.
    pub contract_address: Address,
    pub is_native: bool,
}

/// One market to read: its proxy orderbook and its two tokens as the contracts name them, token X
/// (the base) then token Y (the quote). `native` says which side, if any, is the native token,
/// which `getConfig()` names by its wrapped token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Market {
    pub orderbook: Address,
    pub token_x: Address,
    pub token_y: Address,
    pub native: NativeSide,
}

/// Which side of a market, if any, the native token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NativeSide {
    None,
    X,
    Y,
}

impl Market {
    /// The market an API entry describes, or `None`, logged, when it is one the contracts cannot
    /// trade: both sides native, a native side named by a token other than the wrapped native, or
    /// the same token on both sides.
    pub(super) fn from_api(entry: &ApiMarket, wrapped_native: Address) -> Option<Self> {
        let (base, quote) = (&entry.base_token, &entry.quote_token);
        let orderbook = entry.orderbook_address;
        let native = match (base.is_native, quote.is_native) {
            (false, false) => NativeSide::None,
            (true, false) => NativeSide::X,
            (false, true) => NativeSide::Y,
            (true, true) => {
                warn!(%orderbook, "skipping Hanji market native on both sides");
                return None;
            }
        };
        let market = Market {
            orderbook,
            token_x: base.contract_address,
            token_y: quote.contract_address,
            native,
        };
        if market
            .native_token()
            .is_some_and(|token| token != wrapped_native)
        {
            warn!(
                %orderbook,
                native_token = ?market.native_token(),
                %wrapped_native,
                "skipping Hanji market whose native side is not the wrapped native token"
            );
            return None;
        }
        if market.token_x == market.token_y {
            warn!(%orderbook, "skipping Hanji market trading a token for itself");
            return None;
        }
        Some(market)
    }

    /// The token the contracts name for the native side, if the market has one.
    pub(super) fn native_token(&self) -> Option<Address> {
        match self.native {
            NativeSide::None => None,
            NativeSide::X => Some(self.token_x),
            NativeSide::Y => Some(self.token_y),
        }
    }

    /// The market's base and quote as components carry them: `native` (the chain's native token)
    /// for the native side, the contract's address for the other.
    pub(super) fn published_tokens(&self, native: &Bytes) -> (Bytes, Bytes) {
        let side = |token: Address, is_native: bool| {
            if is_native {
                native.clone()
            } else {
                Bytes::from(token.to_vec())
            }
        };
        (
            side(self.token_x, self.native == NativeSide::X),
            side(self.token_y, self.native == NativeSide::Y),
        )
    }
}

/// Fetches the market list, giving the API `timeout`. A failing status, an unreachable API or one
/// that does not answer in time is a [`FeedError::Connection`]; a body that is not a list, or whose
/// every entry fails to parse, a [`FeedError::Parsing`]. Entries that do not parse or that the
/// contracts cannot trade are left out, logged, and a list with none left is an error rather than
/// an empty list.
pub(super) async fn fetch_markets(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
    wrapped_native: Address,
) -> Result<Vec<Market>, FeedError> {
    let ApiMarkets(entries) =
        fetch_json(client.get(url).timeout(timeout), "Hanji aggregator markets", nothing_refused)
            .await?;
    let markets = markets_from_api(&entries, wrapped_native);
    if markets.is_empty() {
        return Err(FeedError::Parsing(format!(
            "Hanji aggregator markets: none of the {} listed markets is tradable",
            entries.len()
        )));
    }
    Ok(markets)
}

/// The tradable markets among `entries`, in the API's order, each proxy once.
fn markets_from_api(entries: &[ApiMarket], wrapped_native: Address) -> Vec<Market> {
    let mut markets: Vec<Market> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(market) = Market::from_api(entry, wrapped_native) else {
            continue;
        };
        if markets
            .iter()
            .any(|known| known.orderbook == market.orderbook)
        {
            warn!(orderbook = %market.orderbook, "skipping Hanji market the API lists twice");
            continue;
        }
        markets.push(market);
    }
    markets
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;
    use rstest::rstest;

    use super::*;
    use crate::{
        pamm::protocols::hanji::test_support::{fixture, CBBTC, USDC},
        snapshot_feed::http::test_support::{spawn_http_server, MockResponse},
    };

    const WETH: Address = address!("4200000000000000000000000000000000000006");

    fn entry(orderbook: Address, base: (Address, bool), quote: (Address, bool)) -> ApiMarket {
        ApiMarket {
            orderbook_address: orderbook,
            base_token: ApiToken { contract_address: base.0, is_native: base.1 },
            quote_token: ApiToken { contract_address: quote.0, is_native: quote.1 },
        }
    }

    /// The recorded API response lists eight markets, ETH on the base side of ETH/USDC and on the
    /// quote side of cbBTC/ETH.
    #[test]
    fn the_recorded_market_list_parses_with_its_native_sides() {
        let body = serde_json::to_string(
            &fixture()
                .markets
                .iter()
                .map(|market| market.api.clone())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let ApiMarkets(entries) = serde_json::from_str(&body).unwrap();

        let markets = markets_from_api(&entries, WETH);

        assert_eq!(markets.len(), 8);
        let native: Vec<_> = markets
            .iter()
            .map(|market| (market.token_x, market.token_y, market.native))
            .collect();
        assert!(native.contains(&(WETH, USDC, NativeSide::X)));
        assert!(native.contains(&(CBBTC, WETH, NativeSide::Y)));
        assert!(native.contains(&(CBBTC, USDC, NativeSide::None)));
    }

    /// One entry the API gets wrong is dropped; the others are kept.
    #[test]
    fn an_entry_that_does_not_parse_is_left_out() {
        let body = r#"[
            {"orderbookAddress": "0x15c8cb4e336b1e478d155d813830129fb225eee3",
             "baseToken": {"contractAddress": "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf",
                           "isNative": false},
             "quoteToken": {"contractAddress": "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                            "isNative": false}},
            {"orderbookAddress": "not an address"}
        ]"#;

        let ApiMarkets(entries) = serde_json::from_str(body).unwrap();

        assert_eq!(entries.len(), 1);
    }

    #[rstest]
    #[case::native_on_both_sides((WETH, true), (WETH, true))]
    #[case::native_side_not_wrapped_native((CBBTC, true), (USDC, false))]
    #[case::same_token_twice((USDC, false), (USDC, false))]
    fn a_market_the_contracts_cannot_trade_is_left_out(
        #[case] base: (Address, bool),
        #[case] quote: (Address, bool),
    ) {
        let entries = [entry(Address::repeat_byte(1), base, quote)];

        assert!(markets_from_api(&entries, WETH).is_empty());
    }

    /// A list in which no market can be traded is an error, not an empty list.
    #[tokio::test]
    async fn a_list_of_untradable_markets_is_an_error() {
        let body = serde_json::json!([{
            "orderbookAddress": Address::repeat_byte(1),
            "baseToken": { "contractAddress": WETH, "isNative": true },
            "quoteToken": { "contractAddress": WETH, "isNative": true },
        }])
        .to_string();
        let server =
            spawn_http_server(move |_| Some(MockResponse::from(("200 OK", body.clone())))).await;

        let result =
            fetch_markets(&reqwest::Client::new(), &server.url(), Duration::from_secs(2), WETH)
                .await;

        assert!(matches!(result, Err(FeedError::Parsing(_))), "{result:?}");
    }

    #[test]
    fn a_proxy_listed_twice_is_read_once() {
        let orderbook = Address::repeat_byte(1);
        let entries = [
            entry(orderbook, (CBBTC, false), (USDC, false)),
            entry(orderbook, (WETH, true), (USDC, false)),
        ];

        let markets = markets_from_api(&entries, WETH);

        assert_eq!(markets.len(), 1);
        assert_eq!(markets[0].token_x, CBBTC);
    }

    /// The native side is published as the chain's native token, the other side as is.
    #[test]
    fn the_native_side_is_published_as_the_native_token() {
        let native = Bytes::from(vec![0; 20]);
        let market = Market {
            orderbook: Address::repeat_byte(1),
            token_x: CBBTC,
            token_y: WETH,
            native: NativeSide::Y,
        };

        let (base, quote) = market.published_tokens(&native);

        assert_eq!(base, Bytes::from(CBBTC.to_vec()));
        assert_eq!(quote, native);
    }
}
