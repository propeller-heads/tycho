pub mod aerodrome_slipstreams;
pub mod aerodrome_v1;
mod clmm;
pub mod cowamm;
mod cpmm;
pub mod ekubo;
pub mod ekubo_v3;
pub mod erc4626;
pub mod etherfi;
pub mod filters;
pub mod fluid;
pub mod native_wrapper;
pub mod pancakeswap_v2;
pub mod rocketpool;
pub mod safe_math;
pub mod u256_num;
pub mod uniswap_v2;
pub mod uniswap_v3;
pub mod uniswap_v4;
pub mod utils;
pub mod velodrome_slipstreams;
pub mod vm;

#[cfg(test)]
mod test_utils {
    use std::collections::HashMap;

    use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};

    use crate::protocol::models::TryFromWithBlock;

    pub(super) async fn try_decode_snapshot_with_defaults<
        T: TryFromWithBlock<ComponentWithState, BlockHeader>,
    >(
        snapshot: ComponentWithState,
    ) -> Result<T, T::Error> {
        T::try_from_with_header(
            snapshot,
            Default::default(),
            &HashMap::default(),
            &HashMap::default(),
            &Default::default(),
        )
        .await
    }
}
