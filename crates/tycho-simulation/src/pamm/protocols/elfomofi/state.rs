use std::{any::Any, collections::HashMap};

use num_bigint::BigUint;
use num_traits::ToPrimitive;
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

use super::models::{Level, Snapshot};
use crate::book::{all_pairs::PricesAfterFill, component::PairState};

// Conservative protocol-only allowance; router/transfer overhead is estimated separately.
#[expect(clippy::wildcard_enum_match_arm, reason = "Chain is non-exhaustive across crates")]
fn swap_gas(chain: tycho_common::models::Chain) -> Result<u64, SimulationError> {
    match chain {
        tycho_common::models::Chain::Base => Ok(320_000),
        tycho_common::models::Chain::Bsc => Ok(300_000),
        tycho_common::models::Chain::Robinhood => Ok(340_000),
        _ => Err(SimulationError::InvalidInput(
            format!("ElFomoFi is not configured for {chain}"),
            None,
        )),
    }
}

/// Cumulative, fee-inclusive quote ladders fetched from the helper.
///
/// Interpolation is indicative between samples.
#[derive(Clone, Debug, Serialize)]
pub struct ElfomofiState {
    pub(super) snapshot: Snapshot,
    pub(super) base_token: Token,
    pub(super) quote_token: Token,
    used: bool,
}

impl<'de> Deserialize<'de> for ElfomofiState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct SerializedState {
            snapshot: Snapshot,
            base_token: Token,
            quote_token: Token,
            #[serde(default)]
            used: bool,
        }
        let value = SerializedState::deserialize(deserializer)?;
        let mut state = Self::from_snapshot(value.snapshot, value.base_token, value.quote_token)
            .map_err(serde::de::Error::custom)?;
        state.used = value.used;
        Ok(state)
    }
}

impl ElfomofiState {
    pub fn from_snapshot(
        snapshot: Snapshot,
        base_token: Token,
        quote_token: Token,
    ) -> Result<Self, SimulationError> {
        validate_snapshot(&snapshot)?;
        if base_token.address != snapshot.book.base_token ||
            quote_token.address != snapshot.book.quote_token ||
            base_token.chain != snapshot.chain ||
            quote_token.chain != snapshot.chain
        {
            return Err(SimulationError::InvalidInput(
                "ElFomoFi token metadata differs from the snapshot".into(),
                None,
            ));
        }
        Ok(Self { snapshot, base_token, quote_token, used: false })
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    fn book(&self, sell: &Bytes, buy: &Bytes) -> Result<(&[Level], &BigUint), SimulationError> {
        let book = &self.snapshot.book;
        if sell == &book.base_token && buy == &book.quote_token {
            Ok((&book.bids, &book.quote_balance))
        } else if sell == &book.quote_token && buy == &book.base_token {
            Ok((&book.asks, &book.base_balance))
        } else {
            Err(SimulationError::InvalidInput("invalid ElFomoFi token pair".into(), None))
        }
    }

    fn validate_tokens(&self, first: &Token, second: &Token) -> Result<(), SimulationError> {
        if first.chain != self.snapshot.chain || second.chain != self.snapshot.chain {
            return Err(SimulationError::InvalidInput("ElFomoFi token chain mismatch".into(), None));
        }
        Ok(())
    }

    fn interpolate(levels: &[Level], amount: &BigUint) -> Result<BigUint, SimulationError> {
        if amount == &BigUint::ZERO {
            return Ok(BigUint::ZERO);
        }
        let mut input = BigUint::ZERO;
        let mut output = BigUint::ZERO;
        for level in levels {
            if amount <= &level.amount_in {
                return Ok(&output +
                    (amount - &input) * (&level.amount_out - &output) /
                        (&level.amount_in - &input));
            }
            input = level.amount_in.clone();
            output = level.amount_out.clone();
        }
        Err(SimulationError::InvalidInput("amount exceeds ElFomoFi book coverage".into(), None))
    }

    fn limits(levels: &[Level], balance: &BigUint) -> (BigUint, BigUint) {
        let mut input = BigUint::ZERO;
        let mut output = BigUint::ZERO;
        let max_input = (BigUint::from(1u8) << 256) - BigUint::from(1u8);
        for level in levels {
            if &level.amount_out > balance || level.amount_in > max_input {
                let inventory_input = &input +
                    (balance - &output) * (&level.amount_in - &input) /
                        (&level.amount_out - &output);
                let capped = inventory_input.min(max_input);
                let capped_output = &output +
                    (&capped - &input) * (&level.amount_out - &output) /
                        (&level.amount_in - &input);
                return (capped, capped_output);
            }
            input = level.amount_in.clone();
            output = level.amount_out.clone();
        }
        (input, output)
    }
}

pub(super) fn validate_snapshot(snapshot: &Snapshot) -> Result<(), SimulationError> {
    snapshot
        .validate()
        .map_err(|error| SimulationError::RecoverableError(error.to_string()))
}

impl PairState for ElfomofiState {
    fn base_token(&self) -> &Token {
        &self.base_token
    }
    fn quote_token(&self) -> &Token {
        &self.quote_token
    }
    fn directions(&self) -> Vec<(Bytes, Bytes)> {
        if self.used {
            return Vec::new();
        }
        let book = &self.snapshot.book;
        let mut directions = Vec::new();
        if Self::limits(&book.bids, &book.quote_balance).0 > BigUint::ZERO {
            directions.push((self.base_token.address.clone(), self.quote_token.address.clone()));
        }
        if Self::limits(&book.asks, &book.base_balance).0 > BigUint::ZERO {
            directions.push((self.quote_token.address.clone(), self.base_token.address.clone()));
        }
        directions
    }
}

impl PricesAfterFill for ElfomofiState {
    fn marginal_price(
        &self,
        token_in: &Bytes,
        token_out: &Bytes,
        _filled: &BigUint,
    ) -> Result<f64, SimulationError> {
        self.book(token_in, token_out)?;
        Err(SimulationError::RecoverableError("ElFomoFi does not quote after a fill".into()))
    }
}

#[typetag::serde]
impl ProtocolSim for ElfomofiState {
    fn fee(&self) -> f64 {
        0.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        self.validate_tokens(base, quote)?;
        // Tycho's BUY price of base in quote uses the quote -> base ladder.
        let (levels, balance) = self.book(&quote.address, &base.address)?;
        let first = levels
            .first()
            .filter(|_| !self.used && balance != &BigUint::ZERO)
            .ok_or_else(|| SimulationError::RecoverableError("empty ElFomoFi book".into()))?;
        let price = first
            .amount_in
            .to_f64()
            .unwrap_or(f64::INFINITY) /
            first
                .amount_out
                .to_f64()
                .unwrap_or(f64::INFINITY) *
            10.0f64.powf(f64::from(base.decimals) - f64::from(quote.decimals));
        if !price.is_finite() || price <= 0.0 {
            return Err(SimulationError::RecoverableError("invalid ElFomoFi spot price".into()));
        }
        Ok(price)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        self.validate_tokens(token_in, token_out)?;
        let (levels, balance) = self.book(&token_in.address, &token_out.address)?;
        if amount_in == BigUint::ZERO {
            return Ok(GetAmountOutResult::new(
                BigUint::ZERO,
                BigUint::from(swap_gas(self.snapshot.chain)?),
                self.clone_box(),
            ));
        }
        if self.used {
            return Err(SimulationError::InvalidInput(
                "ElFomoFi pair can only be used once per solution".into(),
                None,
            ));
        }
        if amount_in > Self::limits(levels, balance).0 {
            return Err(SimulationError::InvalidInput(
                "amount exceeds ElFomoFi liquidity".into(),
                None,
            ));
        }
        let amount = Self::interpolate(levels, &amount_in)?;
        let mut next = self.clone();
        next.used = true;
        Ok(GetAmountOutResult::new(
            amount,
            BigUint::from(swap_gas(self.snapshot.chain)?),
            Box::new(next),
        ))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let (levels, balance) = self.book(&sell_token, &buy_token)?;
        if self.used {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        Ok(Self::limits(levels, balance))
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
            .is_some_and(|other| {
                self.snapshot == other.snapshot &&
                    self.base_token == other.base_token &&
                    self.quote_token == other.quote_token &&
                    self.used == other.used
            })
    }
}
