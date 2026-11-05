use std::time::{SystemTime, UNIX_EPOCH};

use num_bigint::BigUint;
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

/// `fillOrderRFQTo((uint256,address,address,address,address,uint256,uint256),bytes,uint256,
/// address)` on the 1inch Aggregation Router v5, the only call a binding quote may carry.
const FILL_ORDER_RFQ_TO_SELECTOR: [u8; 4] = [0x5a, 0x09, 0x98, 0x43];

/// Where each argument a quote is checked against sits, in 32-byte words after the selector:
/// the order's seven fields come inline, then the signature's offset, the fill amount and the
/// recipient.
const MAKER_ASSET_WORD: usize = 1;
const TAKER_ASSET_WORD: usize = 2;
const FLAGS_AND_AMOUNT_WORD: usize = 8;
const TARGET_WORD: usize = 9;

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

/// One pair's validated book: the bid and ask ladders between a base and a quote token.
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

/// A binding quote. `calldata` fills a maker-signed 1inch limit-order-protocol v3 RFQ order
/// through `tx_to`, and `partial_fill_offset` is the word the executor lowers when an earlier
/// leg of the route delivered less than `amount_in`.
#[serde_as]
#[derive(Debug, Clone, Deserialize)]
pub struct EuclidFirmQuote {
    pub tx_to: Bytes,
    pub calldata: Bytes,
    pub partial_fill_offset: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub amount_in: BigUint,
    #[serde_as(as = "DisplayFromStr")]
    pub amount_out: BigUint,
    /// Unix seconds after which the settlement rejects the order.
    pub expiry: u64,
    pub order_hash: Bytes,
}

/// Why the gateway declined a quote, e.g. `insufficient_liquidity`.
#[derive(Debug, Clone, Deserialize)]
pub struct EuclidDeclined {
    pub error: String,
}

impl EuclidFirmQuote {
    /// Checks that the quote is the trade `params` asked for: the amount, the call, the tokens
    /// and recipient the calldata names, the fill-amount word the executor patches, and an expiry
    /// still ahead. The executor allows exactly this call, so a quote that fails here would only
    /// revert on chain.
    pub fn validate(&self, params: &GetAmountOutParams) -> Result<(), RFQError> {
        if self.amount_in != params.amount_in {
            return Err(RFQError::FatalError(format!(
                "Euclid quoted amount_in {}, requested {}",
                self.amount_in, params.amount_in
            )));
        }
        if self.tx_to.len() != 20 {
            return Err(RFQError::FatalError(format!(
                "Euclid tx_to is not an address: {}",
                self.tx_to
            )));
        }
        let calldata = self.calldata.as_ref();
        if calldata.get(..4) != Some(&FILL_ORDER_RFQ_TO_SELECTOR[..]) {
            return Err(RFQError::FatalError("Euclid calldata is not a fillOrderRFQTo call".into()));
        }
        if self.partial_fill_offset != FLAGS_AND_AMOUNT_WORD as u64 {
            return Err(RFQError::FatalError(format!(
                "Euclid partial_fill_offset is {}, fillOrderRFQTo carries its amount in word {}",
                self.partial_fill_offset, FLAGS_AND_AMOUNT_WORD
            )));
        }
        let word = |index: usize| {
            calldata
                .get(4 + index * 32..4 + (index + 1) * 32)
                .ok_or_else(|| {
                    RFQError::FatalError(format!(
                        "Euclid calldata ends before word {index} ({} bytes)",
                        calldata.len()
                    ))
                })
        };
        let address = |index: usize| word(index).map(|word| Bytes::from(word[12..].to_vec()));

        let maker_asset = address(MAKER_ASSET_WORD)?;
        if maker_asset != params.token_out {
            return Err(RFQError::FatalError(format!(
                "Euclid order pays {maker_asset}, requested {}",
                params.token_out
            )));
        }
        let taker_asset = address(TAKER_ASSET_WORD)?;
        if taker_asset != params.token_in {
            return Err(RFQError::FatalError(format!(
                "Euclid order takes {taker_asset}, requested {}",
                params.token_in
            )));
        }
        let fill_amount = BigUint::from_bytes_be(word(FLAGS_AND_AMOUNT_WORD)?);
        if fill_amount != params.amount_in {
            return Err(RFQError::FatalError(format!(
                "Euclid calldata fills {fill_amount}, requested {}",
                params.amount_in
            )));
        }
        let target = address(TARGET_WORD)?;
        if target != params.receiver {
            return Err(RFQError::FatalError(format!(
                "Euclid order delivers to {target}, requested {}",
                params.receiver
            )));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| RFQError::FatalError("system clock is before the epoch".into()))?
            .as_secs();
        if self.expiry <= now {
            return Err(RFQError::FatalError(format!(
                "Euclid quote expired at {}, now {now}",
                self.expiry
            )));
        }
        Ok(())
    }
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
    /// The Tycho router the recorded quote was requested for, as sender and receiver.
    const ROUTER: &str = "0x1644d2477f809cc2c71bccfd6dc9497e3f83210d";

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

    /// The recorded quote, with its expiry moved into the future: the original lasted thirty
    /// seconds.
    fn recorded_quote() -> EuclidFirmQuote {
        match serde_json::from_str::<EuclidFirmResponse>(FIRM_QUOTE).unwrap() {
            EuclidFirmResponse::Quote(quote) => EuclidFirmQuote { expiry: u64::MAX, ..quote },
            EuclidFirmResponse::Declined(declined) => panic!("recorded a decline: {declined:?}"),
        }
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

    #[test]
    fn the_recorded_quote_validates_against_its_request() {
        recorded_quote()
            .validate(&recorded_params())
            .unwrap();
    }

    /// Every way a quote can disagree with the request it answers fails validation.
    #[rstest]
    #[case::amount(|p: &mut GetAmountOutParams, _: &mut EuclidFirmQuote| p.amount_in = BigUint::from(1u8))]
    #[case::token_in(|p: &mut GetAmountOutParams, _: &mut EuclidFirmQuote| p.token_in = bytes(USDC))]
    #[case::token_out(|p: &mut GetAmountOutParams, _: &mut EuclidFirmQuote| p.token_out = bytes(WETH))]
    #[case::receiver(|p: &mut GetAmountOutParams, _: &mut EuclidFirmQuote| p.receiver = bytes(USDC))]
    #[case::expired(|_: &mut GetAmountOutParams, q: &mut EuclidFirmQuote| q.expiry = 1)]
    #[case::offset(|_: &mut GetAmountOutParams, q: &mut EuclidFirmQuote| q.partial_fill_offset = 7)]
    #[case::tx_to(|_: &mut GetAmountOutParams, q: &mut EuclidFirmQuote| q.tx_to = Bytes::from(vec![1u8; 4]))]
    #[case::selector(|_: &mut GetAmountOutParams, q: &mut EuclidFirmQuote| {
        let mut calldata = q.calldata.to_vec();
        calldata[0] ^= 0xff;
        q.calldata = calldata.into();
    })]
    #[case::truncated(|_: &mut GetAmountOutParams, q: &mut EuclidFirmQuote| {
        q.calldata = q.calldata[..4 + 9 * 32].to_vec().into();
    })]
    fn a_quote_that_disagrees_with_the_request_fails(
        #[case] tamper: fn(&mut GetAmountOutParams, &mut EuclidFirmQuote),
    ) {
        let mut params = recorded_params();
        let mut quote = recorded_quote();
        tamper(&mut params, &mut quote);
        assert!(matches!(quote.validate(&params), Err(RFQError::FatalError(_))));
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
