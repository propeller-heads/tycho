//! The helper's ABI, the chains it is deployed on with their tuning, and the checks every book
//! passes before it is priced.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use alloy::{
    primitives::{address, Address, U256},
    sol,
    sol_types::SolCall,
};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tracing::warn;
use tycho_common::{models::Chain, Bytes};

use crate::{
    evm::protocol::u256_num::u256_to_biguint, pamm::protocols::elfomofi::state::Ladder,
    snapshot_feed::errors::FeedError,
};

sol! {
    /// One rung of a cumulative ladder: in total, `amountIn` of the token sold buys `amountOut`
    /// of the token bought, net of fees.
    #[derive(Debug)]
    struct CumulativeLevel {
        uint256 amountIn;
        uint256 amountOut;
    }

    /// One pair's book. Asks sell `quote` for `base`, bids sell `base` for `quote`. The balances
    /// are the vault's whole balance of each token, repeated in every book that holds it.
    #[derive(Debug)]
    struct PairOrderbook {
        address base;
        address quote;
        CumulativeLevel[] askCumulativeLevels;
        CumulativeLevel[] bidCumulativeLevels;
        uint256 balanceBase;
        uint256 balanceQuote;
    }

    /// Every book, and the block the helper read them at. On Robinhood the block number is
    /// ArbSys's `arbBlockNumber()`.
    function getAllOrderbooks() external view
        returns (PairOrderbook[] books, uint256 blockNumber, uint256 blockTimestamp);
}

/// The marker some contracts use for the native token. The helper should never name it, and a
/// book that does is skipped: the router trades ERC20s only.
const NATIVE_MARKER: Address = address!("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");

/// A chain ElFomoFi is deployed on, with the helper that serves its books and the feed's tuning
/// there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElfomofiSupportedChain {
    Base,
    Bsc,
    Robinhood,
}

impl TryFrom<Chain> for ElfomofiSupportedChain {
    type Error = FeedError;

    fn try_from(chain: Chain) -> Result<Self, Self::Error> {
        match chain {
            Chain::Base => Ok(ElfomofiSupportedChain::Base),
            Chain::Bsc => Ok(ElfomofiSupportedChain::Bsc),
            Chain::Robinhood => Ok(ElfomofiSupportedChain::Robinhood),
            Chain::Ethereum |
            Chain::Starknet |
            Chain::ZkSync |
            Chain::Arbitrum |
            Chain::Unichain |
            Chain::Polygon |
            Chain::Plasma |
            Chain::Arc |
            Chain::Elysium |
            Chain::Custom(_) |
            _ => Err(FeedError::InvalidInput(format!("ElFomoFi is not deployed on {chain}"))),
        }
    }
}

impl ElfomofiSupportedChain {
    pub fn chain(self) -> Chain {
        match self {
            ElfomofiSupportedChain::Base => Chain::Base,
            ElfomofiSupportedChain::Bsc => Chain::Bsc,
            ElfomofiSupportedChain::Robinhood => Chain::Robinhood,
        }
    }

    /// The helper serving this chain's books. Each chain has its own: on BSC, the Base helper's
    /// address holds an unrelated contract.
    pub(super) fn helper(self) -> Address {
        match self {
            ElfomofiSupportedChain::Base => address!("c1b13606fc7227f2554067afb3fb12de75c02d81"),
            ElfomofiSupportedChain::Bsc => address!("78015e3544d989f0712a0b9986cb05838c3fe06d"),
            ElfomofiSupportedChain::Robinhood => {
                address!("20d24ee45c9b338ef7acf2eede75183b9e2c9e3c")
            }
        }
    }

    /// How far behind the local clock a read's block may be before the poll is refused.
    ///
    /// The venue worsens its prices in steps as the operator's last push ages. Measured on
    /// 2026-11-27 from the timestamp of the block a poll reads, prices hold for 4 s on Base and
    /// BSC and 5 s on Robinhood. Past that, the next step costs Base and Robinhood about 25 bps,
    /// inside the encoder's default 50 bps floor, and BSC 75 bps, past it. Books empty at 14-16 s
    /// (Base), 10-12 s (BSC) and 12-16 s (Robinhood).
    ///
    /// This lag, plus [`Self::max_snapshot_age`], plus the time a transaction takes to land, should
    /// stay inside that hold. A typical fill takes about 1 s of lag, 0.5 s of snapshot age and
    /// 1 s to land, and does. One at both limits does not: on Base and Robinhood it fills about
    /// 25 bps below the quote, and on BSC it reverts on its floor. A tighter bound costs reads:
    /// read lag was p50/p99 0.66/1.43 s on Base, 1.09/2.09 s on BSC and 0.91/3.85 s on Robinhood,
    /// so 1.5 s refuses 12% of BSC reads where 2 s refuses 1.2%.
    pub(super) fn max_read_lag(self) -> Duration {
        match self {
            ElfomofiSupportedChain::Base |
            ElfomofiSupportedChain::Bsc |
            ElfomofiSupportedChain::Robinhood => Duration::from_secs(2),
        }
    }

    /// The longest a published snapshot may serve without a refresh, and the feed's default.
    /// Under the same bound as [`Self::max_read_lag`]. It cannot go much lower: a poll every
    /// second, with a p99 round trip of about 1.2 s, already leaves up to 2 s between snapshots.
    pub fn max_snapshot_age(self) -> Duration {
        match self {
            ElfomofiSupportedChain::Base |
            ElfomofiSupportedChain::Bsc |
            ElfomofiSupportedChain::Robinhood => Duration::from_secs(2),
        }
    }

    /// Gas of the protocol part of one swap: the executor and the router's
    /// `swapWithContractBalance`, as tycho-execution's gas estimator counts it.
    ///
    /// Measured by tycho-execution's `Elfomofi.t.sol` (`testGas`, every account cold) selling the
    /// base token of one pair per chain, rounded up: WETH/USDC on Base, the highest of 345,778 to
    /// 352,258 at blocks 62,300,000 to 62,863,000; ETH/USDT on BSC at 134,869,000 (469,523); and
    /// NVDA/USDG on Robinhood at 121,390,000 (375,375). Selling the quote token costs 7k to 16k
    /// less. The cost moves with the venue's state as well as its code: the same Base swap took
    /// 451,930 at 61,817,000 with the same implementation. The venue's pricing reads reference
    /// pools that differ per pair, so other pairs cost somewhat more or less; re-measure after an
    /// upgrade of the pricing contract.
    pub(super) fn swap_gas(self) -> u64 {
        match self {
            ElfomofiSupportedChain::Base => 353_000,
            ElfomofiSupportedChain::Bsc => 470_000,
            ElfomofiSupportedChain::Robinhood => 376_000,
        }
    }
}

/// One book that passed the checks, in raw units of its tokens.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PairBook {
    pub base: Bytes,
    pub quote: Bytes,
    /// Sells quote, buys base.
    pub asks: Ladder,
    /// Sells base, buys quote.
    pub bids: Ladder,
    pub balance_base: BigUint,
    pub balance_quote: BigUint,
}

/// What one helper call returned, with the books that failed a check left out.
#[derive(Debug, Clone)]
pub(super) struct HelperRead {
    pub block_number: u64,
    pub block_timestamp: u64,
    pub books: Vec<PairBook>,
}

/// Decodes the helper's answer and checks every book. A book that fails a check is skipped, and
/// so is every book of a pair the answer names more than once: which of them prices the pair
/// would be a guess.
///
/// Fails when the answer does not decode, or when it holds books and none of them passes: an
/// answer that withdraws every pair because its format changed is an outage, not an empty venue.
/// An answer with no books at all is the venue serving nothing, and succeeds.
pub(super) fn decode_orderbooks(raw: &[u8]) -> Result<HelperRead, FeedError> {
    let answer = getAllOrderbooksCall::abi_decode_returns(raw)
        .map_err(|e| FeedError::Parsing(format!("ElFomoFi getAllOrderbooks: {e}")))?;
    let block_number = to_u64(answer.blockNumber, "block number")?;
    let block_timestamp = to_u64(answer.blockTimestamp, "block timestamp")?;
    let received = answer.books.len();

    let mut rejected = Vec::new();
    let mut checked = Vec::with_capacity(received);
    for book in answer.books {
        let pair = format!("{}/{}", book.base, book.quote);
        match check_book(book) {
            Ok(book) => checked.push(book),
            Err(reason) => rejected.push(format!("{pair}: {reason}")),
        }
    }
    let books = without_repeated_pairs(checked, &mut rejected);

    if let Some(first) = rejected.first() {
        warn!(skipped = rejected.len(), received, first_reason = %first, "skipping ElFomoFi books");
    }
    if received > 0 && books.is_empty() {
        return Err(FeedError::Parsing(format!(
            "none of the {received} ElFomoFi books passed the checks; first: {}",
            rejected
                .first()
                .map(String::as_str)
                .unwrap_or_default()
        )));
    }
    Ok(HelperRead { block_number, block_timestamp, books })
}

/// The book in raw units, or why it cannot be priced.
fn check_book(book: PairOrderbook) -> Result<PairBook, String> {
    for (name, token) in [("base", book.base), ("quote", book.quote)] {
        if token == Address::ZERO || token == NATIVE_MARKER {
            return Err(format!("{name} token is {token}, not an ERC20"));
        }
    }
    if book.base == book.quote {
        return Err("base and quote are the same token".to_string());
    }
    let asks = ladder(book.askCumulativeLevels).map_err(|e| format!("asks: {e}"))?;
    let bids = ladder(book.bidCumulativeLevels).map_err(|e| format!("bids: {e}"))?;
    Ok(PairBook {
        base: Bytes::from(book.base.to_vec()),
        quote: Bytes::from(book.quote.to_vec()),
        asks,
        bids,
        balance_base: u256_to_biguint(book.balanceBase),
        balance_quote: u256_to_biguint(book.balanceQuote),
    })
}

fn ladder(levels: Vec<CumulativeLevel>) -> Result<Ladder, String> {
    Ladder::new(
        levels
            .into_iter()
            .map(|level| (u256_to_biguint(level.amountIn), u256_to_biguint(level.amountOut)))
            .collect(),
    )
}

/// `books` without any pair that appears more than once, in either orientation; each repeated
/// pair's books are added to `rejected`.
fn without_repeated_pairs(books: Vec<PairBook>, rejected: &mut Vec<String>) -> Vec<PairBook> {
    let pair_key = |book: &PairBook| {
        if book.base < book.quote {
            (book.base.clone(), book.quote.clone())
        } else {
            (book.quote.clone(), book.base.clone())
        }
    };
    let mut counts: HashMap<(Bytes, Bytes), usize> = HashMap::new();
    for book in &books {
        *counts
            .entry(pair_key(book))
            .or_default() += 1;
    }
    let repeated: HashSet<_> = counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(pair, _)| pair)
        .collect();
    let mut kept = Vec::with_capacity(books.len());
    for book in books {
        if repeated.contains(&pair_key(&book)) {
            rejected.push(format!("{}/{}: the pair appears more than once", book.base, book.quote));
        } else {
            kept.push(book);
        }
    }
    kept
}

fn to_u64(value: U256, what: &str) -> Result<u64, FeedError> {
    value
        .try_into()
        .map_err(|e| FeedError::Parsing(format!("ElFomoFi {what} {value}: {e}")))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::pamm::protocols::elfomofi::test_support::{
        big, bytes, helper_answer, ladder, recording, weth_usdc_wire, wire_book, BASE_RECORDING,
        BSC_RECORDING, CBBTC, ROBINHOOD_RECORDING, USDC, WETH,
    };

    /// One book of each recording, checked to the wei against the helper's raw answer (decoded
    /// independently with `cast`).
    #[rstest]
    #[case::base(
        BASE_RECORDING,
        (62_017_050, 1_795_593_785, 12),
        address!("b200000000000000000000c2e324d24d7eecd1fb"),
        (60_456_809, 17_146_572),
        (9_735_857_880, 34_167_438_981),
        (134_146_572, 34_167_438_987)
    )]
    #[case::bsc(
        BSC_RECORDING,
        (135_619_488, 1_795_593_787, 14),
        address!("21caef8a43163eea865baee23b9c2e327696a3bf"),
        (253_292_575_996_800_000_000, 72_000),
        (24_231_680, 48_505_176_275_990_400_000_000),
        (752_320, 59_581_064_238_861_470_876_832)
    )]
    #[case::robinhood(
        ROBINHOOD_RECORDING,
        (122_891_612, 1_795_593_786, 11),
        address!("39dbed3a2bd333467115de45665cc57f813c4571"),
        (165_515_119, 1_890_000_000_000_000_000_000),
        (657_542_294_937_822_440_589_292, 29_076_102_682),
        (236_903_547_961_879_156_240_201, 29_076_102_685)
    )]
    fn recordings_decode_to_the_wei(
        #[case] hex: &str,
        #[case] (block, timestamp, count): (u64, u64, usize),
        #[case] base: Address,
        #[case] first_ask: (u128, u128),
        #[case] last_bid: (u128, u128),
        #[case] balances: (u128, u128),
    ) {
        let read = decode_orderbooks(&recording(hex)).unwrap();

        assert_eq!(
            (read.block_number, read.block_timestamp, read.books.len()),
            (block, timestamp, count)
        );
        let book = read
            .books
            .iter()
            .find(|book| book.base == bytes(base))
            .unwrap();
        assert_eq!(book.asks.first_rung(), Some((&big(first_ask.0), &big(first_ask.1))));
        assert_eq!(book.bids.last_rung(), Some((&big(last_bid.0), &big(last_bid.1))));
        assert_eq!((&book.balance_base, &book.balance_quote), (&big(balances.0), &big(balances.1)));
    }

    #[rstest]
    #[case::zero_base(wire_book(Address::ZERO, USDC, &[], &[(1, 1)], (1, 1)))]
    #[case::native_quote(wire_book(WETH, NATIVE_MARKER, &[], &[(1, 1)], (1, 1)))]
    #[case::same_token(wire_book(WETH, WETH, &[], &[(1, 1)], (1, 1)))]
    #[case::falling_asks(wire_book(WETH, USDC, &[(2, 2), (1, 3)], &[], (1, 1)))]
    #[case::flat_bids(wire_book(WETH, USDC, &[], &[(1, 2), (2, 2)], (1, 1)))]
    fn a_book_failing_a_check_is_skipped(#[case] bad: PairOrderbook) {
        let answer = helper_answer(vec![bad, weth_usdc_wire()], 1, 1);

        let read = decode_orderbooks(&answer).unwrap();

        assert_eq!(read.books.len(), 1);
        assert_eq!(read.books[0].base, bytes(WETH));
    }

    /// Which of two books for one pair prices it would be a guess, so neither does; a pair named
    /// the other way round is the same pair.
    #[rstest]
    #[case::same_orientation(WETH, USDC)]
    #[case::flipped(USDC, WETH)]
    fn every_book_of_a_repeated_pair_is_skipped(#[case] base: Address, #[case] quote: Address) {
        let repeated = wire_book(base, quote, &[], &[(1, 1)], (1, 1));
        let other = wire_book(CBBTC, USDC, &[], &[(1, 1)], (1, 1));
        let answer = helper_answer(vec![weth_usdc_wire(), repeated, other], 1, 1);

        let read = decode_orderbooks(&answer).unwrap();

        assert_eq!(read.books.len(), 1);
        assert_eq!(read.books[0].base, bytes(CBBTC));
    }

    #[test]
    fn an_answer_whose_books_all_fail_fails_the_poll() {
        let answer = helper_answer(vec![wire_book(WETH, WETH, &[], &[(1, 1)], (1, 1))], 1, 1);

        let error = decode_orderbooks(&answer).unwrap_err();

        assert!(matches!(error, FeedError::Parsing(_)), "{error}");
    }

    #[test]
    fn an_answer_without_books_is_an_empty_venue() {
        let read = decode_orderbooks(&helper_answer(vec![], 7, 8)).unwrap();

        assert!(read.books.is_empty());
        assert_eq!((read.block_number, read.block_timestamp), (7, 8));
    }

    #[test]
    fn an_answer_that_does_not_decode_fails_the_poll() {
        let error = decode_orderbooks(&[0u8; 31]).unwrap_err();

        assert!(matches!(error, FeedError::Parsing(_)), "{error}");
    }

    #[test]
    fn a_book_keeps_its_ladders_and_balances() {
        let read = decode_orderbooks(&helper_answer(vec![weth_usdc_wire()], 1, 1)).unwrap();

        let book = &read.books[0];
        assert_eq!(book.bids, ladder(&crate::pamm::protocols::elfomofi::test_support::BIDS));
        assert_eq!(book.asks, ladder(&crate::pamm::protocols::elfomofi::test_support::ASKS));
        assert_eq!(book.quote, bytes(USDC));
    }

    #[rstest]
    #[case::base(Chain::Base, ElfomofiSupportedChain::Base)]
    #[case::bsc(Chain::Bsc, ElfomofiSupportedChain::Bsc)]
    #[case::robinhood(Chain::Robinhood, ElfomofiSupportedChain::Robinhood)]
    fn deployed_chains_resolve_both_ways(
        #[case] chain: Chain,
        #[case] supported: ElfomofiSupportedChain,
    ) {
        assert_eq!(ElfomofiSupportedChain::try_from(chain).unwrap(), supported);
        assert_eq!(supported.chain(), chain);
    }

    #[test]
    fn another_chain_is_invalid_input() {
        let error = ElfomofiSupportedChain::try_from(Chain::Ethereum).unwrap_err();

        assert!(matches!(error, FeedError::InvalidInput(_)), "{error}");
    }
}
