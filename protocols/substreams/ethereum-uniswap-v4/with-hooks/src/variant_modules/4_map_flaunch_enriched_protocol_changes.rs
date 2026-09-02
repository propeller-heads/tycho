use tycho_substreams::prelude::*;

/// Enrich the base Uniswap V4 hooks protocol changes with the Flaunch hook
/// identifier.
///
/// Every pool whose `hooks` static attribute matches one of the configured
/// Flaunch hook addresses is tagged with `hook_identifier = "flaunch_v1"`, which
/// the indexer hooks DCI and tycho-simulation use to route the component to the
/// correct handler (mirrors the `euler_v1` / `angstrom_v1` enrichment).
///
/// `params` is a comma-separated list of the Flaunch hook addresses (hex, with or
/// without a `0x` prefix) — kept in the manifest so a single WASM target serves
/// every network and every deployed Flaunch hook version.
#[substreams::handlers::map]
pub fn map_flaunch_enriched_protocol_changes(
    params: String,
    protocol_changes: BlockChanges,
) -> Result<BlockChanges, substreams::errors::Error> {
    let flaunch_hooks = parse_hook_addresses(&params);
    Ok(tag_flaunch_components(protocol_changes, &flaunch_hooks))
}

fn parse_hook_addresses(params: &str) -> Vec<Vec<u8>> {
    params
        .split(',')
        .map(str::trim)
        .filter(|hook| !hook.is_empty())
        .map(|hook| {
            hex::decode(hook.trim_start_matches("0x")).expect("invalid Flaunch hook address")
        })
        .collect()
}

fn tag_flaunch_components(mut changes: BlockChanges, flaunch_hooks: &[Vec<u8>]) -> BlockChanges {
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
                if flaunch_hooks
                    .iter()
                    .any(|flaunch| flaunch == &hook)
                {
                    component.static_att.push(Attribute {
                        name: "hook_identifier".to_string(),
                        value: b"flaunch_v1".to_vec(),
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

    const FLAUNCH_V1_3: &str = "23321f11a6d44fd1ab790044fdfde5758c902fdc";
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
    fn tags_only_flaunch_hooks() {
        let block =
            block_with(vec![component_with_hook(FLAUNCH_V1_3), component_with_hook(OTHER_HOOK)]);
        let filter = parse_hook_addresses(FLAUNCH_V1_3);

        let out = tag_flaunch_components(block, &filter);
        let comps = &out.changes[0].component_changes;

        assert_eq!(hook_identifier(&comps[0]), Some(b"flaunch_v1".to_vec()));
        assert_eq!(hook_identifier(&comps[1]), None, "non-Flaunch hook must not be tagged");
    }

    #[test]
    fn parses_multiple_hooks_with_and_without_prefix() {
        let hooks = parse_hook_addresses(" 0x23321f11a6d44fd1ab790044fdfde5758c902fdc, 8dc3b85e1dc1c846ebf3971179a751896842e5dc ");
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0], hex::decode(FLAUNCH_V1_3).unwrap());
    }
}
