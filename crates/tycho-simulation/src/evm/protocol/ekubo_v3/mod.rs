use decoder::extension_type;
use revm::primitives::Address;
use tycho_client::feed::synchronizer::ComponentWithState;

mod addresses;
mod attributes;
mod decoder;
mod pool;
pub mod state;

#[cfg(test)]
mod test_cases;

/// Filters out unsupported ekubo_v3 extensions.
pub fn filter_fn(component: &ComponentWithState) -> bool {
    component
        .component
        .static_attributes
        .get("extension")
        .is_some_and(|extension_bytes| {
            Address::try_from(&extension_bytes[..])
                .is_ok_and(|extension| extension_type(extension).is_some())
        })
}
