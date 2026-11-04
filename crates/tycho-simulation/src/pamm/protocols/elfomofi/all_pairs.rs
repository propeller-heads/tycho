use std::{any::Any, collections::HashMap};

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use super::state::ElfomofiState;
use crate::book::all_pairs::{AllPairs, SignedByVenue};

/// All ElFomoFi pairs, available for one swap per solution.
#[derive(Clone, Debug, Serialize, Deserialize, derive_more::From)]
pub struct ElfomofiAllPairsState(AllPairs<ElfomofiState, SignedByVenue>);

impl ElfomofiAllPairsState {
    fn ensure_available(&self) -> Result<(), SimulationError> {
        if self
            .0
            .directions()
            .iter()
            .any(|(sell, buy)| {
                self.0
                    .merged_input(&BigUint::ZERO, sell, buy) >
                    BigUint::ZERO
            })
        {
            return Err(SimulationError::RecoverableError(
                "ElFomoFi already quoted a swap in this solution".into(),
            ));
        }
        Ok(())
    }

    fn validate_tokens(&self, first: &Token, second: &Token) -> Result<(), SimulationError> {
        if first.chain != second.chain ||
            self.0
                .books()
                .first()
                .is_some_and(|book| first.chain != book.state.snapshot().chain)
        {
            return Err(SimulationError::InvalidInput("ElFomoFi token chain mismatch".into(), None));
        }
        Ok(())
    }
}

#[typetag::serde]
impl ProtocolSim for ElfomofiAllPairsState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.validate_tokens(base, quote)?;
        self.ensure_available()?;
        self.0
            .quoting_book(&BigUint::ZERO, &quote.address, &base.address)?
            .spot_price(base, quote)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        self.validate_tokens(token_in, token_out)?;
        self.ensure_available()?;
        let mut result =
            self.0
                .amount_out::<Self>(&amount_in, &token_in.address, &token_out.address)?;
        if amount_in == BigUint::ZERO {
            result.new_state = self.clone_box();
        }
        Ok(result)
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        self.ensure_available()?;
        self.0.get_limits(sell_token, buy_token)
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        Err(TransitionError::DecodeError("ElFomoFi requires complete snapshots".into()))
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
            .downcast_ref::<Self>()
            .is_some_and(|other| self.0.same_books(&other.0))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use tycho_common::models::Chain;

    use super::*;
    use crate::{
        book::{
            all_pairs::{all_pairs_book, QuoteRule, Venue, VenueBook},
            decode_swap_directions, SWAP_DIRECTIONS_ATTRIBUTE,
        },
        pamm::protocols::elfomofi::{
            models::{Book, Level, Snapshot},
            PROTOCOL_SYSTEM, PROTOCOL_TYPE,
        },
    };

    fn token(byte: u8) -> Token {
        Token::new(&Bytes::from(vec![byte; 20]), "TEST", 0, 0, &[], Chain::Base, 100)
    }

    fn book(base: u8, quote: u8) -> VenueBook<ElfomofiState, Venue> {
        let base = token(base);
        let quote = token(quote);
        let snapshot = Snapshot {
            chain: Chain::Base,
            block_number: 100,
            block_hash: Bytes::from(vec![3; 32]),
            block_timestamp: 100,
            observed_at: 100,
            max_age_seconds: 2,
            book: Book {
                base_token: base.address.clone(),
                quote_token: quote.address.clone(),
                bids: vec![Level { amount_in: 100u64.into(), amount_out: 200u64.into() }],
                asks: vec![Level { amount_in: 200u64.into(), amount_out: 100u64.into() }],
                base_balance: 1_000u64.into(),
                quote_balance: 1_000u64.into(),
            },
        };
        VenueBook {
            signer: Venue,
            updated_at: None,
            state: ElfomofiState::from_snapshot(snapshot, base, quote).unwrap(),
        }
    }

    fn venue() -> ElfomofiAllPairsState {
        AllPairs::new(vec![book(1, 2), book(2, 3)], ()).into()
    }

    fn assert_spent(state: &dyn ProtocolSim) {
        for (sell, buy) in [(1, 2), (2, 1), (2, 3), (3, 2)] {
            let (sell, buy) = (token(sell), token(buy));
            assert!(matches!(
                state.get_amount_out(10u64.into(), &sell, &buy),
                Err(SimulationError::RecoverableError(_))
            ));
            assert!(state
                .get_limits(sell.address.clone(), buy.address.clone())
                .is_err());
            assert!(state.spot_price(&sell, &buy).is_err());
        }
    }

    #[test]
    fn any_fill_disables_every_pair_in_both_directions() {
        for (sell, buy, output) in [(1, 2, 20u64), (2, 1, 5), (2, 3, 20), (3, 2, 5)] {
            let initial = venue();
            let result = initial
                .get_amount_out(10u64.into(), &token(sell), &token(buy))
                .unwrap();
            assert_eq!(result.amount, BigUint::from(output));
            assert_spent(result.new_state.as_ref());
            assert!(!result.new_state.eq(&initial));
        }
    }

    #[test]
    fn zero_input_and_failed_quotes_do_not_spend_the_venue() {
        let initial = venue();
        let zero = initial
            .get_amount_out(BigUint::ZERO, &token(1), &token(2))
            .unwrap();
        assert_eq!(zero.amount, BigUint::ZERO);
        assert!(zero.new_state.eq(&initial));
        assert!(initial
            .get_amount_out(101u64.into(), &token(1), &token(2))
            .is_err());
        assert!(zero
            .new_state
            .get_amount_out(10u64.into(), &token(2), &token(3))
            .is_ok());
    }

    #[test]
    fn route_candidates_and_fresh_snapshots_start_independently() {
        let initial = venue();
        let candidate = initial.clone_box();
        let spent = candidate
            .get_amount_out(10u64.into(), &token(1), &token(2))
            .unwrap()
            .new_state;
        assert_spent(spent.as_ref());
        for unused in [initial.clone_box(), candidate, Box::new(venue())] {
            assert!(unused.eq(&initial));
            assert!(unused
                .get_amount_out(10u64.into(), &token(2), &token(3))
                .is_ok());
        }
    }

    #[test]
    fn serialized_state_retains_the_venue_turn() {
        let initial = venue();
        let spent = initial
            .get_amount_out(10u64.into(), &token(1), &token(2))
            .unwrap()
            .new_state;
        let encoded = serde_json::to_vec(spent.as_ref()).unwrap();
        let restored: Box<dyn ProtocolSim> = serde_json::from_slice(&encoded).unwrap();
        assert!(restored.eq(spent.as_ref()));
        assert_spent(restored.as_ref());
        let encoded = serde_json::to_vec(&initial as &dyn ProtocolSim).unwrap();
        let restored: Box<dyn ProtocolSim> = serde_json::from_slice(&encoded).unwrap();
        assert!(restored.eq(&initial));
        assert!(restored
            .get_amount_out(10u64.into(), &token(3), &token(2))
            .is_ok());
    }

    #[test]
    fn component_tokens_and_directions_match_its_books() {
        let component = all_pairs_book::<ElfomofiState, SignedByVenue, ElfomofiAllPairsState>(
            PROTOCOL_SYSTEM,
            PROTOCOL_TYPE,
            Chain::Base,
            vec![book(1, 2), book(2, 3)],
            (),
        )
        .unwrap()
        .component;
        let directions =
            decode_swap_directions(&component.static_attributes[SWAP_DIRECTIONS_ATTRIBUTE])
                .unwrap();
        assert_eq!(
            directions,
            vec![
                (token(1).address, token(2).address),
                (token(2).address, token(1).address),
                (token(2).address, token(3).address),
                (token(3).address, token(2).address),
            ]
        );
        assert_eq!(
            component
                .tokens
                .iter()
                .map(|token| token.address.clone())
                .collect::<BTreeSet<_>>(),
            directions
                .into_iter()
                .flat_map(|(sell, buy)| [sell, buy])
                .collect()
        );
        assert_eq!(
            QuoteRule::from_attributes(&component.static_attributes).unwrap(),
            Some(QuoteRule::OncePerVenue)
        );
        assert!(venue()
            .get_amount_out(10u64.into(), &token(1), &token(3))
            .is_err());
    }

    #[test]
    fn one_sided_books_keep_their_buy_price() {
        let mut one_sided = book(1, 2);
        one_sided
            .state
            .snapshot
            .book
            .asks
            .clear();
        let initial = ElfomofiAllPairsState::from(AllPairs::new(vec![one_sided], ()));
        assert_eq!(
            initial
                .spot_price(&token(2), &token(1))
                .unwrap(),
            0.5
        );
        assert!(initial
            .spot_price(&token(1), &token(2))
            .is_err());
        let spent = initial
            .get_amount_out(10u64.into(), &token(1), &token(2))
            .unwrap()
            .new_state;
        assert!(spent
            .spot_price(&token(2), &token(1))
            .is_err());
    }

    #[test]
    fn token_chains_are_checked_and_no_binding_quote_is_required() {
        let initial = venue();
        let mut wrong_chain = token(1);
        wrong_chain.chain = Chain::Bsc;
        assert!(initial
            .get_amount_out(10u64.into(), &wrong_chain, &token(2))
            .is_err());
        assert!(initial
            .spot_price(&wrong_chain, &token(2))
            .is_err());
        let mut wrong_quote = token(2);
        wrong_quote.chain = Chain::Bsc;
        assert!(initial
            .get_amount_out(10u64.into(), &wrong_chain, &wrong_quote)
            .is_err());
        assert!(initial
            .as_indicatively_priced()
            .is_err());
    }
}
