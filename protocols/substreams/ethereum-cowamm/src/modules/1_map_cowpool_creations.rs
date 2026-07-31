use crate::{
    abi::b_cow_factory::events::CowammPoolCreated,
    modules::utils::Params,
    pb::cowamm::{CowPoolCreation, CowPoolCreations},
};
use anyhow::{Ok, Result};
use substreams_ethereum::pb::eth::v2::Block;

//map cowpool creations from the factory COWAMMPoolCreated announcement event
#[substreams::handlers::map]
pub fn map_cowpool_creations(params: String, block: Block) -> Result<CowPoolCreations> {
    let params = Params::parse_from_query(&params)?;
    let factory_address = params
        .decode_addresses()
        .expect("unable to extract factory address");

    let cow_pool_creations = block
        .logs()
        .filter(|log| log.address() == factory_address && CowammPoolCreated::match_log(log.log))
        .map(|log| {
            let creation = CowammPoolCreated::decode(log.log)
                .expect("COWAMMPoolCreated log matched but failed to decode");
            CowPoolCreation {
                address: creation.b_co_w_pool.clone(),
                lp_token: creation.b_co_w_pool, //address of lptoken is same as the pool address
                created_tx_hash: log.receipt.transaction.hash.clone(),
                ordinal: log.ordinal(),
            }
        })
        .collect::<Vec<CowPoolCreation>>();
    Ok(CowPoolCreations { pools: cow_pool_creations })
}
