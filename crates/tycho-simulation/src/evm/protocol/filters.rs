use tracing::{debug, info};
use tycho_client::feed::synchronizer::ComponentWithState;

/// Filters out pools that DCI currently fails to find some accounts for
pub fn balancer_v2_pool_filter(component: &ComponentWithState) -> bool {
    const UNSUPPORTED_COMPONENT_IDS: [&str; 6] = [
        "0x848a5564158d84b8a8fb68ab5d004fae11619a5400000000000000000000066a",
        "0x596192bb6e41802428ac943d2f1476c1af25cc0e000000000000000000000659",
        "0x05ff47afada98a98982113758878f9a8b9fdda0a000000000000000000000645",
        "0x265b6d1a6c12873a423c177eba6dd2470f40a3b50001000000000000000003fd",
        "0x9f9d900462492d4c21e9523ca95a7cd86142f298000200000000000000000462",
        "0x42ed016f826165c2e5976fe5bc3df540c5ad0af700000000000000000000058b",
    ];

    if UNSUPPORTED_COMPONENT_IDS.contains(
        &component
            .component
            .id
            .to_lowercase()
            .as_str(),
    ) {
        debug!(
            "Filtering out Balancer pools {} that have missing Accounts after DCI update.",
            component.component.id
        );
        return false;
    }

    true
}

/// Filters out uniswap v4 pools with non-Euler hooks
pub fn uniswap_v4_euler_hook_pool_filter(component: &ComponentWithState) -> bool {
    component
        .component
        .static_attributes
        .get("hook_identifier")
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .is_some_and(|s| s == "euler_v1")
}

/// Filters out uniswap v4 pools with non-Angstrom hooks
pub fn uniswap_v4_angstrom_hook_pool_filter(component: &ComponentWithState) -> bool {
    component
        .component
        .static_attributes
        .get("hook_identifier")
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .is_some_and(|s| s == "angstrom_v1")
}

/// Filters out pools that rely on ERC4626 in Balancer V3
pub fn balancer_v3_pool_filter(component: &ComponentWithState) -> bool {
    if let Some(erc4626) = component
        .component
        .static_attributes
        .get("erc4626")
    {
        if erc4626.to_vec() == [1u8] {
            info!(
                "Filtering out Balancer V3 pool {} because it uses ERC4626",
                component.component.id
            );
            return false;
        }
    }
    true
}

pub fn fluid_v1_paused_pools_filter(component: &ComponentWithState) -> bool {
    const PAUSED_POOLS: [&str; 5] = [
        // The components below are properly paused by substreams but the way indexer
        // handles tracing atm wrongly paused all components due to tracing failure. The
        // failure is unrelated to any issues with the protocol itself.
        "0x97479d9c09c7fd333bbfd07e93d4c8a669698ebc",
        "0xd0810e5cf08dcde266ecebef40cad806c7768d72",
        "0xf507a38aaf37339cc3beac4c7a58b17401bdf6bc",
        // The substreams did not detect this component as paused. It still reports
        // a high tvl value.
        "0x2886a01a0645390872a9eb99dae1283664b0c524",
        "0x276084527b801e00db8e4410504f9baf93f72c67",
    ];

    if PAUSED_POOLS.contains(
        &component
            .component
            .id
            .to_lowercase()
            .as_str(),
    ) {
        return false;
    }
    true
}

pub fn erc4626_filter(component: &ComponentWithState) -> bool {
    const UNSUPPORTED_POOLS: [&str; 4] = [
        "0x28B3a8fb53B741A8Fd78c0fb9A6B2393d896a43d",
        "0xe2e7a17dff93280dec073c995595155283e3c372",
        "0xfE6eb3b609a7C8352A241f7F3A21CEA4e9209B8f",
        "0x83f20f44975d03b1b09e64809b757c47f942beea",
    ];
    if UNSUPPORTED_POOLS.contains(
        &component
            .component
            .id
            .to_lowercase()
            .as_str(),
    ) {
        return false;
    }
    true
}
