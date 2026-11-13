//! The questions a check asks a decoded state: each direction's limits, spot price and quotes
//! along the ladder.

use num_bigint::BigUint;
use tycho_simulation::tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim};

/// What a check asks a decoded state.
pub trait Quoter {
    /// `(max input, max output)` of selling `token_in` for `token_out`, or the error, printed.
    fn limits(&self, token_in: &Token, token_out: &Token) -> Result<(BigUint, BigUint), String>;
    /// The price of `base` in `quote`, or the error, printed.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, String>;
    /// What selling `amount_in` of `token_in` pays in `token_out`, or the error, printed.
    fn amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<BigUint, String>;
}

impl Quoter for dyn ProtocolSim {
    fn limits(&self, token_in: &Token, token_out: &Token) -> Result<(BigUint, BigUint), String> {
        self.get_limits(token_in.address.clone(), token_out.address.clone())
            .map_err(|e| e.to_string())
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, String> {
        ProtocolSim::spot_price(self, base, quote).map_err(|e| e.to_string())
    }

    fn amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<BigUint, String> {
        self.get_amount_out(amount_in, token_in, token_out)
            .map(|quote| quote.amount)
            .map_err(|e| e.to_string())
    }
}

/// The fractions of a direction's max input that the `tvl_only` comparison quotes, in thousandths
/// (0.1%, 1% and 10%), as the ladder does.
pub(crate) const LADDER_THOUSANDTHS: [u32; 3] = [1, 10, 100];
