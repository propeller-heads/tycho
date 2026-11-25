//! Contracts of an instance: accounts, and the versions of their code, storage and balances.
//!
//! Each instance stores its own copy of every contract it uses, so the same address can have one
//! account in each instance.
use std::collections::HashMap;

use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{storage::StorageError, Bytes};

use crate::postgres::{
    instance::{gateway::PostgresGatewayNew, schema::tycho::account, InstanceId},
    storage_error_from_diesel,
};

impl PostgresGatewayNew {
    /// Returns the id of each account of the instance, by address. Deleted accounts are found too.
    ///
    /// `NotFound` for the first address that has no account in the instance.
    pub(crate) async fn account_ids(
        &self,
        instance: InstanceId,
        addresses: &[&Bytes],
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<Bytes, i64>, StorageError> {
        let ids: HashMap<Bytes, i64> = account::table
            .filter(account::instance_id.eq(instance))
            .filter(account::address.eq_any(addresses))
            .select((account::address, account::id))
            .load::<(Bytes, i64)>(conn)
            .await
            .map_err(|err| storage_error_from_diesel(err, "Account", &instance.to_string(), None))?
            .into_iter()
            .collect();
        if let Some(missing) = addresses
            .iter()
            .find(|address| !ids.contains_key(**address))
        {
            return Err(StorageError::NotFound("Account".to_string(), missing.to_string()));
        }
        Ok(ids)
    }
}
