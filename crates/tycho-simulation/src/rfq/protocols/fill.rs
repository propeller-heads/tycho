use num_bigint::BigUint;
use num_traits::CheckedSub;
use serde::{Deserialize, Serialize};
use tycho_common::{
    simulation::{
        errors::SimulationError,
        protocol_sim::{GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

/// What a route filled on one directed pair of a venue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct PairFill {
    pub token_in: Bytes,
    pub token_out: Bytes,
    /// The input of every swap the fill merges, in atomic units.
    pub amount_in: BigUint,
}

impl PairFill {
    /// The fill after a swap of `amount_in` on `token_in` -> `token_out`. `filled` is the input
    /// [`filled_on_pair`] returned for that pair.
    pub(crate) fn after(
        filled: Option<&BigUint>,
        token_in: &Bytes,
        token_out: &Bytes,
        amount_in: &BigUint,
    ) -> Self {
        Self {
            token_in: token_in.clone(),
            token_out: token_out.clone(),
            amount_in: filled.map_or_else(|| amount_in.clone(), |filled| filled + amount_in),
        }
    }

    pub(crate) fn is_pair(&self, token_in: &Bytes, token_out: &Bytes) -> bool {
        &self.token_in == token_in && &self.token_out == token_out
    }
}

/// The input an earlier swap filled on `token_in` -> `token_out`, if `fill` is that pair. Fails
/// when `fill` is another pair or direction: the venue already quoted.
pub(crate) fn filled_on_pair<'a>(
    fill: &'a Option<PairFill>,
    token_in: &Bytes,
    token_out: &Bytes,
    venue: &str,
) -> Result<Option<&'a BigUint>, SimulationError> {
    match fill {
        None => Ok(None),
        Some(fill) if fill.is_pair(token_in, token_out) => Ok(Some(&fill.amount_in)),
        Some(_) => {
            Err(SimulationError::RecoverableError(format!("{venue} already quoted in this route")))
        }
    }
}

/// `result` with `new_state` as its state, also when it is a partial fill.
pub(crate) fn with_new_state(
    result: Result<GetAmountOutResult, SimulationError>,
    new_state: Box<dyn ProtocolSim>,
) -> Result<GetAmountOutResult, SimulationError> {
    match with_partial_message(result)? {
        (mut result, None) => {
            result.new_state = new_state;
            Ok(result)
        }
        (mut result, Some(message)) => {
            result.new_state = new_state;
            Err(SimulationError::InvalidInput(message, Some(result)))
        }
    }
}

/// A result with its partial-fill message, if the swap was partial.
fn with_partial_message(
    result: Result<GetAmountOutResult, SimulationError>,
) -> Result<(GetAmountOutResult, Option<String>), SimulationError> {
    match result {
        Ok(result) => Ok((result, None)),
        Err(SimulationError::InvalidInput(message, Some(result))) => Ok((result, Some(message))),
        Err(e) => Err(e),
    }
}

/// The result of a swap of `amount_in` after a swap of `filled` on the same book: the output of
/// both amounts, less the output of `filled`, at zero gas. `amount_out` prices an amount on the
/// book. Returns the partial-fill error when the book cannot fill both amounts.
pub(crate) fn continued_amount_out(
    amount_out: impl Fn(BigUint) -> Result<GetAmountOutResult, SimulationError>,
    filled: &BigUint,
    amount_in: &BigUint,
) -> Result<GetAmountOutResult, SimulationError> {
    let (mut merged, partial) = with_partial_message(amount_out(filled + amount_in))?;
    let (previous, _) = with_partial_message(amount_out(filled.clone()))?;
    merged.amount = merged
        .amount
        .checked_sub(&previous.amount)
        .unwrap_or_default();
    merged.gas = BigUint::default();
    match partial {
        Some(message) => Err(SimulationError::InvalidInput(message, Some(merged))),
        None => Ok(merged),
    }
}

/// The `limits` of a book less the input of `filled` and the output it took.
pub(crate) fn continued_limits(
    amount_out: impl Fn(BigUint) -> Result<GetAmountOutResult, SimulationError>,
    limits: (BigUint, BigUint),
    filled: &BigUint,
) -> Result<(BigUint, BigUint), SimulationError> {
    let (filled_out, _) = with_partial_message(amount_out(filled.clone()))?;
    Ok((
        limits
            .0
            .checked_sub(filled)
            .unwrap_or_default(),
        limits
            .1
            .checked_sub(&filled_out.amount)
            .unwrap_or_default(),
    ))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;
    use rstest::rstest;

    use super::*;
    use crate::evm::protocol::uniswap_v2::state::UniswapV2State;

    fn token(byte: u8) -> Bytes {
        Bytes::from([byte; 20].to_vec())
    }

    /// Prices an amount on a book of `depth` atomic units that pays 2 out for 1 in and costs 100
    /// gas. An amount past the depth is a partial fill.
    fn book(depth: u64) -> impl Fn(BigUint) -> Result<GetAmountOutResult, SimulationError> {
        move |amount_in| {
            let filled = amount_in
                .clone()
                .min(BigUint::from(depth));
            let result = GetAmountOutResult {
                amount: filled * 2u8,
                gas: BigUint::from(100u8),
                new_state: Box::new(UniswapV2State::new(U256::from(1), U256::from(1))),
            };
            if amount_in > BigUint::from(depth) {
                return Err(SimulationError::InvalidInput("partial".into(), Some(result)));
            }
            Ok(result)
        }
    }

    #[rstest]
    #[case::other_pair(1, 3)]
    #[case::opposite_direction(2, 1)]
    fn filled_on_pair_refuses_another_pair_or_direction(#[case] token_in: u8, #[case] out: u8) {
        let fill = Some(PairFill::after(None, &token(1), &token(2), &BigUint::from(5u8)));
        let result = filled_on_pair(&fill, &token(token_in), &token(out), "Venue");
        assert!(matches!(
            result,
            Err(SimulationError::RecoverableError(message)) if message == "Venue already quoted in this route"
        ));
    }

    #[test]
    fn continued_amount_out_pays_the_merged_output_less_the_filled_one() {
        let result =
            continued_amount_out(book(10), &BigUint::from(4u8), &BigUint::from(3u8)).unwrap();
        assert_eq!(result.amount, BigUint::from(6u8));
        assert_eq!(result.gas, BigUint::from(0u8));
    }

    #[test]
    fn continued_amount_out_past_the_depth_is_partial() {
        let result = continued_amount_out(book(10), &BigUint::from(4u8), &BigUint::from(7u8));
        let Err(SimulationError::InvalidInput(message, Some(partial))) = result else {
            panic!("Expected a partial fill, got {result:?}");
        };
        assert_eq!(message, "partial");
        assert_eq!(partial.amount, BigUint::from(12u8));
        assert_eq!(partial.gas, BigUint::from(0u8));
    }

    #[test]
    fn continued_limits_are_the_limits_less_the_fill() {
        let limits = (BigUint::from(10u8), BigUint::from(20u8));
        let result = continued_limits(book(10), limits, &BigUint::from(4u8)).unwrap();
        assert_eq!(result, (BigUint::from(6u8), BigUint::from(12u8)));
    }
}
