use crate::{address::Address, error::Error};

/// The [DeltaAccounting] is modeled as a struct because it has state,
/// the transient deltas, which changes during the simulation.
///
/// Most other Solidity files are modeled as a collection of functions.
///
/// <https://github.com/propeller-heads/tycho/blob/main/crates/tycho-execution/contracts/src/DeltaAccounting.sol>
#[derive(Default)]
pub struct DeltaAccounting {
    /// Transient deltas.
    /// Could be stored via [State::tstore](crate::State::tstore)
    /// but it is both simpler and faster to store them in a dedicated map.
    deltas: rustc_hash::FxHashMap<Address, i64>,
    nonzero_delta_count: u64,
}

impl DeltaAccounting {
    /// Transient delta per token.
    pub fn deltas(&self) -> &rustc_hash::FxHashMap<Address, i64> {
        &self.deltas
    }

    /// Removes the zero entries from the deltas. The count of non-zero deltas is unaffected.
    pub fn drop_zero_entries(&mut self) {
        self.deltas.retain(|_, v| *v != 0);
    }

    pub fn _get_delta(&self, token: Address) -> i64 {
        self.deltas
            .get(&token)
            .copied()
            .unwrap_or(0)
    }

    pub fn _get_nonzero_delta_count(&self) -> u64 {
        self.nonzero_delta_count
    }

    pub fn _update_delta_accounting(&mut self, token: Address, delta_change: i64) {
        if delta_change == 0 {
            return;
        }

        let old_delta = self._get_delta(token);
        let new_delta = old_delta + delta_change;

        if old_delta != 0 && new_delta == 0 {
            self.nonzero_delta_count -= 1;
        } else if old_delta == 0 && new_delta != 0 {
            self.nonzero_delta_count += 1;
        };

        self.deltas.insert(token, new_delta);
    }

    /// Reverts unless every delta is zero.
    pub fn _finalize_balances(&self) -> Result<(), Error> {
        if self._get_nonzero_delta_count() > 0 {
            return Err(Error::revert("_finalize_balances: nonzero_delta_count > 0"));
        }
        Ok(())
    }
}
