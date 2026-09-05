use decoder::{extension_type, ExtensionType};
use revm::primitives::Address;
use tycho_client::feed::synchronizer::ComponentWithState;

mod addresses;
mod attributes;
mod decoder;
mod pool;
pub mod state;

#[cfg(test)]
mod test_cases;

/// The extension type of `component`, or `None` if the extension attribute is missing, malformed,
/// or unsupported.
fn component_extension_type(component: &ComponentWithState) -> Option<ExtensionType> {
    let extension_bytes = component
        .component
        .static_attributes
        .get("extension")?;

    extension_type(Address::try_from(&extension_bytes[..]).ok()?, component.component.chain)
}

/// Filters out unsupported ekubo_v3 extensions, as well as SignedExclusiveSwap pools.
///
/// Swapping on a SignedExclusiveSwap pool requires a per-swap signature obtained off-chain and
/// passed to the encoder as `user_data`. Without it the swap fails at encoding time — after route
/// selection — so consumers with no signature source should exclude these pools up front. Use
/// `filter_fn_with_signed_exclusive_swap` to include them.
pub fn filter_fn(component: &ComponentWithState) -> bool {
    component_extension_type(component)
        .is_some_and(|extension| !matches!(extension, ExtensionType::SignedExclusiveSwap))
}

/// Filters out unsupported ekubo_v3 extensions, keeping SignedExclusiveSwap pools.
///
/// Only use this if you can supply the per-swap signature these pools require; see `filter_fn`.
pub fn filter_fn_with_signed_exclusive_swap(component: &ComponentWithState) -> bool {
    component_extension_type(component).is_some()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use revm::primitives::address;
    use tycho_common::{
        models::protocol::{ProtocolComponent, ProtocolComponentState},
        Bytes,
    };

    use super::{
        addresses::{ORACLE_ADDRESS, SIGNED_EXCLUSIVE_SWAP_ADDRESS},
        *,
    };

    fn component(extension: Option<Bytes>) -> ComponentWithState {
        let static_attributes = extension
            .map(|bytes| HashMap::from([("extension".to_string(), bytes)]))
            .unwrap_or_default();

        ComponentWithState {
            state: ProtocolComponentState::new("test_pool", HashMap::new(), HashMap::new()),
            component: ProtocolComponent { static_attributes, ..Default::default() },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    fn with_extension(extension: Address) -> ComponentWithState {
        component(Some(Bytes::from(extension.as_slice().to_vec())))
    }

    #[test]
    fn signed_exclusive_swap_excluded_by_default() {
        let signed = with_extension(SIGNED_EXCLUSIVE_SWAP_ADDRESS);

        assert!(!filter_fn(&signed));
        assert!(filter_fn_with_signed_exclusive_swap(&signed));
    }

    #[test]
    fn supported_extension_kept_by_both_filters() {
        // Oracle hooks beforeSwap, so it only passes via the known-address check.
        let oracle = with_extension(ORACLE_ADDRESS);

        assert!(filter_fn(&oracle));
        assert!(filter_fn_with_signed_exclusive_swap(&oracle));

        // An extension without swap call points passes regardless of its address.
        let no_call_points = with_extension(address!("0x0000000000000000000000000000000000000001"));

        assert!(filter_fn(&no_call_points));
        assert!(filter_fn_with_signed_exclusive_swap(&no_call_points));
    }

    #[test]
    fn unsupported_extension_excluded_by_both_filters() {
        // Unknown address with the beforeSwap call point set.
        let unknown = with_extension(address!("0x6000000000000000000000000000000000000001"));

        assert!(!filter_fn(&unknown));
        assert!(!filter_fn_with_signed_exclusive_swap(&unknown));
    }

    #[test]
    fn missing_or_malformed_extension_excluded_by_both_filters() {
        for case in [component(None), component(Some(Bytes::from(vec![0u8; 19])))] {
            assert!(!filter_fn(&case));
            assert!(!filter_fn_with_signed_exclusive_swap(&case));
        }
    }
}
