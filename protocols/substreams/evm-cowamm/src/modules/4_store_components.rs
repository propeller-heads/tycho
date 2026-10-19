use crate::pb::cowamm::{BlockPoolChanges, CowPool};
use substreams::{
    prelude::{StoreSetIfNotExists, StoreSetIfNotExistsProto},
    store::StoreNew,
};

#[substreams::handlers::store]
pub fn store_components(changes: BlockPoolChanges, store: StoreSetIfNotExistsProto<CowPool>) {
    for tx_pc in changes
        .tx_protocol_components
        .unwrap()
        .tx_components
    {
        for pc in tx_pc.components {
            let pool_address = &pc.id;
            let pool = CowPool {
                address: hex::decode(pool_address.trim_start_matches("0x"))
                    .expect("failed to decode pool address"),
                token_a: pc.tokens[0].clone(),
                token_b: pc.tokens[1].clone(),
                lp_token: pc.tokens[2].clone(),
                weight_a: pc
                    .static_att
                    .iter()
                    .find(|attr| attr.name == "weight_a")
                    .map(|attr| attr.value.clone())
                    .expect("every cow pool should have weight_a as static attribute"),
                weight_b: pc
                    .static_att
                    .iter()
                    .find(|attr| attr.name == "weight_b")
                    .map(|attr| attr.value.clone())
                    .expect("every cow pool should have weight_b as static attribute"),
                fee: 0_u64,
                created_tx_hash: tx_pc.tx.as_ref().unwrap().hash.clone(),
            };
            store.set_if_not_exists(0, format!("Pool:{}", pool_address), &pool);
        }
    }
}
