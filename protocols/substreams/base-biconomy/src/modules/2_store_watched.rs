use std::collections::BTreeSet;

use substreams::store::{StoreNew, StoreSet, StoreSetString};
use substreams_ethereum::pb::eth;

use crate::{
    biconomy::{calls, hex_address, to_address, Address},
    modules::config::Config,
    writes::executor_writes,
};

/// Addresses whose token movements can change a provider's `available()`: each provider bound
/// to a board, and the vault behind it when the provider exposes `vault()` (a router-vault
/// provider pays out of its vault). Maps `watch:{address}` to the provider.
#[substreams::handlers::store]
pub fn store_watched(params: String, block: eth::v2::Block, store: StoreSetString) {
    let config = Config::parse(&params).expect("valid params");
    let providers: BTreeSet<Address> = block
        .transactions()
        .flat_map(|tx| executor_writes(tx, &config.executor).providers)
        .collect();
    for provider in providers {
        let provider_hex = hex_address(&provider);
        store.set(0, watch_key(&provider), &provider_hex);
        if let Some(vault) = vault_of(&provider) {
            store.set(0, watch_key(&vault), &provider_hex);
        }
    }
}

pub(crate) fn watch_key(address: &[u8]) -> String {
    format!("watch:{}", hex_address(address))
}

fn vault_of(provider: &Address) -> Option<Address> {
    use substreams_ethereum::pb::eth::rpc;
    let responses = substreams_ethereum::rpc::eth_call(&rpc::RpcCalls {
        calls: vec![rpc::RpcCall { to_addr: provider.to_vec(), data: calls::vault() }],
    })
    .responses;
    let response = responses.first()?;
    if response.failed || response.raw.len() != 32 {
        return None;
    }
    to_address(&response.raw).filter(|vault| *vault != [0u8; 20])
}
