use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use async_trait::async_trait;
use chrono::NaiveDateTime;
use mockall::mock;
use tycho_common::{
    models::{
        blockchain::{
            Block, EntryPoint, EntryPointWithTracingParams, TracedEntryPoint, TracingParams,
            TracingResult, Transaction,
        },
        contract::{Account, AccountBalance, AccountDelta},
        protocol::{
            ComponentBalance, ProtocolComponent, ProtocolComponentState,
            ProtocolComponentStateDelta, QualityRange,
        },
        token::Token,
        Address, Chain, ComponentId, ContractId, EntryPointId, ExtractionState, PaginationParams,
        ProtocolType, TxHash,
    },
    storage::{
        BlockIdentifier, BlockOrTimestamp, ChainGateway, ContractStateGateway, EntryPointFilter,
        EntryPointGateway, ExtractionStateGateway, Gateway, ProtocolGateway, StorageError, Version,
        WithTotal,
    },
    Bytes,
};

mock! {
    pub Gateway {}
    #[async_trait]
    impl ExtractionStateGateway for Gateway {
        async fn get_state(&self, name: &str, chain: &Chain) -> Result<ExtractionState, StorageError>;
        async fn save_state(&self, state: &ExtractionState) -> Result<(), StorageError>;
    }

    #[async_trait]
    impl ChainGateway for Gateway {
        async fn upsert_block(&self, new: &[Block]) -> Result<(), StorageError>;
        async fn get_block(&self, id: &BlockIdentifier) -> Result<Block, StorageError>;
        async fn upsert_tx(&self, new: &[Transaction]) -> Result<(), StorageError>;
        async fn get_tx(&self, hash: &TxHash) -> Result<Transaction, StorageError>;
        async fn revert_state(&self, to: &BlockIdentifier) -> Result<(), StorageError>;
    }

    impl EntryPointGateway for Gateway {
        #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
        fn insert_entry_points<'life0, 'life1, 'async_trait>(
            &'life0 self,
            entry_points: &'life1 HashMap<ComponentId, HashSet<EntryPoint>>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<Output = Result<(), StorageError>>
                    + ::core::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
        fn insert_entry_point_tracing_params<'life0, 'life1, 'async_trait>(
            &'life0 self,
            entry_points_params: &'life1 HashMap<
                EntryPointId,
                HashSet<(TracingParams, ComponentId)>,
            >,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<Output = Result<(), StorageError>>
                    + ::core::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
        fn get_entry_points<'life0, 'life1, 'async_trait>(
            &'life0 self,
            filter: EntryPointFilter,
            pagination_params: Option<&'life1 PaginationParams>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                        Output = Result<
                            WithTotal<HashMap<ComponentId, HashSet<EntryPoint>>>,
                            StorageError,
                        >,
                    > + ::core::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
        fn get_entry_points_tracing_params<'life0, 'life1, 'async_trait>(
            &'life0 self,
            filter: EntryPointFilter,
            pagination_params: Option<&'life1 PaginationParams>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                        Output = Result<
                            WithTotal<HashMap<ComponentId, HashSet<EntryPointWithTracingParams>>>,
                            StorageError,
                        >,
                    > + ::core::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
        fn upsert_traced_entry_points<'life0, 'life1, 'async_trait>(
            &'life0 self,
            traced_entry_points: &'life1 [TracedEntryPoint],
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<Output = Result<(), StorageError>>
                    + ::core::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
        fn get_traced_entry_points<'life0, 'life1, 'async_trait>(
            &'life0 self,
            entry_points: &'life1 HashSet<EntryPointId>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                        Output = Result<
                            HashMap<EntryPointId, HashMap<TracingParams, TracingResult>>,
                            StorageError,
                        >,
                    > + ::core::marker::Send
                    + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;
    }

    impl ContractStateGateway for Gateway {
        fn get_contract<'life0, 'life1, 'life2, 'async_trait>(
            &'life0 self,
            id: &'life1 ContractId,
            version: Option<&'life2 Version>,
            include_slots: bool,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<Account, StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity)]
        fn get_contracts<'life0, 'life1, 'life2, 'life3, 'life4, 'async_trait>(
            &'life0 self,
            chain: &'life1 Chain,
            addresses: Option<&'life2 [Address]>,
            version: Option<&'life3 Version>,
            include_slots: bool,
            pagination_params: Option<&'life4 PaginationParams>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<WithTotal<Vec<Account>>, StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            'life3: 'async_trait,
            'life4: 'async_trait,
            Self: 'async_trait;

        fn insert_contract<'life0, 'life1, 'async_trait>(
            &'life0 self,
            new: &'life1 Account,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<(), StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        fn update_contracts<'life0, 'life1, 'async_trait>(
            &'life0 self,
            new: &'life1 [(TxHash, AccountDelta)],
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<(), StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        fn delete_contract<'life0, 'life1, 'life2, 'async_trait>(
            &'life0 self,
            id: &'life1 ContractId,
            at_tx: &'life2 TxHash,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<(), StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            Self: 'async_trait;

        fn get_accounts_delta<'life0, 'life1, 'life2, 'life3, 'async_trait>(
            &'life0 self,
            chain: &'life1 Chain,
            start_version: Option<&'life2 BlockOrTimestamp>,
            end_version: &'life3 BlockOrTimestamp,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<Vec<AccountDelta>, StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            'life3: 'async_trait,
            Self: 'async_trait;


        fn add_account_balances<'life0, 'life1, 'async_trait>(
            &'life0 self,
            account_balances: &'life1 [AccountBalance],
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<(), StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity)]
        fn get_account_balances<'life0, 'life1, 'life2, 'life3, 'async_trait>(
            &'life0 self,
            chain: &'life1 Chain,
            accounts: Option<&'life2 [Address]>,
            version: Option<&'life3 Version>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<HashMap<Address, HashMap<Address, AccountBalance>>, StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            'life3: 'async_trait,
            Self: 'async_trait;

    }

    impl ProtocolGateway for Gateway {
        #[allow(clippy::type_complexity)]
        fn get_protocol_components<'life0, 'life1, 'life2, 'life3, 'life4, 'async_trait>(
            &'life0 self,
            chain: &'life1 Chain,
            system: Option<String>,
            ids: Option<&'life2 [&'life3 str]>,
            min_tvl: Option<f64>,
            pagination_params: Option<&'life4 PaginationParams>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<WithTotal<Vec<ProtocolComponent>>,
                        StorageError,
                    >,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            'life3: 'async_trait,
            'life4: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity)]
        fn get_token_owners<'life0, 'life1, 'life2, 'async_trait>(
            &'life0 self,
            chain: &'life1 Chain,
            tokens: &'life2 [Address],
            min_balance: Option<f64>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<
                        HashMap<Address, (ComponentId, Bytes)>,
                        StorageError,
                    >,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            Self: 'async_trait;

        fn add_protocol_components<'life0, 'life1, 'async_trait>(
            &'life0 self,
            new: &'life1 [ProtocolComponent],
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<(), StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        fn delete_protocol_components<'life0, 'life1, 'async_trait>(
            &'life0 self,
            to_delete: &'life1 [ProtocolComponent],
            block_ts: NaiveDateTime,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<(), StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        fn add_protocol_types<'life0, 'life1, 'async_trait>(
            &'life0 self,
            new_protocol_types: &'life1 [ProtocolType],
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<(), StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity)]
        fn get_protocol_states<'life0, 'life1, 'life2, 'life3, 'life4, 'async_trait>(
            &'life0 self,
            chain: &'life1 Chain,
            at: Option<Version>,
            system: Option<String>,
            ids: Option<&'life2 [&'life3 str]>,
            retrieve_balances: bool,
            pagination_params: Option<&'life4 PaginationParams>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<WithTotal<Vec<ProtocolComponentState>>,
                        StorageError,
                    >,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            'life3: 'async_trait,
            'life4: 'async_trait,
            Self: 'async_trait;

        fn update_protocol_states<'life0, 'life1, 'async_trait>(
            &'life0 self,
            new: &'life1 [(TxHash, ProtocolComponentStateDelta)],
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<(), StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity)]
        fn get_tokens<'life0, 'life1, 'life2, 'life3, 'async_trait>(
            &'life0 self,
            chain: Chain,
            address: Option<&'life1 [&'life2 Address]>,
            quality: QualityRange,
            traded_n_days_ago: Option<NaiveDateTime>,
            pagination_params: Option<&'life3 PaginationParams>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<WithTotal<Vec<Token>>, StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            'life3: 'async_trait,
            Self: 'async_trait;

        fn add_component_balances<'life0, 'life1, 'async_trait>(
            &'life0 self,
            component_balances: &'life1 [ComponentBalance],
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<(), StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        fn add_tokens<'life0, 'life1, 'async_trait>(
            &'life0 self,
            tokens: &'life1 [Token],
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<(), StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        fn update_tokens<'life0, 'life1, 'async_trait>(
            &'life0 self,
            tokens: &'life1 [Token],
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<(), StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        fn get_protocol_states_delta<'life0, 'life1, 'life2, 'life3, 'async_trait>(
            &'life0 self,
            chain: &'life1 Chain,
            start_version: Option<&'life2 BlockOrTimestamp>,
            end_version: &'life3 BlockOrTimestamp,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<
                        Vec<ProtocolComponentStateDelta>,
                        StorageError,
                    >,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            'life3: 'async_trait,
            Self: 'async_trait;

        fn get_balance_deltas<'life0, 'life1, 'life2, 'life3, 'async_trait>(
            &'life0 self,
            chain: &'life1 Chain,
            start_version: Option<&'life2 BlockOrTimestamp>,
            target_version: &'life3 BlockOrTimestamp,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<
                        Vec<ComponentBalance>,
                        StorageError,
                    >,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            'life3: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity)]
        fn get_component_balances<'life0, 'life1, 'life2, 'life3, 'life4, 'async_trait>(
            &'life0 self,
            chain: &'life1 Chain,
            ids: Option<&'life2 [&'life3 str]>,
            version: Option<&'life4 Version>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<HashMap<String, HashMap<Bytes, ComponentBalance>>, StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            'life3: 'async_trait,
            'life4: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity)]
        fn get_token_prices<'life0, 'life1, 'async_trait>(
            &'life0 self,
            chain: &'life1 Chain,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<HashMap<Bytes, f64>, StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait;

        fn upsert_component_tvl<'life0, 'life1, 'life2, 'async_trait>(
            &'life0 self,
            chain: &'life1 Chain,
            tvl_values: &'life2 HashMap<String, f64>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<(), StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity)]
        fn get_protocol_systems<'life0, 'life1, 'life2, 'async_trait>(
            &'life0 self,
            chain: &'life1 Chain,
            pagination_params: Option<&'life2 PaginationParams>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<WithTotal<Vec<String>>, StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            Self: 'async_trait;

        #[allow(clippy::type_complexity)]
        fn get_component_tvls<'life0, 'life1, 'life2, 'life3, 'life4, 'async_trait>(
            &'life0 self,
            chain: &'life1 Chain,
            system: Option<String>,
            ids: Option<&'life2 [&'life3 str]>,
            pagination_params: Option<&'life4 PaginationParams>,
        ) -> ::core::pin::Pin<
            Box<
                dyn ::core::future::Future<
                    Output = Result<WithTotal<HashMap<String, f64>>, StorageError>,
                > + ::core::marker::Send + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            'life2: 'async_trait,
            'life3: 'async_trait,
            'life4: 'async_trait,
            Self: 'async_trait;
    }

    impl Gateway for Gateway {}
}

#[cfg(test)]
pub fn evm_contract_slots(data: impl IntoIterator<Item = (i32, i32)>) -> HashMap<Bytes, Bytes> {
    data.into_iter()
        .map(|(s, v)| {
            (Bytes::from(u32::try_from(s).unwrap()), Bytes::from(u32::try_from(v).unwrap()))
        })
        .collect()
}

/// Creates a block for testing, version 0 is not allowed and will panic.
#[cfg(test)]
pub fn block(version: u64) -> Block {
    if version == 0 {
        panic!("Block version 0 doesn't exist. Smallest version is 1");
    }

    let ts: NaiveDateTime = "2020-01-01T00:00:00"
        .parse()
        .expect("failed parsing block ts");
    Block::new(
        version,
        Chain::Ethereum,
        Bytes::from(version).lpad(32, 0),
        Bytes::from(version - 1).lpad(32, 0),
        ts + Duration::from_secs(version * 12),
    )
}

#[cfg(test)]
pub mod fixtures {
    use std::{collections::HashSet, str::FromStr};

    use prost::Message;
    use tycho_common::{models::protocol::ProtocolComponentStateDelta, Bytes};
    use tycho_storage::postgres::db_fixtures::yesterday_midnight;
    use tycho_substreams::pb::tycho::evm::v1::*;

    use crate::extractor::models::fixtures::HASH_256_0;

    pub fn pb_state_changes() -> EntityChanges {
        let res1_value = Bytes::from(1_000u64)
            .lpad(32, 0)
            .to_vec();
        let res2_value = Bytes::from(500u64).lpad(32, 0).to_vec();
        EntityChanges {
            component_id: "State1".to_owned(),
            attributes: vec![
                Attribute {
                    name: "reserve1".to_owned(),
                    value: res1_value,
                    change: ChangeType::Update.into(),
                },
                Attribute {
                    name: "reserve2".to_owned(),
                    value: res2_value,
                    change: ChangeType::Update.into(),
                },
            ],
        }
    }

    pub fn protocol_state_delta() -> ProtocolComponentStateDelta {
        let res1_value = Bytes::from(1_000u64)
            .lpad(32, 0)
            .to_vec();
        let res2_value = Bytes::from(500u64).lpad(32, 0).to_vec();
        ProtocolComponentStateDelta {
            component_id: "State1".to_string(),
            updated_attributes: vec![
                ("reserve1".to_owned(), Bytes::from(res1_value)),
                ("reserve2".to_owned(), Bytes::from(res2_value)),
            ]
            .into_iter()
            .collect(),
            deleted_attributes: HashSet::new(),
            ..Default::default()
        }
    }

    pub fn pb_protocol_component() -> ProtocolComponent {
        ProtocolComponent {
            id: "d417ff54652c09bd9f31f216b1a2e5d1e28c1dce1ba840c40d16f2b4d09b5902".to_owned(),
            tokens: vec![address_from_str(DAI_ADDRESS), address_from_str(DAI_ADDRESS)],
            contracts: vec![
                Bytes::from_str("0x31fF2589Ee5275a2038beB855F44b9Be993aA804")
                    .unwrap()
                    .0
                    .to_vec(),
                address_from_str(WETH_ADDRESS),
            ],
            static_att: vec![
                Attribute {
                    name: "balance".to_owned(),
                    value: Bytes::from(100u64).lpad(32, 0).to_vec(),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "factory_address".to_owned(),
                    value: b"0x0fwe0g240g20".to_vec(),
                    change: ChangeType::Creation.into(),
                },
            ],
            change: ChangeType::Creation.into(),
            protocol_type: Some(ProtocolType {
                name: "WeightedPool".to_string(),
                financial_type: 0,
                attribute_schema: vec![],
                implementation_type: 0,
            }),
        }
    }

    pub fn pb_blocks(version: u64) -> Block {
        if version == 0 {
            panic!("Block version 0 doesn't exist. It starts at 1");
        }
        let base_ts = yesterday_midnight()
            .and_utc()
            .timestamp() as u64;

        Block {
            number: version,
            hash: Bytes::from(version)
                .lpad(32, 0)
                .to_vec(),
            parent_hash: Bytes::from(version - 1)
                .lpad(32, 0)
                .to_vec(),
            ts: base_ts + version * 1000,
        }
    }

    pub fn pb_transactions(version: u64, index: u64) -> Transaction {
        Transaction {
            hash: Bytes::from(version * 10_000 + index)
                .lpad(32, 0)
                .to_vec(),
            from: Bytes::from(version * 100_000 + index)
                .lpad(20, 0)
                .to_vec(),
            to: Bytes::from(version * 1_000_000 + index)
                .lpad(20, 0)
                .to_vec(),
            index,
        }
    }

    const WETH_ADDRESS: &str = "C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
    const USDC_ADDRESS: &str = "A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
    const DAI_ADDRESS: &str = "6B175474E89094C44Da98b954EedeAC495271d0F";
    const USDT_ADDRESS: &str = "dAC17F958D2ee523a2206206994597C13D831ec7";

    pub fn address_from_str(token: &str) -> Vec<u8> {
        Bytes::from_str(token)
            .unwrap()
            .0
            .to_vec()
    }

    pub fn pb_block_scoped_data(
        msg: impl prost::Message,
        cursor: Option<&str>,
        final_block_height: Option<u64>,
    ) -> crate::pb::sf::substreams::rpc::v2::BlockScopedData {
        use crate::pb::sf::substreams::{rpc::v2::*, v1::Clock};
        let val = msg.encode_to_vec();
        BlockScopedData {
            output: Some(MapModuleOutput {
                name: "map_changes".to_owned(),
                map_output: Some(prost_types::Any {
                    type_url: "tycho.evm.v1.BlockChanges".to_owned(),
                    value: val,
                }),
                debug_info: None,
            }),
            clock: Some(Clock {
                id: HASH_256_0.to_owned(),
                number: 420,
                timestamp: Some(prost_types::Timestamp { seconds: 1000, nanos: 0 }),
            }),
            cursor: cursor
                .unwrap_or("cursor@420")
                .to_owned(),
            final_block_height: final_block_height.unwrap_or(420),
            debug_map_outputs: vec![],
            debug_store_outputs: vec![],
            attestation: "test_attestation".to_owned(),
            is_partial: false,
            partial_index: None,
            is_last_partial: None,
        }
    }

    pub fn pb_vm_block_changes(version: u8) -> BlockChanges {
        match version {
            0 => BlockChanges {
                block: Some(Block {
                    hash: vec![0x31, 0x32, 0x33, 0x34],
                    parent_hash: vec![0x21, 0x22, 0x23, 0x24],
                    number: 1,
                    ts: 1000,
                }),
                changes: vec![
                    TransactionChanges {
                        tx: Some(Transaction {
                            hash: vec![0x11, 0x12, 0x13, 0x14],
                            from: vec![0x41, 0x42, 0x43, 0x44],
                            to: vec![0x51, 0x52, 0x53, 0x54],
                            index: 2,
                        }),
                        contract_changes: vec![ContractChange {
                            address: vec![0x61, 0x62, 0x63, 0x64],
                            balance: vec![0x71, 0x72, 0x73, 0x74],
                            code: vec![0x81, 0x82, 0x83, 0x84],
                            slots: vec![
                                ContractSlot {
                                    slot: vec![0xa1, 0xa2, 0xa3, 0xa4],
                                    value: vec![0xb1, 0xb2, 0xb3, 0xb4],
                                    previous_value: Bytes::new().into(),
                                },
                                ContractSlot {
                                    slot: vec![0xc1, 0xc2, 0xc3, 0xc4],
                                    value: vec![0xd1, 0xd2, 0xd3, 0xd4],
                                    previous_value: Bytes::new().into(),
                                },
                            ],
                            change: ChangeType::Update.into(),
                            token_balances: vec![AccountBalanceChange {
                                token: address_from_str(WETH_ADDRESS),
                                balance: 50000000.encode_to_vec(),
                            }],
                        }],
                        component_changes: vec![ProtocolComponent {
                            id: "d417ff54652c09bd9f31f216b1a2e5d1e28c1dce1ba840c40d16f2b4d09b5902"
                                .to_owned(),
                            tokens: vec![
                                address_from_str(DAI_ADDRESS),
                                address_from_str(DAI_ADDRESS),
                            ],
                            contracts: vec![
                                address_from_str(WETH_ADDRESS),
                                address_from_str(WETH_ADDRESS),
                            ],
                            static_att: vec![
                                Attribute {
                                    name: "key1".to_owned(),
                                    value: b"value1".to_vec(),
                                    change: ChangeType::Creation.into(),
                                },
                                Attribute {
                                    name: "key2".to_owned(),
                                    value: b"value2".to_vec(),
                                    change: ChangeType::Creation.into(),
                                },
                            ],
                            change: ChangeType::Creation.into(),
                            protocol_type: Some(ProtocolType {
                                name: "WeightedPool".to_string(),
                                financial_type: 0,
                                attribute_schema: vec![],
                                implementation_type: 0,
                            }),
                        }],
                        balance_changes: vec![BalanceChange {
                            token: address_from_str(WETH_ADDRESS),
                            balance: 50000000.encode_to_vec(),
                            component_id:
                                "d417ff54652c09bd9f31f216b1a2e5d1e28c1dce1ba840c40d16f2b4d09b5902"
                                    .as_bytes()
                                    .to_vec(),
                        }],
                        ..Default::default()
                    },
                    TransactionChanges {
                        tx: Some(Transaction {
                            hash: vec![
                                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
                            ],
                            from: vec![0x41, 0x42, 0x43, 0x44],
                            to: vec![0x51, 0x52, 0x53, 0x54],
                            index: 5,
                        }),
                        contract_changes: vec![ContractChange {
                            address: vec![0x61, 0x62, 0x63, 0x64],
                            balance: vec![0xf1, 0xf2, 0xf3, 0xf4],
                            code: vec![0x01, 0x02, 0x03, 0x04],
                            slots: vec![
                                ContractSlot {
                                    slot: vec![0x91, 0x92, 0x93, 0x94],
                                    value: vec![0xa1, 0xa2, 0xa3, 0xa4],
                                    previous_value: Bytes::new().into(),
                                },
                                ContractSlot {
                                    slot: vec![0xa1, 0xa2, 0xa3, 0xa4],
                                    value: vec![0xc1, 0xc2, 0xc3, 0xc4],
                                    previous_value: Bytes::new().into(),
                                },
                            ],
                            change: ChangeType::Update.into(),
                            token_balances: vec![AccountBalanceChange {
                                token: address_from_str(WETH_ADDRESS),
                                balance: 10.encode_to_vec(),
                            }],
                        }],
                        balance_changes: vec![BalanceChange {
                            token: address_from_str(WETH_ADDRESS),
                            balance: 10.encode_to_vec(),
                            component_id:
                                "d417ff54652c09bd9f31f216b1a2e5d1e28c1dce1ba840c40d16f2b4d09b5902"
                                    .to_string()
                                    .as_bytes()
                                    .to_vec(),
                        }],
                        ..Default::default()
                    },
                ],
                storage_changes: vec![],
            },
            1 => BlockChanges {
                block: Some(pb_blocks(version as u64)),
                changes: vec![TransactionChanges {
                    tx: Some(pb_transactions(1, 1)),
                    contract_changes: vec![ContractChange {
                        address: address_from_str("0000000000000000000000000000000000000001"),
                        balance: 1_i32.to_be_bytes().to_vec(),
                        code: 123_i32.to_be_bytes().to_vec(),
                        slots: vec![ContractSlot {
                            slot: Bytes::from("0x01").into(),
                            value: Bytes::from("0x01").into(),
                            previous_value: Bytes::new().into(),
                        }],
                        change: ChangeType::Creation.into(),
                        token_balances: vec![
                            AccountBalanceChange {
                                token: address_from_str(WETH_ADDRESS),
                                balance: 1_i32.to_be_bytes().to_vec(),
                            },
                            AccountBalanceChange {
                                token: address_from_str(USDC_ADDRESS),
                                balance: 1_i32.to_be_bytes().to_vec(),
                            },
                        ],
                    }],
                    component_changes: vec![ProtocolComponent {
                        id: "pc_1".to_owned(),
                        tokens: vec![
                            address_from_str(WETH_ADDRESS),
                            address_from_str(USDC_ADDRESS),
                        ],
                        contracts: vec![],
                        static_att: vec![],
                        change: ChangeType::Creation.into(),
                        protocol_type: Some(ProtocolType {
                            name: "pt_1".to_string(),
                            financial_type: 0,
                            attribute_schema: vec![],
                            implementation_type: 0,
                        }),
                    }],
                    balance_changes: vec![
                        BalanceChange {
                            token: address_from_str(USDC_ADDRESS),
                            balance: 1_i32.to_be_bytes().to_vec(),
                            component_id: "pc_1".into(),
                        },
                        BalanceChange {
                            token: address_from_str(WETH_ADDRESS),
                            balance: 1_i32.to_be_bytes().to_vec(),
                            component_id: "pc_1".into(),
                        },
                    ],
                    ..Default::default()
                }],
                storage_changes: vec![],
            },
            2 => BlockChanges {
                block: Some(pb_blocks(version as u64)),
                changes: vec![TransactionChanges {
                    tx: Some(pb_transactions(2, 1)),
                    contract_changes: vec![
                        ContractChange {
                            address: address_from_str("0000000000000000000000000000000000000002"),
                            balance: 2_i32.to_be_bytes().to_vec(),
                            code: 123_i32.to_be_bytes().to_vec(),
                            slots: vec![ContractSlot {
                                slot: Bytes::from("0x01").into(),
                                value: Bytes::from("0x02").into(),
                                previous_value: Bytes::new().into(),
                            }],
                            change: ChangeType::Creation.into(),
                            token_balances: vec![
                                AccountBalanceChange {
                                    token: address_from_str(USDT_ADDRESS),
                                    balance: 20_i32.to_be_bytes().to_vec(),
                                },
                                AccountBalanceChange {
                                    token: address_from_str(USDC_ADDRESS),
                                    balance: 20_i32.to_be_bytes().to_vec(),
                                },
                            ],
                        },
                        ContractChange {
                            address: address_from_str("0000000000000000000000000000000000000001"),
                            balance: 10_i32.to_be_bytes().to_vec(),
                            code: 123_i32.to_be_bytes().to_vec(),
                            slots: vec![
                                ContractSlot {
                                    slot: Bytes::from("0x01").into(),
                                    value: Bytes::from("0x10").into(),
                                    previous_value: Bytes::new().into(),
                                },
                                ContractSlot {
                                    slot: Bytes::from("0x02").into(),
                                    value: Bytes::from("0x0a").into(),
                                    previous_value: Bytes::new().into(),
                                },
                            ],
                            change: ChangeType::Update.into(),
                            token_balances: vec![AccountBalanceChange {
                                token: address_from_str(USDC_ADDRESS),
                                balance: 10_i32.to_be_bytes().to_vec(),
                            }],
                        },
                    ],
                    component_changes: vec![ProtocolComponent {
                        id: "pc_2".to_owned(),
                        tokens: vec![
                            address_from_str(USDT_ADDRESS),
                            address_from_str(USDC_ADDRESS),
                        ],
                        contracts: vec![address_from_str(
                            "0000000000000000000000000000000000000002",
                        )],
                        static_att: vec![],
                        change: ChangeType::Creation.into(),
                        protocol_type: Some(ProtocolType {
                            name: "pt_1".to_string(),
                            financial_type: 0,
                            attribute_schema: vec![],
                            implementation_type: 0,
                        }),
                    }],
                    balance_changes: vec![
                        BalanceChange {
                            token: address_from_str(USDC_ADDRESS),
                            balance: 20_i32.to_be_bytes().to_vec(),
                            component_id: "pc_2".into(),
                        },
                        BalanceChange {
                            token: address_from_str(USDT_ADDRESS),
                            balance: 20_i32.to_be_bytes().to_vec(),
                            component_id: "pc_2".into(),
                        },
                        BalanceChange {
                            token: address_from_str(USDC_ADDRESS),
                            balance: 10_i32.to_be_bytes().to_vec(),
                            component_id: "pc_1".into(),
                        },
                    ],
                    ..Default::default()
                }],
                storage_changes: vec![],
            },
            3 => BlockChanges {
                block: Some(pb_blocks(version as u64)),
                changes: vec![
                    TransactionChanges {
                        tx: Some(pb_transactions(3, 1)),
                        contract_changes: vec![
                            ContractChange {
                                address: address_from_str(
                                    "0000000000000000000000000000000000000001",
                                ),
                                balance: 1_i32.to_be_bytes().to_vec(),
                                code: 123_i32.to_be_bytes().to_vec(),
                                slots: vec![ContractSlot {
                                    slot: Bytes::from("0x01").into(),
                                    value: Bytes::from("0x01").into(),
                                    previous_value: Bytes::new().into(),
                                }],
                                change: ChangeType::Update.into(),
                                token_balances: vec![],
                            },
                            ContractChange {
                                address: address_from_str(
                                    "0000000000000000000000000000000000000002",
                                ),
                                balance: 20_i32.to_be_bytes().to_vec(),
                                code: 123_i32.to_be_bytes().to_vec(),
                                slots: vec![ContractSlot {
                                    slot: Bytes::from("0x02").into(),
                                    value: Bytes::from("0xc8").into(),
                                    previous_value: Bytes::new().into(),
                                }],
                                change: ChangeType::Update.into(),
                                token_balances: vec![AccountBalanceChange {
                                    token: address_from_str(USDC_ADDRESS),
                                    balance: 1_i32.to_be_bytes().to_vec(),
                                }],
                            },
                        ],
                        balance_changes: vec![BalanceChange {
                            token: address_from_str(USDC_ADDRESS),
                            balance: 1_i32.to_be_bytes().to_vec(),
                            component_id: "pc_2".into(),
                        }],
                        ..Default::default()
                    },
                    TransactionChanges {
                        tx: Some(pb_transactions(3, 2)),
                        contract_changes: vec![ContractChange {
                            address: address_from_str("0000000000000000000000000000000000000001"),
                            balance: 1_i32.to_be_bytes().to_vec(),
                            code: 123_i32.to_be_bytes().to_vec(),
                            slots: vec![ContractSlot {
                                slot: Bytes::from("0x01").into(),
                                value: Bytes::from("0x01").into(),
                                previous_value: Bytes::new().into(),
                            }],
                            change: ChangeType::Update.into(),
                            token_balances: vec![AccountBalanceChange {
                                token: address_from_str(USDC_ADDRESS),
                                balance: 100_i32.to_be_bytes().to_vec(),
                            }],
                        }],
                        balance_changes: vec![
                            BalanceChange {
                                token: address_from_str(USDC_ADDRESS),
                                balance: 100_i32.to_be_bytes().to_vec(),
                                component_id: "pc_1".into(),
                            },
                            BalanceChange {
                                token: address_from_str(USDC_ADDRESS),
                                balance: 2_i32.to_be_bytes().to_vec(),
                                component_id: "pc_2".into(),
                            },
                        ],
                        ..Default::default()
                    },
                ],
                storage_changes: vec![],
            },
            4 => BlockChanges {
                block: Some(pb_blocks(version as u64)),
                changes: vec![TransactionChanges {
                    tx: Some(pb_transactions(4, 1)),
                    contract_changes: vec![ContractChange {
                        address: address_from_str("0000000000000000000000000000000000000001"),
                        balance: 1_i32.to_be_bytes().to_vec(),
                        code: 123_i32.to_be_bytes().to_vec(),
                        slots: vec![ContractSlot {
                            slot: Bytes::from("0x03").into(),
                            value: Bytes::from("0x10").into(),
                            previous_value: Bytes::new().into(),
                        }],
                        change: ChangeType::Update.into(),
                        token_balances: vec![],
                    }],
                    component_changes: vec![ProtocolComponent {
                        id: "pc_3".to_owned(),
                        tokens: vec![address_from_str(DAI_ADDRESS), address_from_str(USDC_ADDRESS)],
                        contracts: vec![address_from_str(
                            "0000000000000000000000000000000000000001",
                        )],
                        static_att: vec![],
                        change: ChangeType::Creation.into(),
                        protocol_type: Some(ProtocolType {
                            name: "pt_1".to_string(),
                            financial_type: 0,
                            attribute_schema: vec![],
                            implementation_type: 0,
                        }),
                    }],
                    ..Default::default()
                }],
                storage_changes: vec![],
            },
            5 => BlockChanges {
                block: Some(pb_blocks(version as u64)),
                changes: vec![TransactionChanges {
                    tx: Some(pb_transactions(5, 1)),
                    contract_changes: vec![
                        ContractChange {
                            address: address_from_str("0000000000000000000000000000000000000001"),
                            balance: 1_i32.to_be_bytes().to_vec(),
                            code: 123_i32.to_be_bytes().to_vec(),
                            slots: vec![ContractSlot {
                                slot: Bytes::from("0x01").into(),
                                value: Bytes::from("0x10").into(),
                                previous_value: Bytes::new().into(),
                            }],
                            change: ChangeType::Update.into(),
                            token_balances: vec![
                                AccountBalanceChange {
                                    token: address_from_str(USDC_ADDRESS),
                                    balance: 100_i32.to_be_bytes().to_vec(),
                                },
                                AccountBalanceChange {
                                    token: address_from_str(WETH_ADDRESS),
                                    balance: 100_i32.to_be_bytes().to_vec(),
                                },
                            ],
                        },
                        ContractChange {
                            address: address_from_str("0000000000000000000000000000000000000002"),
                            balance: 1_i32.to_be_bytes().to_vec(),
                            code: 123_i32.to_be_bytes().to_vec(),
                            slots: vec![ContractSlot {
                                slot: Bytes::from("0x01").into(),
                                value: Bytes::from("0x10").into(),
                                previous_value: Bytes::new().into(),
                            }],
                            change: ChangeType::Update.into(),
                            token_balances: vec![AccountBalanceChange {
                                token: address_from_str(USDC_ADDRESS),
                                balance: 1_i32.to_be_bytes().to_vec(),
                            }],
                        },
                    ],
                    balance_changes: vec![
                        BalanceChange {
                            token: address_from_str(USDC_ADDRESS),
                            balance: 1_i32.to_be_bytes().to_vec(),
                            component_id: "pc_1".into(),
                        },
                        BalanceChange {
                            token: address_from_str(USDC_ADDRESS),
                            balance: 1_i32.to_be_bytes().to_vec(),
                            component_id: "pc_3".into(),
                        },
                        BalanceChange {
                            token: address_from_str(WETH_ADDRESS),
                            balance: 1_i32.to_be_bytes().to_vec(),
                            component_id: "pc_1".into(),
                        },
                    ],
                    ..Default::default()
                }],
                storage_changes: vec![],
            },
            _ => panic!("Requested BlockChanges version doesn't exist"),
        }
    }

    pub fn pb_native_block_changes(version: u8) -> BlockChanges {
        match version {
            0 => BlockChanges {
                block: Some(Block {
                    hash: vec![0x0, 0x0, 0x0, 0x0],
                    parent_hash: vec![0x21, 0x22, 0x23, 0x24],
                    number: 1,
                    ts: yesterday_midnight()
                        .and_utc()
                        .timestamp() as u64,
                }),
                changes: vec![
                    TransactionChanges {
                        tx: Some(Transaction {
                            hash: vec![0x0, 0x0, 0x0, 0x0],
                            from: vec![0x0, 0x0, 0x0, 0x0],
                            to: vec![0x0, 0x0, 0x0, 0x0],
                            index: 10,
                        }),
                        entity_changes: vec![
                            EntityChanges {
                                component_id: "State1".to_owned(),
                                attributes: vec![
                                    Attribute {
                                        name: "reserve".to_owned(),
                                        value: Bytes::from(1_000u64)
                                            .lpad(32, 0)
                                            .to_vec(),
                                        change: ChangeType::Update.into(),
                                    },
                                    Attribute {
                                        name: "static_attribute".to_owned(),
                                        value: Bytes::from(1u64).lpad(32, 0).to_vec(),
                                        change: ChangeType::Update.into(),
                                    },
                                ],
                            },
                            EntityChanges {
                                component_id: "State2".to_owned(),
                                attributes: vec![
                                    Attribute {
                                        name: "reserve".to_owned(),
                                        value: Bytes::from(1_000u64)
                                            .lpad(32, 0)
                                            .to_vec(),
                                        change: ChangeType::Update.into(),
                                    },
                                    Attribute {
                                        name: "static_attribute".to_owned(),
                                        value: Bytes::from(1u64).lpad(32, 0).to_vec(),
                                        change: ChangeType::Update.into(),
                                    },
                                ],
                            },
                        ],
                        ..Default::default()
                    },
                    TransactionChanges {
                        tx: Some(Transaction {
                            hash: vec![0x11, 0x12, 0x13, 0x14],
                            from: vec![0x41, 0x42, 0x43, 0x44],
                            to: vec![0x51, 0x52, 0x53, 0x54],
                            index: 11,
                        }),
                        entity_changes: vec![EntityChanges {
                            component_id: "State1".to_owned(),
                            attributes: vec![
                                Attribute {
                                    name: "reserve".to_owned(),
                                    value: Bytes::from(600u64).lpad(32, 0).to_vec(),
                                    change: ChangeType::Update.into(),
                                },
                                Attribute {
                                    name: "new".to_owned(),
                                    value: Bytes::zero(32).to_vec(),
                                    change: ChangeType::Update.into(),
                                },
                            ],
                        }],
                        component_changes: vec![ProtocolComponent {
                            id: "Pool".to_owned(),
                            tokens: vec![
                                address_from_str(DAI_ADDRESS),
                                address_from_str(DAI_ADDRESS),
                            ],
                            contracts: vec![address_from_str(WETH_ADDRESS)],
                            static_att: vec![Attribute {
                                name: "key".to_owned(),
                                value: Bytes::from(600u64).lpad(32, 0).to_vec(),
                                change: ChangeType::Creation.into(),
                            }],
                            change: ChangeType::Creation.into(),
                            protocol_type: Some(ProtocolType {
                                name: "WeightedPool".to_string(),
                                financial_type: 0,
                                attribute_schema: vec![],
                                implementation_type: 0,
                            }),
                        }],
                        balance_changes: vec![BalanceChange {
                            token: address_from_str(DAI_ADDRESS),
                            balance: 1_i32.to_be_bytes().to_vec(),
                            component_id: "Balance1".into(),
                        }],
                        ..Default::default()
                    },
                ],
                storage_changes: vec![],
            },
            1 => BlockChanges {
                block: Some(pb_blocks(version as u64)),
                changes: vec![TransactionChanges {
                    tx: Some(pb_transactions(1, 1)),
                    entity_changes: vec![EntityChanges {
                        component_id: "pc_1".to_owned(),
                        attributes: vec![
                            Attribute {
                                name: "attr_1".to_owned(),
                                value: Bytes::from(1u64).lpad(32, 0).to_vec(),
                                change: ChangeType::Update.into(),
                            },
                            Attribute {
                                name: "attr_2".to_owned(),
                                value: Bytes::from(2u64).lpad(32, 0).to_vec(),
                                change: ChangeType::Update.into(),
                            },
                        ],
                    }],
                    component_changes: vec![ProtocolComponent {
                        id: "pc_1".to_owned(),
                        tokens: vec![
                            address_from_str(WETH_ADDRESS),
                            address_from_str(USDC_ADDRESS),
                        ],
                        contracts: vec![],
                        static_att: vec![Attribute {
                            name: "st_attr_1".to_owned(),
                            value: Bytes::from(1u64).lpad(32, 0).to_vec(),
                            change: ChangeType::Creation.into(),
                        }],
                        change: ChangeType::Creation.into(),
                        protocol_type: Some(ProtocolType {
                            name: "pt_1".to_string(),
                            financial_type: 0,
                            attribute_schema: vec![],
                            implementation_type: 0,
                        }),
                    }],
                    balance_changes: vec![BalanceChange {
                        token: address_from_str(USDC_ADDRESS),
                        balance: 1_i32.to_be_bytes().to_vec(),
                        component_id: "pc_1".into(),
                    }],
                    ..Default::default()
                }],
                storage_changes: vec![],
            },
            2 => BlockChanges {
                block: Some(pb_blocks(version as u64)),
                changes: vec![TransactionChanges {
                    tx: Some(pb_transactions(2, 1)),
                    entity_changes: vec![EntityChanges {
                        component_id: "pc_1".to_owned(),
                        attributes: vec![Attribute {
                            name: "attr_1".to_owned(),
                            value: Bytes::from(10u64).lpad(32, 0).to_vec(),
                            change: ChangeType::Update.into(),
                        }],
                    }],
                    component_changes: vec![ProtocolComponent {
                        id: "pc_2".to_owned(),
                        tokens: vec![
                            address_from_str(USDT_ADDRESS),
                            address_from_str(USDC_ADDRESS),
                        ],
                        contracts: vec![],
                        static_att: vec![],
                        change: ChangeType::Creation.into(),
                        protocol_type: Some(ProtocolType {
                            name: "pt_1".to_string(),
                            financial_type: 0,
                            attribute_schema: vec![],
                            implementation_type: 0,
                        }),
                    }],
                    balance_changes: vec![
                        BalanceChange {
                            token: address_from_str(USDC_ADDRESS),
                            balance: 1_i32.to_be_bytes().to_vec(),
                            component_id: "pc_2".into(),
                        },
                        BalanceChange {
                            token: address_from_str(USDT_ADDRESS),
                            balance: 1_i32.to_be_bytes().to_vec(),
                            component_id: "pc_2".into(),
                        },
                        BalanceChange {
                            token: address_from_str(WETH_ADDRESS),
                            balance: 1_i32.to_be_bytes().to_vec(),
                            component_id: "pc_1".into(),
                        },
                    ],
                    ..Default::default()
                }],
                storage_changes: vec![],
            },
            3 => BlockChanges {
                block: Some(pb_blocks(version as u64)),
                changes: vec![
                    TransactionChanges {
                        tx: Some(pb_transactions(3, 2)),
                        entity_changes: vec![EntityChanges {
                            component_id: "pc_1".to_owned(),
                            attributes: vec![Attribute {
                                name: "attr_1".to_owned(),
                                value: Bytes::from(1_000u64)
                                    .lpad(32, 0)
                                    .to_vec(),
                                change: ChangeType::Update.into(),
                            }],
                        }],
                        balance_changes: vec![BalanceChange {
                            token: address_from_str(USDC_ADDRESS),
                            balance: 3_i32.to_be_bytes().to_vec(),
                            component_id: "pc_2".into(),
                        }],
                        ..Default::default()
                    },
                    TransactionChanges {
                        tx: Some(pb_transactions(3, 1)),
                        entity_changes: vec![EntityChanges {
                            component_id: "pc_1".to_owned(),
                            attributes: vec![Attribute {
                                name: "attr_1".to_owned(),
                                value: Bytes::from(99_999u64)
                                    .lpad(32, 0)
                                    .to_vec(),
                                change: ChangeType::Update.into(),
                            }],
                        }],
                        balance_changes: vec![
                            BalanceChange {
                                token: address_from_str(USDC_ADDRESS),
                                balance: 99999_i32.to_be_bytes().to_vec(),
                                component_id: "pc_2".into(),
                            },
                            BalanceChange {
                                token: address_from_str(WETH_ADDRESS),
                                balance: 1000_i32.to_be_bytes().to_vec(),
                                component_id: "pc_1".into(),
                            },
                        ],
                        ..Default::default()
                    },
                ],
                storage_changes: vec![],
            },
            4 => BlockChanges {
                block: Some(pb_blocks(version as u64)),
                changes: vec![
                    TransactionChanges {
                        tx: Some(pb_transactions(4, 1)),
                        entity_changes: vec![
                            EntityChanges {
                                component_id: "pc_1".to_owned(),
                                attributes: vec![Attribute {
                                    name: "attr_1".to_owned(),
                                    value: Bytes::from(10_000u64)
                                        .lpad(32, 0)
                                        .to_vec(),
                                    change: ChangeType::Update.into(),
                                }],
                            },
                            EntityChanges {
                                component_id: "pc_3".to_owned(),
                                attributes: vec![Attribute {
                                    name: "attr_1".to_owned(),
                                    value: Bytes::from(3u64).lpad(32, 0).to_vec(),
                                    change: ChangeType::Update.into(),
                                }],
                            },
                        ],
                        component_changes: vec![ProtocolComponent {
                            id: "pc_3".to_owned(),
                            tokens: vec![
                                address_from_str(DAI_ADDRESS),
                                address_from_str(WETH_ADDRESS),
                            ],
                            contracts: vec![],
                            static_att: vec![],
                            change: ChangeType::Creation.into(),
                            protocol_type: Some(ProtocolType {
                                name: "pt_2".to_string(),
                                financial_type: 0,
                                attribute_schema: vec![],
                                implementation_type: 0,
                            }),
                        }],
                        ..Default::default()
                    },
                    TransactionChanges {
                        tx: Some(pb_transactions(4, 2)),
                        entity_changes: vec![
                            EntityChanges {
                                component_id: "pc_3".to_owned(),
                                attributes: vec![Attribute {
                                    name: "attr_1".to_owned(),
                                    value: Bytes::from(30u64).lpad(32, 0).to_vec(),
                                    change: ChangeType::Update.into(),
                                }],
                            },
                            EntityChanges {
                                component_id: "pc_1".to_owned(),
                                attributes: vec![Attribute {
                                    name: "attr_1".to_owned(),
                                    value: Bytes::from(100_000u64)
                                        .lpad(32, 0)
                                        .to_vec(),
                                    change: ChangeType::Update.into(),
                                }],
                            },
                        ],
                        balance_changes: vec![
                            BalanceChange {
                                token: address_from_str(USDC_ADDRESS),
                                balance: 3000_i32.to_be_bytes().to_vec(),
                                component_id: "pc_3".into(),
                            },
                            BalanceChange {
                                token: address_from_str(USDC_ADDRESS),
                                balance: 1000_i32.to_be_bytes().to_vec(),
                                component_id: "pc_1".into(),
                            },
                        ],
                        ..Default::default()
                    },
                ],
                storage_changes: vec![],
            },
            5 => BlockChanges {
                block: Some(pb_blocks(version as u64)),
                changes: vec![TransactionChanges {
                    tx: Some(pb_transactions(5, 1)),
                    entity_changes: vec![EntityChanges {
                        component_id: "pc_1".to_owned(),
                        attributes: vec![Attribute {
                            name: "attr_2".to_owned(),
                            value: Bytes::from(1_000_000u64)
                                .lpad(32, 0)
                                .to_vec(),
                            change: ChangeType::Update.into(),
                        }],
                    }],
                    component_changes: vec![ProtocolComponent {
                        id: "pc_2".to_owned(),
                        tokens: vec![
                            address_from_str(USDT_ADDRESS),
                            address_from_str(USDC_ADDRESS),
                        ],
                        contracts: vec![],
                        static_att: vec![],
                        change: ChangeType::Deletion.into(),
                        protocol_type: Some(ProtocolType {
                            name: "pt_1".to_string(),
                            financial_type: 0,
                            attribute_schema: vec![],
                            implementation_type: 0,
                        }),
                    }],
                    balance_changes: vec![BalanceChange {
                        token: address_from_str(WETH_ADDRESS),
                        balance: 1000_i32.to_be_bytes().to_vec(),
                        component_id: "pc_1".into(),
                    }],
                    ..Default::default()
                }],
                storage_changes: vec![],
            },
            _ => panic!("Requested unknown version of block entity changes"),
        }
    }

    pub fn pb_transaction_storage_changes(version: u8) -> TransactionStorageChanges {
        match version {
            0 => TransactionStorageChanges {
                tx: Some(pb_transactions(0, 1)),
                storage_changes: vec![
                    StorageChanges {
                        address: address_from_str("0000000000000000000000000000000000000001"),
                        slots: vec![
                            ContractSlot {
                                slot: Bytes::from("0x01").into(),
                                value: Bytes::from("0x01").into(),
                                previous_value: Bytes::new().into(),
                            },
                            ContractSlot {
                                slot: Bytes::from("0x02").into(),
                                value: Bytes::from("0x02").into(),
                                previous_value: Bytes::new().into(),
                            },
                        ],
                        native_balance: None,
                    },
                    StorageChanges {
                        address: address_from_str("0000000000000000000000000000000000000002"),
                        slots: vec![ContractSlot {
                            slot: Bytes::from("0x03").into(),
                            value: Bytes::from("0x03").into(),
                            previous_value: Bytes::new().into(),
                        }],
                        native_balance: Some(Bytes::from(1000u64).to_vec()),
                    },
                ],
            },
            1 => TransactionStorageChanges {
                tx: Some(pb_transactions(1, 2)),
                storage_changes: vec![
                    StorageChanges {
                        address: address_from_str("0000000000000000000000000000000000000001"),
                        slots: vec![ContractSlot {
                            slot: Bytes::from("0x01").into(),
                            value: Bytes::from("0x04").into(),
                            previous_value: Bytes::new().into(),
                        }],
                        native_balance: None,
                    },
                    StorageChanges {
                        address: address_from_str("0000000000000000000000000000000000000002"),
                        slots: vec![ContractSlot {
                            slot: Bytes::from("0x05").into(),
                            value: Bytes::from("0x05").into(),
                            previous_value: Bytes::new().into(),
                        }],
                        native_balance: None,
                    },
                ],
            },
            _ => panic!("Requested TransactionStorageChanges version doesn't exist"),
        }
    }
}
