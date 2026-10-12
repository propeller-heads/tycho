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
