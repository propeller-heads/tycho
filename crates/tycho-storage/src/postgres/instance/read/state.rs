use std::collections::HashMap;

use diesel::{pg::Pg, prelude::*};
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{
    models::{protocol::ComponentBalance, Address, Chain, ComponentId},
    storage::{StorageError, Version},
    Bytes,
};

use super::version::check_current;
use crate::postgres::{
    instance::{
        active::active_instance_head,
        gateway::PostgresGatewayNew,
        orm as iorm,
        schema::tycho::{component_balance, protocol_component},
        InstanceId,
    },
    PostgresError,
};

impl PostgresGatewayNew {
    /// Returns the token balances of the components of `system`'s active instance, by external id
    /// and token address.
    ///
    /// - `system`: the protocol system, because the answer is keyed by external id and an external
    ///   id can repeat across protocol systems. `NotFound` for an unknown name. A protocol system
    ///   without an active instance has no balances.
    /// - `ids`: only components with these external ids.
    /// - `at`: the version. `Unsupported` for a version before the head of the instance.
    ///
    /// Components without balances are absent. `modify_tx` is always 32 zero bytes. `NotFound` for
    /// an unknown chain.
    pub async fn get_component_balances(
        &self,
        chain: &Chain,
        system: &str,
        ids: Option<&[&str]>,
        at: Option<&Version>,
        conn: &mut AsyncPgConnection,
    ) -> Result<HashMap<ComponentId, HashMap<Address, ComponentBalance>>, StorageError> {
        self.get_chain_id(chain)?;
        self.get_protocol_system_id(system)?;
        let Some(instance) = active_instance_head(system, conn).await? else {
            return Ok(HashMap::new());
        };
        check_current(&instance, at, conn).await?;

        let rows = balance_rows(instance.id, ids, conn).await?;
        let mut balances: HashMap<ComponentId, HashMap<Address, ComponentBalance>> = HashMap::new();
        for (external_id, row) in rows {
            let balance = ComponentBalance::new(
                row.token.clone(),
                row.new_balance,
                row.balance_float,
                Bytes::zero(32),
                &external_id,
            );
            balances
                .entry(external_id)
                .or_default()
                .insert(row.token, balance);
        }
        Ok(balances)
    }
}

/// Loads the balance rows of the components of `instance`, each with its component's external id.
/// `ids`: only components with these external ids.
async fn balance_rows(
    instance: InstanceId,
    ids: Option<&[&str]>,
    conn: &mut AsyncPgConnection,
) -> Result<Vec<(ComponentId, iorm::ComponentBalance)>, StorageError> {
    let mut query = component_balance::table
        .inner_join(
            protocol_component::table.on(protocol_component::instance_id
                .eq(component_balance::instance_id)
                .and(protocol_component::id.eq(component_balance::protocol_component_id))),
        )
        .filter(component_balance::instance_id.eq(instance))
        .filter(protocol_component::instance_id.eq(instance))
        .select((protocol_component::external_id, iorm::ComponentBalance::as_select()))
        .into_boxed::<Pg>();
    if let Some(ids) = ids {
        query = query.filter(protocol_component::external_id.eq_any(ids));
    }
    Ok(query
        .load(conn)
        .await
        .map_err(PostgresError::from)?)
}
