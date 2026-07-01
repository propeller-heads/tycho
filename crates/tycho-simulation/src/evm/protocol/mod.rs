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
pub mod lunarbase;
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

/// Builds the `Arc<Token>`-typed `ProtocolComponent` embedded in a `SwapQuoter` state from the
/// address-typed component carried in a snapshot, resolving each token address against `all_tokens`.
///
/// Returns an error if any of the component's tokens is missing from `all_tokens`.
pub(crate) fn build_swap_quoter_component(
    component: &tycho_common::models::protocol::ProtocolComponent,
    all_tokens: &std::collections::HashMap<tycho_common::Bytes, tycho_common::models::token::Token>,
) -> Result<
    std::sync::Arc<
        tycho_common::models::protocol::ProtocolComponent<
            std::sync::Arc<tycho_common::models::token::Token>,
        >,
    >,
    crate::protocol::errors::InvalidSnapshotError,
> {
    use std::sync::Arc;
    let tokens = component
        .tokens
        .iter()
        .map(|addr| {
            all_tokens
                .get(addr)
                .cloned()
                .map(Arc::new)
                .ok_or_else(|| {
                    crate::protocol::errors::InvalidSnapshotError::MissingAttribute(format!(
                        "token {addr}"
                    ))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Arc::new(tycho_common::models::protocol::ProtocolComponent::new(
        &component.id,
        &component.protocol_system,
        &component.protocol_type_name,
        component.chain,
        tokens,
        component.contract_addresses.clone(),
        component.static_attributes.clone(),
        component.change,
        component.creation_tx.clone(),
        component.created_at,
    )))
}

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
