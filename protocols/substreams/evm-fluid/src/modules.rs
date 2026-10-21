use crate::abi::{
    dex_t1_admin::events::{
        LogInitializePriceParams, LogPauseSwapAndArbitrage, LogUnpauseSwapAndArbitrage,
    },
    dex_t1_deployment_logic::events::DexT1Deployed,
    erc20,
};
use anyhow::{anyhow, Ok, Result};
use ethabi::ethereum_types::Address;
use itertools::Itertools;
use prost::Message;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use substreams::{hex, prelude::*};
use substreams_ethereum::{
    pb::{eth, eth::v2::TransactionTrace},
    rpc::RpcBatch,
    Event,
};
use substreams_helper::{common::HasAddresser, event_handler::EventHandler, hex::Hexable};
use tycho_substreams::{
    block_storage::get_block_storage_changes,
    contract::extract_contract_changes_builder,
    entrypoint::create_entrypoint,
    prelude::{entry_point_params::TraceData, *},
};

#[derive(Debug, Deserialize, Serialize)]
pub struct DeploymentParameters {
    liquidity_contract: Address,
    factory_address: Address,
    // Reserve resolvers by their deployment block
    resolvers: Vec<(u64, Address)>,
    tvl_query_frequency: u64,
    tvl_query_start_block: u64,
}

impl DeploymentParameters {
    fn get_reserves_resolver(&self, block_number: u64) -> Address {
        for (deploy_block, address) in self.resolvers.iter() {
            if block_number > *deploy_block {
                return *address;
            }
        }
        panic!("No reserves resolver found for block {}", block_number);
    }

    fn should_query_tvl(&self, block_number: u64) -> bool {
        if block_number < self.tvl_query_start_block {
            return false;
        }
        block_number.is_multiple_of(self.tvl_query_frequency)
    }

    fn should_track(&self, address: &[u8]) -> bool {
        address == self.liquidity_contract.as_bytes() ||
            address == self.factory_address.as_bytes() ||
            self.resolvers
                .iter()
                .any(|(_, a)| a.as_bytes() == address)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PreResolverDexes {
    dexes: Vec<Address>,
    /// Block the first resolver contract was deployed
    deploy_block: u64,
}

impl PreResolverDexes {
    fn get_hex_addresses(&self) -> Vec<String> {
        self.dexes
            .iter()
            .map(|a| a.to_hex())
            .collect()
    }
}

#[substreams::handlers::map]
pub fn map_dex_deployed(
    params: String,
    block: eth::v2::Block,
) -> Result<BlockTransactionProtocolComponents> {
    // Note: since we buffer components, it is not necessary to aggregate them per
    // transaction here.
    let mut new_dexes = vec![];
    let params: DeploymentParameters = serde_qs::from_str(&params)?;
    get_new_dexes(&block, &mut new_dexes, &params);

    // add token decimals to static attributes
    let mut batch = RpcBatch::new();
    let mut tokens = vec![];
    for components in new_dexes.iter_mut() {
        for component in components.components.iter() {
            for t in component.tokens.iter() {
                tokens.push(t.clone());
                batch = batch.add(erc20::functions::Decimals {}, t.clone());
            }
        }
    }
    // Handle any bad responses so we can unwrap later safely
    let decimal_responses = batch
        .execute()
        .map_err(|e| anyhow!("Failed getting decimal batch: {}", e))?
        .responses;
    if decimal_responses
        .iter()
        .any(|r| r.failed)
    {
        return Err(anyhow!("Decimal method reverted"));
    }

    decimal_responses
        .into_iter()
        .zip(tokens)
        .map(|(r, t)| {
            // handle ETH address
            if t == ZERO_ADDRESS {
                BigInt::from(18)
            } else {
                RpcBatch::decode::<_, erc20::functions::Decimals>(&r).unwrap_or_else(|| {
                    panic!("Failed to decode decimals for token {}", hex::encode(t))
                })
            }
        })
        .tuples::<(_, _)>()
        .zip(
            new_dexes
                .iter_mut()
                .flat_map(|c| c.components.iter_mut()),
        )
        .for_each(|((t0_decimals, t1_decimals), components)| {
            components.static_att.push(Attribute {
                name: "t0_decimals".to_string(),
                value: t0_decimals.to_signed_bytes_be(),
                change: ChangeType::Creation.into(),
            });
            components.static_att.push(Attribute {
                name: "t1_decimals".to_string(),
                value: t1_decimals.to_signed_bytes_be(),
                change: ChangeType::Creation.into(),
            });
        });

    Ok(BlockTransactionProtocolComponents { tx_components: new_dexes })
}

fn get_new_dexes(
    block: &eth::v2::Block,
    new_dexes: &mut Vec<TransactionProtocolComponents>,
    params: &DeploymentParameters,
) {
    // Extract new dex pools from LogDexDeployed events
    let mut on_dex_deployed =
        |event: DexT1Deployed, tx: &eth::v2::TransactionTrace, _log: &eth::v2::Log| {
            let tycho_tx: Transaction = tx.into();
            let resolver_address = params.get_reserves_resolver(block.number);

            new_dexes.push(TransactionProtocolComponents {
                tx: Some(tycho_tx.clone()),
                components: vec![ProtocolComponent {
                    id: event.dex.to_hex(),
                    tokens: vec![
                        coerce_native_address(event.supply_token.clone()),
                        coerce_native_address(event.borrow_token.clone()),
                    ],
                    contracts: vec![
                        params
                            .liquidity_contract
                            .as_bytes()
                            .to_vec(),
                        resolver_address.as_bytes().to_vec(),
                        event.dex.clone(),
                    ],
                    static_att: vec![
                        Attribute {
                            name: "reserves_resolver_address".to_string(),
                            value: resolver_address.as_bytes().to_vec(),
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "deploy_tx".to_string(),
                            value: tx.hash.clone(),
                            change: ChangeType::Creation.into(),
                        },
                    ],
                    change: i32::from(ChangeType::Creation),
                    protocol_type: Some(ProtocolType {
                        name: "fluid_dex_pool".to_string(),
                        financial_type: FinancialType::Swap.into(),
                        attribute_schema: vec![],
                        implementation_type: ImplementationType::Custom.into(),
                    }),
                }],
            })
        };

    let mut eh = EventHandler::new(block);

    eh.filter_by_address(vec![params.factory_address]);

    eh.on::<DexT1Deployed, _>(&mut on_dex_deployed);
    eh.handle_events();
}

const ETH_ADDRESS: &[u8] = &hex!("EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE");
const ZERO_ADDRESS: &[u8] = &hex!("0000000000000000000000000000000000000000");
fn coerce_native_address(address: Vec<u8>) -> Vec<u8> {
    if address.as_slice() == ETH_ADDRESS {
        ZERO_ADDRESS.to_vec()
    } else {
        address
    }
}

/// Buffer components to be emitted only once they have liquidity.
///
/// At the moment pools are created the contracts are not fully ready to be swapped on
/// so the entrypoint traces lack some contracts. By emitting them at a later point,
/// we ensure that the pool is ready for swapping and DCI can trace the contracts
/// required for this call to work correctly.
#[substreams::handlers::store]
pub fn store_components(dexes_deployed: BlockTransactionProtocolComponents, store: StoreSetRaw) {
    for change in dexes_deployed.tx_components {
        for new_component in change.components.iter() {
            store.set(0, &new_component.id, &new_component.encode_to_vec());
        }
    }
}

/// Stores known contracts involved in quoting.
#[substreams::handlers::store]
pub fn store_contract_addresses(
    dexes_deployed: BlockTransactionProtocolComponents,
    store: StoreSetInt64,
) {
    for change in dexes_deployed.tx_components {
        for new_protocol_component in change.components.iter() {
            let addresses = new_protocol_component
                .contracts
                .iter()
                .map(hex::encode)
                .collect();
            store.set_many(0, &addresses, &1i64);
        }
    }
}

#[substreams::handlers::map]
fn map_initialized_components(
    params: String,
    block: eth::v2::Block,
    components_store: StoreGetRaw,
) -> Result<BlockTransactionProtocolComponents> {
    let params = serde_qs::from_str::<PreResolverDexes>(&params)?;
    let pre_resolver_dexes = params.get_hex_addresses();

    let mut new_components = HashMap::new();
    // emit all components that were deployed before the resolvers were deployed
    for tx in block.transactions() {
        if tx.hash == hex!("eb645a1cec04e843da6b268282d7002e3abaeb792cc380db11982b89dd52997a") {
            for address in pre_resolver_dexes.iter() {
                try_emit_buffered_component(&components_store, &mut new_components, tx, address)?;
            }
        }
    }

    for log_view in block.logs() {
        let log = log_view.log;
        let tx = log_view.receipt.transaction;
        if LogInitializePriceParams::match_and_decode(log).is_some() {
            let hex_address = log.address.to_hex();
            if pre_resolver_dexes.contains(&hex_address) && block.number <= params.deploy_block {
                continue; // these are processed separately
            }
            try_emit_buffered_component(&components_store, &mut new_components, tx, &hex_address)?;
        }
    }

    Ok(BlockTransactionProtocolComponents { tx_components: new_components.into_values().collect() })
}

fn try_emit_buffered_component(
    components_store: &StoreGetRaw,
    new_components: &mut HashMap<Vec<u8>, TransactionProtocolComponents>,
    tx: &TransactionTrace,
    address: &str,
) -> Result<(), substreams::errors::Error> {
    // Process all chosen addresses
    if let Some(component_raw) = components_store.get_last(address) {
        let component = ProtocolComponent::decode(component_raw.as_slice())?;
        new_components
            .entry(tx.hash.clone())
            .and_modify(|tx_components: &mut TransactionProtocolComponents| {
                tx_components
                    .components
                    .push(component.clone())
            })
            .or_insert_with(|| TransactionProtocolComponents {
                tx: Some(tx.into()),
                components: vec![component],
            });
    } else {
        substreams::log::info!("Component missing from store: {}", address);
    }
    Ok(())
}

#[substreams::handlers::store]
fn store_initialized_components(
    components: BlockTransactionProtocolComponents,
    store: StoreSetInt64,
) {
    for tx_components in components.tx_components {
        for new_component in tx_components.components.iter() {
            store.set(0, &new_component.id, &1i64);
        }
    }
}

#[substreams::handlers::map]
fn map_protocol_changes(
    params: String,
    block: eth::v2::Block,
    initialised_components: BlockTransactionProtocolComponents,
    component_store: StoreGetRaw,
    component_initialized_store: StoreGetInt64,
    contracts_store: StoreGetRaw,
) -> Result<BlockChanges, substreams::errors::Error> {
    // We merge contract changes by transaction (identified by transaction index)
    // making it easy to sort them at the very end.
    let params: DeploymentParameters = serde_qs::from_str(&params)?;
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    let default_attributes = vec![Attribute {
        name: "paused".to_string(),
        value: vec![0u8],
        change: ChangeType::Creation.into(),
    }];

    // Aggregate newly created components per tx
    initialised_components
        .tx_components
        .iter()
        .for_each(|tx_component| {
            // initialise builder if not yet present for this tx
            let tx = tx_component.tx.as_ref().unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(tx));

            // iterate over individual components created within this tx
            tx_component
                .components
                .iter()
                .for_each(|component| {
                    builder.add_protocol_component(component);
                    let calldata = hex::decode("bd964d38000000000000000000000000")
                        .expect("above literal should be safe to decode")
                        .into_iter()
                        .chain(component.contracts[2].clone())
                        .collect();

                    let reserves_resolver = component
                        .static_att
                        .iter()
                        .find(|attr| attr.name == "reserves_resolver_address")
                        .expect("component has 'reserves_resolver_address' attribute")
                        .value
                        .clone();

                    // create an entrypoint for each component
                    let (entrypoint, params) = create_entrypoint(
                        reserves_resolver,
                        "getPoolReservesAdjusted(address)".to_string(),
                        component.id.clone(),
                        TraceData::Rpc(RpcTraceData { caller: None, calldata }),
                    );
                    builder.add_entrypoint(&entrypoint);
                    builder.add_entrypoint_params(&params);
                    builder.add_entity_change(&EntityChanges {
                        component_id: component.id.clone(),
                        attributes: default_attributes.clone(),
                    })
                });
        });

    // Extract and insert any storage changes that happened for any of the components.
    extract_contract_changes_builder(
        &block,
        |addr| {
            params.should_track(addr) ||
                contracts_store
                    .get_last(hex::encode(addr))
                    .is_some()
        },
        &mut transaction_changes,
    );

    // handle attributes if a pool was paused or unpaused
    add_paused_attributes(&component_initialized_store, &mut transaction_changes, &block);

    if params.should_query_tvl(block.number) ||
        !initialised_components
            .tx_components
            .is_empty()
    {
        query_and_emit_balances(
            &block,
            component_store,
            component_initialized_store,
            params.get_reserves_resolver(block.number),
            &mut transaction_changes,
        );
    }

    let block_storage_changes = get_block_storage_changes(&block);

    // Process all `transaction_changes` for final output in the `BlockChanges`,
    //  sorted by transaction index (the key).
    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
        storage_changes: block_storage_changes,
    })
}

pub fn add_paused_attributes(
    dex_addresses: &StoreGetInt64,
    tx_changes: &mut HashMap<u64, TransactionChangesBuilder>,
    block: &eth::v2::Block,
) {
    for log_view in block.logs() {
        let log = log_view.log;
        let tx = log_view.receipt.transaction;
        if !dex_addresses.has_address(Address::from_slice(log.address.as_slice())) {
            continue;
        }

        if LogPauseSwapAndArbitrage::match_and_decode(log).is_some() {
            let builder = tx_changes
                .entry(tx.index as u64)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));

            builder.add_entity_change(&EntityChanges {
                component_id: log.address.to_hex(),
                attributes: vec![Attribute {
                    name: "paused".to_string(),
                    value: vec![1u8],
                    change: ChangeType::Creation.into(),
                }],
            })
        }

        if LogUnpauseSwapAndArbitrage::match_and_decode(log).is_some() {
            let builder = tx_changes
                .entry(tx.index as u64)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx.into()));

            builder.add_entity_change(&EntityChanges {
                component_id: log.address.to_hex(),
                attributes: vec![Attribute {
                    name: "paused".to_string(),
                    value: vec![],
                    change: ChangeType::Deletion.into(),
                }],
            })
        }
    }
}

fn from_adjusted_amount(adjusted_amount: BigInt, decimals: i32) -> BigInt {
    let diff = decimals - 12;

    if diff == 0 {
        adjusted_amount
    } else if diff < 0 {
        // Divide by 10^(-diff)
        let divisor = BigInt::from(10u64).pow(diff.unsigned_abs());
        adjusted_amount / divisor
    } else {
        // Multiply by 10^(diff)
        let multiplier = BigInt::from(10u64).pow(diff as u32);
        adjusted_amount * multiplier
    }
}

fn query_and_emit_balances(
    block: &eth::v2::Block,
    component_store: StoreGetRaw,
    component_initialized_store: StoreGetInt64,
    resolver_address: Address,
    transaction_changes: &mut HashMap<u64, TransactionChangesBuilder>,
) {
    if let Some(tx) = block.transactions().last() {
        let reserves_call =
            crate::abi::reserves_resolver::functions::GetAllPoolsReservesAdjusted {};
        if let Some(reserves) = reserves_call.call(resolver_address.as_bytes().to_vec()) {
            for pool_with_reserves in reserves {
                let pool_hex_address = pool_with_reserves.0.to_hex();
                if component_initialized_store
                    .get_last(&pool_hex_address)
                    .is_some()
                {
                    let component = component_store
                        .get_last(&pool_hex_address)
                        .map(|v| {
                            ProtocolComponent::decode(v.as_slice())
                                .expect("component serialization valid")
                        })
                        .expect("initialized component exists in store");

                    let t0_decimals = component
                        .static_att
                        .iter()
                        .find(|a| a.name == "t0_decimals")
                        .map(|attr| BigInt::from_signed_bytes_be(&attr.value).to_u64())
                        .expect("t0_decimals attribute exists");

                    let t1_decimals = component
                        .static_att
                        .iter()
                        .find(|a| a.name == "t1_decimals")
                        .map(|attr| BigInt::from_signed_bytes_be(&attr.value).to_u64())
                        .expect("t1_decimals attribute exists");

                    let builder = transaction_changes
                        .entry(tx.index as u64)
                        .or_insert_with(|| TransactionChangesBuilder::new(&(tx.into())));

                    builder.add_balance_change(&BalanceChange {
                        token: coerce_native_address(pool_with_reserves.1.clone()),
                        balance: from_adjusted_amount(
                            pool_with_reserves.5 .0 + pool_with_reserves.6 .0,
                            t0_decimals as i32,
                        )
                        .to_signed_bytes_be(),
                        component_id: pool_hex_address.clone().into(),
                    });

                    builder.add_balance_change(&BalanceChange {
                        token: coerce_native_address(pool_with_reserves.2.clone()),
                        balance: from_adjusted_amount(
                            pool_with_reserves.5 .1 + pool_with_reserves.6 .1,
                            t1_decimals as i32,
                        )
                        .to_signed_bytes_be(),
                        component_id: pool_hex_address.clone().into(),
                    })
                } else {
                    substreams::log::debug!(
                        "Skipping balance emission uninitialized pool: {}",
                        pool_with_reserves.0.to_hex()
                    );
                }
            }
        } else {
            substreams::log::info!("Reserves call failed");
        };
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::str::FromStr;

    pub const LIQUIDITY_CONTRACT_ADDRESS: &[u8] = &hex!("52Aa899454998Be5b000Ad077a46Bbe360F4e497");
    pub const RESERVE_RESOLVER: &[(u64, [u8; 20])] = &[
        (22487434, hex!("C93876C0EEd99645DD53937b25433e311881A27C")),
        (0, hex!("b387f9C2092cF7c4943F97842887eBff7AE96EB3")),
    ];
    pub const PRE_RESOLVER_DEXES: &[&str] = &[
        "0x667701e51b4d1ca244f17c78f7ab8744b4c99f9b",
        "0x3c0441b42195f4ad6aa9a0978e06096ea616cda7",
        "0xc800b0e15c40a1ff0539218100c86f4c1bac8d9c",
        "0x86f874212335af27c41cdb855c2255543d1499ce",
        "0x6d83f60eeac0e50a1250760151e81db2a278e03a",
        "0x276084527b801e00db8e4410504f9baf93f72c67",
        "0x836951eb21f3df98273517b7249dceff270d34bf",
        "0x085b07a30381f3cc5a4250e10e4379d465b770ac",
        "0x25f0a3b25cbc0ca0417770f686209628323ff901",
        "0x0b1a513ee24972daef112bc777a5610d4325c9e7",
        "0x2886a01a0645390872a9eb99dae1283664b0c524",
        "0xde632c3a214d5f14c1d8ddf0b92f8bcd188fee45",
        "0x080574d224e960c272e005aa03efbe793f317640",
        "0x8710039d5de6840ede452a85672b32270a709ae2",
        "0x1d3e52a11b98ed2aab7eb0bfe1cbb6525233204d",
    ];

    #[test]
    fn test_show_encoded_deployment_params() {
        let params = DeploymentParameters {
            liquidity_contract: Address::from_slice(LIQUIDITY_CONTRACT_ADDRESS),
            factory_address: Address::from_str("0x91716C4EDA1Fb55e84Bf8b4c7085f84285c19085")
                .unwrap(),
            resolvers: RESERVE_RESOLVER
                .iter()
                .map(|(b, a)| (*b, Address::from(a)))
                .collect(),
            tvl_query_frequency: 300,
            tvl_query_start_block: 23740216,
        };

        dbg!(serde_qs::to_string(&params).unwrap());
    }

    #[test]
    fn test_show_encoded_pre_resolver_dexes() {
        let params = PreResolverDexes {
            deploy_block: 21596670,
            dexes: PRE_RESOLVER_DEXES
                .iter()
                .map(|a| Address::from_str(a).unwrap())
                .collect(),
        };

        dbg!(serde_qs::to_string(&params).unwrap());
    }
}
