//! Registry mapping protocol names to their canonical decoder state types and
//! default component filters.
//!
//! This is the single source of truth consumed by
//! [`ProtocolStreamBuilder::exchange_by_name`](crate::evm::stream::ProtocolStreamBuilder::exchange_by_name).
//! Adding stream support for a new protocol means adding one row to the `PROTOCOLS` table.
use tycho_client::feed::{
    component_tracker::ComponentFilter, synchronizer::ComponentWithState, BlockHeader,
};
use tycho_common::simulation::protocol_sim::ProtocolSim;

use crate::{
    evm::{
        engine_db::tycho_db::PreCachedDB,
        protocol::{
            aerodrome_slipstreams::state::AerodromeSlipstreamsState,
            aerodrome_v1::state::AerodromeV1State,
            cowamm::state::CowAMMState,
            curve::CurveState,
            ekubo::state::EkuboState,
            ekubo_v3::{self, state::EkuboV3State},
            erc4626::state::ERC4626State,
            etherfi::state::EtherfiState,
            filters::{
                balancer_v2_pool_filter, balancer_v3_pool_filter, curve_filter, erc4626_filter,
                fluid_v1_paused_pools_filter,
            },
            fluid::FluidV1,
            lunarbase::LunarBaseState,
            pancakeswap_v2::state::PancakeswapV2State,
            rocketpool::state::RocketpoolState,
            uniswap_v2::state::UniswapV2State,
            uniswap_v3::state::UniswapV3State,
            uniswap_v4::state::UniswapV4State,
            velodrome_slipstreams::state::VelodromeSlipstreamsState,
            vm::state::EVMPoolState,
        },
        stream::ProtocolStreamBuilder,
    },
    protocol::{errors::InvalidSnapshotError, models::TryFromWithBlock},
};

/// Client-side component filter, matching the signature accepted by
/// [`ProtocolStreamBuilder::exchange`](crate::evm::stream::ProtocolStreamBuilder::exchange).
pub type ComponentFilterFn = fn(&ComponentWithState) -> bool;

/// Monomorphized registration hook: applies `builder.exchange::<T>(...)` for one
/// concrete state type `T`.
type RegisterFn = fn(
    ProtocolStreamBuilder,
    &str,
    ComponentFilter,
    Option<ComponentFilterFn>,
) -> ProtocolStreamBuilder;

/// How one protocol is registered: which state type decodes it (via `register`)
/// and which client-side filter it needs by default.
pub(crate) struct ProtocolEntry {
    pub(crate) register: RegisterFn,
    pub(crate) default_filter: Option<ComponentFilterFn>,
}

fn entry_for<T>(default_filter: Option<ComponentFilterFn>) -> ProtocolEntry
where
    T: ProtocolSim
        + TryFromWithBlock<ComponentWithState, BlockHeader, Error = InvalidSnapshotError>
        + Send
        + 'static,
{
    ProtocolEntry { register: ProtocolStreamBuilder::exchange::<T>, default_filter }
}

/// One row of [`PROTOCOLS`]: a protocol name paired with a thunk that builds its
/// [`ProtocolEntry`] on demand.
type ProtocolRow = (&'static str, fn() -> ProtocolEntry);

/// One row per supported protocol name. `entry()` and `supported_protocols()` both
/// derive from this table, so the two cannot drift apart.
///
/// `vm:`-prefixed entries backed by [`EVMPoolState`] require embedded adapter bytecode in
/// `vm::constants::get_adapter_file` — an `EVMPoolState` registered without one fails at decode
/// time. `vm:curve` keeps its `vm:` name for extractor compatibility but decodes via the native
/// [`CurveState`], so it is exempt from that requirement.
static PROTOCOLS: &[ProtocolRow] = &[
    ("uniswap_v2", || entry_for::<UniswapV2State>(None)),
    ("sushiswap_v2", || entry_for::<UniswapV2State>(None)),
    ("quickswap_v2", || entry_for::<UniswapV2State>(None)),
    ("pancakeswap_v2", || entry_for::<PancakeswapV2State>(None)),
    ("uniswap_v3", || entry_for::<UniswapV3State>(None)),
    ("pancakeswap_v3", || entry_for::<UniswapV3State>(None)),
    ("uniswap_v4", || entry_for::<UniswapV4State>(None)),
    ("uniswap_v4_hooks", || entry_for::<UniswapV4State>(None)),
    ("ekubo_v2", || entry_for::<EkuboState>(None)),
    ("ekubo_v3", || entry_for::<EkuboV3State>(Some(ekubo_v3::filter_fn))),
    ("aerodrome_v1", || entry_for::<AerodromeV1State>(None)),
    ("aerodrome_slipstreams", || entry_for::<AerodromeSlipstreamsState>(None)),
    ("velodrome_slipstreams", || entry_for::<VelodromeSlipstreamsState>(None)),
    ("cowamm", || entry_for::<CowAMMState>(None)),
    ("etherfi", || entry_for::<EtherfiState>(None)),
    ("rocketpool", || entry_for::<RocketpoolState>(None)),
    ("erc4626", || entry_for::<ERC4626State>(Some(erc4626_filter))),
    ("fluid_v1", || entry_for::<FluidV1>(Some(fluid_v1_paused_pools_filter))),
    ("lunarbase", || entry_for::<LunarBaseState>(None)),
    ("vm:balancer_v2", || entry_for::<EVMPoolState<PreCachedDB>>(Some(balancer_v2_pool_filter))),
    ("vm:balancer_v3", || entry_for::<EVMPoolState<PreCachedDB>>(Some(balancer_v3_pool_filter))),
    ("vm:bopamm", || entry_for::<EVMPoolState<PreCachedDB>>(None)),
    ("vm:curve", || entry_for::<CurveState>(Some(curve_filter))),
    ("vm:fermiswap", || entry_for::<EVMPoolState<PreCachedDB>>(None)),
    ("vm:maverick_v2", || entry_for::<EVMPoolState<PreCachedDB>>(None)),
    ("vm:liquidityparty", || entry_for::<EVMPoolState<PreCachedDB>>(None)),
];

/// All protocol names registrable via
/// [`ProtocolStreamBuilder::exchange_by_name`](crate::evm::stream::ProtocolStreamBuilder::exchange_by_name).
pub fn supported_protocols() -> Vec<&'static str> {
    PROTOCOLS
        .iter()
        .map(|(name, _)| *name)
        .collect()
}

pub(crate) fn entry(name: &str) -> Option<ProtocolEntry> {
    PROTOCOLS
        .iter()
        .find(|(protocol_name, _)| *protocol_name == name)
        .map(|(_, make_entry)| make_entry())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::protocol::filters::{balancer_v2_pool_filter, curve_filter};

    #[test]
    fn test_supported_protocols_all_resolve() {
        for name in supported_protocols() {
            assert!(entry(name).is_some(), "'{name}' listed but has no registry entry");
        }
    }

    #[test]
    fn test_unknown_protocol_has_no_entry() {
        assert!(entry("definitely_not_a_protocol").is_none());
        // Typo of vm:balancer_v2 — must NOT fall into a vm: catch-all.
        assert!(entry("vm:blancer_v2").is_none());
    }

    #[test]
    fn test_entry_maps_name_to_expected_state_type() {
        for name in ["uniswap_v2", "sushiswap_v2", "quickswap_v2"] {
            let register = entry(name)
                .expect("registered")
                .register;
            assert!(
                std::ptr::fn_addr_eq(
                    register,
                    ProtocolStreamBuilder::exchange::<UniswapV2State> as RegisterFn
                ),
                "'{name}' must register UniswapV2State"
            );
        }
        let curve_register = entry("vm:curve")
            .expect("registered")
            .register;
        assert!(std::ptr::fn_addr_eq(
            curve_register,
            ProtocolStreamBuilder::exchange::<CurveState> as RegisterFn
        ));
        let bopamm_register = entry("vm:bopamm")
            .expect("registered")
            .register;
        assert!(std::ptr::fn_addr_eq(
            bopamm_register,
            ProtocolStreamBuilder::exchange::<EVMPoolState<PreCachedDB>> as RegisterFn
        ));
    }

    #[test]
    fn test_default_filters_wired() {
        let balancer_filter = entry("vm:balancer_v2")
            .expect("vm:balancer_v2 registered")
            .default_filter
            .expect("vm:balancer_v2 has a default filter");
        assert!(std::ptr::fn_addr_eq(
            balancer_filter,
            balancer_v2_pool_filter as ComponentFilterFn
        ));
        let curve_default = entry("vm:curve")
            .expect("vm:curve registered")
            .default_filter
            .expect("vm:curve has a default filter");
        assert!(std::ptr::fn_addr_eq(curve_default, curve_filter as ComponentFilterFn));
        assert!(entry("uniswap_v2")
            .expect("uniswap_v2 registered")
            .default_filter
            .is_none());
    }
}
