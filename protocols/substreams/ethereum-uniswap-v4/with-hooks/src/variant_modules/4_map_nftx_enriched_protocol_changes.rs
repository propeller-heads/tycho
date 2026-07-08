use tycho_substreams::prelude::*;

/// Enrich the base Uniswap V4 hooks protocol changes with the NFTX hook
/// identifier.
///
/// Every pool whose `hooks` static attribute matches one of the configured NFTX
/// hook addresses is tagged with `hook_identifier = "nftx_v1"`, which the indexer
/// hooks DCI and tycho-simulation use to route the component (mirrors the
/// `euler_v1` / `angstrom_v1` enrichment).
///
/// `params` is a comma-separated list of the NFTX hook addresses (hex, with or
/// without a `0x` prefix).
#[substreams::handlers::map]
pub fn map_nftx_enriched_protocol_changes(
    params: String,
    protocol_changes: BlockChanges,
) -> Result<BlockChanges, substreams::errors::Error> {
    let nftx_hooks = parse_hook_addresses(&params);
    Ok(tag_nftx_components(protocol_changes, &nftx_hooks))
}

fn parse_hook_addresses(params: &str) -> Vec<Vec<u8>> {
    params
        .split(',')
        .map(str::trim)
        .filter(|hook| !hook.is_empty())
        .map(|hook| hex::decode(hook.trim_start_matches("0x")).expect("invalid NFTX hook address"))
        .collect()
}

fn tag_nftx_components(mut changes: BlockChanges, nftx_hooks: &[Vec<u8>]) -> BlockChanges {
    for tx_changes in &mut changes.changes {
        for component in &mut tx_changes.component_changes {
            if component.change != i32::from(ChangeType::Creation) {
                continue;
            }
            // Clone the hooks value first so the immutable borrow ends before we
            // push the new attribute.
            let hook = component
                .static_att
                .iter()
                .find(|attr| attr.name == "hooks")
                .map(|attr| attr.value.clone());
            if let Some(hook) = hook {
                if nftx_hooks
                    .iter()
                    .any(|nftx| nftx == &hook)
                {
                    component.static_att.push(Attribute {
                        name: "hook_identifier".to_string(),
                        value: b"nftx_v1".to_vec(),
                        change: ChangeType::Creation.into(),
                    });
                }
            }
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    const NFTX_HOOK: &str = "d2094b5cdb1a12b6274e4a4d3a252cd94c51efcc";
    const OTHER_HOOK: &str = "0000000aa232009084bd71a5797d089aa4edfad4";

    fn component_with_hook(hook_hex: &str) -> ProtocolComponent {
        ProtocolComponent {
            id: "0xpool".to_string(),
            change: i32::from(ChangeType::Creation),
            static_att: vec![Attribute {
                name: "hooks".to_string(),
                value: hex::decode(hook_hex).unwrap(),
                change: ChangeType::Creation.into(),
            }],
            ..Default::default()
        }
    }

    fn block_with(components: Vec<ProtocolComponent>) -> BlockChanges {
        BlockChanges {
            block: None,
            changes: vec![TransactionChanges {
                component_changes: components,
                ..Default::default()
            }],
            storage_changes: vec![],
        }
    }

    fn hook_identifier(component: &ProtocolComponent) -> Option<Vec<u8>> {
        component
            .static_att
            .iter()
            .find(|a| a.name == "hook_identifier")
            .map(|a| a.value.clone())
    }

    #[test]
    fn tags_only_nftx_hooks() {
        let block =
            block_with(vec![component_with_hook(NFTX_HOOK), component_with_hook(OTHER_HOOK)]);
        let filter = parse_hook_addresses(NFTX_HOOK);

        let out = tag_nftx_components(block, &filter);
        let comps = &out.changes[0].component_changes;

        assert_eq!(hook_identifier(&comps[0]), Some(b"nftx_v1".to_vec()));
        assert_eq!(hook_identifier(&comps[1]), None, "non-NFTX hook must not be tagged");
    }
}
