use std::collections::HashMap;

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};
use tycho_common::simulation::errors::SimulationError;
pub use tycho_common::{dto::ChangeType, models::Chain};

use crate::{
    evm::protocol::{u256_num, utils::bytes_to_address},
    serde_helpers::{hex_bytes, hex_bytes_option},
};

#[derive(PartialEq, Serialize, Deserialize, Clone, Debug)]
pub struct AccountUpdate {
    pub address: Address,
    pub chain: Chain,
    pub slots: HashMap<U256, U256>,
    pub balance: Option<U256>,
    #[serde(with = "hex_bytes_option")]
    pub code: Option<Vec<u8>>,
    pub change: ChangeType,
}

impl AccountUpdate {
    pub fn new(
        address: Address,
        chain: Chain,
        slots: HashMap<U256, U256>,
        balance: Option<U256>,
        code: Option<Vec<u8>>,
        change: ChangeType,
    ) -> Self {
        Self { address, chain, slots, balance, code, change }
    }
}

impl TryFrom<tycho_common::dto::AccountUpdate> for AccountUpdate {
    type Error = SimulationError;

    fn try_from(value: tycho_common::dto::AccountUpdate) -> Result<Self, Self::Error> {
        Ok(Self {
            chain: value.chain.into(),
            address: bytes_to_address(&value.address)?,
            slots: u256_num::map_slots_to_u256(value.slots),
            balance: value
                .balance
                .map(|balance| u256_num::bytes_to_u256(balance.into())),
            code: value.code.map(|code| code.to_vec()),
            change: value.change,
        })
    }
}

#[derive(PartialEq, Clone, Serialize, Deserialize, Default)]
#[serde(rename = "Account")]
pub struct ResponseAccount {
    pub chain: Chain,
    pub address: Address,
    pub title: String,
    pub slots: HashMap<U256, U256>,
    pub native_balance: U256,
    pub token_balances: HashMap<Address, U256>,
    #[serde(with = "hex_bytes")]
    pub code: Vec<u8>,
}

impl ResponseAccount {
    pub fn new(
        chain: Chain,
        address: Address,
        title: String,
        slots: HashMap<U256, U256>,
        native_balance: U256,
        token_balances: HashMap<Address, U256>,
        code: Vec<u8>,
    ) -> Self {
        Self { chain, address, title, slots, native_balance, token_balances, code }
    }
}

/// Implement Debug for ResponseAccount manually to avoid printing the code field.
impl std::fmt::Debug for ResponseAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseAccount")
            .field("chain", &self.chain)
            .field("address", &self.address)
            .field("title", &self.title)
            .field("slots", &self.slots)
            .field("native_balance", &self.native_balance)
            .field("token_balances", &self.token_balances)
            .field("code", &format!("[{} bytes]", self.code.len()))
            .finish()
    }
}

impl TryFrom<tycho_common::dto::ResponseAccount> for ResponseAccount {
    type Error = SimulationError;

    #[allow(deprecated)]
    fn try_from(value: tycho_common::dto::ResponseAccount) -> Result<Self, Self::Error> {
        let token_balances = value
            .token_balances
            .into_iter()
            .map(|(address, balance)| {
                Ok((bytes_to_address(&address)?, u256_num::bytes_to_u256(balance.into())))
            })
            .collect::<Result<HashMap<_, _>, SimulationError>>()?;

        Ok(Self {
            chain: value.chain.into(),
            address: bytes_to_address(&value.address)?,
            title: value.title.clone(),
            slots: u256_num::map_slots_to_u256(value.slots),
            native_balance: u256_num::bytes_to_u256(value.native_balance.into()),
            token_balances,
            code: value.code.to_vec(),
        })
    }
}

impl From<tycho_common::models::contract::Account> for ResponseAccount {
    fn from(value: tycho_common::models::contract::Account) -> Self {
        Self {
            chain: value.chain,
            address: Address::from_slice(&value.address[..20]),
            title: value.title,
            slots: u256_num::map_slots_to_u256(value.slots),
            native_balance: u256_num::bytes_to_u256(value.native_balance.into()),
            token_balances: value
                .token_balances
                .into_iter()
                .map(|(addr, ab)| {
                    (Address::from_slice(&addr[..20]), u256_num::bytes_to_u256(ab.balance.into()))
                })
                .collect(),
            code: value.code.to_vec(),
        }
    }
}

impl From<tycho_common::models::contract::AccountDelta> for AccountUpdate {
    fn from(value: tycho_common::models::contract::AccountDelta) -> Self {
        let code = value.code().clone().map(|c| c.to_vec());
        let change = value.change_type().into();
        Self {
            chain: value.chain,
            address: Address::from_slice(&value.address[..20]),
            slots: value
                .slots
                .into_iter()
                .map(|(k, v)| {
                    (
                        u256_num::bytes_to_u256(k.into()),
                        u256_num::bytes_to_u256(v.unwrap_or_default().into()),
                    )
                })
                .collect(),
            balance: value
                .balance
                .map(|b| u256_num::bytes_to_u256(b.into())),
            code,
            change,
        }
    }
}

#[cfg(test)]
mod tests {
    use tycho_common::Bytes;

    use super::*;

    fn make_dto_response_account(address: Bytes) -> tycho_common::dto::ResponseAccount {
        #[allow(deprecated)]
        tycho_common::dto::ResponseAccount::new(
            tycho_common::dto::Chain::Ethereum,
            address,
            "test".to_string(),
            HashMap::new(),
            Bytes::zero(32),
            HashMap::new(),
            Bytes::from(vec![0xDE, 0xAD]),
            Bytes::from("0x00"),
            Bytes::from("0x00"),
            Bytes::from("0x00"),
            None,
        )
    }

    #[test]
    fn test_response_account_conversion_succeeds() {
        let dto = make_dto_response_account(Bytes::zero(20));

        let result = ResponseAccount::try_from(dto).unwrap();

        assert_eq!(result.address, Address::ZERO);
        assert_eq!(result.code, vec![0xDE, 0xAD]);
    }

    #[test]
    fn test_response_account_conversion_short_address_fails() {
        let dto = make_dto_response_account(Bytes::from(vec![0x01]));

        let result = ResponseAccount::try_from(dto);

        assert!(result.is_err());
    }
}
