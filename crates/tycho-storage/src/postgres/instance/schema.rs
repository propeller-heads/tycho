// @generated automatically by Diesel CLI.

pub mod tycho {
    pub mod sql_types {
        #[derive(diesel::query_builder::QueryId, diesel::sql_types::SqlType)]
        #[diesel(postgres_type(name = "entry_point_tracing_type"))]
        pub struct EntryPointTracingType;

        #[derive(diesel::query_builder::QueryId, diesel::sql_types::SqlType)]
        #[diesel(postgres_type(name = "instance_status", schema = "tycho"))]
        pub struct InstanceStatus;
    }

    diesel::table! {
        tycho.account (instance_id, id) {
            instance_id -> Int4,
            id -> Int8,
            address -> Bytea,
            #[max_length = 255]
            title -> Varchar,
            created_at -> Nullable<Timestamptz>,
            creation_block -> Nullable<Int8>,
            creation_tx_hash -> Nullable<Bytea>,
            deleted_at -> Nullable<Timestamptz>,
            deletion_tx_hash -> Nullable<Bytea>,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.account_balance (instance_id, account_id, token) {
            instance_id -> Int4,
            account_id -> Int8,
            token -> Bytea,
            balance -> Bytea,
            valid_from -> Timestamptz,
            tx_index -> Int4,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.account_balance_history (instance_id, account_id, token, valid_to, valid_from, tx_index) {
            instance_id -> Int4,
            account_id -> Int8,
            token -> Bytea,
            balance -> Bytea,
            valid_from -> Timestamptz,
            valid_to -> Timestamptz,
            tx_index -> Int4,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.component_balance (instance_id, protocol_component_id, token) {
            instance_id -> Int4,
            protocol_component_id -> Int8,
            token -> Bytea,
            new_balance -> Bytea,
            previous_value -> Bytea,
            balance_float -> Float8,
            valid_from -> Timestamptz,
            tx_index -> Int4,
            inserted_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.component_balance_history (instance_id, protocol_component_id, token, valid_to, valid_from, tx_index) {
            instance_id -> Int4,
            protocol_component_id -> Int8,
            token -> Bytea,
            new_balance -> Bytea,
            previous_value -> Bytea,
            balance_float -> Float8,
            valid_from -> Timestamptz,
            valid_to -> Timestamptz,
            tx_index -> Int4,
            inserted_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.component_tvl (instance_id, protocol_component_id) {
            instance_id -> Int4,
            protocol_component_id -> Int8,
            tvl -> Float8,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.contract_code (instance_id, account_id) {
            instance_id -> Int4,
            account_id -> Int8,
            code -> Bytea,
            hash -> Bytea,
            modify_tx_hash -> Bytea,
            valid_from -> Timestamptz,
            tx_index -> Int4,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.contract_code_history (instance_id, account_id, valid_to, valid_from, tx_index) {
            instance_id -> Int4,
            account_id -> Int8,
            code -> Bytea,
            hash -> Bytea,
            modify_tx_hash -> Bytea,
            valid_from -> Timestamptz,
            valid_to -> Timestamptz,
            tx_index -> Int4,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.contract_storage (instance_id, account_id, slot) {
            instance_id -> Int4,
            account_id -> Int8,
            slot -> Bytea,
            value -> Nullable<Bytea>,
            previous_value -> Nullable<Bytea>,
            valid_from -> Timestamptz,
            tx_index -> Int4,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.contract_storage_history (instance_id, account_id, slot, valid_to, valid_from, tx_index) {
            instance_id -> Int4,
            account_id -> Int8,
            slot -> Bytea,
            value -> Nullable<Bytea>,
            previous_value -> Nullable<Bytea>,
            valid_from -> Timestamptz,
            valid_to -> Timestamptz,
            tx_index -> Int4,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.entry_point (instance_id, id) {
            instance_id -> Int4,
            id -> Int8,
            external_id -> Text,
            target -> Bytea,
            signature -> Text,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        use diesel::sql_types::*;
        use super::sql_types::EntryPointTracingType;

        tycho.entry_point_tracing_params (instance_id, id) {
            instance_id -> Int4,
            id -> Int8,
            entry_point_id -> Int8,
            tracing_type -> EntryPointTracingType,
            data -> Nullable<Jsonb>,
            data_hash -> Nullable<Bytea>,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.entry_point_tracing_params_calls_account (instance_id, entry_point_tracing_params_id, account_id) {
            instance_id -> Int4,
            entry_point_tracing_params_id -> Int8,
            account_id -> Int8,
        }
    }

    diesel::table! {
        tycho.entry_point_tracing_result (instance_id, id) {
            instance_id -> Int4,
            id -> Int8,
            entry_point_tracing_params_id -> Int8,
            detection_block_number -> Int8,
            detection_block_hash -> Bytea,
            detection_data -> Jsonb,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.extraction_state (instance_id) {
            instance_id -> Int4,
            cursor -> Nullable<Bytea>,
            block_number -> Int8,
            block_hash -> Bytea,
            block_ts -> Timestamptz,
            attributes -> Nullable<Jsonb>,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.protocol_component (instance_id, id) {
            instance_id -> Int4,
            id -> Int8,
            #[max_length = 255]
            external_id -> Varchar,
            #[max_length = 255]
            protocol_type -> Varchar,
            attributes -> Nullable<Jsonb>,
            created_at -> Timestamptz,
            creation_block -> Int8,
            creation_tx_hash -> Bytea,
            deleted_at -> Nullable<Timestamptz>,
            deletion_tx_hash -> Nullable<Bytea>,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.protocol_component_has_entry_point_tracing_params (instance_id, protocol_component_id, entry_point_tracing_params_id) {
            instance_id -> Int4,
            protocol_component_id -> Int8,
            entry_point_tracing_params_id -> Int8,
        }
    }

    diesel::table! {
        tycho.protocol_component_holds_contract (instance_id, protocol_component_id, account_id) {
            instance_id -> Int4,
            protocol_component_id -> Int8,
            account_id -> Int8,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.protocol_component_holds_token (instance_id, protocol_component_id, token) {
            instance_id -> Int4,
            protocol_component_id -> Int8,
            token -> Bytea,
            token_index -> Int2,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.protocol_component_uses_entry_point (instance_id, protocol_component_id, entry_point_id) {
            instance_id -> Int4,
            protocol_component_id -> Int8,
            entry_point_id -> Int8,
        }
    }

    diesel::table! {
        use diesel::sql_types::*;
        use super::sql_types::InstanceStatus;

        tycho.protocol_instance (id) {
            id -> Int4,
            protocol_system_id -> Int8,
            label -> Text,
            status -> InstanceStatus,
            base_block_number -> Nullable<Int8>,
            base_block_hash -> Nullable<Bytea>,
            base_block_ts -> Nullable<Timestamptz>,
            source -> Text,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.protocol_state (instance_id, protocol_component_id, attribute_name) {
            instance_id -> Int4,
            protocol_component_id -> Int8,
            attribute_name -> Varchar,
            attribute_value -> Bytea,
            previous_value -> Nullable<Bytea>,
            valid_from -> Timestamptz,
            tx_index -> Int4,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::table! {
        tycho.protocol_state_history (instance_id, protocol_component_id, attribute_name, valid_to, valid_from, tx_index) {
            instance_id -> Int4,
            protocol_component_id -> Int8,
            attribute_name -> Varchar,
            attribute_value -> Bytea,
            previous_value -> Nullable<Bytea>,
            valid_from -> Timestamptz,
            valid_to -> Timestamptz,
            tx_index -> Int4,
            inserted_ts -> Timestamptz,
            modified_ts -> Timestamptz,
        }
    }

    diesel::joinable!(extraction_state -> protocol_instance (instance_id));

    diesel::allow_tables_to_appear_in_same_query!(
        account,
        account_balance,
        account_balance_history,
        component_balance,
        component_balance_history,
        component_tvl,
        contract_code,
        contract_code_history,
        contract_storage,
        contract_storage_history,
        entry_point,
        entry_point_tracing_params,
        entry_point_tracing_params_calls_account,
        entry_point_tracing_result,
        extraction_state,
        protocol_component,
        protocol_component_has_entry_point_tracing_params,
        protocol_component_holds_contract,
        protocol_component_holds_token,
        protocol_component_uses_entry_point,
        protocol_instance,
        protocol_state,
        protocol_state_history,
    );
}
