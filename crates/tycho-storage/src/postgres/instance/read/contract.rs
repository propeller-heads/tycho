use std::collections::HashMap;

use diesel::{pg::Pg, prelude::*};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{
    models::{contract::AccountBalance, Address, Balance, Chain},
    storage::{StorageError, Version},
    Bytes,
};

use super::version::check_current;
use crate::postgres::{
    instance::{
        active::active_instance_head,
        gateway::PostgresGatewayNew,
        schema::tycho::{account, account_balance},
        InstanceId,
    },
    PostgresError,
};

impl PostgresGatewayNew {
    /// Returns the token balances of the accounts of `protocol_system`'s active instance, by
    /// account address and token address.
    ///
    /// - `protocol_system`: the protocol system whose copy of the accounts to read. `NotFound` for
    ///   an unknown name. A protocol system without an active instance has no balances.
    /// - `accounts`: only these accounts.
    /// - `at`: the version. `Unsupported` for a version before the head of the instance.
    /// - `include_native`: add the balance of the chain's native token.
    ///
    /// Accounts without balances are absent. `modify_tx` is always 32 zero bytes. `NotFound` for
    /// an unknown chain.
    pub async fn get_account_balances(
        &self,
        chain: &Chain,
        protocol_system: &str,
        accounts: Option<&[Address]>,
        at: Option<&Version>,
        include_native: bool,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<Address, HashMap<Address, AccountBalance>>, StorageError> {
        self.get_chain_id(chain)?;
        self.get_protocol_system_id(protocol_system)?;
        let Some(active) = active_instance_head(protocol_system, conn).await? else {
            return Ok(HashMap::new());
        };
        check_current(&active, at, conn).await?;
        load_account_balances(chain, active.id, accounts, include_native, conn).await
    }
}

/// Loads the token balances of the accounts of `instance`, by account address and token address.
/// `accounts`: only these accounts; `None` for all of them.
async fn load_account_balances(
    chain: &Chain,
    instance: InstanceId,
    accounts: Option<&[Address]>,
    include_native: bool,
    conn: &mut AsyncPgConnection,
) -> Result<HashMap<Address, HashMap<Address, AccountBalance>>, StorageError> {
    let mut query = account_balance::table
        .inner_join(
            account::table.on(account::instance_id
                .eq(account_balance::instance_id)
                .and(account::id.eq(account_balance::account_id))),
        )
        .filter(account_balance::instance_id.eq(instance))
        .filter(account::instance_id.eq(instance))
        .select((account::address, account_balance::token, account_balance::balance))
        .into_boxed::<Pg>();
    if let Some(accounts) = accounts {
        query = query.filter(account::address.eq_any(accounts));
    }
    if !include_native {
        query = query.filter(account_balance::token.ne(chain.native_token().address));
    }
    let rows: Vec<(Address, Address, Balance)> = query
        .load(conn)
        .await
        .map_err(PostgresError::from)?;

    let mut balances: HashMap<Address, HashMap<Address, AccountBalance>> = HashMap::new();
    for (address, token, balance) in rows {
        let account_balance =
            AccountBalance::new(address.clone(), token.clone(), balance, Bytes::zero(32));
        balances
            .entry(address)
            .or_default()
            .insert(token, account_balance);
    }
    Ok(balances)
}
