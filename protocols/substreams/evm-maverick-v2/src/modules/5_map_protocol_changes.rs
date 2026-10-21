use crate::{modules::utils::Params, pb::maverick::v2::Pool};
use anyhow::Result;
use itertools::Itertools;
use std::collections::HashMap;
use substreams::{pb::substreams::StoreDeltas, prelude::StoreGetProto, store::StoreGet};
use substreams_ethereum::pb::eth::v2::Block;
use substreams_helper::hex::Hexable;
use tycho_substreams::{
    balances::aggregate_balances_changes, contract::extract_contract_changes_builder, prelude::*,
};

#[substreams::handlers::map]
pub fn map_protocol_changes(
    params: String,
    block: Block,
    protocol_components: BlockTransactionProtocolComponents,
    balance_deltas: BlockBalanceDeltas,
    pool_store: StoreGetProto<Pool>,
    balance_store: StoreDeltas,
) -> Result<BlockChanges> {
    let params = Params::parse_from_query(&params)?;
    let (factory_address, quoter_address) = params.decode_addresses()?;
    let mut transaction_changes: HashMap<_, TransactionChangesBuilder> = HashMap::new();

    protocol_components
        .tx_components
        .iter()
        .for_each(|tx_component| {
            let tx = tx_component.tx.as_ref().unwrap();
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(tx));

            tx_component
                .components
                .iter()
                .for_each(|c| {
                    builder.add_protocol_component(c);
                });
        });

    aggregate_balances_changes(balance_store, balance_deltas)
        .into_iter()
        .for_each(|(_, (tx, balances))| {
            let builder = transaction_changes
                .entry(tx.index)
                .or_insert_with(|| TransactionChangesBuilder::new(&tx));
            balances
                .values()
                .for_each(|token_bc_map| {
                    token_bc_map
                        .values()
                        .for_each(|bc| builder.add_balance_change(bc))
                });
        });

    extract_contract_changes_builder(
        &block,
        |addr| {
            pool_store
                .get_last(format!("Pool:0x{}", hex::encode(addr)))
                .is_some() ||
                addr.eq(factory_address.as_slice())
        },
        &mut transaction_changes,
    );

    block
        .transactions()
        .for_each(|block_tx| {
            block_tx.calls.iter().for_each(|call| {
                if call.address == quoter_address {
                    let mut contract_change =
                        InterimContractChange::new(call.address.as_slice(), true);

                    if let Some(code_change) = &call.code_changes.first() {
                        contract_change.set_code(&code_change.new_code);
                    }

                    let builder = transaction_changes
                        .entry(block_tx.index.into())
                        .or_insert_with(|| TransactionChangesBuilder::new(&(block_tx.into())));
                    builder.add_contract_changes(&contract_change);
                }
            });
        });

    transaction_changes
        .iter_mut()
        .for_each(|(_, change)| {
            // this indirection is necessary due to borrowing rules.
            let addresses = change
                .changed_contracts()
                .map(|e| e.to_vec())
                .collect::<Vec<_>>();
            addresses
                .into_iter()
                .for_each(|address| {
                    // check if the address is not a pool
                    if address != factory_address.as_slice() && address != quoter_address.as_slice()
                    {
                        let pool = pool_store
                            .get_last(format!("Pool:0x{}", hex::encode(address)))
                            .unwrap();
                        change.mark_component_as_updated(&pool.address.to_hex());
                    }
                })
        });

    Ok(BlockChanges {
        block: Some((&block).into()),
        changes: transaction_changes
            .drain()
            .sorted_unstable_by_key(|(index, _)| *index)
            .filter_map(|(_, builder)| builder.build())
            .collect::<Vec<_>>(),
    })
}
