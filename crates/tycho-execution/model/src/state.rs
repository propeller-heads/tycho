use std::borrow::Cow;

use crate::{Address, Error, params::ParamValue};

/// Models the Ethereum world state, which changes during the simulation.
///
/// Includes:
///
/// - Ether balances
/// - ERC20 token balances
/// - ERC20 allowances
/// - Transient storage
/// - `msg.sender`
///
/// Balances are signed integers and can be negative.
/// A negative balance assumes that the owner had that amount of assets
/// before the transaction.
/// If the caller can gain more than they spend,
/// they can profit because they can temporarily own very large amounts
/// using flashloans.
/// Only the delta matters.
#[derive(Clone, Debug)]
pub struct State {
    owner_to_eth_balance: rustc_hash::FxHashMap<Address, i64>,
    /// first element of key tuple is owner.
    /// second element of key tuple is token.
    owner_and_token_to_balance: rustc_hash::FxHashMap<(Address, Address), i64>,
    /// first element of key tuple is token.
    /// second element of key tuple is owner who approves spender to spend their assets.
    /// third element of key tuple is spender who is approved to spend owner's assets.
    token_and_owner_and_spender_to_allowance:
        rustc_hash::FxHashMap<(Address, Address, Address), i64>,

    /// transient storage
    transient: rustc_hash::FxHashMap<Cow<'static, str>, ParamValue>,
    /// sender of the current message call
    msg_sender: Address,
    /// whether permit2.permit has been called to give router the permit
    /// simplification
    is_permit2_given: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            msg_sender: Address::Sender,
            owner_to_eth_balance: rustc_hash::FxHashMap::default(),
            owner_and_token_to_balance: rustc_hash::FxHashMap::default(),
            token_and_owner_and_spender_to_allowance: rustc_hash::FxHashMap::default(),
            transient: rustc_hash::FxHashMap::default(),
            is_permit2_given: false,
        }
    }
}

impl State {
    /// ETH balance per owner.
    pub fn eth_balances(&self) -> &rustc_hash::FxHashMap<Address, i64> {
        &self.owner_to_eth_balance
    }

    /// ERC20 balance per (owner, token).
    pub fn token_balances(&self) -> &rustc_hash::FxHashMap<(Address, Address), i64> {
        &self.owner_and_token_to_balance
    }

    /// ERC20 allowance per (token, owner, spender).
    pub fn allowances(&self) -> &rustc_hash::FxHashMap<(Address, Address, Address), i64> {
        &self.token_and_owner_and_spender_to_allowance
    }

    /// Removes the zero entries from the balance and allowance maps.
    pub fn drop_zero_entries(&mut self) {
        self.owner_to_eth_balance
            .retain(|_, v| *v != 0);
        self.owner_and_token_to_balance
            .retain(|_, v| *v != 0);
        self.token_and_owner_and_spender_to_allowance
            .retain(|_, v| *v != 0);
    }

    /// Simulate `assembly { tstore(key, value) }`
    pub fn tstore<K: Into<Cow<'static, str>>, V: Into<ParamValue>>(&mut self, key: K, value: V) {
        self.transient
            .insert(key.into(), value.into());
    }

    /// Simulate `assembly { tstore(key, 0) }` for [ParamValue]s that have no null value.
    /// For example, [Executor](crate::model::executors::Executor).
    pub fn tdelete<K: Into<Cow<'static, str>>>(&mut self, key: K) {
        self.transient.remove(&key.into());
    }

    /// Simulate `assembly { tload(key) }`.
    ///
    /// In order to reveal potential problems
    /// this intentionally errors if `key` hasn't previously been [State::tstore]d.
    pub fn tload<K: Into<Cow<'static, str>>, T>(&self, key: K) -> Result<T, Error>
    where
        ParamValue: TryInto<T>,
    {
        let key = key.into();
        let Some(param) = self.transient.get(&key) else {
            return Err(Error::TLoadShouldHavePreviousTStore { key });
        };
        let Ok(result) = param.clone().try_into() else {
            return Err(Error::TLoadShouldBeConvertibleInto { key, value: param.clone() });
        };
        Ok(result)
    }

    /// Simulate `msg.sender`.
    /// Changed via [State::enter_callback] and [State::leave_callback]
    pub fn msg_sender(&self) -> Address {
        self.msg_sender
    }

    /// Simulate entering a callback by setting [State::msg_sender] to `callback_from`.
    pub fn enter_callback(&mut self, callback_from: Address) {
        self.msg_sender = callback_from;
    }

    /// Simulate leaving callback by resetting [State::msg_sender] to `msg.sender`.
    pub fn leave_callback(&mut self) {
        self.msg_sender = Address::Sender;
    }

    /// Simulate `IERC20(token).balanceOf(owner)`
    #[expect(
        clippy::needless_pass_by_ref_mut,
        reason = "public API; changing the signature is a breaking change"
    )]
    pub fn erc20_balance_of(&mut self, token: Address, owner: Address) -> Result<i64, Error> {
        if token.is_never_erc20() {
            return Err(Error::revert("erc20_balance_of: token is never erc20"));
        }
        Ok(self
            .owner_and_token_to_balance
            .get(&(owner, token))
            .cloned()
            .unwrap_or(0))
    }

    /// Simulate `from` executing `IERC20(token).safeTransfer(to, amount)`
    pub fn erc20_safe_transfer(
        &mut self,
        token: Address,
        from: Address,
        to: Address,
        amount: i64,
    ) -> Result<(), Error> {
        if amount < 0 {
            return Err(Error::warning(
                "cannot erc20_safe_transfer negative amount. possibly a bug in the model",
            ));
        }
        if token.is_never_erc20() {
            return Err(Error::revert("erc20_safe_transfer: token is never erc20"));
        }
        *self
            .owner_and_token_to_balance
            .entry((from, token))
            .or_insert(0) -= amount;
        *self
            .owner_and_token_to_balance
            .entry((to, token))
            .or_insert(0) += amount;
        Ok(())
    }

    /// Simulate `sender` executing `IERC20(token).safeTransferFrom(from, to, amount)`
    pub fn erc20_safe_transfer_from(
        &mut self,
        token: Address,
        sender: Address,
        from: Address,
        to: Address,
        amount: i64,
    ) -> Result<(), Error> {
        if amount < 0 {
            return Err(Error::warning(
                "cannot erc20_safe_transfer_from negative amount. possibly a bug in the model",
            ));
        }
        if token.is_never_erc20() {
            return Err(Error::revert("erc20_safe_transfer_from: token is never erc20"));
        }

        *self
            .token_and_owner_and_spender_to_allowance
            .entry((token, from, sender))
            .or_insert(0) -= amount;
        *self
            .owner_and_token_to_balance
            .entry((from, token))
            .or_insert(0) -= amount;
        *self
            .owner_and_token_to_balance
            .entry((to, token))
            .or_insert(0) += amount;
        Ok(())
    }

    /// Simulate `IERC20(token).allowance(owner, spender)`
    pub fn erc20_allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> Result<i64, Error> {
        if token.is_never_erc20() {
            return Err(Error::revert("erc20_allowance: token is never erc20"));
        }
        Ok(self
            .token_and_owner_and_spender_to_allowance
            .get(&(token, owner, spender))
            .cloned()
            .unwrap_or(0))
    }

    /// <https://github.com/OpenZeppelin/openzeppelin-contracts/blob/v5.5.0/contracts/token/ERC20/utils/SafeERC20.sol#L105>
    pub fn erc20_force_approve(
        &mut self,
        token: Address,
        owner: Address,
        spender: Address,
        amount: i64,
    ) -> Result<(), Error> {
        if amount < 0 {
            return Err(Error::warning(
                "cannot erc20_force_approve negative amount. possibly a bug in the model",
            ));
        }
        if token.is_never_erc20() {
            return Err(Error::revert("erc20_force_approve: token is never erc20"));
        }
        self.token_and_owner_and_spender_to_allowance
            .insert((token, owner, spender), amount);
        Ok(())
    }

    /// <https://github.com/Uniswap/permit2/blob/main/src/AllowanceTransfer.sol#L43>
    pub fn permit2_permit(&mut self, _owner: Address) {
        // assume that this makes any and all following calls
        // to permit2_transfer_from succeed.
        // simplification.
        // TODO consider modeling this further
        self.is_permit2_given = true;
    }

    /// Simulate router executing `permit2.transferFrom(owner, spender, amount, token)`.
    ///
    /// <https://github.com/Uniswap/permit2/blob/main/src/AllowanceTransfer.sol#L59>
    pub fn permit2_transfer_from(
        &mut self,
        _sender: Address,
        owner: Address,
        receiver: Address,
        amount: i64,
        token: Address,
    ) -> Result<(), Error> {
        if amount < 0 {
            return Err(Error::warning(
                "cannot permit2_transfer_from negative amount. likely a bug in the model",
            ));
        }
        if !self.is_permit2_given {
            // removing this causes suspicious outcomes - as expected
            return Err(Error::revert("permit not given"));
        }
        *self
            .owner_and_token_to_balance
            .entry((owner, token))
            .or_insert(0) -= amount;
        *self
            .owner_and_token_to_balance
            .entry((receiver, token))
            .or_insert(0) += amount;
        Ok(())
    }

    /// Simulate `owner.balance`
    #[expect(
        clippy::needless_pass_by_ref_mut,
        reason = "public API; changing the signature is a breaking change"
    )]
    pub fn eth_balance(&mut self, owner: Address) -> i64 {
        self.owner_to_eth_balance
            .get(&owner)
            .cloned()
            .unwrap_or(0)
    }

    /// Simulate `from` executing `Address.sendValue(to, amount)`
    pub fn eth_send_value(&mut self, from: Address, to: Address, amount: i64) -> Result<(), Error> {
        if amount < 0 {
            return Err(Error::warning(
                "cannot eth_send_value negative amount. likely a bug in the model",
            ));
        }
        *self
            .owner_to_eth_balance
            .entry(from)
            .or_insert(0) -= amount;
        *self
            .owner_to_eth_balance
            .entry(to)
            .or_insert(0) += amount;
        Ok(())
    }
}
