use substreams::store::{StoreNew, StoreSet, StoreSetRaw};
use tycho_substreams::prelude as tycho;

/// The latest value of every attribute `map_state_writes` emits, keyed by attribute name and
/// ordered by transaction index, so a later module can read it as of any transaction.
#[substreams::handlers::store]
pub fn store_state(writes: tycho::BlockChanges, store: StoreSetRaw) {
    for tx_changes in writes.changes {
        let Some(tx) = tx_changes.tx else { continue };
        for change in tx_changes.entity_changes {
            for attribute in change.attributes {
                store.set(tx.index, attribute.name, &attribute.value);
            }
        }
    }
}
