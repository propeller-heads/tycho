//! Each property fires: a simple pool with one fault breaks the property that fault targets.

use std::{any::Any, collections::HashMap};

use num_bigint::BigUint;
use rstest::rstest;
use serde::{ser::Error as _, Deserialize, Serialize, Serializer};
use tycho_common::{
    dto::ProtocolStateDelta,
    models::{token::Token, Chain},
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};
use tycho_simulation::evm::protocol::checkable::{CheckableState, LimitKind};

use super::{
    check_state, Check, CopiesAlike, FeeFraction, FiniteSpotPrice, LimitsAsDeclared,
    LimitsNotGrown, LimitsWithoutError, MonotonicOutput, NewStateSameType, NewStateValid,
    NoRoundTripGain, NoSplitGain, QuoteAtLeastSpotBefore, QuoteAtMostSpotAfter, QuotesWithoutPanic,
    SerdeRoundTrip, SpotPriceWhereTradable, SpotPricesProductAtLeastOne,
};
use crate::checks::amounts::Amount;

/// What a [`Faulty`] state does wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
enum Fault {
    None,
    LimitsError,
    DeclaresUnderCap,
    QuotePanics,
    SpotPriceNan,
    FeeAboveOne,
    SpotPriceErrors,
    SpotPriceHalved,
    SpotPriceDoubled,
    OutputFallsFrom1000,
    OutputPlusOne,
    OutputDoubled,
    NewStateOfOtherType,
    NewStateInvalid,
    NewStateWithMoreRoom,
    SerialisationFails,
    CloneDiffers,
    /// Invalid: the fault of the state a quote returns under [`Fault::NewStateInvalid`].
    Invalid,
}

/// A pool that pays one unit of the other token for each unit it is sold, at a spot price of 1
/// and no fee, up to what it holds of the token bought: every property holds exactly, except
/// where its fault says otherwise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Faulty {
    /// What the pool holds of each token, in the order of their addresses.
    reserves: [u128; 2],
    #[serde(serialize_with = "serialize_fault")]
    fault: Fault,
}

fn serialize_fault<S: Serializer>(fault: &Fault, serializer: S) -> Result<S::Ok, S::Error> {
    if *fault == Fault::SerialisationFails {
        return Err(S::Error::custom("this state does not serialise"));
    }
    serializer.serialize_str(&format!("{fault:?}"))
}

impl Faulty {
    fn new(fault: Fault) -> Self {
        // 1000 tokens of 18 decimals on each side.
        Self { reserves: [10u128.pow(21); 2], fault }
    }

    fn with_reserves(&self, reserves: [u128; 2]) -> Self {
        Self { reserves, fault: self.fault }
    }

    /// The indices of the token sold and the token bought.
    fn indices(sell_token: &Bytes, buy_token: &Bytes) -> (usize, usize) {
        if sell_token < buy_token {
            (0, 1)
        } else {
            (1, 0)
        }
    }
}

#[typetag::serde]
impl ProtocolSim for Faulty {
    fn fee(&self) -> f64 {
        if self.fault == Fault::FeeAboveOne {
            1.5
        } else {
            0.0
        }
    }

    fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
        if self.fault == Fault::SpotPriceErrors {
            return Err(SimulationError::FatalError("spot price unavailable".to_string()));
        }
        Ok(if self.fault == Fault::SpotPriceNan {
            f64::NAN
        } else if self.fault == Fault::SpotPriceHalved {
            0.5
        } else if self.fault == Fault::SpotPriceDoubled {
            2.0
        } else {
            1.0
        })
    }

    #[expect(
        clippy::panic_in_result_fn,
        reason = "the QuotePanics fault panics, which quotes_without_panic reports"
    )]
    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        assert!(self.fault != Fault::QuotePanics, "quoting panics");
        let (sold, bought) = Self::indices(&token_in.address, &token_out.address);
        let past_limit =
            || SimulationError::InvalidInput(format!("{amount_in} is past the limit"), None);
        let amount_in = u128::try_from(&amount_in).map_err(|_too_wide| past_limit())?;
        if amount_in > self.reserves[bought] {
            return Err(past_limit());
        }
        let paid = if self.fault == Fault::OutputFallsFrom1000 && amount_in >= 1000 {
            amount_in / 2
        } else {
            amount_in
        };
        let mut after = self.reserves;
        after[sold] = after[sold]
            .checked_add(amount_in)
            .ok_or_else(past_limit)?;
        after[bought] -= paid;
        let amount = if self.fault == Fault::OutputPlusOne {
            paid + 1
        } else if self.fault == Fault::OutputDoubled {
            paid * 2
        } else {
            paid
        };
        let new_state: Box<dyn ProtocolSim> = if self.fault == Fault::NewStateOfOtherType {
            Box::new(OtherType(self.with_reserves(after)))
        } else if self.fault == Fault::NewStateInvalid {
            Box::new(Self { reserves: after, fault: Fault::Invalid })
        } else if self.fault == Fault::NewStateWithMoreRoom {
            Box::new(self.with_reserves([u128::MAX / 2; 2]))
        } else {
            Box::new(self.with_reserves(after))
        };
        Ok(GetAmountOutResult::new(BigUint::from(amount), BigUint::ZERO, new_state))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        if self.fault == Fault::LimitsError {
            return Err(SimulationError::FatalError("no limits".to_string()));
        }
        let (_, bought) = Self::indices(&sell_token, &buy_token);
        let limit = BigUint::from(self.reserves[bought]);
        Ok((limit.clone(), limit))
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        if self.fault == Fault::CloneDiffers {
            Box::new(self.with_reserves([1, 1]))
        } else {
            Box::new(self.clone())
        }
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
            .is_some_and(|other| self == other)
    }
}

/// A [`Faulty`] pool under another type, for [`Fault::NewStateOfOtherType`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct OtherType(Faulty);

#[typetag::serde]
impl ProtocolSim for OtherType {
    fn fee(&self) -> f64 {
        self.0.fee()
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.0.spot_price(base, quote)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        self.0
            .get_amount_out(amount_in, token_in, token_out)
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
        delta: ProtocolStateDelta,
        tokens: &HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.0
            .delta_transition(delta, tokens, balances)
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
            .is_some_and(|other| self == other)
    }
}

impl CheckableState for Faulty {
    /// The pool pays at most what it holds of the token bought, and refuses one unit more.
    fn limit_kind(&self, _sell_token: &Bytes, _buy_token: &Bytes) -> LimitKind {
        if self.fault == Fault::DeclaresUnderCap {
            LimitKind::UnderCap
        } else {
            LimitKind::Hard
        }
    }

    fn check_valid(&self) -> Result<(), String> {
        if self.fault == Fault::Invalid {
            return Err("the fault makes it invalid".to_string());
        }
        Ok(())
    }
}

fn token(last_byte: u8) -> Token {
    Token::new(
        &Bytes::from([[0u8; 19].as_slice(), &[last_byte]].concat()),
        &format!("T{last_byte}"),
        18,
        0,
        &[Some(100_000)],
        Chain::Ethereum,
        100,
    )
}

/// The names of the properties `state` breaks.
fn broken(fault: Fault) -> Vec<&'static str> {
    let amounts = [
        Amount::Raw(BigUint::from(999u32)),
        Amount::Raw(BigUint::from(1000u32)),
        Amount::Raw(BigUint::from(10u64.pow(18))),
        Amount::LimitPlus(-1),
        Amount::LimitPlus(0),
    ];
    let mut broken = check_state(&Faulty::new(fault), &[token(1), token(2)], &amounts)
        .into_iter()
        .map(|violation| violation.property)
        .collect::<Vec<_>>();
    broken.sort_unstable();
    broken.dedup();
    broken
}

#[test]
fn test_state_without_fault_breaks_nothing() {
    assert_eq!(broken(Fault::None), Vec::<&str>::new());
}

#[rstest]
#[case::limits_error(Fault::LimitsError, LimitsWithoutError::NAME)]
#[case::declares_under_cap(Fault::DeclaresUnderCap, LimitsAsDeclared::NAME)]
#[case::quote_panics(Fault::QuotePanics, QuotesWithoutPanic::NAME)]
#[case::spot_price_nan(Fault::SpotPriceNan, FiniteSpotPrice::NAME)]
#[case::fee_above_one(Fault::FeeAboveOne, FeeFraction::NAME)]
#[case::spot_price_errors(Fault::SpotPriceErrors, SpotPriceWhereTradable::NAME)]
#[case::spot_price_halved(Fault::SpotPriceHalved, SpotPricesProductAtLeastOne::NAME)]
#[case::spot_price_doubled(Fault::SpotPriceDoubled, QuoteAtLeastSpotBefore::NAME)]
#[case::spot_price_halved_after(Fault::SpotPriceHalved, QuoteAtMostSpotAfter::NAME)]
#[case::output_falls(Fault::OutputFallsFrom1000, MonotonicOutput::NAME)]
#[case::output_plus_one(Fault::OutputPlusOne, NoSplitGain::NAME)]
#[case::output_doubled(Fault::OutputDoubled, NoRoundTripGain::NAME)]
#[case::new_state_of_other_type(Fault::NewStateOfOtherType, NewStateSameType::<Faulty>::NAME)]
#[case::new_state_invalid(Fault::NewStateInvalid, NewStateValid::<Faulty>::NAME)]
#[case::new_state_with_more_room(Fault::NewStateWithMoreRoom, LimitsNotGrown::NAME)]
#[case::serialisation_fails(Fault::SerialisationFails, SerdeRoundTrip::NAME)]
#[case::clone_differs(Fault::CloneDiffers, CopiesAlike::NAME)]
fn test_fault_breaks_its_property(#[case] fault: Fault, #[case] property: &str) {
    let broken = broken(fault);

    assert!(broken.contains(&property), "{fault:?} breaks {broken:?}, not {property}");
}
