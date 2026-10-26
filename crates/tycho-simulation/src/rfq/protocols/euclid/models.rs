use std::time::{SystemTime, UNIX_EPOCH};

use alloy::{
    primitives::{address, Address, U256},
    sol,
    sol_types::SolCall,
};
use num_bigint::BigUint;
use num_traits::Zero;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use tracing::debug;
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    Bytes,
};

use crate::{
    book::{
        levels::{InvalidLevel, Levels, PriceLevel},
        wire::{EntryReader, SkipInvalidEntries},
    },
    rfq::errors::RFQError,
    snapshot_feed::errors::FeedError,
};

sol! {
    /// A 1inch limit-order-protocol v3 RFQ order, the type the Aggregation Router v5 fills.
    struct OrderRFQ {
        uint256 info;
        address makerAsset;
        address takerAsset;
        address maker;
        address allowedSender;
        uint256 makingAmount;
        uint256 takingAmount;
    }

    /// `AggregationRouterV5.fillOrderRFQTo`, the only call a binding quote may carry.
    function fillOrderRFQTo(OrderRFQ order, bytes signature, uint256 flagsAndAmount, address target);
}

/// The 1inch Aggregation Router v5, which settles every Euclid order; the executor pins the same
/// address.
pub const AGGREGATION_ROUTER_V5: Address = address!("1111111254EEB25477B68fb85Ed929f73A960582");

/// The chains Euclid's RFQ gateway quotes on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EuclidSupportedChain {
    Ethereum,
}

impl EuclidSupportedChain {
    /// The chain id the gateway's endpoints address this chain by.
    pub fn id(self) -> u64 {
        match self {
            EuclidSupportedChain::Ethereum => 1,
        }
    }
}

impl TryFrom<Chain> for EuclidSupportedChain {
    type Error = String;

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "Euclid serves one chain; every other, present or future, is refused"
    )]
    fn try_from(chain: Chain) -> Result<Self, Self::Error> {
        match chain {
            Chain::Ethereum => Ok(EuclidSupportedChain::Ethereum),
            other => Err(format!("Euclid does not serve {other}")),
        }
    }
}

/// One frame of the levels stream: every pair the gateway prices on the chain. Each frame
/// replaces the one before it, so a pair missing from a frame is no longer priced.
#[serde_as]
#[derive(Debug, Clone, Deserialize)]
pub struct EuclidLevelsFrame {
    pub chain_id: u64,
    #[serde_as(deserialize_as = "SkipInvalidEntries")]
    pub pairs: Vec<EuclidPairLevels>,
}

/// One pair as the gateway publishes it. Prices and sizes travel as decimal strings.
#[serde_as]
#[derive(Debug, Clone, Deserialize)]
pub struct EuclidPairLevels {
    pub base_address: Bytes,
    pub quote_address: Bytes,
    /// `[price, size]`, best first: quote per base unit, base quantity.
    #[serde_as(as = "Vec<(DisplayFromStr, DisplayFromStr)>")]
    pub bids: Vec<(f64, f64)>,
    #[serde_as(as = "Vec<(DisplayFromStr, DisplayFromStr)>")]
    pub asks: Vec<(f64, f64)>,
    /// When the gateway built these levels, in milliseconds since the epoch.
    pub timestamp: u64,
}

/// One pair's validated book: the bid and ask ladders between a base and a quote token. The
/// first level of each side is the least the gateway signs for on that side, at that side's
/// best price.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EuclidBook {
    pub base: Bytes,
    pub quote: Bytes,
    /// When the gateway built these levels, in milliseconds since the epoch.
    pub updated_at_ms: u64,
    /// Selling base: `quote` per base unit, base quantities.
    pub bids: Levels,
    /// Buying base: `quote` per base unit, base quantities.
    pub asks: Levels,
}

impl TryFrom<EuclidPairLevels> for EuclidBook {
    type Error = InvalidLevel;

    fn try_from(pair: EuclidPairLevels) -> Result<Self, InvalidLevel> {
        let ladder = |side: Vec<(f64, f64)>| {
            Levels::new(
                side.into_iter()
                    .map(|(price, quantity)| PriceLevel { price, quantity })
                    .collect(),
            )
        };
        Ok(EuclidBook {
            bids: ladder(pair.bids)?,
            asks: ladder(pair.asks)?,
            base: pair.base_address,
            quote: pair.quote_address,
            updated_at_ms: pair.timestamp,
        })
    }
}

impl EuclidBook {
    /// The book's liquidity in quote-token units: the mean of what each side is worth.
    pub fn notional_tvl(&self) -> f64 {
        (self.bids.notional() + self.asks.notional()) / 2.0
    }
}

/// Pairs from one levels frame, excluding pairs with invalid levels, in the order Euclid sent
/// them: every pair the gateway prices on the chain, so a pair missing from one frame is no
/// longer priced.
///
/// The serde form follows the venue API model [`EuclidBook`], so the writer and the reader must
/// run the same tycho-simulation version.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EuclidPrices {
    /// The chain the frame prices, as the gateway numbers it.
    pub chain_id: u64,
    /// The valid pairs of the frame, in the order Euclid sent them.
    pub pairs: Vec<EuclidBook>,
}

impl EuclidPrices {
    /// Reads every pair of `frame`, dropping the ones whose levels are invalid. Fails only when
    /// none is valid.
    pub(super) fn from_frame(frame: EuclidLevelsFrame) -> Result<Self, FeedError> {
        let mut reader = EntryReader::new();
        let pairs = reader.read(frame.pairs, EuclidBook::try_from);
        reader
            .finish()
            .map_err(|reason| FeedError::Parsing(format!("Euclid levels frame: {reason}")))?;
        debug!(pairs = pairs.len(), "decoded levels frame");
        Ok(EuclidPrices { chain_id: frame.chain_id, pairs })
    }
}

/// The gateway's answer to a binding quote request: the quote, or why it declined to give one.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum EuclidFirmResponse {
    Quote(EuclidFirmQuote),
    Declined(EuclidDeclined),
}

/// A binding quote: `calldata` fills a maker-signed 1inch limit-order-protocol v3 RFQ order
/// through `tx_to`. Everything the fill pays, admits and expires at is in the signed order, so
/// it is read from there rather than from the fields beside it.
#[serde_as]
#[derive(Debug, Clone, Deserialize)]
pub struct EuclidFirmQuote {
    pub tx_to: Bytes,
    pub calldata: Bytes,
    #[serde_as(as = "DisplayFromStr")]
    pub amount_in: BigUint,
    /// How the gateway records the fill.
    pub order_hash: Bytes,
}

/// Why the gateway declined a quote, e.g. `insufficient_liquidity`.
#[derive(Debug, Clone, Deserialize)]
pub struct EuclidDeclined {
    pub error: String,
}

impl EuclidFirmQuote {
    /// What the signed order pays for the trade `params` asked for, once the quote is checked
    /// to be that trade: the amount, the router and call the executor allows, the assets, the
    /// fill amount the executor patches, the recipient, the filler the order admits, and an
    /// expiry still ahead. A quote that fails here would only revert on chain.
    pub fn amount_out(&self, params: &GetAmountOutParams) -> Result<BigUint, RFQError> {
        if self.amount_in != params.amount_in {
            return Err(RFQError::FatalError(format!(
                "Euclid quoted amount_in {}, requested {}",
                self.amount_in, params.amount_in
            )));
        }
        if self.tx_to.as_ref() != AGGREGATION_ROUTER_V5.as_slice() {
            return Err(RFQError::FatalError(format!(
                "Euclid order settles through {}, not the 1inch Aggregation Router v5",
                self.tx_to
            )));
        }
        let call = fillOrderRFQToCall::abi_decode(self.calldata.as_ref()).map_err(|e| {
            RFQError::FatalError(format!("Euclid calldata is not a fillOrderRFQTo call: {e}"))
        })?;
        let order = &call.order;
        if order.makerAsset.as_slice() != params.token_out.as_ref() {
            return Err(RFQError::FatalError(format!(
                "Euclid order pays {}, requested {}",
                order.makerAsset, params.token_out
            )));
        }
        if order.takerAsset.as_slice() != params.token_in.as_ref() {
            return Err(RFQError::FatalError(format!(
                "Euclid order takes {}, requested {}",
                order.takerAsset, params.token_in
            )));
        }
        // A plain amount: the router fills exactly this much of the taker asset.
        let fill = to_biguint(call.flagsAndAmount);
        if fill != params.amount_in {
            return Err(RFQError::FatalError(format!(
                "Euclid calldata fills {fill}, requested {}",
                params.amount_in
            )));
        }
        let taking_amount = to_biguint(order.takingAmount);
        if fill.is_zero() || fill > taking_amount {
            return Err(RFQError::FatalError(format!(
                "Euclid calldata fills {fill} of an order taking {taking_amount}"
            )));
        }
        if call.target.as_slice() != params.receiver.as_ref() {
            return Err(RFQError::FatalError(format!(
                "Euclid order delivers to {}, requested {}",
                call.target, params.receiver
            )));
        }
        if order.allowedSender.as_slice() != params.sender.as_ref() {
            return Err(RFQError::FatalError(format!(
                "Euclid order admits {} as its filler, requested sender {}",
                order.allowedSender, params.sender
            )));
        }
        // `info` carries the nonce in its low 64 bits and the expiry in the 64 above.
        let expiry = ((order.info >> 64usize) & U256::from(u64::MAX)).to::<u64>();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| RFQError::FatalError(format!("system clock is before the epoch: {e}")))?
            .as_secs();
        if expiry <= now {
            return Err(RFQError::FatalError(format!(
                "Euclid order expired at {expiry}, now {now}"
            )));
        }
        // What the router pays for `fill`: the maker's amount, pro rata, rounded down.
        Ok(to_biguint(order.makingAmount) * fill / taking_amount)
    }
}

fn to_biguint(value: U256) -> BigUint {
    BigUint::from_bytes_be(&value.to_be_bytes::<32>())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;

    use super::*;

    /// A levels frame and a binding quote recorded from Euclid's gateway on Ethereum.
    const LEVELS_FRAME: &str = include_str!("test_responses/levels_frame.json");
    const FIRM_QUOTE: &str = include_str!("test_responses/firm_quote.json");

    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    /// The TychoRouter the recorded quote was requested for, as sender and receiver: the one the
    /// router tests deploy, so the same recording fills through it on a fork.
    const ROUTER: &str = "0x6bc529dc7b81a031828ddce2bc419d01ff268c66";
    /// What the recorded order pays for its 0.003 WETH, in USDC.
    const RECORDED_AMOUNT_OUT: &str = "6241692";

    fn bytes(value: &str) -> Bytes {
        Bytes::from_str(value).unwrap()
    }

    /// The request the recorded quote answers: 0.003 WETH for USDC, to the router.
    fn recorded_params() -> GetAmountOutParams {
        GetAmountOutParams {
            amount_in: BigUint::from_str("3000000000000000").unwrap(),
            token_in: bytes(WETH),
            token_out: bytes(USDC),
            sender: bytes(ROUTER),
            receiver: bytes(ROUTER),
            origin: None,
        }
    }

    fn recorded_quote() -> EuclidFirmQuote {
        match serde_json::from_str::<EuclidFirmResponse>(FIRM_QUOTE).unwrap() {
            EuclidFirmResponse::Quote(quote) => quote,
            EuclidFirmResponse::Declined(declined) => panic!("recorded a decline: {declined:?}"),
        }
    }

    /// The recorded quote with its order changed by `change` and re-encoded; the original's
    /// expiry is moved far ahead first, since the recording lasted thirty seconds.
    fn quote_with(change: impl FnOnce(&mut fillOrderRFQToCall)) -> EuclidFirmQuote {
        let quote = recorded_quote();
        let mut call = fillOrderRFQToCall::abi_decode(quote.calldata.as_ref()).unwrap();
        call.order.info =
            (call.order.info & U256::from(u64::MAX)) | (U256::from(u64::MAX) << 64usize);
        change(&mut call);
        EuclidFirmQuote { calldata: call.abi_encode().into(), ..quote }
    }

    #[test]
    fn a_recorded_frame_parses_into_validated_books() {
        let frame: EuclidLevelsFrame = serde_json::from_str(LEVELS_FRAME).unwrap();
        assert_eq!(frame.chain_id, 1);
        assert_eq!(frame.pairs.len(), 1);

        let book = EuclidBook::try_from(frame.pairs[0].clone()).unwrap();
        assert_eq!(book.base, bytes(WETH));
        assert_eq!(book.quote, bytes(USDC));
        assert!(!book.bids.is_empty() && !book.asks.is_empty());
        // The gateway publishes its minimum trade as the first level at the best price, so two
        // levels can share a price.
        assert_eq!(book.bids[0].price, book.bids[1].price);
        assert!(book.asks[0].price > book.bids[0].price);
    }

    #[test]
    fn a_pair_that_does_not_parse_is_dropped_and_the_rest_kept() {
        let frame = format!(
            r#"{{"chain_id":1,"pairs":[
                {{"base_address":"{WETH}","quote_address":"{USDC}","bids":[["not a price","1"]],"asks":[],"timestamp":1}},
                {{"base_address":"{WETH}","quote_address":"{USDC}","bids":[["3000","1"]],"asks":[["3001","1"]],"timestamp":2}}
            ]}}"#
        );
        let frame: EuclidLevelsFrame = serde_json::from_str(&frame).unwrap();
        assert_eq!(frame.pairs.len(), 1);
        assert_eq!(frame.pairs[0].timestamp, 2);
    }

    #[test]
    fn a_frame_whose_every_pair_fails_to_parse_is_an_error() {
        let frame = format!(
            r#"{{"chain_id":1,"pairs":[{{"base_address":"{WETH}","quote_address":"{USDC}","bids":[["x","1"]],"asks":[],"timestamp":1}}]}}"#
        );
        assert!(serde_json::from_str::<EuclidLevelsFrame>(&frame).is_err());
    }

    #[test]
    fn a_frame_with_no_pairs_parses_as_empty() {
        let frame: EuclidLevelsFrame =
            serde_json::from_str(r#"{"chain_id":1,"timestamp":0,"pairs":[]}"#).unwrap();
        assert!(frame.pairs.is_empty());
    }

    #[rstest]
    #[case::negative_size("3000", "-1", InvalidLevel::Quantity(-1.0))]
    #[case::zero_price("0", "1", InvalidLevel::Price(0.0))]
    fn an_invalid_level_rejects_the_book(
        #[case] price: &str,
        #[case] size: &str,
        #[case] expected: InvalidLevel,
    ) {
        let pair: EuclidPairLevels = serde_json::from_str(&format!(
            r#"{{"base_address":"{WETH}","quote_address":"{USDC}","bids":[["{price}","{size}"]],"asks":[],"timestamp":1}}"#
        ))
        .unwrap();
        assert_eq!(EuclidBook::try_from(pair).unwrap_err(), expected);
    }

    /// A pair with an invalid level costs that pair and no other; a frame with none valid fails.
    #[test]
    fn prices_keep_the_valid_pairs_of_a_frame() {
        let frame = |pairs: &str| -> EuclidLevelsFrame {
            serde_json::from_str(&format!(r#"{{"chain_id":1,"pairs":[{pairs}]}}"#)).unwrap()
        };
        let valid = format!(
            r#"{{"base_address":"{WETH}","quote_address":"{USDC}","bids":[["3000","1"]],"asks":[["3001","1"]],"timestamp":2}}"#
        );
        let invalid = format!(
            r#"{{"base_address":"{WETH}","quote_address":"{USDC}","bids":[["3000","-1"]],"asks":[],"timestamp":1}}"#
        );

        let prices = EuclidPrices::from_frame(frame(&format!("{invalid},{valid}"))).unwrap();
        assert_eq!(prices.chain_id, 1);
        assert_eq!(prices.pairs.len(), 1);
        assert_eq!(prices.pairs[0].updated_at_ms, 2);

        assert!(matches!(EuclidPrices::from_frame(frame(&invalid)), Err(FeedError::Parsing(_))));
        assert!(EuclidPrices::from_frame(frame(""))
            .unwrap()
            .pairs
            .is_empty());
    }

    #[test]
    fn prices_survive_a_serde_round_trip() {
        let frame: EuclidLevelsFrame = serde_json::from_str(LEVELS_FRAME).unwrap();
        let prices = EuclidPrices::from_frame(frame).unwrap();
        let sent = serde_json::to_string(&prices).unwrap();
        let received: EuclidPrices = serde_json::from_str(&sent).unwrap();
        assert_eq!(received, prices);
    }

    #[test]
    fn notional_tvl_is_the_mean_of_both_sides() {
        let pair: EuclidPairLevels = serde_json::from_str(&format!(
            r#"{{"base_address":"{WETH}","quote_address":"{USDC}","bids":[["3000","2"]],"asks":[["3002","1"]],"timestamp":1}}"#
        ))
        .unwrap();
        // (3000 * 2 + 3002 * 1) / 2
        assert_eq!(
            EuclidBook::try_from(pair)
                .unwrap()
                .notional_tvl(),
            4501.0
        );
    }

    #[test]
    fn a_decline_parses_as_one() {
        let response: EuclidFirmResponse =
            serde_json::from_str(r#"{"error":"insufficient_liquidity"}"#).unwrap();
        assert!(
            matches!(response, EuclidFirmResponse::Declined(d) if d.error == "insufficient_liquidity")
        );
    }

    /// The recorded quote is the trade it was requested for, and pays what its signed order
    /// says, not what the JSON beside it says.
    #[test]
    fn the_recorded_quote_pays_what_its_order_signs() {
        let amount_out = quote_with(|_| {})
            .amount_out(&recorded_params())
            .unwrap();
        assert_eq!(amount_out, BigUint::from_str(RECORDED_AMOUNT_OUT).unwrap());
    }

    /// A smaller fill of the same order pays pro rata, rounded down, as the router does.
    #[test]
    fn a_partial_fill_pays_pro_rata() {
        let half = BigUint::from_str("1500000000000000").unwrap();
        let quote = quote_with(|call| call.flagsAndAmount = U256::from(1_500_000_000_000_000u64));
        let quote = EuclidFirmQuote { amount_in: half.clone(), ..quote };
        let params = GetAmountOutParams { amount_in: half, ..recorded_params() };
        assert_eq!(
            quote.amount_out(&params).unwrap(),
            BigUint::from_str(RECORDED_AMOUNT_OUT).unwrap() / 2u8
        );
    }

    /// Every way a quote can disagree with the request it answers fails, each for its own
    /// reason.
    #[rstest]
    #[case::amount(
        |p: &mut GetAmountOutParams, _: &mut fillOrderRFQToCall| p.amount_in = BigUint::from(1u8),
        "quoted amount_in"
    )]
    #[case::token_in(
        |p: &mut GetAmountOutParams, _: &mut fillOrderRFQToCall| p.token_in = bytes(USDC),
        "order takes"
    )]
    #[case::token_out(
        |p: &mut GetAmountOutParams, _: &mut fillOrderRFQToCall| p.token_out = bytes(WETH),
        "order pays"
    )]
    #[case::receiver(
        |p: &mut GetAmountOutParams, _: &mut fillOrderRFQToCall| p.receiver = bytes(USDC),
        "delivers to"
    )]
    #[case::sender(
        |p: &mut GetAmountOutParams, _: &mut fillOrderRFQToCall| p.sender = bytes(USDC),
        "admits"
    )]
    #[case::fill_word(
        |_: &mut GetAmountOutParams, c: &mut fillOrderRFQToCall| c.flagsAndAmount = U256::from(1u8),
        "calldata fills 1,"
    )]
    #[case::expired(
        |_: &mut GetAmountOutParams, c: &mut fillOrderRFQToCall| {
            c.order.info = c.order.info & U256::from(u64::MAX) | (U256::from(1u8) << 64usize);
        },
        "expired at 1,"
    )]
    fn a_quote_that_disagrees_with_the_request_fails(
        #[case] tamper: fn(&mut GetAmountOutParams, &mut fillOrderRFQToCall),
        #[case] reason: &str,
    ) {
        let mut params = recorded_params();
        let quote = quote_with(|call| tamper(&mut params, call));
        let error = quote.amount_out(&params).unwrap_err();
        assert!(
            matches!(&error, RFQError::FatalError(message) if message.contains(reason)),
            "expected a failure about {reason:?}, got {error:?}"
        );
    }

    /// A fill of more than the order takes would revert at the router, so it fails here; so
    /// does a fill of nothing, which the router would read as the whole order.
    #[rstest]
    #[case::over_the_order(|taking: U256| taking + U256::from(1u8))]
    #[case::nothing(|_: U256| U256::ZERO)]
    fn a_fill_the_order_does_not_cover_fails(#[case] fill: fn(U256) -> U256) {
        let mut params = recorded_params();
        let quote = quote_with(|call| {
            call.flagsAndAmount = fill(call.order.takingAmount);
            params.amount_in = to_biguint(call.flagsAndAmount);
        });
        let quote = EuclidFirmQuote { amount_in: params.amount_in.clone(), ..quote };
        let error = quote.amount_out(&params).unwrap_err();
        assert!(
            matches!(&error, RFQError::FatalError(message) if message.contains("of an order taking")),
            "{error:?}"
        );
    }

    /// Calldata that is not a `fillOrderRFQTo` call, or not a whole one, fails before any field
    /// is read; an order for another router fails whatever the calldata says.
    #[rstest]
    #[case::selector(|q: &mut EuclidFirmQuote| {
        let mut calldata = q.calldata.to_vec();
        calldata[0] ^= 0xff;
        q.calldata = calldata.into();
    }, "not a fillOrderRFQTo call")]
    #[case::truncated(|q: &mut EuclidFirmQuote| {
        q.calldata = q.calldata[..4 + 9 * 32].to_vec().into();
    }, "not a fillOrderRFQTo call")]
    #[case::tx_to(|q: &mut EuclidFirmQuote| q.tx_to = bytes(USDC), "settles through")]
    fn a_quote_that_is_not_a_router_fill_fails(
        #[case] tamper: fn(&mut EuclidFirmQuote),
        #[case] reason: &str,
    ) {
        let mut quote = quote_with(|_| {});
        tamper(&mut quote);
        let error = quote
            .amount_out(&recorded_params())
            .unwrap_err();
        assert!(
            matches!(&error, RFQError::FatalError(message) if message.contains(reason)),
            "expected a failure about {reason:?}, got {error:?}"
        );
    }

    #[test]
    fn only_ethereum_is_served() {
        assert_eq!(
            EuclidSupportedChain::try_from(Chain::Ethereum),
            Ok(EuclidSupportedChain::Ethereum)
        );
        assert!(EuclidSupportedChain::try_from(Chain::Base).is_err());
        assert_eq!(EuclidSupportedChain::Ethereum.id(), 1);
    }
}
