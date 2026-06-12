use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use alloy::primitives::{Address, U256};
use revm::state::Bytecode;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, simulation::errors::SimulationError, Bytes};

use super::{state::EVMPoolState, state_builder::EVMPoolStateBuilder};
use crate::{
    evm::{
        engine_db::{tycho_db::PreCachedDB, SHARED_TYCHO_DB},
        protocol::vm::{constants::get_adapter_file, utils::json_deserialize_address_list},
        simulation::BlockEnvOverrides,
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for EVMPoolState<PreCachedDB> {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState`, block `BlockHeader` and HashMap of all available tokens into
    /// an `EVMPoolState`.
    ///
    /// Errors with a `InvalidSnapshotError`.
    #[allow(deprecated)]
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let id = snapshot.component.id.clone();
        let tokens = snapshot.component.tokens.clone();

        // Decode involved contracts
        let mut stateless_contracts = HashMap::new();
        let mut index = 0;

        loop {
            let address_key = format!("stateless_contract_addr_{index}");
            if let Some(encoded_address_bytes) = snapshot
                .state
                .attributes
                .get(&address_key)
            {
                let encoded_address = hex::encode(encoded_address_bytes);
                // Stateless contracts address are UTF-8 encoded
                let address_hex = encoded_address
                    .strip_prefix("0x")
                    .unwrap_or(&encoded_address);

                let decoded = match hex::decode(address_hex) {
                    Ok(decoded_bytes) => match String::from_utf8(decoded_bytes) {
                        Ok(decoded_string) => decoded_string,
                        Err(_) => continue,
                    },
                    Err(_) => continue,
                };

                let code_key = format!("stateless_contract_code_{index}");
                let code = snapshot
                    .state
                    .attributes
                    .get(&code_key)
                    .map(|value| value.to_vec());

                stateless_contracts.insert(decoded, code);
                index += 1;
            } else {
                break;
            }
        }
        let involved_contracts = snapshot
            .component
            .contract_addresses
            .iter()
            .map(|bytes: &Bytes| Address::from_slice(bytes.as_ref()))
            .collect::<HashSet<Address>>();

        let potential_rebase_tokens: HashSet<Address> = if let Some(bytes) = snapshot
            .component
            .static_attributes
            .get("rebase_tokens")
        {
            if let Ok(vecs) = json_deserialize_address_list(bytes) {
                vecs.into_iter()
                    .map(|addr| Address::from_slice(&addr))
                    .collect()
            } else {
                HashSet::new()
            }
        } else {
            HashSet::new()
        };

        // Decode balances
        let balance_owner = snapshot
            .state
            .attributes
            .get("balance_owner")
            .map(|owner| Address::from_slice(owner.as_ref()));
        let component_balances = snapshot
            .state
            .balances
            .iter()
            .map(|(k, v)| (Address::from_slice(k), U256::from_be_slice(v)))
            .collect::<HashMap<_, _>>();
        let account_balances = account_balances
            .iter()
            .filter(|(k, _)| involved_contracts.contains(&Address::from_slice(k)))
            .map(|(k, v)| {
                let addr = Address::from_slice(k);
                let balances = v
                    .iter()
                    .map(|(k, v)| (Address::from_slice(k), U256::from_be_slice(v)))
                    .collect();
                (addr, balances)
            })
            .collect::<HashMap<_, _>>();

        let manual_updates = snapshot
            .component
            .static_attributes
            .contains_key("manual_updates");

        let protocol_name = snapshot
            .component
            .protocol_system
            .strip_prefix("vm:")
            .unwrap_or({
                snapshot
                    .component
                    .protocol_system
                    .as_str()
            });
        let adapter_bytecode;
        if let Some(adapter_bytecode_path) = &decoder_context.adapter_path {
            let bytecode_bytes = std::fs::read(adapter_bytecode_path).map_err(|e| {
                SimulationError::FatalError(format!(
                    "Failed to read adapter bytecode from {adapter_bytecode_path}: {e}"
                ))
            })?;
            adapter_bytecode = Bytecode::new_raw(bytecode_bytes.into());
        } else {
            adapter_bytecode = Bytecode::new_raw(get_adapter_file(protocol_name)?.into());
        }
        let adapter_contract_address = Address::from_str(&format!(
            "{hex_protocol_name:0>40}",
            hex_protocol_name = hex::encode(protocol_name)
        ))
        .map_err(|_| {
            InvalidSnapshotError::ValueError(
                "Error converting protocol name to address".to_string(),
            )
        })?;
        let mut vm_traces = false;
        if let Some(trace) = &decoder_context.vm_traces {
            vm_traces = *trace;
        }
        // A protocol may override only one block env field. In that case the VM call will use the
        // overridden field together with the current block's other field, so block.number and
        // block.timestamp may not correspond to the same real chain block.
        let block_number = snapshot
            .state
            .attributes
            .get("override_block_number")
            .map(|block_number| {
                <[u8; 8]>::try_from(block_number.as_ref())
                    .map(u64::from_be_bytes)
                    .map_err(|_| {
                        InvalidSnapshotError::ValueError(
                            "override_block_number attribute must be an 8-byte big-endian u64"
                                .to_string(),
                        )
                    })
            })
            .transpose()?;
        let block_timestamp = snapshot
            .state
            .attributes
            .get("override_block_timestamp")
            .map(|block_timestamp| {
                <[u8; 8]>::try_from(block_timestamp.as_ref())
                    .map(u64::from_be_bytes)
                    .map_err(|_| {
                        InvalidSnapshotError::ValueError(
                            "override_block_timestamp attribute must be an 8-byte big-endian u64"
                                .to_string(),
                        )
                    })
            })
            .transpose()?;
        let block_overrides = if block_number.is_some() || block_timestamp.is_some() {
            Some(BlockEnvOverrides { number: block_number, timestamp: block_timestamp })
        } else {
            None
        };
        let mut pool_state_builder =
            EVMPoolStateBuilder::new(id.clone(), tokens.clone(), adapter_contract_address)
                .balances(component_balances)
                .disable_overwrite_tokens(potential_rebase_tokens)
                .account_balances(account_balances)
                .adapter_contract_bytecode(adapter_bytecode)
                .involved_contracts(involved_contracts)
                .stateless_contracts(stateless_contracts)
                .manual_updates(manual_updates)
                .trace(vm_traces)
                .block_overrides(block_overrides);

        if let Some(balance_owner) = balance_owner {
            pool_state_builder = pool_state_builder.balance_owner(balance_owner)
        };

        let mut pool_state = pool_state_builder
            .build(SHARED_TYCHO_DB.clone())
            .await
            .map_err(InvalidSnapshotError::VMError)?;

        pool_state.set_spot_prices(all_tokens)?;

        Ok(pool_state)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, fs, path::Path};

    use chrono::DateTime;
    use revm::{primitives::KECCAK_EMPTY, state::AccountInfo};
    use serde_json::Value;
    use tycho_common::models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        Chain, ChangeType,
    };

    use super::*;
    use crate::evm::{
        engine_db::{create_engine, engine_db_interface::EngineDatabaseInterface},
        protocol::vm::constants::{BALANCER_V2, CURVE},
        tycho_models::AccountUpdate,
    };

    #[test]
    fn test_to_adapter_file_name() {
        assert_eq!(get_adapter_file("balancer_v2").unwrap(), BALANCER_V2);
        assert_eq!(get_adapter_file("curve").unwrap(), CURVE);
    }

    fn vm_component() -> ProtocolComponent {
        let creation_time = DateTime::from_timestamp(1622526000, 0)
            .unwrap()
            .naive_utc(); //Sample timestamp

        let mut static_attributes: HashMap<String, Bytes> = HashMap::new();
        static_attributes.insert("manual_updates".to_string(), Bytes::from_str("0x01").unwrap());

        let dai_addr = Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();
        let bal_addr = Bytes::from_str("0xba100000625a3754423978a60c9317c58a424e3d").unwrap();
        let tokens = vec![dai_addr, bal_addr];

        ProtocolComponent {
            id: "0x4626d81b3a1711beb79f4cecff2413886d461677000200000000000000000011".to_string(),
            protocol_system: "vm:balancer_v2".to_string(),
            protocol_type_name: "balancer_v2_pool".to_string(),
            chain: Chain::Ethereum,
            tokens,
            contract_addresses: vec![
                Bytes::from_str("0xBA12222222228d8Ba445958a75a0704d566BF2C8").unwrap()
            ],
            static_attributes,
            change: ChangeType::Creation,
            creation_tx: Bytes::from_str("0x0000").unwrap(),
            created_at: creation_time,
        }
    }

    fn load_balancer_account_data() -> Vec<AccountUpdate> {
        let project_root = env!("CARGO_MANIFEST_DIR");
        let asset_path =
            Path::new(project_root).join("tests/assets/decoder/balancer_v2_snapshot.json");
        let json_data = fs::read_to_string(asset_path).expect("Failed to read test asset");
        let data: Value = serde_json::from_str(&json_data).expect("Failed to parse JSON");

        let accounts: Vec<AccountUpdate> = serde_json::from_value(data["accounts"].clone())
            .expect("Expected accounts to match AccountUpdate structure");
        accounts
    }

    #[tokio::test]
    async fn test_try_from_with_header() {
        let attributes: HashMap<String, Bytes> = vec![
            (
                "balance_owner".to_string(),
                Bytes::from_str("0xBA12222222228d8Ba445958a75a0704d566BF2C8").unwrap(),
            ),
            ("override_block_number".to_string(), Bytes::from(123_u64.to_be_bytes().to_vec())),
            ("override_block_timestamp".to_string(), Bytes::from(456_u64.to_be_bytes().to_vec())),
            ("reserve1".to_string(), Bytes::from(200_u64.to_le_bytes().to_vec())),
        ]
        .into_iter()
        .collect();
        let tokens = [
            Token::new(
                &Bytes::from_str("0x6b175474e89094c44da98b954eedeac495271d0f").unwrap(),
                "DAI",
                18,
                0,
                &[Some(10_000)],
                tycho_common::models::Chain::Ethereum,
                100,
            ),
            Token::new(
                &Bytes::from_str("0xba100000625a3754423978a60c9317c58a424e3d").unwrap(),
                "BAL",
                18,
                0,
                &[Some(10_000)],
                tycho_common::models::Chain::Ethereum,
                100,
            ),
        ]
        .into_iter()
        .map(|t| (t.address.clone(), t))
        .collect::<HashMap<_, _>>();
        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "0x4626d81b3a1711beb79f4cecff2413886d461677000200000000000000000011"
                    .to_owned(),
                attributes,
                balances: HashMap::new(),
            },
            component: vm_component(),
            component_tvl: None,
            entrypoints: Vec::new(),
        };
        // Initialize engine with balancer storage
        let block = BlockHeader::default();
        let accounts = load_balancer_account_data();
        let db = SHARED_TYCHO_DB.clone();
        let engine = create_engine(db.clone(), false).unwrap();
        for account in accounts.clone() {
            engine
                .state
                .init_account(
                    account.address,
                    AccountInfo {
                        balance: account.balance.unwrap_or_default(),
                        nonce: 0u64,
                        code_hash: KECCAK_EMPTY,
                        code: account
                            .code
                            .clone()
                            .map(|arg0: Vec<u8>| Bytecode::new_raw(arg0.into())),
                    },
                    None,
                    false,
                )
                .expect("Failed to init account");
        }
        db.update(accounts, Some(block.clone()))
            .unwrap();
        let account_balances = HashMap::from([(
            Bytes::from("0xBA12222222228d8Ba445958a75a0704d566BF2C8"),
            HashMap::from([
                (
                    Bytes::from("0x6b175474e89094c44da98b954eedeac495271d0f"),
                    Bytes::from(100_u64.to_le_bytes().to_vec()),
                ),
                (
                    Bytes::from("0xba100000625a3754423978a60c9317c58a424e3d"),
                    Bytes::from(100_u64.to_le_bytes().to_vec()),
                ),
            ]),
        )]);

        let decoder_context = DecoderContext::new();
        let res = EVMPoolState::try_from_with_header(
            snapshot,
            block,
            &account_balances,
            &tokens,
            &decoder_context,
        )
        .await
        .unwrap();

        let res_pool = res;

        assert_eq!(
            res_pool.get_balance_owner(),
            Some(Address::from_str("0xBA12222222228d8Ba445958a75a0704d566BF2C8").unwrap())
        );
        let mut exp_involved_contracts = HashSet::new();
        exp_involved_contracts
            .insert(Address::from_str("0xBA12222222228d8Ba445958a75a0704d566BF2C8").unwrap());
        assert_eq!(res_pool.get_involved_contracts(), exp_involved_contracts);
        assert!(res_pool.get_manual_updates());
        assert_eq!(
            res_pool.get_block_overrides(),
            Some(BlockEnvOverrides { number: Some(123), timestamp: Some(456) })
        );
    }
}
