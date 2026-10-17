use substreams::store::{StoreNew, StoreSet, StoreSetInt64};
use tycho_substreams::prelude::*;

use crate::{
    keys::sy_rate_needed_key,
    sy::{is_class_attribute, TokenClass},
};

/// Records the SYs that have, or have had, a token converted at the exchange rate.
///
/// Only such an SY needs its rate read when no live market prices against it: a `one_to_one`
/// conversion is a decimal rescale and never looks at the rate. Set whenever `map_sy_state` emits
/// an `index_rate` class, at creation or on a re-classification.
///
/// Never cleared. A re-classification leaves out every token whose probe did not answer, which
/// keeps its class, so a change without an `index_rate` class does not show the SY has none. An
/// SY re-classified away from `index_rate` keeps its rate read, one read per refresh block.
#[substreams::handlers::store]
pub fn store_sy_rate_needed(sy_state: BlockChanges, store: StoreSetInt64) {
    for tx_changes in sy_state.changes {
        for change in tx_changes.entity_changes {
            if sets_an_index_rate_class(&change.attributes) {
                store.set(0, sy_rate_needed_key(&change.component_id), &1);
            }
        }
    }
}

/// Whether a change sets any token's class to `index_rate`.
fn sets_an_index_rate_class(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        is_class_attribute(&attribute.name) &&
            attribute.change == i32::from(ChangeType::Update) &&
            attribute.value ==
                TokenClass::IndexRate
                    .as_str()
                    .as_bytes()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute(name: &str, value: &str, change: ChangeType) -> Attribute {
        Attribute {
            name: name.to_string(),
            value: value.as_bytes().to_vec(),
            change: change.into(),
        }
    }

    #[test]
    fn a_rate_converted_token_needs_the_rate() {
        let attributes = [
            attribute("token_in_class_0xaa", "one_to_one", ChangeType::Update),
            attribute("token_out_class_0xbb", "index_rate", ChangeType::Update),
        ];
        assert!(sets_an_index_rate_class(&attributes));
    }

    #[test]
    fn one_to_one_tokens_alone_set_nothing() {
        let attributes = [attribute("token_in_class_0xaa", "one_to_one", ChangeType::Update)];
        assert!(!sets_an_index_rate_class(&attributes));
    }

    /// A re-profile whose `index_rate` token did not answer its probe carries only the other
    /// tokens. That token keeps its class, so the change must not read as "no rate needed".
    #[test]
    fn a_re_profile_without_the_rate_converted_token_leaves_the_flag() {
        let attributes = [
            attribute("token_in_class_0xaa", "one_to_one", ChangeType::Update),
            attribute("token_out_class_0xaa", "one_to_one", ChangeType::Update),
            attribute("token_out_class_0xcc", "", ChangeType::Deletion),
        ];
        assert!(!sets_an_index_rate_class(&attributes));
    }

    #[test]
    fn a_pause_sets_nothing() {
        assert!(!sets_an_index_rate_class(&[attribute("paused", "\u{1}", ChangeType::Update)]));
    }

    #[test]
    fn a_deleted_index_rate_class_sets_nothing() {
        let attributes = [attribute("token_out_class_0xbb", "index_rate", ChangeType::Deletion)];
        assert!(!sets_an_index_rate_class(&attributes));
    }
}
