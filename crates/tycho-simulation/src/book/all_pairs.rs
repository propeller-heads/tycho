//! A venue published as one component for all of its pairs.
//!
//! A feed publishes a book per pair by default, which is what a consumer routes through. A venue
//! that signs a quote per trade can only sign so many for one route, and a consumer that carries
//! simulated state from one swap to the next does so per component: so a venue can instead be
//! published as one component covering every pair it prices, where a swap records the book it
//! used and the next swap on the state it returned is held to the venue's [`QuoteRule`].
//!
//! The books are the ones the per-pair layout would publish. Picking a book is picking among
//! them, every price comes from one of them, and a quote goes to the one that priced the swap.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use chrono::{DateTime, Utc};
use num_bigint::BigUint;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tycho_common::{
    models::{token::Token, Chain},
    simulation::{
        errors::SimulationError,
        protocol_sim::{GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::{
    book::{component::PairState, Book},
    protocol::models::ProtocolComponent,
};

/// Static attribute: the directions the venue quotes, 40 bytes each, the token sold first.
pub const SWAP_DIRECTIONS_ATTRIBUTE: &str = "swap_directions";

/// Which components a feed publishes for its venue, as its builder settled it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComponentLayout {
    /// One component per pair the venue prices, each priced by that pair's own book.
    PerPair,
    /// One component for every pair the venue prices, priced by all of its books at once and
    /// holding a route to this rule.
    AllPairs(QuoteRule),
}

/// How often one route may take a quote from a venue published as [`ComponentLayout::AllPairs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuoteRule {
    /// Each market maker signs once. For a venue that names its makers and lets the taker pick
    /// one: a maker's second quote prices as if the first had not happened, so a route that took
    /// both would count the same liquidity twice.
    OncePerMaker,
    /// The venue signs once, whoever is behind the quote.
    OncePerVenue,
}

impl QuoteRule {
    /// Static attribute: the rule a component's books are quoted under.
    pub const ATTRIBUTE: &'static str = "quote_rule";

    const ALL: [QuoteRule; 2] = [QuoteRule::OncePerMaker, QuoteRule::OncePerVenue];

    /// The rule a component is published under, which is how often a route may quote it.
    /// `None` for a component that names no rule, which is one component of one pair.
    pub fn from_attributes(
        attributes: &HashMap<String, Bytes>,
    ) -> Result<Option<QuoteRule>, String> {
        let Some(value) = attributes.get(Self::ATTRIBUTE) else {
            return Ok(None);
        };
        Self::ALL
            .into_iter()
            .find(|rule| rule.as_str().as_bytes() == value.as_ref())
            .map(Some)
            .ok_or_else(|| {
                format!("Unknown quote_rule attribute: {}", String::from_utf8_lossy(value))
            })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            QuoteRule::OncePerMaker => "once_per_maker",
            QuoteRule::OncePerVenue => "once_per_venue",
        }
    }

    /// Whether a book of `maker` may still be quoted, given what `used` already took.
    fn allows(self, used: &HashSet<String>, maker: Option<&str>) -> bool {
        match self {
            QuoteRule::OncePerMaker => !used.contains(maker.unwrap_or_default()),
            QuoteRule::OncePerVenue => used.is_empty(),
        }
    }

    /// What a swap against `maker` spends, which is what the next swap is held to.
    fn spend(self, maker: Option<&str>) -> String {
        match self {
            QuoteRule::OncePerMaker => maker.unwrap_or_default().to_string(),
            QuoteRule::OncePerVenue => String::new(),
        }
    }
}

/// One of the books a venue publishes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct VenueBook<S> {
    /// The market maker whose ladders these are, where the venue names one. Books without a
    /// maker count as one and the same under [`QuoteRule::OncePerMaker`].
    pub maker: Option<String>,
    /// When the venue last updated this book, as the per-pair layout would publish it.
    pub updated_at: Option<DateTime<Utc>>,
    pub state: S,
}

/// Every book a venue publishes, behind one component.
///
/// A swap goes to the book that pays best among those still allowed to quote, and the state it
/// returns records that book as spent. Nothing here prices anything itself.
///
/// The books are shared, so what a swap copies to hand back the state it leaves behind is a
/// reference count and the set of books it has spent.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound = "S: Serialize + DeserializeOwned + PairState")]
pub(crate) struct AllPairs<S> {
    books: Arc<Books<S>>,
    /// What [`QuoteRule::spend`] returned for every swap this state is downstream of.
    used: HashSet<String>,
    rule: QuoteRule,
}

/// A venue's books and, for each direction, which of them price it.
///
/// Serialized as the books alone: the index is derived from them, and is built again whenever
/// a set of books comes from anywhere other than a source.
#[derive(Debug, Deserialize)]
#[serde(from = "Vec<VenueBook<S>>", bound = "S: DeserializeOwned + PairState")]
struct Books<S> {
    books: Vec<VenueBook<S>>,
    /// Indices into `books`, by token sold and then token bought, so a swap looks only at the
    /// books that price it and a lookup costs no allocation.
    by_direction: HashMap<Bytes, HashMap<Bytes, Vec<usize>>>,
}

impl<S: Serialize> Serialize for Books<S> {
    fn serialize<W: serde::Serializer>(&self, serializer: W) -> Result<W::Ok, W::Error> {
        self.books.serialize(serializer)
    }
}

impl<S: PairState> From<Vec<VenueBook<S>>> for Books<S> {
    fn from(books: Vec<VenueBook<S>>) -> Self {
        let mut by_direction: HashMap<Bytes, HashMap<Bytes, Vec<usize>>> = HashMap::new();
        let mut kept: Vec<VenueBook<S>> = Vec::new();
        for book in books {
            // A book that prices nothing is left out rather than held: it would put its tokens
            // on the component while naming no way to trade them, and a consumer routing
            // through such a pair only learns it cannot at simulation time.
            let directions = book.state.directions();
            if directions.is_empty() {
                continue;
            }
            let index = kept.len();
            for (sold, bought) in directions {
                by_direction
                    .entry(sold)
                    .or_default()
                    .entry(bought)
                    .or_default()
                    .push(index);
            }
            kept.push(book);
        }
        Books { books: kept, by_direction }
    }
}

impl<S: PairState> AllPairs<S> {
    pub fn new(books: Vec<VenueBook<S>>, rule: QuoteRule) -> Self {
        AllPairs { books: Arc::new(books.into()), used: HashSet::new(), rule }
    }

    pub fn rule(&self) -> QuoteRule {
        self.rule
    }

    /// Every direction the venue prices, which is what its component advertises.
    pub fn directions(&self) -> BTreeSet<(Bytes, Bytes)> {
        self.books
            .by_direction
            .iter()
            .flat_map(|(sold, bought)| {
                bought
                    .keys()
                    .map(|bought| (sold.clone(), bought.clone()))
            })
            .collect()
    }

    pub fn books(&self) -> &[VenueBook<S>] {
        &self.books.books
    }

    /// When the venue last updated all of this: the oldest of its books' times, so the
    /// component is no fresher than the stalest book behind it, and none at all once a book
    /// reports none.
    fn updated_at(&self) -> Option<DateTime<Utc>> {
        self.books()
            .iter()
            .map(|book| book.updated_at)
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .min()
    }

    /// The books that price `token_in` into `token_out`, spent or not.
    pub fn pricing(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> impl Iterator<Item = &VenueBook<S>> {
        self.books
            .by_direction
            .get(token_in)
            .and_then(|bought| bought.get(token_out))
            .into_iter()
            .flatten()
            .map(|index| &self.books.books[*index])
    }

    /// The books that price `token_in` into `token_out` and may still quote.
    ///
    /// The error says which of the two it is: a direction the venue does not price is the same
    /// invalid input it is for a per-pair book, while books whose turn this route has spent are
    /// liquidity that exists and is unavailable here.
    fn candidates(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<impl Iterator<Item = &VenueBook<S>>, SimulationError> {
        let mut pricing = self
            .pricing(token_in, token_out)
            .peekable();
        if pricing.peek().is_none() {
            return Err(SimulationError::InvalidInput(
                format!("Venue prices no book from {token_in} into {token_out}"),
                None,
            ));
        }
        let mut quotable = pricing
            .filter(|book| {
                self.rule
                    .allows(&self.used, book.maker.as_deref())
            })
            .peekable();
        if quotable.peek().is_none() {
            return Err(SimulationError::RecoverableError(
                "Every book pricing this swap already quoted in this route".to_string(),
            ));
        }
        Ok(quotable)
    }

    /// The same books with the turn of `maker`'s book spent.
    fn spending(&self, maker: Option<&str>) -> Self {
        let mut used = self.used.clone();
        used.insert(self.rule.spend(maker));
        AllPairs { books: Arc::clone(&self.books), used, rule: self.rule }
    }
}

impl<S: PairState> VenueBook<S> {
    /// The book's own `Token`s for a swap that sells `token_in`. A state prices the pair it
    /// names, so it is given that pair rather than the caller's copy of the two tokens.
    fn tokens(&self, token_in: &Bytes) -> (&Token, &Token) {
        let (base, quote) = (self.state.base_token(), self.state.quote_token());
        if token_in == &base.address {
            (base, quote)
        } else {
            (quote, base)
        }
    }
}

/// How good an answer to a swap is: a complete fill beats a partial one, a partial fill beats a
/// refusal, and more bought beats less.
fn rank(answer: &Result<GetAmountOutResult, SimulationError>) -> (u8, Option<&BigUint>) {
    match answer {
        Ok(result) => (2, Some(&result.amount)),
        Err(SimulationError::InvalidInput(_, Some(partial))) => (1, Some(&partial.amount)),
        Err(_) => (0, None),
    }
}

impl<S: PairState> AllPairs<S> {
    /// The book that answers `amount_in` best, and its answer.
    fn best(
        &self,
        amount_in: &BigUint,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<(&VenueBook<S>, Result<GetAmountOutResult, SimulationError>), SimulationError> {
        let mut best: Option<(&VenueBook<S>, Result<GetAmountOutResult, SimulationError>)> = None;
        for book in self.candidates(token_in, token_out)? {
            let (sold, bought) = book.tokens(token_in);
            let answer = book
                .state
                .get_amount_out(amount_in.clone(), sold, bought);
            if best
                .as_ref()
                .is_none_or(|(_, taken)| rank(taken) < rank(&answer))
            {
                best = Some((book, answer));
            }
        }
        best.ok_or_else(|| {
            SimulationError::FatalError("A book that prices a swap gives no answer".to_string())
        })
    }

    /// The best price among the books that may still quote `base` into `quote`.
    pub fn spot_price(&self, base: &Bytes, quote: &Bytes) -> Result<f64, SimulationError> {
        let mut best: Option<Result<f64, SimulationError>> = None;
        for book in self.candidates(base, quote)? {
            let (base, quote) = book.tokens(base);
            let price = book.state.spot_price(base, quote);
            if best
                .as_ref()
                .is_none_or(|taken| taken.as_ref().ok() < price.as_ref().ok())
            {
                best = Some(price);
            }
        }
        best.unwrap_or_else(|| {
            Err(SimulationError::FatalError("A book that prices a pair has no price".to_string()))
        })
    }

    /// The limits of the book that buys most in total among those that may still quote the
    /// direction. A swap fills against one book, so the venue's limit is that book's, not the
    /// sum of every book's.
    pub fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let mut best: Option<Result<(BigUint, BigUint), SimulationError>> = None;
        for book in self.candidates(&sell_token, &buy_token)? {
            let limits = book
                .state
                .get_limits(sell_token.clone(), buy_token.clone());
            let bought = |limits: &Result<(BigUint, BigUint), SimulationError>| {
                limits
                    .as_ref()
                    .ok()
                    .map(|(_, bought)| bought.clone())
            };
            if best
                .as_ref()
                .is_none_or(|taken| bought(taken) < bought(&limits))
            {
                best = Some(limits);
            }
        }
        best.unwrap_or_else(|| {
            Err(SimulationError::FatalError("A book that prices a pair has no limits".to_string()))
        })
    }
}

impl<S: PairState> AllPairs<S> {
    /// Whether these are the same books, in the same order, with the same turns spent.
    pub fn same_books(&self, other: &Self) -> bool {
        self.rule == other.rule &&
            self.used == other.used &&
            self.books().len() == other.books().len() &&
            self.books()
                .iter()
                .zip(other.books().iter())
                .all(|(ours, theirs)| ours.maker == theirs.maker && ours.state.eq(&theirs.state))
    }
}

/// A venue's state under [`ComponentLayout::AllPairs`]: the wrapper a venue registers with
/// `#[typetag::serde]`, which [`AllPairs`] itself cannot be — typetag registers concrete types,
/// and a generic has as many as it has parameters.
///
/// Implementing it is naming the books and how to put a set of them back into the venue's own
/// state; the swap and the quote routing come with it.
pub(crate) trait AllPairsState: ProtocolSim + Clone + Sized + 'static {
    /// The per-pair state this venue's books are, the same one its `PerPair` layout publishes.
    type Pair: PairState;

    fn pairs(&self) -> &AllPairs<Self::Pair>;

    fn with_pairs(&self, pairs: AllPairs<Self::Pair>) -> Self;

    /// The swap against the best book available, returning the state that book has quoted in.
    ///
    /// The returned state replaces the book's own, including the one attached to a partial fill:
    /// what a consumer carries on is the venue, not the single book it just used.
    fn amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let (book, answer) = self
            .pairs()
            .best(&amount_in, token_in, token_out)?;
        let quoted = || -> Box<dyn ProtocolSim> {
            Box::new(
                self.with_pairs(
                    self.pairs()
                        .spending(book.maker.as_deref()),
                ),
            )
        };
        match answer {
            Ok(mut result) => {
                result.new_state = quoted();
                Ok(result)
            }
            Err(SimulationError::InvalidInput(message, Some(mut partial))) => {
                partial.new_state = quoted();
                Err(SimulationError::InvalidInput(message, Some(partial)))
            }
            Err(refused) => Err(refused),
        }
    }

    /// Whether `other` is this venue's books, which is what a venue's `eq` compares.
    fn same_books(&self, other: &dyn ProtocolSim) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|other| self.pairs().same_books(other.pairs()))
    }

    /// The book a quote for this swap goes to: the one [`Self::amount_out`] picks on this state.
    ///
    /// Asked on the state a swap returned, the book it used is spent, so the quote names another
    /// book or the state reports that there is none.
    fn quoting_book(
        &self,
        amount_in: &BigUint,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<&Self::Pair, SimulationError> {
        let (book, _) = self
            .pairs()
            .best(amount_in, token_in, token_out)?;
        Ok(&book.state)
    }
}

/// The id of a venue's component covering every pair it prices: its protocol system, which
/// names both the venue and the path its swaps settle through. A feed serves one chain and a
/// consumer keys its components within one, so the venue is the whole of this component's
/// identity, where a pair's id needs its two tokens as well.
pub(crate) fn all_pairs_component_id(protocol_system: &str) -> Bytes {
    Bytes::from(protocol_system.as_bytes())
}

pub(crate) fn encode_swap_directions(directions: &BTreeSet<(Bytes, Bytes)>) -> Bytes {
    let mut encoded = Vec::with_capacity(directions.len() * 40);
    for (token_in, token_out) in directions {
        encoded.extend_from_slice(token_in);
        encoded.extend_from_slice(token_out);
    }
    encoded.into()
}

/// The directions a component advertises in its [`SWAP_DIRECTIONS_ATTRIBUTE`].
pub fn decode_swap_directions(attribute: &[u8]) -> Result<Vec<(Bytes, Bytes)>, String> {
    let (directions, rest) = attribute.as_chunks::<40>();
    if !rest.is_empty() {
        return Err(format!(
            "Swap directions attribute holds {} bytes, not a multiple of 40",
            attribute.len()
        ));
    }
    Ok(directions
        .iter()
        .map(|direction| (Bytes::from(&direction[..20]), Bytes::from(&direction[20..])))
        .collect())
}

/// The venue's one book, priced by `state`: every token its books name, the directions they
/// price and the rule they are quoted under.
pub(crate) fn all_pairs_book<W: AllPairsState>(
    protocol_system: &str,
    protocol_type: &str,
    chain: Chain,
    state: W,
) -> Book {
    let pairs = state.pairs();
    let tokens: BTreeMap<Bytes, Token> = pairs
        .books()
        .iter()
        .flat_map(|book| [book.state.base_token(), book.state.quote_token()])
        .map(|token| (token.address.clone(), token.clone()))
        .collect();
    let directions = pairs.directions();
    let updated_at = pairs.updated_at();
    let component = ProtocolComponent::new(
        all_pairs_component_id(protocol_system),
        protocol_system.to_string(),
        protocol_type.to_string(),
        chain,
        tokens.into_values().collect(),
        vec![],
        HashMap::from([
            (SWAP_DIRECTIONS_ATTRIBUTE.to_string(), encode_swap_directions(&directions)),
            (QuoteRule::ATTRIBUTE.to_string(), pairs.rule().as_str().as_bytes().into()),
        ]),
        Default::default(),
        Default::default(),
    );
    Book { component, state: Arc::new(state), updated_at }
}

#[cfg(test)]
mod tests {
    use std::any::Any;

    use rstest::rstest;
    use tycho_common::{
        dto::ProtocolStateDelta,
        simulation::{errors::TransitionError, protocol_sim::Balances},
    };

    use super::*;
    use crate::book::{sim, test_token_map};

    /// A book that pays `out_per_in` for every whole token up to `depth` of them, and stops
    /// there. Both its tokens have 18 decimals, so a whole token is `1e18` either side.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct TestBook {
        base: Token,
        quote: Token,
        out_per_in: u64,
        depth: u64,
    }

    impl PairState for TestBook {
        fn base_token(&self) -> &Token {
            &self.base
        }

        fn quote_token(&self) -> &Token {
            &self.quote
        }

        /// A book paying nothing per token has no levels and prices neither way; `depth` of
        /// zero is a priced direction with nothing behind it.
        fn directions(&self) -> Vec<(Bytes, Bytes)> {
            sim::directions(&self.base, &self.quote, self.out_per_in > 0, false)
        }
    }

    #[typetag::serde]
    impl ProtocolSim for TestBook {
        fn fee(&self) -> f64 {
            0.0
        }

        fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
            Ok(self.out_per_in as f64)
        }

        fn get_amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            _token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            assert_eq!(token_in.address, self.base.address, "a book is given its own tokens");
            let filled = amount_in.clone().min(whole(self.depth));
            let result = GetAmountOutResult {
                amount: &filled * BigUint::from(self.out_per_in),
                gas: BigUint::from(100u64),
                new_state: self.clone_box(),
            };
            if filled == amount_in {
                Ok(result)
            } else {
                Err(SimulationError::InvalidInput("Out of depth".to_string(), Some(result)))
            }
        }

        fn get_limits(
            &self,
            _sell_token: Bytes,
            _buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            Ok((whole(self.depth), whole(self.depth * self.out_per_in)))
        }

        fn delta_transition(
            &mut self,
            _delta: ProtocolStateDelta,
            _tokens: &HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError> {
            unimplemented!("a test book never transitions")
        }

        fn clone_box(&self) -> Box<dyn ProtocolSim> {
            Box::new(self.clone())
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }

        fn eq(&self, other: &dyn ProtocolSim) -> bool {
            other
                .as_any()
                .downcast_ref::<TestBook>()
                .is_some_and(|other| {
                    self.out_per_in == other.out_per_in && self.depth == other.depth
                })
        }
    }

    /// A venue publishing [`TestBook`]s under one component.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct TestVenue(AllPairs<TestBook>);

    impl AllPairsState for TestVenue {
        type Pair = TestBook;

        fn pairs(&self) -> &AllPairs<TestBook> {
            &self.0
        }

        fn with_pairs(&self, pairs: AllPairs<TestBook>) -> Self {
            TestVenue(pairs)
        }
    }

    #[typetag::serde]
    impl ProtocolSim for TestVenue {
        fn fee(&self) -> f64 {
            0.0
        }

        fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
            self.0
                .spot_price(&base.address, &quote.address)
        }

        fn get_amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            self.amount_out(amount_in, &token_in.address, &token_out.address)
        }

        fn get_limits(
            &self,
            sell_token: Bytes,
            buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            self.0.get_limits(sell_token, buy_token)
        }

        fn delta_transition(
            &mut self,
            _delta: ProtocolStateDelta,
            _tokens: &HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError> {
            unimplemented!("a test venue never transitions")
        }

        fn clone_box(&self) -> Box<dyn ProtocolSim> {
            Box::new(self.clone())
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }

        fn eq(&self, other: &dyn ProtocolSim) -> bool {
            self.same_books(other)
        }
    }

    /// `count` whole tokens of 18 decimals.
    fn whole(count: u64) -> BigUint {
        BigUint::from(count) * BigUint::from(10u64).pow(18)
    }

    fn addr(byte: u8) -> Bytes {
        Bytes::from(vec![byte; 20])
    }

    /// Three tokens, A, B and C, all with 18 decimals.
    fn tokens() -> HashMap<Bytes, Token> {
        test_token_map(&[(&addr(0xaa), "A", 18), (&addr(0xbb), "B", 18), (&addr(0xcc), "C", 18)])
    }

    /// A book of `maker` paying `out_per_in` B for every A up to `depth` of them.
    fn book(maker: &str, out_per_in: u64, depth: u64) -> VenueBook<TestBook> {
        let tokens = tokens();
        VenueBook {
            maker: Some(maker.to_string()),
            updated_at: None,
            state: TestBook {
                base: tokens[&addr(0xaa)].clone(),
                quote: tokens[&addr(0xbb)].clone(),
                out_per_in,
                depth,
            },
        }
    }

    fn venue(books: Vec<VenueBook<TestBook>>, rule: QuoteRule) -> TestVenue {
        TestVenue(AllPairs::new(books, rule))
    }

    /// The swap goes to the book paying most, whoever that is.
    #[test]
    fn the_best_paying_book_takes_the_swap() {
        let venue = venue(
            vec![book("cheap", 2, 10), book("generous", 3, 10), book("shallow", 9, 0)],
            QuoteRule::OncePerMaker,
        );

        let swap = venue
            .amount_out(whole(1), &addr(0xaa), &addr(0xbb))
            .unwrap();

        assert_eq!(swap.amount, whole(3));
    }

    /// A book that fills part of the amount beats one that refuses outright, and the shortfall
    /// is reported with what it would pay.
    #[test]
    fn a_partial_fill_beats_a_refusal() {
        let venue =
            venue(vec![book("shallow", 2, 1), book("empty", 5, 0)], QuoteRule::OncePerMaker);

        match venue.amount_out(whole(4), &addr(0xaa), &addr(0xbb)) {
            Err(SimulationError::InvalidInput(_, Some(partial))) => {
                assert_eq!(partial.amount, whole(2));
            }
            other => panic!("expected the partially filled result, got {other:?}"),
        }
    }

    /// What one swap spends is what the next is held to: another maker may still quote where
    /// the venue as a whole may not, and the partial fill's state is the venue's, not the book's.
    #[rstest]
    #[case::once_per_maker(QuoteRule::OncePerMaker, Some(2))]
    #[case::once_per_venue(QuoteRule::OncePerVenue, None)]
    fn the_second_swap_is_held_to_the_rule(
        #[case] rule: QuoteRule,
        #[case] second_pays: Option<u64>,
    ) {
        let venue = venue(vec![book("generous", 3, 10), book("cheap", 2, 10)], rule);

        let first = venue
            .amount_out(whole(1), &addr(0xaa), &addr(0xbb))
            .unwrap();
        assert_eq!(first.amount, whole(3));

        let second = first.new_state.get_amount_out(
            whole(1),
            &tokens()[&addr(0xaa)],
            &tokens()[&addr(0xbb)],
        );
        match (second, second_pays) {
            (Ok(second), Some(pays)) => assert_eq!(second.amount, whole(pays)),
            (Err(SimulationError::RecoverableError(message)), None) => {
                assert_eq!(message, "Every book pricing this swap already quoted in this route")
            }
            (second, _) => panic!("expected a second swap paying {second_pays:?}, got {second:?}"),
        }
    }

    /// The state a partial fill carries on is the venue's, so what the route learns about the
    /// quote it spent survives a shortfall.
    #[test]
    fn a_partial_fill_carries_the_venue_on() {
        let venue = venue(vec![book("shallow", 2, 1)], QuoteRule::OncePerVenue);

        let Err(SimulationError::InvalidInput(_, Some(partial))) =
            venue.amount_out(whole(4), &addr(0xaa), &addr(0xbb))
        else {
            panic!("expected a partial fill");
        };
        let after = partial.new_state.get_amount_out(
            whole(1),
            &tokens()[&addr(0xaa)],
            &tokens()[&addr(0xbb)],
        );

        assert!(matches!(after, Err(SimulationError::RecoverableError(_))), "got {after:?}");
    }

    /// A direction no book prices is the invalid input it is for a per-pair book, not an
    /// exhausted venue.
    #[test]
    fn a_direction_the_venue_does_not_price_is_invalid_input() {
        let venue = venue(vec![book("generous", 3, 10)], QuoteRule::OncePerMaker);

        let reverse = venue.amount_out(whole(1), &addr(0xbb), &addr(0xaa));
        let unknown = venue.amount_out(whole(1), &addr(0xaa), &addr(0xcc));

        assert!(matches!(reverse, Err(SimulationError::InvalidInput(_, None))), "got {reverse:?}");
        assert!(matches!(unknown, Err(SimulationError::InvalidInput(_, None))), "got {unknown:?}");
    }

    /// Spot price and limits come from the best book available, and a spent venue reports
    /// neither.
    #[test]
    fn the_price_and_the_limits_are_the_best_books() {
        // The cheap book takes more A in total, the generous one buys more B with what it takes.
        let venue =
            venue(vec![book("cheap", 2, 10), book("generous", 3, 8)], QuoteRule::OncePerVenue);

        assert_eq!(
            venue
                .0
                .spot_price(&addr(0xaa), &addr(0xbb))
                .unwrap(),
            3.0
        );
        assert_eq!(
            venue
                .0
                .get_limits(addr(0xaa), addr(0xbb))
                .unwrap(),
            (whole(8), whole(24)),
            "the limits of the book that buys most, not the sum of both"
        );

        let spent = venue
            .amount_out(whole(1), &addr(0xaa), &addr(0xbb))
            .unwrap()
            .new_state;
        assert!(matches!(
            spent.get_limits(addr(0xaa), addr(0xbb)),
            Err(SimulationError::RecoverableError(_))
        ));
    }

    /// A consumer reads the directions off the component rather than pairing up its tokens: the
    /// venue prices A into B and B into C, but nothing between A and C.
    #[test]
    fn the_component_advertises_the_directions_and_the_rule() {
        let tokens = tokens();
        let b_to_c = VenueBook {
            maker: None,
            updated_at: None,
            state: TestBook {
                base: tokens[&addr(0xbb)].clone(),
                quote: tokens[&addr(0xcc)].clone(),
                out_per_in: 1,
                depth: 1,
            },
        };

        let book = all_pairs_book(
            "rfq:venue",
            "venue_pool",
            Chain::Ethereum,
            venue(vec![book("generous", 3, 10), b_to_c], QuoteRule::OncePerMaker),
        );

        // "rfq:venue" in ASCII.
        assert_eq!(book.component.id.to_string(), "0x7266713a76656e7565");
        assert_eq!(
            book.component
                .tokens
                .iter()
                .map(|token| token.symbol.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B", "C"]
        );
        let directions = &book.component.static_attributes[SWAP_DIRECTIONS_ATTRIBUTE];
        assert_eq!(
            decode_swap_directions(directions).unwrap(),
            vec![(addr(0xaa), addr(0xbb)), (addr(0xbb), addr(0xcc))]
        );
        assert_eq!(
            book.component.static_attributes[QuoteRule::ATTRIBUTE],
            Bytes::from("once_per_maker".as_bytes())
        );
    }

    /// A book with nothing on it is not published at all: it would put its tokens on the
    /// component while naming no way to trade them, which is a route that only fails once it
    /// is simulated. What the component carries is what its directions name.
    #[test]
    fn a_book_that_prices_nothing_is_left_out() {
        let tokens = tokens();
        let silent = VenueBook {
            maker: None,
            updated_at: None,
            state: TestBook {
                base: tokens[&addr(0xbb)].clone(),
                quote: tokens[&addr(0xcc)].clone(),
                out_per_in: 0,
                depth: 10,
            },
        };

        let book = all_pairs_book(
            "rfq:venue",
            "venue_pool",
            Chain::Ethereum,
            venue(vec![book("generous", 3, 10), silent], QuoteRule::OncePerMaker),
        );

        let directions =
            decode_swap_directions(&book.component.static_attributes[SWAP_DIRECTIONS_ATTRIBUTE])
                .unwrap();
        assert_eq!(directions, vec![(addr(0xaa), addr(0xbb))]);
        assert_eq!(
            book.component
                .tokens
                .iter()
                .map(|token| token.symbol.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B"]
        );
        assert_eq!(
            directions
                .iter()
                .flat_map(|(sold, bought)| [sold.clone(), bought.clone()])
                .collect::<BTreeSet<_>>(),
            book.component
                .tokens
                .iter()
                .map(|token| token.address.clone())
                .collect::<BTreeSet<_>>(),
            "the component carries the tokens its directions name, and no others"
        );
    }

    /// The directions survive serialization, which is what a consumer's recording replays.
    #[test]
    fn a_serialized_venue_still_prices_its_directions() {
        let venue = venue(vec![book("generous", 3, 10)], QuoteRule::OncePerMaker);

        let replayed: TestVenue =
            serde_json::from_str(&serde_json::to_string(&venue).unwrap()).unwrap();

        assert_eq!(
            replayed
                .amount_out(whole(1), &addr(0xaa), &addr(0xbb))
                .unwrap()
                .amount,
            whole(3)
        );
    }

    /// A consumer reads the rule back off the component it is published on.
    #[test]
    fn the_rule_travels_as_its_attribute() {
        let attribute = |value: &str| {
            HashMap::from([(QuoteRule::ATTRIBUTE.to_string(), Bytes::from(value.as_bytes()))])
        };

        assert_eq!(
            QuoteRule::from_attributes(&attribute("once_per_venue")),
            Ok(Some(QuoteRule::OncePerVenue))
        );
        assert_eq!(
            QuoteRule::from_attributes(&attribute("once_per_maker")),
            Ok(Some(QuoteRule::OncePerMaker))
        );
        assert_eq!(QuoteRule::from_attributes(&HashMap::new()), Ok(None));
        assert_eq!(
            QuoteRule::from_attributes(&attribute("once_per_hour")),
            Err("Unknown quote_rule attribute: once_per_hour".to_string())
        );
    }

    #[test]
    fn swap_directions_are_forty_bytes_each() {
        let directions = BTreeSet::from([(addr(0x11), addr(0x22))]);

        let encoded = encode_swap_directions(&directions);

        assert_eq!(
            encoded.to_string(),
            "0x1111111111111111111111111111111111111111\
             2222222222222222222222222222222222222222"
        );
        assert!(decode_swap_directions(&[0u8; 41]).is_err());
    }
}
