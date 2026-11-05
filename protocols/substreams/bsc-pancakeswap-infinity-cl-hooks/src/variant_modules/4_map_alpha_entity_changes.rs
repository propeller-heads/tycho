use tycho_substreams::prelude::*;

/// New pools followed by start-time updates, the `created_pools` input of the CL package's
/// `map_protocol_changes`. Creations go first so that, within one transaction, a later update
/// overrides the start time a creation carries.
#[substreams::handlers::map]
pub fn map_alpha_entity_changes(
    created: BlockEntityChanges,
    updates: BlockEntityChanges,
) -> Result<BlockEntityChanges, substreams::errors::Error> {
    Ok(merge(created, updates))
}

/// Split out so tests can call it: the handler macro rewrites the signature to take raw pointers.
fn merge(created: BlockEntityChanges, updates: BlockEntityChanges) -> BlockEntityChanges {
    let mut changes = created.changes;
    changes.extend(updates.changes);
    BlockEntityChanges { block: None, changes }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(index: u64) -> TransactionEntityChanges {
        TransactionEntityChanges {
            tx: Some(Transaction { index, ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn creations_precede_updates() {
        let merged = merge(
            BlockEntityChanges { block: None, changes: vec![change(4)] },
            BlockEntityChanges { block: None, changes: vec![change(4), change(9)] },
        );
        let order: Vec<u64> = merged
            .changes
            .iter()
            .map(|c| c.tx.as_ref().unwrap().index)
            .collect();
        assert_eq!(order, vec![4, 4, 9]);
        assert!(merged.changes[0]
            .component_changes
            .is_empty());
    }
}
