//! A venue published as one component for all of its pairs.
//!
//! By default a feed publishes one component per pair, which is what a consumer routes through.
//! A venue that signs a quote per trade can only sign so many for one route. A consumer carries
//! simulated state between swaps per component, so the venue can instead publish one component
//! for every pair it prices. Each swap then records the book it used, and the next swap on the
//! returned state is held to the venue's [`QuoteRule`].
//!
//! The books are the ones the per-pair layout would publish. Every price comes from one of
//! them, and a quote goes to the book that priced the swap.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt::Debug,
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
    book::{
        component::{encode_swap_directions, pair_book, PairState, SWAP_DIRECTIONS_ATTRIBUTE},
        Book, Books,
    },
    protocol::models::ProtocolComponent,
};

/// Which components a feed publishes for its venue. The venue's feed builder picks the layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComponentLayout<R> {
    /// One component per pair the venue prices, each priced by that pair's own book.
    PerPair,
    /// One component for every pair the venue prices, priced by all of its books at once. A
    /// route may quote that component only as often as `R` allows — a [`QuoteRule`] where the
    /// venue names its makers, and nothing to choose where it signs for itself.
    AllPairs(R),
}

impl<R> From<Option<R>> for ComponentLayout<R> {
    /// One component for all of a venue's pairs, quoted under the rule its feed was built with;
    /// one per pair where it was not built for that.
    fn from(rule: Option<R>) -> Self {
        rule.map_or(ComponentLayout::PerPair, ComponentLayout::AllPairs)
    }
}

/// How often one route may take a quote from a venue published as [`ComponentLayout::AllPairs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuoteRule {
    /// Each market maker signs once. For a venue that names its makers and lets the taker pick
    /// one: a maker's second quote prices as if the first had not happened, so a route that took
    /// both would count the same liquidity twice.
    OncePerMaker,
    /// The venue signs once per route, whichever maker backs the quote.
    OncePerVenue,
}

impl QuoteRule {
    /// Static attribute: the rule a component's books are quoted under.
    pub const ATTRIBUTE: &'static str = "quote_rule";

    const ALL: [QuoteRule; 2] = [QuoteRule::OncePerMaker, QuoteRule::OncePerVenue];

    /// Reads the rule from a component's [`ATTRIBUTE`](Self::ATTRIBUTE). `None` where the
    /// attribute is absent, as it is on a per-pair component, and an error for a value no rule
    /// goes by.
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
}

/// What a route has already taken from a venue, and whom it may still quote.
pub(crate) trait Turns:
    Clone + Debug + PartialEq + Serialize + DeserializeOwned + 'static
{
    /// Who signs the books these turns count.
    type Signer: Clone + Debug + PartialEq + Serialize + DeserializeOwned;

    /// What the venue's feed builder chooses about how often one route may quote it.
    type Rule: Copy + Debug;

    /// Nothing taken yet, under `rule`.
    fn untaken(rule: Self::Rule) -> Self;

    /// The rule the venue's component advertises.
    fn quote_rule(&self) -> QuoteRule;

    /// Whether a book `signer` put up may still be quoted.
    fn allows(&self, signer: &Self::Signer) -> bool;

    /// Takes the turn a swap against `signer` costs.
    fn spend(&mut self, signer: &Self::Signer);
}

/// A market maker a venue names, whose ladders a book holds.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct MarketMaker(pub String);

/// The venue, which signs its own quotes: one signer for all of its books.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Venue;

/// What a route has taken from a venue that names its market makers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SignedByMakers {
    /// Under [`QuoteRule::OncePerMaker`]: the makers this route has quoted.
    PerMaker(HashSet<MarketMaker>),
    /// Under [`QuoteRule::OncePerVenue`]: whether this route has quoted the venue at all.
    PerVenue(bool),
}

impl Turns for SignedByMakers {
    type Signer = MarketMaker;
    type Rule = QuoteRule;

    fn untaken(rule: QuoteRule) -> Self {
        match rule {
            QuoteRule::OncePerMaker => SignedByMakers::PerMaker(HashSet::new()),
            QuoteRule::OncePerVenue => SignedByMakers::PerVenue(false),
        }
    }

    fn quote_rule(&self) -> QuoteRule {
        match self {
            SignedByMakers::PerMaker(_) => QuoteRule::OncePerMaker,
            SignedByMakers::PerVenue(_) => QuoteRule::OncePerVenue,
        }
    }

    fn allows(&self, signer: &MarketMaker) -> bool {
        match self {
            SignedByMakers::PerMaker(makers) => !makers.contains(signer),
            SignedByMakers::PerVenue(signed) => !signed,
        }
    }

    fn spend(&mut self, signer: &MarketMaker) {
        match self {
            SignedByMakers::PerMaker(makers) => {
                makers.insert(signer.clone());
            }
            SignedByMakers::PerVenue(signed) => *signed = true,
        }
    }
}

/// What a route has taken from a venue that signs its own quotes: one turn, taken or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SignedByVenue(bool);

impl Turns for SignedByVenue {
    type Signer = Venue;
    /// One signer is one turn, so the feed builder chooses nothing beyond the layout.
    type Rule = ();

    fn untaken((): ()) -> Self {
        SignedByVenue(false)
    }

    fn quote_rule(&self) -> QuoteRule {
        QuoteRule::OncePerVenue
    }

    fn allows(&self, _signer: &Venue) -> bool {
        !self.0
    }

    fn spend(&mut self, _signer: &Venue) {
        self.0 = true;
    }
}

/// One of the books a venue publishes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct VenueBook<P, S> {
    /// Who signs a quote from this book, and so whose turn a swap against it spends.
    pub signer: S,
    /// When the venue last updated this book, as the per-pair layout would publish it.
    pub updated_at: Option<DateTime<Utc>>,
    pub state: P,
}

/// Every book a venue publishes, behind one component.
///
/// A swap goes to the book that pays best among those still allowed to quote, and the state it
/// returns records that book as spent. Nothing here prices anything itself.
///
/// The books are shared, so what a swap copies to hand back the state it leaves behind is a
/// reference count and the set of books it has spent.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound = "P: Serialize + DeserializeOwned + PairState, T: Turns")]
pub(crate) struct AllPairs<P, T: Turns> {
    books: Arc<VenueBooks<P, T::Signer>>,
    /// The turns every swap this state is downstream of has taken.
    spent: T,
}

/// A venue's books and, for each direction, which of them price it.
///
/// Serialized as the books alone: the index is derived from them, and is built again whenever
/// a set of books comes from anywhere other than a source.
#[derive(Debug, Deserialize)]
#[serde(
    from = "Vec<VenueBook<P, S>>",
    bound = "P: DeserializeOwned + PairState, S: DeserializeOwned"
)]
struct VenueBooks<P, S> {
    books: Vec<VenueBook<P, S>>,
    /// Indices into `books`, by token sold and then token bought, so a swap looks only at the
    /// books that price it and a lookup costs no allocation.
    by_direction: HashMap<Bytes, HashMap<Bytes, Vec<usize>>>,
}

impl<P: Serialize, S: Serialize> Serialize for VenueBooks<P, S> {
    fn serialize<W: serde::Serializer>(&self, serializer: W) -> Result<W::Ok, W::Error> {
        self.books.serialize(serializer)
    }
}

impl<P: PairState, S> From<Vec<VenueBook<P, S>>> for VenueBooks<P, S> {
    fn from(books: Vec<VenueBook<P, S>>) -> Self {
        let mut by_direction: HashMap<Bytes, HashMap<Bytes, Vec<usize>>> = HashMap::new();
        let mut kept = Vec::new();
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
        VenueBooks { books: kept, by_direction }
    }
}

/// A book and what it answered to a swap.
type Answered<'a, P, S> = (&'a VenueBook<P, S>, Result<GetAmountOutResult, SimulationError>);

impl<P: PairState, T: Turns> AllPairs<P, T> {
    pub fn new(books: Vec<VenueBook<P, T::Signer>>, rule: T::Rule) -> Self {
        AllPairs { books: Arc::new(books.into()), spent: T::untaken(rule) }
    }

    pub fn rule(&self) -> QuoteRule {
        self.spent.quote_rule()
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

    pub fn books(&self) -> &[VenueBook<P, T::Signer>] {
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
    pub fn books_pricing(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> impl Iterator<Item = &VenueBook<P, T::Signer>> {
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
    ) -> Result<impl Iterator<Item = &VenueBook<P, T::Signer>>, SimulationError> {
        let mut pricing = self
            .books_pricing(token_in, token_out)
            .peekable();
        if pricing.peek().is_none() {
            return Err(SimulationError::InvalidInput(
                format!("Venue prices no book from {token_in} into {token_out}"),
                None,
            ));
        }
        let mut quotable = pricing
            .filter(|book| self.spent.allows(&book.signer))
            .peekable();
        if quotable.peek().is_none() {
            return Err(SimulationError::RecoverableError(
                "Every book pricing this swap already quoted in this route".to_string(),
            ));
        }
        Ok(quotable)
    }

    /// The same books with the turn of `signer`'s book spent.
    fn spending(&self, signer: &T::Signer) -> Self {
        let mut spent = self.spent.clone();
        spent.spend(signer);
        AllPairs { books: Arc::clone(&self.books), spent }
    }

    /// The book that answers `amount_in` best, and its answer.
    fn best(
        &self,
        amount_in: &BigUint,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<Answered<'_, P, T::Signer>, SimulationError> {
        let mut best: Option<Answered<'_, P, T::Signer>> = None;
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
        best.ok_or_else(|| no_book_ranked("answer this swap"))
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
        best.unwrap_or_else(|| Err(no_book_ranked("price this pair")))
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
        best.unwrap_or_else(|| Err(no_book_ranked("report its limits")))
    }

    /// Whether these are the same books, in the same order, with the same turns spent.
    pub fn same_books(&self, other: &Self) -> bool {
        self.spent == other.spent &&
            self.books().len() == other.books().len() &&
            self.books()
                .iter()
                .zip(other.books().iter())
                .all(|(ours, theirs)| {
                    ours.signer == theirs.signer && ours.state.eq(&theirs.state)
                })
    }

    /// The swap against the best book available, handed back as `W` — the venue's own state
    /// type, which is what a consumer carries on rather than the single book it just used.
    ///
    /// `W` has spent the turn of the book that filled, including on the state attached to a
    /// partial fill.
    pub fn amount_out<W>(
        &self,
        amount_in: &BigUint,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<GetAmountOutResult, SimulationError>
    where
        W: From<Self> + ProtocolSim + 'static,
    {
        let (book, answer) = self.best(amount_in, token_in, token_out)?;
        let quoted = || -> Box<dyn ProtocolSim> { Box::new(W::from(self.spending(&book.signer))) };
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

    /// The book a quote for this swap goes to: the one [`Self::amount_out`] picks on this state.
    ///
    /// Asked on the state a swap returned, the book it used is spent, so the quote names another
    /// book or the state reports that there is none.
    pub fn quoting_book(
        &self,
        amount_in: &BigUint,
        token_in: &Bytes,
        token_out: &Bytes,
    ) -> Result<&P, SimulationError> {
        let (book, _) = self.best(amount_in, token_in, token_out)?;
        Ok(&book.state)
    }
}

impl<P: PairState, S> VenueBook<P, S> {
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

/// A loop over [`AllPairs::candidates`] that ranked nothing. Candidates are at least one book
/// or an error, so this is a bug here rather than anything the venue did.
fn no_book_ranked(what: &str) -> SimulationError {
    SimulationError::FatalError(format!(
        "Books were reported for this direction, and none of them was ranked to {what}"
    ))
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

/// What a venue refuses whatever book would price it, such as a size below every maker's floor.
///
/// A venue that refuses on size alone answers it here rather than letting a book price something
/// the venue will not sign. Refusing nothing is the default.
pub(crate) trait RefusesQuote {
    fn check_amount(
        &self,
        _amount_in: &BigUint,
        _token_in: &Bytes,
        _token_out: &Bytes,
    ) -> Result<(), SimulationError> {
        Ok(())
    }
}

/// Declares a venue's all-pairs state: the wrapper type and the `ProtocolSim` and
/// `IndicativelyPriced` impls that delegate to the [`AllPairs`] it holds.
///
/// `#[typetag::serde]` registers concrete types, so each venue needs its own wrapper even
/// though none of them holds anything of its own. The doc comments written at the invocation
/// describe the venue's books; the rule a swap spends them under is the same everywhere and is
/// written here. [`RefusesQuote`] is the one thing a venue answers for itself, so each writes
/// its own impl beside the invocation.
macro_rules! all_pairs_state {
    (
        $(#[$doc:meta])*
        $wrapper:ident($pair:ty, $turns:ty);
    ) => {
        $(#[$doc])*
        ///
        /// A swap spends the turn of the book it filled against, as the [`QuoteRule`] the
        /// component was published under counts it. The next swap on the state it returned sees
        /// that turn gone.
        ///
        /// [`QuoteRule`]: crate::book::all_pairs::QuoteRule
        #[derive(Clone, Debug, Serialize, Deserialize, ::derive_more::From)]
        pub struct $wrapper($crate::book::all_pairs::AllPairs<$pair, $turns>);

        #[typetag::serde]
        impl ::tycho_common::simulation::protocol_sim::ProtocolSim for $wrapper {
            fn fee(&self) -> f64 {
                0.0
            }

            fn spot_price(
                &self,
                base: &::tycho_common::models::token::Token,
                quote: &::tycho_common::models::token::Token,
            ) -> ::std::result::Result<f64, ::tycho_common::simulation::errors::SimulationError>
            {
                self.0
                    .spot_price(&base.address, &quote.address)
            }

            fn get_amount_out(
                &self,
                amount_in: ::num_bigint::BigUint,
                token_in: &::tycho_common::models::token::Token,
                token_out: &::tycho_common::models::token::Token,
            ) -> ::std::result::Result<
                ::tycho_common::simulation::protocol_sim::GetAmountOutResult,
                ::tycho_common::simulation::errors::SimulationError,
            > {
                $crate::book::all_pairs::RefusesQuote::check_amount(
                    self,
                    &amount_in,
                    &token_in.address,
                    &token_out.address,
                )?;
                self.0
                    .amount_out::<$wrapper>(&amount_in, &token_in.address, &token_out.address)
            }

            fn get_limits(
                &self,
                sell_token: ::tycho_common::Bytes,
                buy_token: ::tycho_common::Bytes,
            ) -> ::std::result::Result<
                (::num_bigint::BigUint, ::num_bigint::BigUint),
                ::tycho_common::simulation::errors::SimulationError,
            > {
                self.0.get_limits(sell_token, buy_token)
            }

            fn as_indicatively_priced(
                &self,
            ) -> ::std::result::Result<
                &dyn ::tycho_common::simulation::indicatively_priced::IndicativelyPriced,
                ::tycho_common::simulation::errors::SimulationError,
            > {
                ::std::result::Result::Ok(self)
            }

            fn delta_transition(
                &mut self,
                _delta: ::tycho_common::dto::ProtocolStateDelta,
                _tokens: &::std::collections::HashMap<
                    ::tycho_common::Bytes,
                    ::tycho_common::models::token::Token,
                >,
                _balances: &::tycho_common::simulation::protocol_sim::Balances,
            ) -> ::std::result::Result<(), ::tycho_common::simulation::errors::TransitionError>
            {
                ::std::result::Result::Err(
                    ::tycho_common::simulation::errors::TransitionError::DecodeError(
                        "Not implemented".into(),
                    ),
                )
            }

            fn clone_box(
                &self,
            ) -> ::std::boxed::Box<dyn ::tycho_common::simulation::protocol_sim::ProtocolSim> {
                ::std::boxed::Box::new(self.clone())
            }

            fn as_any(&self) -> &dyn ::std::any::Any {
                self
            }

            fn as_any_mut(&mut self) -> &mut dyn ::std::any::Any {
                self
            }

            fn eq(
                &self,
                other: &dyn ::tycho_common::simulation::protocol_sim::ProtocolSim,
            ) -> bool {
                other
                    .as_any()
                    .downcast_ref::<$wrapper>()
                    .is_some_and(|other| self.0.same_books(&other.0))
            }
        }

        #[::async_trait::async_trait]
        impl ::tycho_common::simulation::indicatively_priced::IndicativelyPriced for $wrapper {
            /// Asks the book that [`ProtocolSim::get_amount_out`] picks on this state, and no
            /// other: on the state a swap returned, that book's turn is spent, so the quote
            /// goes to another book or the state reports that there is none left.
            async fn request_signed_quote(
                &self,
                params: ::tycho_common::models::protocol::GetAmountOutParams,
            ) -> ::std::result::Result<
                ::tycho_common::simulation::indicatively_priced::SignedQuote,
                ::tycho_common::simulation::errors::SimulationError,
            > {
                ::tycho_common::simulation::indicatively_priced::IndicativelyPriced::request_signed_quote(
                    self.0.quoting_book(
                        &params.amount_in,
                        &params.token_in,
                        &params.token_out,
                    )?,
                    params,
                )
                .await
            }
        }
    };
}

pub(crate) use all_pairs_state;

/// The id of a venue's component covering every pair it prices: its protocol system, which
/// names both the venue and the path its swaps settle through. A feed serves one chain and a
/// consumer keys its components within one, so the venue is the whole of this component's
/// identity, where a pair's id needs its two tokens as well.
pub(crate) fn all_pairs_component_id(protocol_system: &str) -> Bytes {
    Bytes::from(protocol_system.as_bytes())
}

/// The venue's one book, priced by `books` under `rule`: every token they name, the directions
/// they price and the rule they are quoted under.
///
/// `None` where the venue priced nothing a consumer could route through, which is a component
/// that would carry no tokens and no directions.
pub(crate) fn all_pairs_book<P: PairState, T: Turns, W>(
    protocol_system: &str,
    protocol_type: &str,
    chain: Chain,
    books: Vec<VenueBook<P, T::Signer>>,
    rule: T::Rule,
) -> Option<Book>
where
    W: From<AllPairs<P, T>> + ProtocolSim + 'static,
{
    let pairs = AllPairs::new(books, rule);
    if pairs.books().is_empty() {
        return None;
    }
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
    Some(Book { component, state: Arc::new(W::from(pairs)), updated_at })
}

/// Gathers a venue's books into the components it publishes them as.
///
/// A source decodes the same books whichever layout it serves, so it pushes each one with the
/// component id it would carry on its own and takes the [`Books`] at the end. Under
/// [`ComponentLayout::PerPair`] that id is a component; under [`ComponentLayout::AllPairs`] the
/// books end up behind one.
pub(crate) struct BookCollector<P, T: Turns> {
    protocol_system: &'static str,
    protocol_type: &'static str,
    chain: Chain,
    layout: ComponentLayout<T::Rule>,
    books: Books,
    venue_books: Vec<VenueBook<P, T::Signer>>,
}

impl<P: PairState, T: Turns> BookCollector<P, T> {
    pub fn new(
        protocol_system: &'static str,
        protocol_type: &'static str,
        chain: Chain,
        layout: impl Into<ComponentLayout<T::Rule>>,
    ) -> Self {
        BookCollector {
            protocol_system,
            protocol_type,
            chain,
            layout: layout.into(),
            books: Books::new(),
            venue_books: Vec::new(),
        }
    }

    /// Takes one book of the venue, under the `component_id` the per-pair layout gives it.
    pub fn push(&mut self, component_id: Bytes, book: VenueBook<P, T::Signer>) {
        match self.layout {
            ComponentLayout::PerPair => self.books.insert(pair_book(
                self.protocol_system,
                self.protocol_type,
                self.chain,
                component_id,
                book.state,
                book.updated_at,
            )),
            ComponentLayout::AllPairs(_) => self.venue_books.push(book),
        }
    }

    /// The components the venue publishes for everything pushed so far, the all-pairs one as
    /// `W` — the venue's own state type.
    pub fn finish<W>(mut self) -> Books
    where
        W: From<AllPairs<P, T>> + ProtocolSim + 'static,
    {
        if let ComponentLayout::AllPairs(rule) = self.layout {
            self.books
                .extend(all_pairs_book::<P, T, W>(
                    self.protocol_system,
                    self.protocol_type,
                    self.chain,
                    self.venue_books,
                    rule,
                ));
        }
        self.books
    }
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
    use crate::book::{decode_swap_directions, sim, test_token_map};

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
    #[derive(Clone, Debug, Serialize, Deserialize, derive_more::From)]
    struct TestVenue(AllPairs<TestBook, SignedByMakers>);

    impl RefusesQuote for TestVenue {}

    impl TestVenue {
        /// What [`ProtocolSim::get_amount_out`] answers, by the addresses the tests name.
        fn amount_out(
            &self,
            amount_in: &BigUint,
            token_in: &Bytes,
            token_out: &Bytes,
        ) -> Result<GetAmountOutResult, SimulationError> {
            self.0
                .amount_out::<TestVenue>(amount_in, token_in, token_out)
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
            self.check_amount(&amount_in, &token_in.address, &token_out.address)?;
            self.0
                .amount_out::<TestVenue>(&amount_in, &token_in.address, &token_out.address)
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
            other
                .as_any()
                .downcast_ref::<TestVenue>()
                .is_some_and(|other| self.0.same_books(&other.0))
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
    fn book(maker: &str, out_per_in: u64, depth: u64) -> VenueBook<TestBook, MarketMaker> {
        let tokens = tokens();
        VenueBook {
            signer: MarketMaker(maker.to_string()),
            updated_at: None,
            state: TestBook {
                base: tokens[&addr(0xaa)].clone(),
                quote: tokens[&addr(0xbb)].clone(),
                out_per_in,
                depth,
            },
        }
    }

    fn venue(books: Vec<VenueBook<TestBook, MarketMaker>>, rule: QuoteRule) -> TestVenue {
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
            .amount_out(&whole(1), &addr(0xaa), &addr(0xbb))
            .unwrap();

        assert_eq!(swap.amount, whole(3));
    }

    /// A book that fills part of the amount beats one that refuses outright, and the shortfall
    /// is reported with what it would pay.
    #[test]
    fn a_partial_fill_beats_a_refusal() {
        let venue =
            venue(vec![book("shallow", 2, 1), book("empty", 5, 0)], QuoteRule::OncePerMaker);

        match venue.amount_out(&whole(4), &addr(0xaa), &addr(0xbb)) {
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
            .amount_out(&whole(1), &addr(0xaa), &addr(0xbb))
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
            venue.amount_out(&whole(4), &addr(0xaa), &addr(0xbb))
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

        let reverse = venue.amount_out(&whole(1), &addr(0xbb), &addr(0xaa));
        let unknown = venue.amount_out(&whole(1), &addr(0xaa), &addr(0xcc));

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
            .amount_out(&whole(1), &addr(0xaa), &addr(0xbb))
            .unwrap()
            .new_state;
        assert!(matches!(
            spent.get_limits(addr(0xaa), addr(0xbb)),
            Err(SimulationError::RecoverableError(_))
        ));
    }

    /// A venue whose books all price nothing is a component with no tokens and no directions,
    /// which a consumer can do nothing with.
    #[test]
    fn a_venue_that_prices_nothing_publishes_no_component() {
        let book = all_pairs_book::<TestBook, SignedByMakers, TestVenue>(
            "rfq:venue",
            "venue_pool",
            Chain::Ethereum,
            vec![book("empty", 0, 0)],
            QuoteRule::OncePerMaker,
        );

        assert!(book.is_none(), "{book:?}");
    }

    /// A consumer reads the directions off the component rather than pairing up its tokens: the
    /// venue prices A into B and B into C, but nothing between A and C.
    #[test]
    fn the_component_advertises_the_directions_and_the_rule() {
        let tokens = tokens();
        let b_to_c = VenueBook {
            signer: MarketMaker("mm".to_string()),
            updated_at: None,
            state: TestBook {
                base: tokens[&addr(0xbb)].clone(),
                quote: tokens[&addr(0xcc)].clone(),
                out_per_in: 1,
                depth: 1,
            },
        };

        let book = all_pairs_book::<TestBook, SignedByMakers, TestVenue>(
            "rfq:venue",
            "venue_pool",
            Chain::Ethereum,
            vec![book("generous", 3, 10), b_to_c],
            QuoteRule::OncePerMaker,
        )
        .expect("a venue pricing two pairs has a component");

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
            signer: MarketMaker("mm".to_string()),
            updated_at: None,
            state: TestBook {
                base: tokens[&addr(0xbb)].clone(),
                quote: tokens[&addr(0xcc)].clone(),
                out_per_in: 0,
                depth: 10,
            },
        };

        let book = all_pairs_book::<TestBook, SignedByMakers, TestVenue>(
            "rfq:venue",
            "venue_pool",
            Chain::Ethereum,
            vec![book("generous", 3, 10), silent],
            QuoteRule::OncePerMaker,
        )
        .expect("one book prices a pair");

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
                .amount_out(&whole(1), &addr(0xaa), &addr(0xbb))
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

    /// A source pushes the same books either way, so the layout alone decides what it publishes.
    #[rstest]
    #[case::per_pair(ComponentLayout::PerPair, vec!["0x01", "0x02"])]
    #[case::all_pairs(ComponentLayout::AllPairs(QuoteRule::OncePerVenue), vec!["0x7266713a76656e7565"])]
    fn the_layout_decides_which_components_a_venues_books_become(
        #[case] layout: ComponentLayout<QuoteRule>,
        #[case] expected: Vec<&str>,
    ) {
        let mut collector = BookCollector::new("rfq:venue", "venue_pool", Chain::Ethereum, layout);
        collector.push(Bytes::from([0x01]), book("mm1", 2, 10));
        collector.push(Bytes::from([0x02]), book("mm2", 3, 10));

        let books = collector.finish::<TestVenue>();

        let mut ids: Vec<&str> = books
            .keys()
            .map(String::as_str)
            .collect();
        ids.sort();
        assert_eq!(ids, expected);
    }
}
