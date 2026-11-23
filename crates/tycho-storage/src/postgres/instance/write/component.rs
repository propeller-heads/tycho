//! Protocol components of an instance.
use std::collections::HashMap;

use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::storage::StorageError;

use crate::postgres::{
    instance::{gateway::PostgresGatewayNew, schema::tycho::protocol_component, InstanceId},
    storage_error_from_diesel,
};

impl PostgresGatewayNew {
    /// Returns the id of each component of the instance, by external id. Deleted components are
    /// found too.
    ///
    /// `NotFound` for the first external id that has no component in the instance.
    pub(crate) async fn component_ids(
        &self,
        instance: InstanceId,
        external_ids: &[&str],
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<String, i64>, StorageError> {
        let ids: HashMap<String, i64> = protocol_component::table
            .filter(protocol_component::instance_id.eq(instance))
            .filter(protocol_component::external_id.eq_any(external_ids))
            .select((protocol_component::external_id, protocol_component::id))
            .load::<(String, i64)>(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(err, "ProtocolComponent", &instance.to_string(), None)
            })?
            .into_iter()
            .collect();
        if let Some(missing) = external_ids
            .iter()
            .find(|id| !ids.contains_key(**id))
        {
            return Err(StorageError::NotFound(
                "ProtocolComponent".to_string(),
                (*missing).to_string(),
            ));
        }
        Ok(ids)
    }
}
