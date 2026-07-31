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

#[cfg(test)]
mod tests {
    use super::*;
    use substreams_ethereum::pb::eth::v2::Log;

    // Pinned on-chain value; fails if the generated ABI constant ever drifts.
    const COWAMM_POOL_CREATED_TOPIC: &str =
        "0d03834d0d86c7f57e877af40e26f176dc31bd637535d4ba153d1ac9de88a7ea";

    #[test]
    fn matches_the_onchain_pool_created_topic() {
        let log = Log {
            topics: vec![hex::decode(COWAMM_POOL_CREATED_TOPIC).unwrap(), vec![0u8; 32]],
            ..Default::default()
        };
        assert!(CowammPoolCreated::match_log(&log));
    }
}
