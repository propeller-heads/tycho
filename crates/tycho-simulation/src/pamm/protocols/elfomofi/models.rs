use std::collections::HashSet;

use alloy::{primitives::U256, sol, sol_types::SolCall};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{models::Chain, Bytes};

use crate::snapshot_feed::errors::FeedError;

pub const BASE_HELPER: &str = "0xc1b13606fc7227f2554067afb3fb12de75c02d81";
pub const BSC_HELPER: &str = "0x78015e3544d989f0712a0b9986cb05838c3fe06d";
pub const ROBINHOOD_HELPER: &str = "0x20d24ee45c9b338ef7acf2eede75183b9e2c9e3c";

#[expect(clippy::wildcard_enum_match_arm, reason = "Chain is non-exhaustive across crates")]
pub fn helper_for(chain: Chain) -> Result<&'static str, FeedError> {
    match chain {
        Chain::Base => Ok(BASE_HELPER),
        Chain::Bsc => Ok(BSC_HELPER),
        Chain::Robinhood => Ok(ROBINHOOD_HELPER),
        _ => Err(unsupported_chain(chain)),
    }
}

pub(crate) fn max_snapshot_age(chain: Chain) -> Result<std::time::Duration, FeedError> {
    helper_for(chain)?;
    Ok(std::time::Duration::from_secs(2))
}

fn unsupported_chain(chain: Chain) -> FeedError {
    FeedError::InvalidInput(format!("ElFomoFi is not configured for {chain}"))
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
    pub chain: Chain,
    pub block_number: u64,
    pub block_hash: Bytes,
    pub block_timestamp: u64,
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

    /// Shared book identity with a stable token orientation.
    pub fn component_id(&self, chain: Chain) -> Result<Bytes, FeedError> {
        let (first, second) = if self.base_token < self.quote_token {
            (&self.base_token, &self.quote_token)
        } else {
            (&self.quote_token, &self.base_token)
        };
        helper_for(chain)?;
        Ok(crate::book::component::pair_component_id(super::PROTOCOL_SYSTEM, first, second))
    }
}

impl Snapshot {
    pub fn validate(&self) -> Result<(), FeedError> {
        helper_for(self.chain)?;
        self.book.validate()?;
        if self.block_hash.len() != 32 ||
            self.max_age_seconds == 0 ||
            self.block_number == 0 ||
            self.block_timestamp == 0
        {
            return Err(FeedError::Parsing("Invalid ElFomoFi snapshot metadata".into()));
        }
        Ok(())
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
    if max_age == 0 ||
        timestamp > now.saturating_add(max_age) ||
        now.saturating_sub(timestamp) >= max_age
    {
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
}

pub(crate) fn decode_orderbooks(
    chain: Chain,
    raw: &[u8],
    block_number: u64,
    block_hash: &Bytes,
    block_timestamp: u64,
    max_age_seconds: u64,
) -> Result<Vec<Snapshot>, FeedError> {
    helper_for(chain)?;
    if block_number == 0 || block_hash.len() != 32 || block_timestamp == 0 || max_age_seconds == 0 {
        return Err(FeedError::Parsing("Invalid ElFomoFi snapshot metadata".into()));
    }
    let decoded = getAllOrderbooksCall::abi_decode_returns_validate(raw)
        .map_err(|error| FeedError::Parsing(format!("Invalid ElFomoFi helper ABI: {error}")))?;
    if decoded.blockNumber != U256::from(block_number) ||
        decoded.blockTimestamp != U256::from(block_timestamp)
    {
        return Err(FeedError::Parsing(
            "ElFomoFi helper metadata differs from pinned header".into(),
        ));
    }
    let had_books = !decoded.books.is_empty();
    let mut pairs = HashSet::new();
    let mut duplicates = HashSet::new();
    let mut invalid = 0usize;
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
            max_age_seconds,
            book,
        };
        if let Err(error) = snapshot.book.validate() {
            tracing::warn!(base = %snapshot.book.base_token, quote = %snapshot.book.quote_token,
                %error, "Skipping invalid ElFomoFi book");
            invalid += 1;
            continue;
        }
        let pair = snapshot.book.component_id(chain)?;
        if !pairs.insert(pair.clone()) {
            duplicates.insert(pair);
            continue;
        }
        snapshots.push(snapshot);
    }
    if !duplicates.is_empty() {
        snapshots.retain(|snapshot| {
            !duplicates.contains(
                &snapshot
                    .book
                    .component_id(chain)
                    .expect("validated chain"),
            )
        });
    }
    if invalid > 0 || !duplicates.is_empty() {
        tracing::warn!(
            invalid_books = invalid,
            duplicate_pairs = duplicates.len(),
            "Skipping invalid ElFomoFi books"
        );
    }
    if had_books && snapshots.is_empty() {
        return Err(FeedError::Parsing("ElFomoFi helper returned books but none were valid".into()));
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
    use alloy::primitives::Address;

    use super::*;

    #[test]
    fn decodes_all_chain_helper_fixtures_without_loss_of_integer_precision() {
        let fixtures = [
            (
                Chain::Base,
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../protocols/fixtures/elfomofi/base-52898481.hex"
                )),
                52898481,
                1792586309,
                11,
            ),
            (
                Chain::Bsc,
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../protocols/fixtures/elfomofi/bsc-135271915.hex"
                )),
                135271915,
                1795437317,
                14,
            ),
            (
                Chain::Robinhood,
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../protocols/fixtures/elfomofi/robinhood-94288199.hex"
                )),
                94288199,
                1792586311,
                10,
            ),
        ];
        for (chain, fixture, number, timestamp, books) in fixtures {
            let raw = hex::decode(fixture.trim().trim_start_matches("0x")).unwrap();
            let snapshots =
                decode_orderbooks(chain, &raw, number, &Bytes::from(vec![1; 32]), timestamp, 10)
                    .unwrap();
            assert_eq!(snapshots.len(), books);
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
    fn freshness_accepts_bounded_clock_skew_and_rejects_expiry() {
        assert!(ensure_timestamp_fresh(100, 109, 10).is_ok());
        assert!(ensure_timestamp_fresh(100, 110, 10).is_err());
        assert!(ensure_timestamp_fresh(101, 100, 10).is_ok());
        assert!(ensure_timestamp_fresh(110, 100, 10).is_ok());
        assert!(ensure_timestamp_fresh(111, 100, 10).is_err());
        assert!(ensure_timestamp_fresh(100, 100, 0).is_err());
    }

    #[test]
    fn every_chain_accepts_a_block_from_the_previous_integer_second() {
        for chain in [Chain::Base, Chain::Bsc, Chain::Robinhood] {
            let age = max_snapshot_age(chain)
                .unwrap()
                .as_secs();
            assert_eq!(age, 2);
            assert!(ensure_timestamp_fresh(100, 101, age).is_ok());
            assert!(ensure_timestamp_fresh(100, 102, age).is_err());
        }
    }

    #[test]
    fn invalid_and_duplicate_pairs_leave_other_books_servable() {
        let pair = |base, quote| PairOrderbook {
            base: Address::repeat_byte(base),
            quote: Address::repeat_byte(quote),
            askCumulativeLevels: vec![CumulativeLevel {
                amountIn: U256::from(10),
                amountOut: U256::from(20),
            }],
            bidCumulativeLevels: vec![],
            balanceBase: U256::from(100),
            balanceQuote: U256::from(100),
        };
        let mut invalid_levels = pair(5, 6);
        invalid_levels.askCumulativeLevels[0].amountIn = U256::ZERO;
        let response = getAllOrderbooksReturn {
            books: vec![pair(1, 2), pair(3, 4), pair(0, 2), pair(3, 3), pair(4, 3), invalid_levels],
            blockNumber: U256::from(100),
            blockTimestamp: U256::from(1000),
        };
        let raw = getAllOrderbooksCall::abi_encode_returns(&response);
        let decode = |raw: &[u8], number| {
            decode_orderbooks(Chain::Base, raw, number, &Bytes::from(vec![1; 32]), 1000, 2)
        };
        let snapshots = decode(&raw, 100).unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].book.base_token, Bytes::from(vec![1; 20]));
        assert!(decode(&raw, 101).is_err(), "global helper/header mismatches must fail");
        assert!(decode(&[0x01], 100).is_err(), "invalid outer ABI must fail");
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
