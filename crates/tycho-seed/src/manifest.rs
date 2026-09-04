//! Rewrites a Substreams manifest so every module starts at the seed block.

use anyhow::{Context, Result};

/// Sets every explicit `initialBlock`, inline on a module or under `networks.<name>.initialBlock`,
/// to `block`. Modules without an explicit `initialBlock` inherit it from their inputs, which all
/// land on `block` as well.
pub fn set_initial_blocks(manifest: &mut serde_yaml::Value, block: u64) {
    if let Some(modules) = manifest
        .get_mut("modules")
        .and_then(|modules| modules.as_sequence_mut())
    {
        for module in modules {
            if let Some(initial_block) = module.get_mut("initialBlock") {
                *initial_block = block.into();
            }
        }
    }

    if let Some(networks) = manifest
        .get_mut("networks")
        .and_then(|networks| networks.as_mapping_mut())
    {
        for network in networks.values_mut() {
            let Some(initial_blocks) = network
                .get_mut("initialBlock")
                .and_then(|initial_blocks| initial_blocks.as_mapping_mut())
            else {
                continue;
            };

            for initial_block in initial_blocks.values_mut() {
                *initial_block = block.into();
            }
        }
    }
}

pub fn rewrite(manifest_yaml: &str, block: u64) -> Result<String> {
    let mut manifest: serde_yaml::Value =
        serde_yaml::from_str(manifest_yaml).context("parsing the manifest")?;
    set_initial_blocks(&mut manifest, block);

    serde_yaml::to_string(&manifest).context("serializing the manifest")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_network_and_inline_initial_blocks_only() {
        let manifest = "\
network: mainnet
networks:
  mainnet:
    initialBlock:
      map_events: 24134506
modules:
  - name: map_events
    kind: map
    initialBlock: 24134506
  - name: map_components
    kind: map
params:
  map_events: \"unchanged\"
";

        let rewritten = rewrite(manifest, 25000000).unwrap();
        let value: serde_yaml::Value = serde_yaml::from_str(&rewritten).unwrap();

        assert_eq!(value["networks"]["mainnet"]["initialBlock"]["map_events"], 25000000);
        assert_eq!(value["modules"][0]["initialBlock"], 25000000);
        assert!(value["modules"][1]
            .get("initialBlock")
            .is_none());
        assert_eq!(value["params"]["map_events"], "unchanged");
    }
}
