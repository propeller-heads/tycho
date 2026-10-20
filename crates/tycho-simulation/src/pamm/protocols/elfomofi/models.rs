use std::collections::HashSet;

use alloy::{primitives::U256, sol, sol_types::SolCall};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{models::Chain, Bytes};

use crate::snapshot_feed::errors::FeedError;

pub const BASE_HELPER: &str = "0xc1b13606fc7227f2554067afb3fb12de75c02d81";
pub const BASE_ROUTER: &str = "0xf0f0f0f0fb0d738452efd03a28e8be14c76d5f73";
pub const BSC_HELPER: &str = "0x78015e3544d989f0712a0b9986cb05838c3fe06d";
pub const ROBINHOOD_HELPER: &str = "0x20d24ee45c9b338ef7acf2eede75183b9e2c9e3c";
pub const PRICING: &str = "0xffffffbb2d432b8acb4c57d556c0c721a431d038";
pub const PARTNER_ID: u64 = 299;

pub fn helper_for(chain: Chain) -> Result<&'static str, FeedError> {
    match chain {
        Chain::Base => Ok(BASE_HELPER),
        Chain::Bsc => Ok(BSC_HELPER),
        Chain::Robinhood => Ok(ROBINHOOD_HELPER),
        _ => Err(unsupported_chain(chain)),
    }
}

pub fn router_for(chain: Chain) -> Result<&'static str, FeedError> {
    match chain {
        Chain::Base | Chain::Bsc | Chain::Robinhood => Ok(BASE_ROUTER),
        _ => Err(unsupported_chain(chain)),
    }
}

pub fn pricing_for(chain: Chain) -> Result<&'static str, FeedError> {
    router_for(chain)?;
    Ok(PRICING)
}

fn unsupported_chain(chain: Chain) -> FeedError {
    FeedError::InvalidInput(format!("ElFomoFi is not configured for {chain}"))
}

fn default_chain() -> Chain {
    Chain::Base
}

/// Cumulative raw-token input and output amounts returned by the public helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Level {
    pub amount_in: BigUint,
    pub amount_out: BigUint,
}

/// Asks sell quote for base; bids sell base for quote. Fees are included in the helper's outputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Book {
    pub base_token: Bytes,
    pub quote_token: Bytes,
    pub asks: Vec<Level>,
    pub bids: Vec<Level>,
    pub base_balance: BigUint,
    pub quote_balance: BigUint,
}

/// Atomic orderbook state observed at a verified canonical full-block header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default = "default_chain")]
    pub chain: Chain,
    pub block_number: u64,
    pub block_hash: Bytes,
    pub block_timestamp: u64,
    pub observed_at: u64,
    pub max_age_seconds: u64,
    pub book: Book,
}

impl Book {
    pub fn validate(&self) -> Result<(), FeedError> {
        for address in [&self.base_token, &self.quote_token] {
            validate_address(address)?;
        }
        if self.base_token == self.quote_token {
            return Err(FeedError::Parsing("ElFomoFi pair contains the same token twice".into()));
        }
        if self.base_balance.bits() > 256 || self.quote_balance.bits() > 256 {
            return Err(FeedError::Parsing("ElFomoFi balances exceed uint256".into()));
        }
        validate_levels(&self.asks)?;
        validate_levels(&self.bids)
    }

    /// Hex-encodes the canonical ASCII router/pair identity expected by Tycho's component model.
    pub fn component_id(&self, chain: Chain) -> Result<String, FeedError> {
        let (first, second) = if self.base_token < self.quote_token {
            (&self.base_token, &self.quote_token)
        } else {
            (&self.quote_token, &self.base_token)
        };
        let router = router_for(chain)?;
        Ok(hex::encode(format!("elfomofi:{router}:{first}:{second}").as_bytes()))
    }
}

impl Snapshot {
    pub fn validate(&self) -> Result<(), FeedError> {
        router_for(self.chain)?;
        self.book.validate()?;
        if self.block_hash.len() != 32 ||
            self.max_age_seconds == 0 ||
            self.block_number == 0 ||
            self.block_timestamp == 0 ||
            self.observed_at < self.block_timestamp
        {
            return Err(FeedError::Parsing("Invalid ElFomoFi snapshot metadata".into()));
        }
        Ok(())
    }

    /// Requires both the block and the local observation to remain within the configured TTL.
    pub fn ensure_fresh(&self, now: u64) -> Result<(), FeedError> {
        ensure_timestamp_fresh(self.block_timestamp, now, self.max_age_seconds)?;
        ensure_timestamp_fresh(self.observed_at, now, self.max_age_seconds)
    }
}

pub(crate) fn validate_address(address: &Bytes) -> Result<(), FeedError> {
    if address.len() != 20 || address.as_ref() == [0; 20] || address.as_ref() == [0xee; 20] {
        return Err(FeedError::InvalidInput(format!(
            "Unsupported ElFomoFi ERC20 address: {address}"
        )));
    }
    Ok(())
}

pub(crate) fn ensure_timestamp_fresh(
    timestamp: u64,
    now: u64,
    max_age: u64,
) -> Result<(), FeedError> {
    if timestamp > now || now - timestamp >= max_age {
        return Err(FeedError::Connection(format!(
            "ElFomoFi state timestamp {timestamp} is outside the {max_age}s freshness window at {now}"
        )));
    }
    Ok(())
}

fn validate_levels(levels: &[Level]) -> Result<(), FeedError> {
    let mut previous_input = BigUint::default();
    let mut previous_output = BigUint::default();
    for level in levels {
        if level.amount_in.bits() > 256 || level.amount_out.bits() > 256 {
            return Err(FeedError::Parsing("ElFomoFi cumulative amounts exceed uint256".into()));
        }
        if level.amount_in <= previous_input || level.amount_out <= previous_output {
            return Err(FeedError::Parsing("Non-increasing ElFomoFi cumulative levels".into()));
        }
        previous_input = level.amount_in.clone();
        previous_output = level.amount_out.clone();
    }
    Ok(())
}

sol! {
    struct CumulativeLevel { uint256 amountIn; uint256 amountOut; }
    struct PairOrderbook {
        address base;
        address quote;
        CumulativeLevel[] askCumulativeLevels;
        CumulativeLevel[] bidCumulativeLevels;
        uint256 balanceBase;
        uint256 balanceQuote;
    }
    function getAllOrderbooks() external view returns (
        PairOrderbook[] books, uint256 blockNumber, uint256 blockTimestamp
    );
    function getAmountOut(address from, address to, uint256 amount, address executor, uint256 partnerId)
        external view returns (uint256 toAmount, uint256 quoteId, bytes data);
}

pub(crate) fn decode_orderbooks(
    chain: Chain,
    raw: &[u8],
    block_number: u64,
    block_hash: Bytes,
    block_timestamp: u64,
    observed_at: u64,
    max_age_seconds: u64,
) -> Result<Vec<Snapshot>, FeedError> {
    helper_for(chain)?;
    let decoded = getAllOrderbooksCall::abi_decode_returns_validate(raw)
        .map_err(|error| FeedError::Parsing(format!("Invalid ElFomoFi helper ABI: {error}")))?;
    if decoded.blockNumber != U256::from(block_number) ||
        decoded.blockTimestamp != U256::from(block_timestamp)
    {
        return Err(FeedError::Parsing(
            "ElFomoFi helper metadata differs from pinned header".into(),
        ));
    }
    let mut pairs = HashSet::new();
    let mut snapshots = Vec::with_capacity(decoded.books.len());
    for raw_book in decoded.books {
        let book = Book {
            base_token: Bytes::from(raw_book.base.as_slice()),
            quote_token: Bytes::from(raw_book.quote.as_slice()),
            asks: levels(raw_book.askCumulativeLevels),
            bids: levels(raw_book.bidCumulativeLevels),
            base_balance: big(raw_book.balanceBase),
            quote_balance: big(raw_book.balanceQuote),
        };
        let snapshot = Snapshot {
            chain,
            block_number,
            block_hash: block_hash.clone(),
            block_timestamp,
            observed_at,
            max_age_seconds,
            book,
        };
        snapshot.validate()?;
        if !pairs.insert(snapshot.book.component_id(chain)?) {
            return Err(FeedError::Parsing(
                "Duplicate ElFomoFi unordered pair in helper response".into(),
            ));
        }
        snapshots.push(snapshot);
    }
    Ok(snapshots)
}

fn levels(raw: Vec<CumulativeLevel>) -> Vec<Level> {
    raw.into_iter()
        .map(|level| Level { amount_in: big(level.amountIn), amount_out: big(level.amountOut) })
        .collect()
}

fn big(value: U256) -> BigUint {
    BigUint::from_bytes_be(&value.to_be_bytes::<32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_all_chain_helper_fixtures_without_loss_of_integer_precision() {
        let fixtures = [
            (
                Chain::Base,
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../protocols/fixtures/elfomofi/base-51169738.hex"
                )),
                51_169_738,
                1_789_128_823,
                10,
            ),
            (
                Chain::Bsc,
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../protocols/fixtures/elfomofi/bsc-125861977.hex"
                )),
                125_861_977,
                1_791_200_531,
                12,
            ),
            (
                Chain::Robinhood,
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../protocols/fixtures/elfomofi/robinhood-80758189.hex"
                )),
                80_758_189,
                1_791_200_532,
                9,
            ),
        ];
        for (chain, fixture, number, timestamp, books) in fixtures {
            let raw = hex::decode(fixture.trim().trim_start_matches("0x")).unwrap();
            let snapshots = decode_orderbooks(
                chain,
                &raw,
                number,
                Bytes::from(vec![1; 32]),
                timestamp,
                timestamp + 1,
                10,
            )
            .unwrap();
            assert_eq!(snapshots.len(), books);
            if chain == Chain::Base {
                assert_eq!(
                    snapshots
                        .iter()
                        .filter(|snapshot| snapshot.book.asks.is_empty() &&
                            snapshot.book.bids.is_empty())
                        .count(),
                    2
                );
            }
            for snapshot in snapshots {
                snapshot.validate().unwrap();
                assert_eq!(snapshot.chain, chain);
                assert_eq!(
                    serde_json::from_slice::<Snapshot>(&serde_json::to_vec(&snapshot).unwrap())
                        .unwrap(),
                    snapshot
                );
            }
        }
    }

    #[test]
    fn freshness_rejects_deadline_boundary_and_future_timestamps() {
        assert!(ensure_timestamp_fresh(100, 109, 10).is_ok());
        assert!(ensure_timestamp_fresh(100, 110, 10).is_err());
        assert!(ensure_timestamp_fresh(101, 100, 10).is_err());
    }

    #[test]
    fn book_rejects_non_monotonic_and_uint256_overflow_amounts() {
        let mut book = Book {
            base_token: Bytes::from(vec![1; 20]),
            quote_token: Bytes::from(vec![2; 20]),
            asks: vec![Level { amount_in: BigUint::from(10u8), amount_out: BigUint::from(20u8) }],
            bids: Vec::new(),
            base_balance: BigUint::from(10u8),
            quote_balance: BigUint::from(20u8),
        };
        assert!(book.validate().is_ok());
        book.asks.push(book.asks[0].clone());
        assert!(book.validate().is_err());
        book.asks.pop();
        book.base_balance = BigUint::from(1u8) << 256;
        assert!(book.validate().is_err());
    }
}
