use std::collections::{hash_map::Entry, HashMap, HashSet};

use chrono::{DateTime, NaiveDateTime};
use tracing::warn;
use tycho_common::{
    models::{
        blockchain::{
            Block, EntryPoint, RPCTracerParams, TracingParams, Transaction, TxWithChanges,
        },
        contract::{AccountBalance, AccountDelta, ContractChanges, ContractStorageChange},
        protocol::{ComponentBalance, ProtocolComponent, ProtocolComponentStateDelta},
        Address, Chain, ChangeType, ComponentId, EntryPointId, ProtocolType, TxHash,
    },
    Bytes,
};
use tycho_substreams::pb::tycho::evm::v1 as substreams;

use crate::extractor::{
    models::{BlockChanges, TxWithContractChanges},
    u256_num::bytes_to_f64,
    ExtractionError,
};

pub trait TryFromMessage {
    type Args<'a>;

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError>
    where
        Self: Sized;
}

impl TryFromMessage for AccountDelta {
    type Args<'a> = (substreams::ContractChange, Chain);

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError> {
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
    type Args<'a> = (substreams::AccountBalanceChange, &'a Address, &'a Transaction);

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError> {
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
    type Args<'a> = (substreams::Block, Chain);

    /// Parses block from tychos protobuf block message
    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError> {
        let (msg, chain) = args;

        Ok(Self {
            chain,
            number: msg.number,
            hash: msg.hash.into(),
            parent_hash: msg.parent_hash.into(),
            ts: DateTime::from_timestamp(msg.ts as i64, 0)
                .ok_or_else(|| {
                    ExtractionError::DecodeError(format!(
                        "Failed to convert timestamp {} to datetime!",
                        msg.ts
                    ))
                })?
                .naive_utc(),
        })
    }
}

impl TryFromMessage for Transaction {
    type Args<'a> = (substreams::Transaction, &'a TxHash);

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError> {
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
    type Args<'a> = (substreams::BalanceChange, &'a Transaction);

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError> {
        let (msg, tx) = args;
        let balance_float = bytes_to_f64(&msg.balance).unwrap_or(f64::NAN);
        Ok(Self {
            token: msg.token.into(),
            balance: Bytes::from(msg.balance),
            balance_float,
            modify_tx: tx.hash.clone(),
            component_id: String::from_utf8(msg.component_id)
                .map_err(|error| ExtractionError::DecodeError(error.to_string()))?,
        })
    }
}

impl TryFromMessage for ProtocolComponent {
    type Args<'a> = (
        substreams::ProtocolComponent,
        Chain,
        &'a str,
        &'a HashMap<String, ProtocolType>,
        TxHash,
        NaiveDateTime,
    );

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError> {
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
            .ok_or(ExtractionError::DecodeError("Missing protocol type".to_owned()))?;

        if !protocol_types.contains_key(&protocol_type.name) {
            return Err(ExtractionError::DecodeError(format!(
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
    type Args<'a> = substreams::ChangeType;

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError> {
        match args {
            substreams::ChangeType::Creation => Ok(ChangeType::Creation),
            substreams::ChangeType::Update => Ok(ChangeType::Update),
            substreams::ChangeType::Deletion => Ok(ChangeType::Deletion),
            substreams::ChangeType::Unspecified => Err(ExtractionError::DecodeError(format!(
                "Unknown ChangeType enum member encountered: {args:?}"
            ))),
        }
    }
}

impl TryFromMessage for ProtocolComponentStateDelta {
    type Args<'a> = substreams::EntityChanges;

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError> {
        let msg = args;

        let (mut updates, mut deletions) = (HashMap::new(), HashSet::new());

        for attribute in msg.attributes.into_iter() {
            match ChangeType::try_from_message(attribute.change())? {
                ChangeType::Update | ChangeType::Creation => {
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
        })
    }
}

impl TryFromMessage for EntryPoint {
    type Args<'a> = substreams::EntryPoint;

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError> {
        let msg = args;

        Ok(Self { external_id: msg.id, target: msg.target.into(), signature: msg.signature })
    }
}

impl TryFromMessage for TracingParams {
    type Args<'a> = substreams::EntryPointParams;

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError> {
        let msg = args;
        let trace_data = msg.trace_data.ok_or_else(|| {
            ExtractionError::DecodeError("Missing trace data in EntryPointParams".to_owned())
        })?;

        match trace_data {
            substreams::entry_point_params::TraceData::Rpc(rpc_data) => {
                let caller = rpc_data.caller.map(|c| c.into());
                Ok(Self::RPCTracer(RPCTracerParams::new(caller, rpc_data.calldata.into())))
            }
        }
    }
}

impl TryFromMessage for TxWithChanges {
    type Args<'a> =
        (substreams::TransactionChanges, &'a Block, &'a str, &'a HashMap<String, ProtocolType>);

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError> {
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

        // Parse the new protocol components
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

        // Parse the account updates
        for contract_change in msg.contract_changes.clone().into_iter() {
            let update = AccountDelta::try_from_message((contract_change, block.chain))?;
            account_updates.insert(update.address.clone(), update);
        }

        // Parse the state updates
        for state_msg in msg.entity_changes.into_iter() {
            let state = ProtocolComponentStateDelta::try_from_message(state_msg)?;
            // Check if a state update for the same component already exists
            // If it exists, overwrite the existing state update with the new one and log a warning
            match state_updates.entry(state.component_id.clone()) {
                Entry::Vacant(e) => {
                    e.insert(state);
                }
                Entry::Occupied(mut e) => {
                    warn!("Received two state updates for the same component. Overwriting state for component {}", e.key());
                    e.insert(state);
                }
            }
        }

        // Parse the component balance changes
        for balance_change in msg.balance_changes.into_iter() {
            let component_id = String::from_utf8(balance_change.component_id.clone())
                .map_err(|error| ExtractionError::DecodeError(error.to_string()))?;
            let token_address = Bytes::from(balance_change.token.clone());
            let balance = ComponentBalance::try_from_message((balance_change, &tx))?;

            balance_changes
                .entry(component_id)
                .or_default()
                .insert(token_address, balance);
        }

        // Parse the account balance changes
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

        // Parse the entrypoints
        for msg_entrypoint in msg.entrypoints.into_iter() {
            let component_id = msg_entrypoint.component_id.clone();
            let entrypoint = EntryPoint::try_from_message(msg_entrypoint)?;
            entrypoints
                .entry(component_id)
                .or_default()
                .insert(entrypoint);
        }

        // Parse the entrypoint params
        for msg_entrypoint_params in msg.entrypoint_params.into_iter() {
            let entrypoint_id = msg_entrypoint_params
                .entrypoint_id
                .clone();
            let component_id = msg_entrypoint_params
                .component_id
                .clone()
                .ok_or(ExtractionError::DecodeError(
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
    type Args<'a> = (substreams::TransactionStorageChanges, &'a Block);

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError> {
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
        substreams::BlockChanges,
        &'a str,
        Chain,
        &'a str,
        &'a HashMap<String, ProtocolType>,
        u64,
        Option<u32>,
    );

    fn try_from_message(args: Self::Args<'_>) -> Result<Self, ExtractionError> {
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
                        ExtractionError::DecodeError(
                            "TransactionChanges misses a transaction".to_owned(),
                        )
                    })?;

                    TxWithChanges::try_from_message((
                        change,
                        &block,
                        protocol_system,
                        protocol_types,
                    ))
                })
                .collect::<Result<Vec<TxWithChanges>, ExtractionError>>()?;

            // Sort updates by transaction index
            let mut txs_with_update = txs_with_update;
            txs_with_update.sort_unstable_by_key(|update| update.tx.index);

            let block_storage_changes = msg
                .storage_changes
                .into_iter()
                .map(|change| TxWithContractChanges::try_from_message((change, &block)))
                .collect::<Result<Vec<TxWithContractChanges>, ExtractionError>>()?;

            let mut block_changes = Self::new(
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
            Err(ExtractionError::Empty)
        }
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use rstest::rstest;

    use super::*;
    use crate::{extractor::models::fixtures::create_transaction, testing::fixtures};

    #[test]
    fn test_parse_protocol_state_update() {
        let msg = fixtures::pb_state_changes();

        let res = ProtocolComponentStateDelta::try_from_message(msg).unwrap();

        assert_eq!(res, fixtures::protocol_state_delta());
    }

    #[test]
    fn test_parse_tx_with_storage_changes() {
        let msg = fixtures::pb_transaction_storage_changes(0);
        let tx = Transaction::new(
            Bytes::from_str("0x0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap(),
            Bytes::default(),
            Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
            Some(Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap()),
            1,
        );
        let exp = TxWithContractChanges {
            tx,
            contract_changes: HashMap::from([
                (
                    Bytes::from_str("0000000000000000000000000000000000000001").unwrap(),
                    ContractChanges::new(
                        Bytes::from_str("0000000000000000000000000000000000000001").unwrap(),
                        HashMap::from([
                            (
                                Bytes::from_str("0x01").unwrap(),
                                ContractStorageChange::initial(Bytes::from_str("0x01").unwrap()),
                            ),
                            (
                                Bytes::from_str("0x02").unwrap(),
                                ContractStorageChange::initial(Bytes::from_str("0x02").unwrap()),
                            ),
                        ]),
                        None,
                    ),
                ),
                (
                    Bytes::from_str("0000000000000000000000000000000000000002").unwrap(),
                    ContractChanges::new(
                        Bytes::from_str("0000000000000000000000000000000000000002").unwrap(),
                        HashMap::from([(
                            Bytes::from_str("0x03").unwrap(),
                            ContractStorageChange::initial(Bytes::from_str("0x03").unwrap()),
                        )]),
                        Some(Bytes::from(1000u64)),
                    ),
                ),
            ]),
        };

        let res = TxWithContractChanges::try_from_message((msg, &Block::default())).unwrap();

        assert_eq!(res, exp);
    }

    #[test]
    fn test_parse_protocol_component() {
        let msg = fixtures::pb_protocol_component();

        let expected_chain = Chain::Ethereum;
        let expected_protocol_system = "ambient".to_string();
        let expected_attribute_map: HashMap<String, Bytes> = vec![
            ("balance".to_string(), Bytes::from(100u64).lpad(32, 0)),
            ("factory_address".to_string(), Bytes::from(b"0x0fwe0g240g20".to_vec())),
        ]
        .into_iter()
        .collect();

        let protocol_type_id = "WeightedPool".to_string();
        let protocol_types: HashMap<String, ProtocolType> =
            HashMap::from([(protocol_type_id.clone(), ProtocolType::default())]);

        // Call the try_from_message method
        let result = ProtocolComponent::try_from_message((
            msg,
            expected_chain,
            &expected_protocol_system,
            &protocol_types,
            Bytes::from_str("0x0e22048af8040c102d96d14b0988c6195ffda24021de4d856801553aa468bcac")
                .unwrap(),
            Default::default(),
        ));

        // Assert the result
        assert!(result.is_ok());

        // Unwrap the result for further assertions
        let protocol_component = result.unwrap();

        // Assert specific properties of the protocol component
        assert_eq!(
            protocol_component.id,
            "d417ff54652c09bd9f31f216b1a2e5d1e28c1dce1ba840c40d16f2b4d09b5902".to_string()
        );
        assert_eq!(protocol_component.protocol_system, expected_protocol_system);
        assert_eq!(protocol_component.protocol_type_name, protocol_type_id);
        assert_eq!(protocol_component.chain, expected_chain);
        assert_eq!(
            protocol_component.tokens,
            vec![
                Bytes::from_str("6B175474E89094C44Da98b954EedeAC495271d0F").unwrap(),
                Bytes::from_str("6B175474E89094C44Da98b954EedeAC495271d0F").unwrap(),
            ]
        );
        assert_eq!(
            protocol_component.contract_addresses,
            vec![
                Bytes::from_str("31fF2589Ee5275a2038beB855F44b9Be993aA804").unwrap(),
                Bytes::from_str("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            ]
        );
        assert_eq!(protocol_component.static_attributes, expected_attribute_map);
    }

    pub fn transaction() -> Transaction {
        create_transaction(
            "0000000000000000000000000000000000000000000000000000000011121314",
            "0000000000000000000000000000000000000000000000000000000031323334",
            2,
        )
    }

    #[test]
    fn test_parse_component_balance() {
        let tx = transaction();
        let expected_balance: f64 = 3000.0;
        let msg_balance = expected_balance.to_be_bytes().to_vec();

        let expected_token = Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap();
        let msg_token = expected_token.0.to_vec();
        let expected_component_id =
            "d417ff54652c09bd9f31f216b1a2e5d1e28c1dce1ba840c40d16f2b4d09b5902";
        let msg_component_id = expected_component_id
            .as_bytes()
            .to_vec();
        let msg = substreams::BalanceChange {
            balance: msg_balance.to_vec(),
            token: msg_token,
            component_id: msg_component_id,
        };
        let from_message = ComponentBalance::try_from_message((msg, &tx)).unwrap();

        assert_eq!(from_message.balance, msg_balance);
        assert_eq!(from_message.modify_tx, tx.hash);
        assert_eq!(from_message.token, expected_token);
        assert_eq!(from_message.component_id, expected_component_id);
    }

    #[rstest]
    #[case::rpc_trace_data(
        substreams::entry_point_params::TraceData::Rpc(
            substreams::RpcTraceData{
                    caller: Some(Bytes::from_str("0x1234567890123456789012345678901234567890")
                        .unwrap()
                        .to_vec()),
                    calldata: Bytes::from_str("0xabcdef")
                        .unwrap()
                        .to_vec(),
                },
        ),
        TracingParams::RPCTracer(
            RPCTracerParams{
                    caller: Some(Address::from_str("0x1234567890123456789012345678901234567890").unwrap()),
                    calldata: Bytes::from_str("0xabcdef").unwrap(),
                    state_overrides: None,
                    prune_addresses: None,
                }
        )
    )]
    fn test_parse_entrypoint_params(
        #[case] trace_data: substreams::entry_point_params::TraceData,
        #[case] expected: TracingParams,
    ) {
        let msg = substreams::EntryPointParams {
            entrypoint_id: "test_entrypoint".to_string(),
            component_id: Some("test_component".to_string()),
            trace_data: Some(trace_data),
        };

        let result = TracingParams::try_from_message(msg).unwrap();

        assert_eq!(result, expected);
    }
}
