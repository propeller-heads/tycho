use std::collections::{hash_map::Entry, HashMap, HashSet};

use chrono::{DateTime, NaiveDateTime};
use tracing::warn;
use tycho_common::{
    models::{
        blockchain::{
            Block, BlockChanges, EntryPoint, RPCTracerParams, TracingParams, Transaction,
            TxWithChanges, TxWithContractChanges,
        },
        contract::{AccountBalance, AccountDelta, ContractChanges, ContractStorageChange},
        protocol::{ComponentBalance, ProtocolComponent, ProtocolComponentStateDelta},
        Address, Chain, ChangeType, ComponentId, EntryPointId, ProtocolType, TxHash,
    },
    Bytes,
};

use crate::{error::DecodeError, pb::tycho::evm::v1 as pb, u256_num::bytes_to_f64};

/// Converts protobuf messages into domain model types.
pub trait TryFromMessage {
    type Args<'a>;

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError>
    where
        Self: Sized;
}

impl TryFromMessage for AccountDelta {
    type Args<'a> = (pb::ContractChange, Chain);

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError> {
        let (msg, chain) = args;
        let change = ChangeType::try_from_message(msg.change())?;
        // For Creation deltas, code must always be Some — even for EOAs where code is empty
        // bytes. None means "code unchanged" for Updates, but is incorrect for Creations.
        let code = match change {
            ChangeType::Creation => Some(msg.code.into()),
            _ => {
                if !msg.code.is_empty() {
                    Some(msg.code.into())
                } else {
                    None
                }
            }
        };
        let update = AccountDelta::new(
            chain,
            msg.address.into(),
            msg.slots
                .into_iter()
                .map(|cs| (cs.slot.into(), Some(cs.value.into())))
                .collect(),
            if !msg.balance.is_empty() { Some(msg.balance.into()) } else { None },
            code,
            change,
        );
        Ok(update)
    }
}

impl TryFromMessage for AccountBalance {
    type Args<'a> = (pb::AccountBalanceChange, &'a Address, &'a Transaction);

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError> {
        let (msg, addr, tx) = args;
        Ok(Self {
            token: msg.token.into(),
            balance: Bytes::from(msg.balance),
            modify_tx: tx.hash.clone(),
            account: addr.clone(),
        })
    }
}

impl TryFromMessage for Block {
    type Args<'a> = (pb::Block, Chain);

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError> {
        let (msg, chain) = args;

        Ok(Self {
            chain,
            number: msg.number,
            hash: msg.hash.into(),
            parent_hash: msg.parent_hash.into(),
            ts: DateTime::from_timestamp(msg.ts as i64, 0)
                .ok_or_else(|| {
                    DecodeError::Decode(format!(
                        "Failed to convert timestamp {} to datetime!",
                        msg.ts
                    ))
                })?
                .naive_utc(),
        })
    }
}

impl TryFromMessage for Transaction {
    type Args<'a> = (pb::Transaction, &'a TxHash);

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError> {
        let (msg, block_hash) = args;

        let to = if !msg.to.is_empty() { Some(msg.to.into()) } else { None };

        Ok(Self {
            hash: msg.hash.into(),
            block_hash: block_hash.clone(),
            from: msg.from.into(),
            to,
            index: msg.index,
        })
    }
}

impl TryFromMessage for ComponentBalance {
    type Args<'a> = (pb::BalanceChange, &'a Transaction);

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError> {
        let (msg, tx) = args;
        let balance_float = bytes_to_f64(&msg.balance).unwrap_or(f64::NAN);
        Ok(Self {
            token: msg.token.into(),
            balance: Bytes::from(msg.balance),
            balance_float,
            modify_tx: tx.hash.clone(),
            component_id: String::from_utf8(msg.component_id)
                .map_err(|error| DecodeError::Decode(error.to_string()))?,
        })
    }
}

impl TryFromMessage for ProtocolComponent {
    type Args<'a> = (
        pb::ProtocolComponent,
        Chain,
        &'a str,
        &'a HashMap<String, ProtocolType>,
        TxHash,
        NaiveDateTime,
    );

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError> {
        let (msg, chain, protocol_system, protocol_types, tx_hash, creation_ts) = args;
        let tokens: Vec<Bytes> = msg
            .tokens
            .clone()
            .into_iter()
            .map(Into::into)
            .collect();

        let contract_ids = msg
            .contracts
            .clone()
            .into_iter()
            .map(Into::into)
            .collect();

        let static_attributes = msg
            .static_att
            .clone()
            .into_iter()
            .map(|attribute| (attribute.name, Bytes::from(attribute.value)))
            .collect();

        let protocol_type = msg
            .protocol_type
            .clone()
            .ok_or(DecodeError::Decode("Missing protocol type".to_owned()))?;

        if !protocol_types.contains_key(&protocol_type.name) {
            return Err(DecodeError::Decode(format!(
                "Unknown protocol type name: {}",
                protocol_type.name
            )));
        }

        Ok(Self {
            id: msg.id.clone(),
            protocol_type_name: protocol_type.name,
            protocol_system: protocol_system.to_owned(),
            tokens,
            contract_addresses: contract_ids,
            static_attributes,
            chain,
            change: ChangeType::try_from_message(msg.change())?,
            creation_tx: tx_hash,
            created_at: creation_ts,
        })
    }
}

impl TryFromMessage for ChangeType {
    type Args<'a> = pb::ChangeType;

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError> {
        match args {
            pb::ChangeType::Creation => Ok(ChangeType::Creation),
            pb::ChangeType::Update => Ok(ChangeType::Update),
            pb::ChangeType::Deletion => Ok(ChangeType::Deletion),
            pb::ChangeType::Unspecified => Err(DecodeError::Decode(format!(
                "Unknown ChangeType enum member encountered: {args:?}"
            ))),
        }
    }
}

impl TryFromMessage for ProtocolComponentStateDelta {
    type Args<'a> = pb::EntityChanges;

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError> {
        let msg = args;

        let (mut updates, mut deletions, mut created) =
            (HashMap::new(), HashSet::new(), HashSet::new());

        for attribute in msg.attributes.into_iter() {
            match ChangeType::try_from_message(attribute.change())? {
                ChangeType::Creation => {
                    created.insert(attribute.name.clone());
                    updates.insert(attribute.name, Bytes::from(attribute.value));
                }
                ChangeType::Update => {
                    updates.insert(attribute.name, Bytes::from(attribute.value));
                }
                ChangeType::Deletion => {
                    deletions.insert(attribute.name);
                }
            }
        }

        Ok(Self {
            component_id: msg.component_id,
            updated_attributes: updates,
            deleted_attributes: deletions,
            created_attributes: created,
        })
    }
}

impl TryFromMessage for EntryPoint {
    type Args<'a> = pb::EntryPoint;

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError> {
        let msg = args;

        Ok(Self { external_id: msg.id, target: msg.target.into(), signature: msg.signature })
    }
}

impl TryFromMessage for TracingParams {
    type Args<'a> = pb::EntryPointParams;

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError> {
        let msg = args;
        let trace_data = msg.trace_data.ok_or_else(|| {
            DecodeError::Decode("Missing trace data in EntryPointParams".to_owned())
        })?;

        match trace_data {
            pb::entry_point_params::TraceData::Rpc(rpc_data) => {
                let caller = rpc_data.caller.map(|c| c.into());
                Ok(Self::RPCTracer(RPCTracerParams::new(caller, rpc_data.calldata.into())))
            }
        }
    }
}

impl TryFromMessage for TxWithChanges {
    type Args<'a> = (pb::TransactionChanges, &'a Block, &'a str, &'a HashMap<String, ProtocolType>);

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError> {
        let (msg, block, protocol_system, protocol_types) = args;
        let tx = Transaction::try_from_message((
            msg.tx
                .expect("TransactionChanges should have a transaction"),
            &block.hash.clone(),
        ))?;

        let mut new_protocol_components: HashMap<ComponentId, ProtocolComponent> = HashMap::new();
        let mut account_updates: HashMap<Address, AccountDelta> = HashMap::new();
        let mut state_updates: HashMap<ComponentId, ProtocolComponentStateDelta> = HashMap::new();
        let mut balance_changes: HashMap<ComponentId, HashMap<Address, ComponentBalance>> =
            HashMap::new();
        let mut account_balance_changes: HashMap<Address, HashMap<Address, AccountBalance>> =
            HashMap::new();
        let mut entrypoints: HashMap<ComponentId, HashSet<EntryPoint>> = HashMap::new();
        let mut entrypoint_params: HashMap<EntryPointId, HashSet<(TracingParams, ComponentId)>> =
            HashMap::new();

        for change in msg.component_changes.into_iter() {
            let component = ProtocolComponent::try_from_message((
                change,
                block.chain,
                protocol_system,
                protocol_types,
                tx.hash.clone(),
                block.ts,
            ))?;
            new_protocol_components.insert(component.id.clone(), component);
        }

        for contract_change in msg.contract_changes.clone().into_iter() {
            let update = AccountDelta::try_from_message((contract_change, block.chain))?;
            account_updates.insert(update.address.clone(), update);
        }

        for state_msg in msg.entity_changes.into_iter() {
            let state = ProtocolComponentStateDelta::try_from_message(state_msg)?;
            match state_updates.entry(state.component_id.clone()) {
                Entry::Vacant(e) => {
                    e.insert(state);
                }
                Entry::Occupied(mut e) => {
                    warn!(
                        "Received two state updates for the same component. \
                         Overwriting state for component {}",
                        e.key()
                    );
                    e.insert(state);
                }
            }
        }

        for balance_change in msg.balance_changes.into_iter() {
            let component_id = String::from_utf8(balance_change.component_id.clone())
                .map_err(|error| DecodeError::Decode(error.to_string()))?;
            let token_address = Bytes::from(balance_change.token.clone());
            let balance = ComponentBalance::try_from_message((balance_change, &tx))?;

            balance_changes
                .entry(component_id)
                .or_default()
                .insert(token_address, balance);
        }

        for contract_change in msg.contract_changes.into_iter() {
            for balance_change in contract_change
                .token_balances
                .into_iter()
            {
                let account_addr = contract_change.address.clone().into();
                let token_address = Bytes::from(balance_change.token.clone());
                let balance =
                    AccountBalance::try_from_message((balance_change, &account_addr, &tx))?;

                account_balance_changes
                    .entry(account_addr)
                    .or_default()
                    .insert(token_address, balance);
            }
        }

        for msg_entrypoint in msg.entrypoints.into_iter() {
            let component_id = msg_entrypoint.component_id.clone();
            let entrypoint = EntryPoint::try_from_message(msg_entrypoint)?;
            entrypoints
                .entry(component_id)
                .or_default()
                .insert(entrypoint);
        }

        for msg_entrypoint_params in msg.entrypoint_params.into_iter() {
            let entrypoint_id = msg_entrypoint_params
                .entrypoint_id
                .clone();
            let component_id = msg_entrypoint_params
                .component_id
                .clone()
                .ok_or(DecodeError::Decode(
                    "Entrypoint params should have a component id".to_owned(),
                ))?;
            let tracing_data = TracingParams::try_from_message(msg_entrypoint_params)?;
            entrypoint_params
                .entry(entrypoint_id)
                .or_default()
                .insert((tracing_data, component_id));
        }

        Ok(Self {
            tx,
            protocol_components: new_protocol_components,
            account_deltas: account_updates,
            state_updates,
            balance_changes,
            account_balance_changes,
            entrypoints,
            entrypoint_params,
        })
    }
}

impl TryFromMessage for TxWithContractChanges {
    type Args<'a> = (pb::TransactionStorageChanges, &'a Block);

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError> {
        let (msg, block) = args;
        let tx = Transaction::try_from_message((
            msg.tx
                .expect("TransactionChanges should have a transaction"),
            &block.hash.clone(),
        ))?;
        let mut all_storage_changes = HashMap::new();
        msg.storage_changes
            .into_iter()
            .for_each(|contract_changes| {
                let mut storage_changes = HashMap::new();
                for change in contract_changes.slots.into_iter() {
                    storage_changes.insert(
                        change.slot.into(),
                        ContractStorageChange::new(change.value, change.previous_value),
                    );
                }
                let contract_change = ContractChanges::new(
                    contract_changes.address.clone().into(),
                    storage_changes,
                    contract_changes
                        .native_balance
                        .map(Into::into),
                );
                all_storage_changes.insert(contract_changes.address.into(), contract_change);
            });

        Ok(Self { tx, contract_changes: all_storage_changes })
    }
}

impl TryFromMessage for BlockChanges {
    type Args<'a> = (
        pb::BlockChanges,
        &'a str,
        Chain,
        &'a str,
        &'a HashMap<String, ProtocolType>,
        u64,
        Option<u32>,
    );

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, DecodeError> {
        let (
            msg,
            extractor,
            chain,
            protocol_system,
            protocol_types,
            finalized_block_height,
            partial_block_index,
        ) = args;

        if let Some(block) = msg.block {
            let block = Block::try_from_message((block, chain))?;

            let txs_with_update = msg
                .changes
                .into_iter()
                .map(|change| {
                    change.tx.as_ref().ok_or_else(|| {
                        DecodeError::Decode("TransactionChanges misses a transaction".to_owned())
                    })?;

                    TxWithChanges::try_from_message((
                        change,
                        &block,
                        protocol_system,
                        protocol_types,
                    ))
                })
                .collect::<Result<Vec<TxWithChanges>, DecodeError>>()?;

            let mut txs_with_update = txs_with_update;
            txs_with_update.sort_unstable_by_key(|update| update.tx.index);

            let block_storage_changes = msg
                .storage_changes
                .into_iter()
                .map(|change| TxWithContractChanges::try_from_message((change, &block)))
                .collect::<Result<Vec<TxWithContractChanges>, DecodeError>>()?;

            let mut block_changes = BlockChanges::new(
                extractor.to_string(),
                chain,
                block,
                finalized_block_height,
                false,
                txs_with_update,
                block_storage_changes,
            );
            block_changes.set_partial_block_index(partial_block_index);

            Ok(block_changes)
        } else {
            Err(DecodeError::Empty)
        }
    }
}
