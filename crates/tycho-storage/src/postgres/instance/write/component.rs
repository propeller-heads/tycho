//! Protocol components of an instance.
use std::collections::{BTreeSet, HashMap, HashSet};

use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use tycho_common::{models::protocol::ProtocolComponent, storage::StorageError, Bytes};

use super::token::check_tokens_stored;
use crate::postgres::{
    convert_int,
    instance::{
        gateway::PostgresGatewayNew,
        origins::BatchOrigins,
        orm::{
            NewProtocolComponent, NewProtocolComponentHoldsContract, NewProtocolComponentHoldsToken,
        },
        schema::tycho::{
            protocol_component, protocol_component_holds_contract, protocol_component_holds_token,
            protocol_instance,
        },
        InstanceId,
    },
    schema::{protocol_system, protocol_type},
    storage_error_from_diesel,
};

impl PostgresGatewayNew {
    /// Returns the id of each component of the instance, by external id. Deleted components are
    /// found too, so a batch still writes the balances of a deleted component, as the writes did
    /// before the instance tables. Do not filter on `deleted_at`.
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

    /// Stores the components of a batch in the instance, with their token and contract links.
    /// Each component takes the block number of its creation transaction from `origins`.
    ///
    /// A component of another protocol system than the instance fails with `Unexpected`, also
    /// when it is stored already.
    ///
    /// A component whose external id is stored already, or comes earlier in `components`, is
    /// then skipped with its links, with no error. It is skipped before the other checks, so its
    /// creation transaction can come from an earlier batch.
    ///
    /// `NotFound` for a protocol type or token that is not stored, a contract with no account in
    /// the instance, or a creation transaction that is not in the batch. `Unexpected` for a
    /// component that lists a token or a contract twice. The caller's transaction then rolls back
    /// the rows this method stored.
    pub(crate) async fn insert_protocol_components(
        &self,
        instance: InstanceId,
        origins: &BatchOrigins,
        components: &[ProtocolComponent],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let mut seen = HashSet::with_capacity(components.len());
        let mut unique: Vec<&ProtocolComponent> = components
            .iter()
            .filter(|component| seen.insert(component.id.as_str()))
            .collect();
        check_protocol_system(instance, &unique, conn).await?;
        let external_ids: Vec<&str> = unique
            .iter()
            .map(|component| component.id.as_str())
            .collect();
        let stored: HashSet<String> = protocol_component::table
            .filter(protocol_component::instance_id.eq(instance))
            .filter(protocol_component::external_id.eq_any(&external_ids))
            .select(protocol_component::external_id)
            .load::<String>(conn)
            .await
            .map_err(|err| {
                storage_error_from_diesel(err, "ProtocolComponent", &instance.to_string(), None)
            })?
            .into_iter()
            .collect();
        unique.retain(|component| !stored.contains(&component.id));
        if unique.is_empty() {
            return Ok(());
        }
        let type_names: Vec<&str> = unique
            .iter()
            .map(|component| component.protocol_type_name.as_str())
            .collect();
        check_protocol_types_stored(&type_names, conn).await?;
        for component in &unique {
            check_links_unique(component)?;
        }

        let rows = unique
            .iter()
            .map(|component| new_component_row(instance, origins, component))
            .collect::<Result<Vec<_>, StorageError>>()?;
        let mut inserted: HashMap<String, i64> = HashMap::with_capacity(rows.len());
        for chunk in rows.chunks(NewProtocolComponent::MAX_BATCH_SIZE) {
            let ids: Vec<(String, i64)> = diesel::insert_into(protocol_component::table)
                .values(chunk)
                .on_conflict((protocol_component::instance_id, protocol_component::external_id))
                .do_nothing()
                .returning((protocol_component::external_id, protocol_component::id))
                .get_results(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(err, "ProtocolComponent", &instance.to_string(), None)
                })?;
            inserted.extend(ids);
        }
        let new: Vec<(&ProtocolComponent, i64)> = unique
            .into_iter()
            .filter_map(|component| {
                inserted
                    .get(&component.id)
                    .map(|id| (component, *id))
            })
            .collect();

        self.insert_token_links(instance, &new, conn)
            .await?;
        self.insert_contract_links(instance, &new, conn)
            .await
    }

    /// Links each new component to its tokens, in the order of `tokens`.
    async fn insert_token_links(
        &self,
        instance: InstanceId,
        new: &[(&ProtocolComponent, i64)],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let tokens: Vec<&Bytes> = new
            .iter()
            .flat_map(|(component, _)| &component.tokens)
            .collect();
        if tokens.is_empty() {
            return Ok(());
        }
        check_tokens_stored(&tokens, conn).await?;
        let mut links = Vec::with_capacity(tokens.len());
        for (component, id) in new {
            for (index, token) in component.tokens.iter().enumerate() {
                links.push(NewProtocolComponentHoldsToken {
                    instance_id: instance,
                    protocol_component_id: *id,
                    token: token.clone(),
                    token_index: convert_int(index, "token index")?,
                });
            }
        }
        for chunk in links.chunks(NewProtocolComponentHoldsToken::MAX_BATCH_SIZE) {
            diesel::insert_into(protocol_component_holds_token::table)
                .values(chunk)
                .execute(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(
                        err,
                        "ProtocolComponentHoldsToken",
                        &instance.to_string(),
                        None,
                    )
                })?;
        }
        Ok(())
    }

    /// Links each new component to the accounts of its contracts, in the same instance.
    async fn insert_contract_links(
        &self,
        instance: InstanceId,
        new: &[(&ProtocolComponent, i64)],
        conn: &mut AsyncPgConnection,
    ) -> Result<(), StorageError> {
        let addresses: Vec<&Bytes> = new
            .iter()
            .flat_map(|(component, _)| &component.contract_addresses)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if addresses.is_empty() {
            return Ok(());
        }
        let account_ids = self
            .account_ids(instance, &addresses, conn)
            .await?;
        let links: Vec<NewProtocolComponentHoldsContract> = new
            .iter()
            .flat_map(|(component, id)| {
                component
                    .contract_addresses
                    .iter()
                    .map(|address| NewProtocolComponentHoldsContract {
                        instance_id: instance,
                        protocol_component_id: *id,
                        account_id: account_ids[address],
                    })
            })
            .collect();
        for chunk in links.chunks(NewProtocolComponentHoldsContract::MAX_BATCH_SIZE) {
            diesel::insert_into(protocol_component_holds_contract::table)
                .values(chunk)
                .execute(conn)
                .await
                .map_err(|err| {
                    storage_error_from_diesel(
                        err,
                        "ProtocolComponentHoldsContract",
                        &instance.to_string(),
                        None,
                    )
                })?;
        }
        Ok(())
    }
}

/// The row of a new component. Static attributes are stored as a JSON object of hex strings, or
/// as null when there are none.
fn new_component_row(
    instance: InstanceId,
    origins: &BatchOrigins,
    component: &ProtocolComponent,
) -> Result<NewProtocolComponent, StorageError> {
    let origin = origins.change(&component.creation_tx)?;
    let attributes = (!component.static_attributes.is_empty())
        .then(|| serde_json::to_value(&component.static_attributes))
        .transpose()
        .map_err(|err| {
            StorageError::Unexpected(format!(
                "Static attributes of component {}: {err}",
                component.id
            ))
        })?;
    Ok(NewProtocolComponent {
        instance_id: instance,
        external_id: component.id.clone(),
        protocol_type: component.protocol_type_name.clone(),
        attributes,
        created_at: component.created_at,
        creation_block: origin.block_number,
        creation_tx_hash: component.creation_tx.clone(),
        deleted_at: None,
        deletion_tx_hash: None,
    })
}

/// Fails with `Unexpected` when a component belongs to another protocol system than the instance.
///
/// The instance tables do not store the system of a component: it is the system of its instance.
/// State, balances and links refer to a component inside its instance, so this check keeps the
/// data of each system in an instance of that system.
async fn check_protocol_system(
    instance: InstanceId,
    components: &[&ProtocolComponent],
    conn: &mut AsyncPgConnection,
) -> Result<(), StorageError> {
    let system: String = protocol_instance::table
        .inner_join(protocol_system::table)
        .filter(protocol_instance::id.eq(instance))
        .select(protocol_system::name)
        .first(conn)
        .await
        .map_err(|err| {
            storage_error_from_diesel(err, "ProtocolInstance", &instance.to_string(), None)
        })?;
    if let Some(other) = components
        .iter()
        .find(|component| component.protocol_system != system)
    {
        return Err(StorageError::Unexpected(format!(
            "Component {} of {} cannot go into instance {instance} of {system}",
            other.id, other.protocol_system
        )));
    }
    Ok(())
}

/// Fails with `Unexpected` when a component lists a token or a contract twice. A component has
/// one link per token and per contract.
fn check_links_unique(component: &ProtocolComponent) -> Result<(), StorageError> {
    for (kind, addresses) in
        [("token", &component.tokens), ("contract", &component.contract_addresses)]
    {
        let mut seen = HashSet::with_capacity(addresses.len());
        if let Some(repeated) = addresses
            .iter()
            .find(|address| !seen.insert(*address))
        {
            return Err(StorageError::Unexpected(format!(
                "Component {} lists the {kind} {repeated} twice",
                component.id
            )));
        }
    }
    Ok(())
}

/// Fails with `NotFound` for the first of `names` that is not a stored protocol type.
///
/// Instance components name their type with no foreign key into `public.protocol_type`, so a
/// writer checks the names first.
pub(super) async fn check_protocol_types_stored(
    names: &[&str],
    conn: &mut AsyncPgConnection,
) -> Result<(), StorageError> {
    let unique: HashSet<&str> = names.iter().copied().collect();
    let stored: HashSet<String> = protocol_type::table
        .filter(protocol_type::name.eq_any(&unique))
        .select(protocol_type::name)
        .load::<String>(conn)
        .await
        .map_err(|err| storage_error_from_diesel(err, "ProtocolType", "batch", None))?
        .into_iter()
        .collect();
    if let Some(missing) = names
        .iter()
        .find(|name| !stored.contains(**name))
    {
        return Err(StorageError::NotFound("ProtocolType".to_string(), (*missing).to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{NaiveDate, NaiveDateTime};
    use diesel::prelude::*;
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use serde_json::json;
    use tycho_common::{
        models::{
            blockchain::{Block, Transaction},
            protocol::ProtocolComponent as DomainComponent,
            Chain, ChangeType,
        },
        storage::StorageError,
        Bytes,
    };

    use super::check_protocol_types_stored;
    use crate::postgres::{
        cache::WriteOp,
        db_fixtures,
        instance::{
            gateway::PostgresGatewayNew,
            origins::BatchOrigins,
            orm::ProtocolComponent,
            schema::tycho::{
                protocol_component, protocol_component_holds_contract,
                protocol_component_holds_token,
            },
            InstanceId,
        },
        testing::instance_test_connection,
    };

    #[tokio::test]
    async fn test_component_ids_finds_only_the_instance_components() {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let a = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        let b = db_fixtures::insert_instance(&mut conn, system, "0.0.2").await;
        let pool_of_a = db_fixtures::insert_instance_component(&mut conn, a, "pool", &[]).await;
        db_fixtures::insert_instance_component(&mut conn, b, "pool", &[]).await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;

        let ids = gateway
            .component_ids(a, &["pool"], &mut conn)
            .await
            .unwrap();

        assert_eq!(ids.len(), 1);
        assert_eq!(ids["pool"], pool_of_a);
    }

    #[tokio::test]
    async fn test_component_ids_fails_for_an_unknown_id() {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        db_fixtures::insert_instance_component(&mut conn, instance, "pool", &[]).await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;

        let result = gateway
            .component_ids(instance, &["pool", "missing"], &mut conn)
            .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("ProtocolComponent".to_string(), "missing".to_string()))
        );
    }

    #[tokio::test]
    async fn test_component_ids_finds_a_deleted_component() {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        let pool = db_fixtures::insert_instance_component(&mut conn, instance, "pool", &[]).await;
        let deleted_at = NaiveDate::from_ymd_opt(2021, 1, 1)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("a valid date");
        diesel::update(protocol_component::table)
            .filter(protocol_component::id.eq(pool))
            .set(protocol_component::deleted_at.eq(deleted_at))
            .execute(&mut conn)
            .await
            .expect("deleting the component failed");
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;

        let ids = gateway
            .component_ids(instance, &["pool"], &mut conn)
            .await
            .unwrap();

        assert_eq!(ids["pool"], pool);
    }

    #[tokio::test]
    async fn test_check_protocol_types_stored_accepts_stored_names() {
        let mut conn = instance_test_connection().await;
        db_fixtures::insert_protocol_type(&mut conn, "pool", None, None, None).await;

        let result = check_protocol_types_stored(&["pool", "pool"], &mut conn).await;

        assert_eq!(result, Ok(()));
    }

    #[tokio::test]
    async fn test_check_protocol_types_stored_fails_for_an_unknown_name() {
        let mut conn = instance_test_connection().await;
        db_fixtures::insert_protocol_type(&mut conn, "pool", None, None, None).await;

        let result = check_protocol_types_stored(&["pool", "missing"], &mut conn).await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("ProtocolType".to_string(), "missing".to_string()))
        );
    }

    fn ts(second: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 26)
            .and_then(|date| date.and_hms_opt(12, 0, second))
            .expect("a valid timestamp")
    }

    /// A batch of block 10 at second 10 with the transaction `0xa1` at index 1, and of block 11 at
    /// second 11 with the transaction `0xb2` at index 2.
    fn origins() -> BatchOrigins {
        let block = |number: u8| {
            Block::new(
                number.into(),
                Chain::Ethereum,
                Bytes::from(vec![number]),
                Bytes::default(),
                ts(number.into()),
            )
        };
        let tx = |hash: u8, block: &Block, index: u64| {
            Transaction::new(
                Bytes::from(vec![hash]),
                block.hash.clone(),
                Bytes::default(),
                None,
                index,
            )
        };
        let (block_10, block_11) = (block(10), block(11));
        let ops = vec![
            WriteOp::UpsertBlock(vec![block_10.clone(), block_11.clone()]),
            WriteOp::UpsertTx(vec![tx(0xa1, &block_10, 1), tx(0xb2, &block_11, 2)]),
        ];
        BatchOrigins::from_ops(&ops).expect("a valid batch")
    }

    fn token(id: u8) -> Bytes {
        Bytes::from(vec![0x70, id])
    }

    fn contract(id: u8) -> Bytes {
        Bytes::from(vec![0xc0, id])
    }

    fn component(
        id: &str,
        tokens: Vec<Bytes>,
        contracts: Vec<Bytes>,
        creation_tx: u8,
    ) -> DomainComponent {
        DomainComponent::new(
            id,
            "vm:curve",
            "pool",
            Chain::Ethereum,
            tokens,
            contracts,
            HashMap::new(),
            ChangeType::Creation,
            Bytes::from(vec![creation_tx]),
            ts(1),
        )
    }

    /// An instance with the protocol type `pool`, the tokens 0 and 1, and the account of
    /// contract 1.
    async fn setup() -> (AsyncPgConnection, PostgresGatewayNew, InstanceId) {
        let mut conn = instance_test_connection().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:curve".to_string()).await;
        let instance = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        db_fixtures::insert_protocol_type(&mut conn, "pool", None, None, None).await;
        for id in [0, 1] {
            db_fixtures::insert_token(&mut conn, &hex::encode(token(id)), "TKN", 18, None).await;
        }
        db_fixtures::insert_instance_account(&mut conn, instance, contract(1)).await;
        let gateway = PostgresGatewayNew::from_connection(&mut conn).await;
        (conn, gateway, instance)
    }

    async fn stored(conn: &mut AsyncPgConnection, instance: InstanceId) -> Vec<ProtocolComponent> {
        protocol_component::table
            .filter(protocol_component::instance_id.eq(instance))
            .order_by(protocol_component::external_id)
            .select(ProtocolComponent::as_select())
            .load(conn)
            .await
            .expect("loading the components failed")
    }

    /// Token links of the instance as (component id, token, index), by component and index.
    async fn token_links(
        conn: &mut AsyncPgConnection,
        instance: InstanceId,
    ) -> Vec<(i64, Bytes, i16)> {
        protocol_component_holds_token::table
            .filter(protocol_component_holds_token::instance_id.eq(instance))
            .order_by((
                protocol_component_holds_token::protocol_component_id,
                protocol_component_holds_token::token_index,
            ))
            .select((
                protocol_component_holds_token::protocol_component_id,
                protocol_component_holds_token::token,
                protocol_component_holds_token::token_index,
            ))
            .load(conn)
            .await
            .expect("loading the token links failed")
    }

    /// Contract links of the instance as (component id, account id).
    async fn contract_links(conn: &mut AsyncPgConnection, instance: InstanceId) -> Vec<(i64, i64)> {
        protocol_component_holds_contract::table
            .filter(protocol_component_holds_contract::instance_id.eq(instance))
            .order_by((
                protocol_component_holds_contract::protocol_component_id,
                protocol_component_holds_contract::account_id,
            ))
            .select((
                protocol_component_holds_contract::protocol_component_id,
                protocol_component_holds_contract::account_id,
            ))
            .load(conn)
            .await
            .expect("loading the contract links failed")
    }

    #[tokio::test]
    async fn test_insert_protocol_components_stores_the_component() {
        let (mut conn, gateway, instance) = setup().await;
        let mut with_fee = component("a", vec![token(0)], vec![], 0xb2);
        with_fee.static_attributes = HashMap::from([("fee".to_string(), Bytes::from(100u8))]);
        let without_attributes = component("b", vec![token(0)], vec![], 0xa1);

        gateway
            .insert_protocol_components(
                instance,
                &origins(),
                &[with_fee, without_attributes],
                &mut conn,
            )
            .await
            .unwrap();

        let stored = stored(&mut conn, instance).await;
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].external_id, "a");
        assert_eq!(stored[0].protocol_type, "pool");
        assert_eq!(stored[0].creation_block, 11);
        assert_eq!(stored[0].creation_tx_hash, Bytes::from(vec![0xb2]));
        assert_eq!(stored[0].created_at, ts(1));
        assert_eq!(stored[0].attributes, Some(json!({"fee": "0x64"})));
        assert_eq!(stored[0].deleted_at, None);
        assert_eq!((stored[1].creation_block, stored[1].attributes.as_ref()), (10, None));
    }

    #[tokio::test]
    async fn test_insert_protocol_components_links_tokens_in_order() {
        let (mut conn, gateway, instance) = setup().await;

        gateway
            .insert_protocol_components(
                instance,
                &origins(),
                &[component("a", vec![token(1), token(0)], vec![], 0xa1)],
                &mut conn,
            )
            .await
            .unwrap();

        let id = stored(&mut conn, instance).await[0].id;
        assert_eq!(
            token_links(&mut conn, instance).await,
            vec![(id, token(1), 0), (id, token(0), 1)]
        );
    }

    #[tokio::test]
    async fn test_insert_protocol_components_links_the_instance_account() {
        let (mut conn, gateway, instance) = setup().await;
        let account = gateway
            .account_ids(instance, &[&contract(1)], &mut conn)
            .await
            .unwrap()[&contract(1)];

        gateway
            .insert_protocol_components(
                instance,
                &origins(),
                &[component("a", vec![token(0)], vec![contract(1)], 0xa1)],
                &mut conn,
            )
            .await
            .unwrap();

        let id = stored(&mut conn, instance).await[0].id;
        assert_eq!(contract_links(&mut conn, instance).await, vec![(id, account)]);
    }

    #[tokio::test]
    async fn test_insert_protocol_components_without_tokens_has_no_token_link() {
        let (mut conn, gateway, instance) = setup().await;

        gateway
            .insert_protocol_components(
                instance,
                &origins(),
                &[component("a", vec![], vec![], 0xa1)],
                &mut conn,
            )
            .await
            .unwrap();

        assert_eq!(stored(&mut conn, instance).await.len(), 1);
        assert_eq!(token_links(&mut conn, instance).await, vec![]);
    }

    #[tokio::test]
    async fn test_resent_component_keeps_the_first_row_and_links() {
        let (mut conn, gateway, instance) = setup().await;
        gateway
            .insert_protocol_components(
                instance,
                &origins(),
                &[component("a", vec![token(0)], vec![], 0xa1)],
                &mut conn,
            )
            .await
            .unwrap();

        // Sent again in a later batch: its creation transaction `0xff` is not in this batch, and
        // its protocol type is not stored.
        let mut again = component("a", vec![token(1)], vec![contract(1)], 0xff);
        again.protocol_type_name = "missing".to_string();
        gateway
            .insert_protocol_components(instance, &origins(), &[again], &mut conn)
            .await
            .unwrap();

        let stored = stored(&mut conn, instance).await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].creation_tx_hash, Bytes::from(vec![0xa1]));
        assert_eq!(token_links(&mut conn, instance).await, vec![(stored[0].id, token(0), 0)]);
        assert_eq!(contract_links(&mut conn, instance).await, vec![]);
    }

    #[tokio::test]
    async fn test_same_component_twice_in_one_op_is_stored_once() {
        let (mut conn, gateway, instance) = setup().await;

        gateway
            .insert_protocol_components(
                instance,
                &origins(),
                &[
                    component("a", vec![token(0)], vec![], 0xa1),
                    component("a", vec![token(1)], vec![contract(1)], 0xb2),
                ],
                &mut conn,
            )
            .await
            .unwrap();

        let stored = stored(&mut conn, instance).await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].creation_tx_hash, Bytes::from(vec![0xa1]));
        assert_eq!(token_links(&mut conn, instance).await, vec![(stored[0].id, token(0), 0)]);
        assert_eq!(contract_links(&mut conn, instance).await, vec![]);
    }

    #[tokio::test]
    async fn test_insert_protocol_components_fails_for_an_unknown_type() {
        let (mut conn, gateway, instance) = setup().await;
        let mut unknown = component("a", vec![token(0)], vec![], 0xa1);
        unknown.protocol_type_name = "missing".to_string();

        let result = gateway
            .insert_protocol_components(instance, &origins(), &[unknown], &mut conn)
            .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("ProtocolType".to_string(), "missing".to_string()))
        );
        assert!(stored(&mut conn, instance)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn test_insert_protocol_components_fails_for_an_unknown_token() {
        let (mut conn, gateway, instance) = setup().await;

        let result = gateway
            .insert_protocol_components(
                instance,
                &origins(),
                &[component("a", vec![token(9)], vec![], 0xa1)],
                &mut conn,
            )
            .await;

        assert_eq!(result, Err(StorageError::NotFound("Token".to_string(), token(9).to_string())));
    }

    #[tokio::test]
    async fn test_insert_protocol_components_fails_for_another_protocol_system() {
        let (mut conn, gateway, instance) = setup().await;
        let mut other = component("a", vec![token(0)], vec![], 0xa1);
        other.protocol_system = "vm:other".to_string();

        let result = gateway
            .insert_protocol_components(instance, &origins(), &[other], &mut conn)
            .await;

        assert_eq!(
            result,
            Err(StorageError::Unexpected(format!(
                "Component a of vm:other cannot go into instance {instance} of vm:curve"
            )))
        );
        assert!(stored(&mut conn, instance)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn test_insert_protocol_components_fails_for_a_token_listed_twice() {
        let (mut conn, gateway, instance) = setup().await;

        let result = gateway
            .insert_protocol_components(
                instance,
                &origins(),
                &[component("a", vec![token(0), token(1), token(0)], vec![], 0xa1)],
                &mut conn,
            )
            .await;

        assert_eq!(
            result,
            Err(StorageError::Unexpected(format!(
                "Component a lists the token {} twice",
                token(0)
            )))
        );
        assert!(stored(&mut conn, instance)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn test_insert_protocol_components_fails_for_a_contract_listed_twice() {
        let (mut conn, gateway, instance) = setup().await;

        let result = gateway
            .insert_protocol_components(
                instance,
                &origins(),
                &[component("a", vec![token(0)], vec![contract(1), contract(1)], 0xa1)],
                &mut conn,
            )
            .await;

        assert_eq!(
            result,
            Err(StorageError::Unexpected(format!(
                "Component a lists the contract {} twice",
                contract(1)
            )))
        );
        assert!(stored(&mut conn, instance)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn test_insert_protocol_components_fails_for_an_account_of_another_instance() {
        let (mut conn, gateway, instance) = setup().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:other".to_string()).await;
        let other = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        db_fixtures::insert_instance_account(&mut conn, other, contract(2)).await;

        let result = gateway
            .insert_protocol_components(
                instance,
                &origins(),
                &[component("a", vec![token(0)], vec![contract(2)], 0xa1)],
                &mut conn,
            )
            .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound("Account".to_string(), contract(2).to_string()))
        );
    }

    #[tokio::test]
    async fn test_insert_protocol_components_fails_for_a_tx_outside_the_batch() {
        let (mut conn, gateway, instance) = setup().await;

        let result = gateway
            .insert_protocol_components(
                instance,
                &origins(),
                &[component("a", vec![token(0)], vec![], 0xff)],
                &mut conn,
            )
            .await;

        assert_eq!(
            result,
            Err(StorageError::NotFound(
                "Transaction".to_string(),
                Bytes::from(vec![0xff]).to_string()
            ))
        );
    }

    #[tokio::test]
    async fn test_insert_protocol_components_writes_only_its_instance() {
        let (mut conn, gateway, instance) = setup().await;
        let system = db_fixtures::insert_protocol_system(&mut conn, "vm:other".to_string()).await;
        let other = db_fixtures::insert_instance(&mut conn, system, "0.0.1").await;
        let other_pool =
            db_fixtures::insert_instance_component(&mut conn, other, "a", &[token(1)]).await;

        gateway
            .insert_protocol_components(
                instance,
                &origins(),
                &[component("a", vec![token(0)], vec![], 0xa1)],
                &mut conn,
            )
            .await
            .unwrap();

        let ours = stored(&mut conn, instance).await;
        assert_eq!(ours.len(), 1);
        assert_eq!(token_links(&mut conn, instance).await, vec![(ours[0].id, token(0), 0)]);
        let theirs = stored(&mut conn, other).await;
        assert_eq!((theirs.len(), theirs[0].id), (1, other_pool));
        assert_eq!(token_links(&mut conn, other).await, vec![(other_pool, token(1), 0)]);
    }
}
