use std::collections::HashMap;

use tracing::{debug, info};
use tycho_client::feed::synchronizer::ComponentWithState;
use tycho_common::Bytes;

pub use crate::evm::protocol::ekubo_v3::{
    filter_fn as ekubo_v3_extension_filter,
    filter_fn_with_signed_exclusive_swap as ekubo_v3_extension_filter_with_signed_exclusive_swap,
};

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

/// Filters out uniswap v4 pools with Angstrom hooks.
///
/// Encoding an Angstrom swap requires per-block attestations fetched from the Angstrom API,
/// authenticated with `ANGSTROM_API_KEY`. Without the key the swap fails at encoding time —
/// after route selection — so consumers without the key should exclude these pools up front.
/// [`ProtocolStreamBuilder`](crate::evm::stream::ProtocolStreamBuilder) applies this filter to
/// `uniswap_v4_hooks` automatically when no filter function is provided and the key is unset.
pub fn uniswap_v4_non_angstrom_hook_pool_filter(component: &ComponentWithState) -> bool {
    !uniswap_v4_angstrom_hook_pool_filter(component)
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

/// Filters `vm:curve` components to those the hybrid `CurveState` can quote correctly.
///
/// Excludes pools with rate-bearing or rebasing coins. Such a coin prices through a per-block rate
/// (an oracle, an ERC4626 vault, or an in-place rebase exposed via `stored_rates`) whose source is
/// an external contract that is not part of the indexed pool state. The hybrid reads that rate via
/// VM getters against the locally indexed storage, so it gets a stale or default value and the
/// quote drifts from the on-chain swap (observed up to ~30% on oracle-rate NG pools). Supporting
/// these requires DCI on the rate source in the substreams.
///
/// Detection uses the substreams' static markers:
/// - `rebase_tokens` — non-empty list of rebasing coins (e.g. stETH, ETHx).
/// - `asset_types` — per-coin Curve NG asset type encoded as a hex int; any non-zero entry (oracle
///   / rebasing / ERC4626) is unsupported. Standard coins encode as `"0x"` / `"0x00"`.
pub fn curve_filter(component: &ComponentWithState) -> bool {
    let attrs = &component.component.static_attributes;
    if attr_json_list_non_empty(attrs, "rebase_tokens") || asset_types_has_non_standard(attrs) {
        debug!(
            "Filtering out curve pool {} with rate-bearing/rebasing coins (unsupported by hybrid)",
            component.component.id
        );
        return false;
    }
    true
}

/// Filters out Killed LiquidityParty pools
///
/// The Substreams module sets a `killed` dynamic attribute (`0x01`) and this
/// filter removes such components from the stream.
/// Attempting a swap on a killed pool will revert.
pub fn liquidityparty_killed_pools_filter(component: &ComponentWithState) -> bool {
    if let Some(killed) = component.state.attributes.get("killed") {
        if killed.to_vec() == [1u8] {
            debug!(
                "Filtering out LiquidityParty pool {} because it is killed",
                component.component.id
            );
            return false;
        }
    }
    true
}

/// Parse a static attribute whose value is the UTF-8 bytes of a JSON array of strings.
fn attr_json_list(attrs: &HashMap<String, Bytes>, key: &str) -> Option<Vec<String>> {
    let text = std::str::from_utf8(attrs.get(key)?.as_ref()).ok()?;
    serde_json::from_str::<Vec<String>>(text).ok()
}

/// True when `key` is a present, non-empty JSON list (the substreams only emits `rebase_tokens`
/// when at least one coin rebases).
fn attr_json_list_non_empty(attrs: &HashMap<String, Bytes>, key: &str) -> bool {
    attr_json_list(attrs, key).is_some_and(|list| !list.is_empty())
}

/// True when `asset_types` contains any non-standard coin. Each entry is a hex-encoded integer
/// (`"0x"`/`"0x00"` = standard; `"0x01"` oracle, `"0x02"` rebasing, `"0x03"` ERC4626).
fn asset_types_has_non_standard(attrs: &HashMap<String, Bytes>) -> bool {
    attr_json_list(attrs, "asset_types").is_some_and(|types| {
        types.iter().any(|entry| {
            let digits = entry.trim_start_matches("0x");
            digits.chars().any(|c| c != '0')
        })
    })
}

/// Drops Pendle SY components with no quotable conversion in either direction.
///
/// An SY the indexer could not classify contributes no wrap edges at all, so it would decode and
/// then fail on every call. Whether a token is quotable is fixed at component creation, which is
/// what makes it a filter rather than a runtime check.
///
/// Expired markets are deliberately **not** filtered here. Expiry is state, not configuration: it
/// depends on the block being quoted, and a filter only sees the component. Testing it against
/// wall-clock time would drop live markets during a historical replay and keep dead ones in a
/// snapshot of the past. `PendleState` handles it instead — every edge errors and `get_limits`
/// reports zero depth once `block_timestamp >= expiry`.
pub fn pendle_filter(component: &ComponentWithState) -> bool {
    if component.component.protocol_type_name != "pendle_sy" {
        return true;
    }
    component
        .component
        .static_attributes
        .keys()
        .any(|name| name.starts_with("token_in_class_") || name.starts_with("token_out_class_"))
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

#[cfg(test)]
mod tests {
    use tycho_common::models::protocol::{ProtocolComponent, ProtocolComponentState};

    use super::*;

    fn attrs(pairs: &[(&str, &str)]) -> HashMap<String, Bytes> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Bytes::from(v.as_bytes().to_vec())))
            .collect()
    }

    fn hooks_component(hook_identifier: Option<&str>) -> ComponentWithState {
        let static_attributes = match hook_identifier {
            Some(id) => attrs(&[("hook_identifier", id)]),
            None => HashMap::new(),
        };
        ComponentWithState {
            state: ProtocolComponentState::new("test_pool", HashMap::new(), HashMap::new()),
            component: ProtocolComponent { static_attributes, ..Default::default() },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    #[test]
    fn non_angstrom_filter_excludes_angstrom_pools() {
        assert!(!uniswap_v4_non_angstrom_hook_pool_filter(&hooks_component(Some("angstrom_v1"))));
        assert!(uniswap_v4_non_angstrom_hook_pool_filter(&hooks_component(Some("euler_v1"))));
        assert!(uniswap_v4_non_angstrom_hook_pool_filter(&hooks_component(None)));
    }

    #[test]
    fn curve_keeps_standard_pool() {
        // 3pool-style: no rate markers.
        assert!(!asset_types_has_non_standard(&attrs(&[])));
        assert!(!attr_json_list_non_empty(&attrs(&[]), "rebase_tokens"));
        // An all-standard NG pool emits asset_types but every entry is zero.
        assert!(!asset_types_has_non_standard(&attrs(&[("asset_types", r#"["0x00","0x00"]"#)])));
    }

    #[test]
    fn curve_excludes_oracle_asset_type() {
        // apyUSD/apxUSD: coin 0 is an oracle rate token.
        assert!(asset_types_has_non_standard(&attrs(&[("asset_types", r#"["0x01","0x00"]"#)])));
        // ERC4626 (0x03) and rebasing (0x02) asset types are also non-standard.
        assert!(asset_types_has_non_standard(&attrs(&[("asset_types", r#"["0x00","0x03"]"#)])));
    }

    #[test]
    fn curve_excludes_rebase_tokens() {
        // ETH/ETHx and legacy stETH expose a non-empty rebase_tokens list.
        let m = attrs(&[("rebase_tokens", r#"["0xa35b1b31ce002fbf2058d22f30f95d405200a15b"]"#)]);
        assert!(attr_json_list_non_empty(&m, "rebase_tokens"));
    }

    #[test]
    fn curve_zero_encoded_as_empty_hex_is_standard() {
        // BigInt 0 serializes to "0x" (empty bytes); must count as standard.
        assert!(!asset_types_has_non_standard(&attrs(&[("asset_types", r#"["0x","0x"]"#)])));
    }
}
