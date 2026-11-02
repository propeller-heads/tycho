use crate::{address::Address, error::Error, state::State};

/// The [Vault] is modeled as a struct because it has state,
/// which changes during the simulation, like the vault balances and the deltas.
///
/// Most other Solidity files are modeled as a collection of functions.
///
/// <https://github.com/propeller-heads/tycho-execution/blob/main/foundry/src/Vault.sol>
#[derive(Default)]
pub struct Vault {
    /// Vault balances.
    /// First element of key tuple is owner.
    /// Second element of key tuple is token.
    pub owner_and_token_to_balance: rustc_hash::FxHashMap<(Address, Address), i64>,

    /// Transient deltas.
    /// Could be stored via [State::tstore]
    /// but it is both simpler and faster to store them in a dedicated map.
    pub deltas: rustc_hash::FxHashMap<Address, i64>,
    nonzero_delta_count: u64,
}

impl Vault {
    /// <https://github.com/OpenZeppelin/openzeppelin-contracts/blob/v5.5.0/contracts/token/ERC6909/ERC6909.sol#L89>
    pub fn _mint(&mut self, to: Address, token: Address, amount: i64) -> Result<(), Error> {
        if to == Address::Zero {
            return Err(Error::revert("_mint: to == address(0)"));
        }
        self._update(Address::Zero, to, token, amount)
    }

    /// <https://github.com/OpenZeppelin/openzeppelin-contracts/blob/v5.5.0/contracts/token/ERC6909/ERC6909.sol#L123>
    pub fn _burn(&mut self, from: Address, token: Address, amount: i64) -> Result<(), Error> {
        if from == Address::Zero {
            return Err(Error::revert("_burn: from == address(0)"));
        }
        self._update(from, Address::Zero, token, amount)
    }

    /// <https://github.com/propeller-heads/tycho-execution/blob/d27e2a6f4d9ea6f4cba53b2fc1f54cd6676b60d2/foundry/src/Vault.sol#L61>
    #[expect(
        clippy::unnecessary_wraps,
        reason = "public API; changing the signature is a breaking change"
    )]
    pub fn _update(
        &mut self,
        from: Address,
        to: Address,
        token: Address,
        amount: i64,
    ) -> Result<(), Error> {
        if from != Address::Zero {
            // intentionally skipping the check that `from` has at last `amount` balance.
            // allows balances to go into the negative,
            // which simulates/assumes that `from` had a balance before the
            // simulated transaction
            *self
                .owner_and_token_to_balance
                .entry((from, token))
                .or_insert(0) -= amount;
        }

        if to != Address::Zero {
            *self
                .owner_and_token_to_balance
                .entry((to, token))
                .or_insert(0) += amount;
        }
        Ok(())
    }

    /// <https://github.com/propeller-heads/tycho-execution/blob/d27e2a6f4d9ea6f4cba53b2fc1f54cd6676b60d2/foundry/src/Vault.sol#L185>
    pub fn _get_delta(&self, token: Address) -> i64 {
        self.deltas
            .get(&token)
            .cloned()
            .unwrap_or(0)
    }

    /// <https://github.com/propeller-heads/tycho-execution/blob/d27e2a6f4d9ea6f4cba53b2fc1f54cd6676b60d2/foundry/src/Vault.sol#L209>
    pub fn _get_nonzero_delta_count(&self) -> u64 {
        self.nonzero_delta_count
    }

    /// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/Vault.sol#L246>
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

    /// <https://github.com/propeller-heads/tycho-execution/blob/d27e2a6f4d9ea6f4cba53b2fc1f54cd6676b60d2/foundry/src/Vault.sol#L275>
    pub fn _debit_vault(
        &mut self,
        user: Address,
        token: Address,
        amount: i64,
    ) -> Result<(), Error> {
        if amount == 0 {
            return Ok(());
        }

        // intentionally skipping the check that `user` has at last `amount` balance.
        // allows balances to go into the negative,
        // which simulates/assumes that `from` had a balance before the
        // simulated transaction
        self._burn(user, token, amount)
    }

    /// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/Vault.sol#L295>
    pub fn _credit_vault(
        &mut self,
        user: Address,
        token: Address,
        amount: i64,
    ) -> Result<(), Error> {
        if amount == 0 {
            return Ok(());
        }
        self._mint(user, token, amount)
    }

    /// <https://github.com/propeller-heads/tycho-execution/blob/d27e2a6f4d9ea6f4cba53b2fc1f54cd6676b60d2/foundry/src/Vault.sol#L313>
    pub fn _credit_vault_for_fees(
        &mut self,
        user: Address,
        token: Address,
        amount: i64,
    ) -> Result<(), Error> {
        if amount == 0 {
            return Ok(());
        }
        self._mint(user, token, amount)
    }

    /// <https://github.com/propeller-heads/tycho-execution/blob/0454514f4f6ccff55dcaa8e3abbb4ac494d89eba/foundry/src/Vault.sol#L332>
    #[expect(
        clippy::needless_pass_by_ref_mut,
        reason = "public API; changing the signature is a breaking change"
    )]
    pub fn _finalize_balances(
        &mut self,
        state: &mut State,
        user: Address,
        token_in: Address,
        amount_in: i64,
    ) -> Result<(), Error> {
        if state.tload("use_vault")? {
            if self._get_nonzero_delta_count() > 1 {
                return Err(Error::revert(
                    "finalize_balances: use_vault && nonzero_delta_count > 1",
                ));
            } else if self._get_nonzero_delta_count() == 1 {
                let input_delta = self._get_delta(token_in);
                if input_delta == 0 || input_delta != -amount_in {
                    return Err(Error::revert(
                        "finalize_balances: use_vault && nonzero_delta_count == 1 && (input_delta == 0 || input_delta != -amount_in)",
                    ));
                }
                self._burn(user, token_in, amount_in)?;
            }
        } else if self._get_nonzero_delta_count() > 0 {
            return Err(Error::revert("finalize_balances: !use_vault && nonzero_delta_count > 0"));
        }

        Ok(())
    }
}
