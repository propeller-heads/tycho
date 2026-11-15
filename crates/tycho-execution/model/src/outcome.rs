use serde::{Serialize, ser::SerializeMap};

use crate::{Address, State, log::Log, model::DeltaAccounting, params::Params};

/// Combines all elements of a successful [Result] of [simulate](crate::simulate::simulate):
/// The input [Params] and the output [State], [DeltaAccounting] and [Log].
/// Serialized into the YAML that's written to stdout for every suspicious state.
pub struct Outcome<T> {
    params: Params,
    state: State,
    delta_accounting: DeltaAccounting,
    log: T,
}

impl<T: Log> Outcome<T> {
    pub fn new(
        params: Params,
        mut state: State,
        mut delta_accounting: DeltaAccounting,
        log: T,
    ) -> Self {
        // in order to keep the serialized output readable and compact
        // remove zero valued entries in various maps
        state.drop_zero_entries();
        delta_accounting.drop_zero_entries();

        Self { params, state, delta_accounting, log }
    }

    /// The [Outcome] is considered suspicious if:
    /// 1. The caller ends up with more ETH or WETH on the addresses they control
    ///    ([Address::is_sender_controlled]) than they had before the transaction (stealing assets).
    /// 2. The router loses ETH or WETH it held before the transaction.
    pub fn is_suspicious(&self) -> bool {
        let msg_value: i64 = self
            .params
            .get("msg_value")
            .unwrap_or(0);
        self.final_caller_eth_balance() > msg_value || self.final_router_eth_balance() < 0
    }

    /// On-chain ETH and WETH balance delta of [Address::Router].
    /// A negative value means the router lost tokens.
    pub fn final_router_eth_balance(&self) -> i64 {
        self.state
            .eth_balances()
            .get(&Address::Router)
            .copied()
            .unwrap_or(0) +
            self.state
                .token_balances()
                .get(&(Address::Router, Address::WETH))
                .copied()
                .unwrap_or(0)
    }

    /// Return the positive or negative ETH and WETH balance the caller
    /// has at the end of the simulated transaction
    /// across all addresses they control ([Address::is_sender_controlled]).
    pub fn final_caller_eth_balance(&self) -> i64 {
        let mut total_eth = 0;
        for (owner, balance) in self.state.eth_balances() {
            if owner.is_sender_controlled() {
                total_eth += balance;
            }
        }
        for ((owner, token), balance) in self.state.token_balances() {
            if owner.is_sender_controlled() {
                // TODO if you introduce other value holding erc20 tokens,
                // make sure to add code for them here and introduce
                // some fixed conversion rate to eth
                if *token == Address::WETH {
                    total_eth += balance;
                }
            }
        }
        for ((token, _, spender), router_allowance) in self.state.allowances() {
            if spender.is_sender_controlled() {
                // TODO if you introduce other value holding erc20 tokens,
                // make sure to add code for them here and introduce
                // some fixed conversion rate to eth
                if *token == Address::WETH {
                    total_eth += router_allowance;
                }
            }
        }
        total_eth
    }
}

/// [Outcome]'s serialization is so customized that it leads to
/// much simpler code implementing it from scratch instead of
/// using serde's configuration attributes.
impl<T: Log + Serialize> Serialize for Outcome<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_map(None)?;
        s.serialize_entry("params", &self.params)?;
        let final_caller_eth_balance = self.final_caller_eth_balance();
        if final_caller_eth_balance > 0 {
            s.serialize_entry("final_caller_eth_balance", &final_caller_eth_balance)?;
        }
        let final_router_eth_balance = self.final_router_eth_balance();
        if final_router_eth_balance < 0 {
            s.serialize_entry("final_router_eth_balance", &final_router_eth_balance)?;
        }

        if !self.state.eth_balances().is_empty() {
            s.serialize_entry("eth_balance", self.state.eth_balances())?;
        }

        if !self.state.token_balances().is_empty() {
            s.serialize_entry("erc20_balance", self.state.token_balances())?;
        }

        if !self.state.allowances().is_empty() {
            s.serialize_entry("erc20_allowance", self.state.allowances())?;
        }

        if self
            .delta_accounting
            ._get_nonzero_delta_count() >
            0
        {
            s.serialize_entry(
                "nonzero_delta_count",
                &self
                    .delta_accounting
                    ._get_nonzero_delta_count(),
            )?;
        }

        if !self
            .delta_accounting
            .deltas()
            .is_empty()
        {
            s.serialize_entry("delta", self.delta_accounting.deltas())?;
        }

        if !self.log.is_empty() {
            s.serialize_entry("log", &self.log)?;
        }
        s.end()
    }
}
