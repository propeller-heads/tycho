use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use miette::{ensure, miette, IntoDiagnostic, WrapErr};
use serde::Deserialize;
use tracing::{debug, info};

/// Compile the release WASM binary for the Substreams package in `package_dir`. Does nothing when
/// `prebuilt` holds, since the binary the manifest points at then already exists.
fn build_wasm(package_dir: &Path, prebuilt: bool) -> miette::Result<()> {
    if prebuilt {
        info!("Expecting a pre-built WASM binary in {}", package_dir.display());
        return Ok(());
    }

    info!("Building WASM binary in {}", package_dir.display());
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
        .current_dir(package_dir)
        // RUSTUP_TOOLCHAIN outranks a rust-toolchain.toml, so the value rustup exported for this
        // crate would build the package with its nightly toolchain, which carries no wasm32
        // target, instead of the version the Substreams workspace pins.
        .env_remove("RUSTUP_TOOLCHAIN")
        // The manifest looks for the binary under the package's own target directory, so a shared
        // one exported by a developer or a CI runner would hide it from `substreams pack`.
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .status()
        .into_diagnostic()
        .wrap_err("Failed to run cargo build for the Substreams package")?;

    if !status.success() {
        return Err(miette!(
            "cargo build failed for the Substreams package in {}",
            package_dir.display()
        ));
    }

    Ok(())
}

/// Build a Substreams package, returning the path of the packed spkg.
///
/// `initial_block` forces every module to start at that block; pass `None` to pack the manifest
/// as it is, leaving each module's declared `initialBlock` intact. `prebuilt_wasm` skips
/// compiling the package's WASM binary, for callers that already hold one.
pub fn build_spkg(
    yaml_file_path: &PathBuf,
    initial_block: Option<u64>,
    prebuilt_wasm: bool,
) -> miette::Result<String> {
    info!("Building spkg from {:?}", yaml_file_path);

    let content = fs::read_to_string(yaml_file_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to read {}", yaml_file_path.display()))?;
    let mut data: serde_yaml::Value = serde_yaml::from_str(&content)
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to parse {}", yaml_file_path.display()))?;

    let parent_dir = yaml_file_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    // Mirrors the `{spkgDefaultName}` the CLI would pick, so that packing to an explicit path
    // still yields the conventional file name.
    let package_field = |field: &str| -> miette::Result<&str> {
        data.get("package")
            .and_then(|package| package.get(field))
            .and_then(|value| value.as_str())
            .ok_or_else(|| miette!("`package.{field}` not found in {}", yaml_file_path.display()))
    };
    let spkg_file_name =
        format!("{}-{}.spkg", package_field("name")?.replace('_', "-"), package_field("version")?);
    let spkg_name = parent_dir
        .join(&spkg_file_name)
        .to_string_lossy()
        .to_string();

    // `substreams pack` only reads the WASM the manifest points at, so compile it first.
    build_wasm(parent_dir, prebuilt_wasm)?;

    ensure!(
        Command::new("substreams")
            .arg("--version")
            .output()
            .is_ok(),
        "Substreams CLI is not installed or not found in PATH"
    );

    // The manifest is piped in as `-` so the checked-in file is never rewritten. Its relative
    // paths (the wasm binary, the proto import paths) resolve against the working directory rather
    // than the manifest, so pack from the directory holding it.
    let mut child = Command::new("substreams")
        .args(["pack", "-", "--output-file", &spkg_file_name])
        .current_dir(parent_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .into_diagnostic()
        .wrap_err("Failed to spawn the substreams pack command")?;

    let piped = {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| miette!("Failed to open the stdin of the substreams pack command"))?;

        // Pulling every module to the same block is only correct when the caller asked for a
        // specific start block: modules that legitimately start later would otherwise be
        // forced to start earlier. Without an override the manifest goes over the pipe
        // verbatim.
        //
        // A rejected manifest makes pack exit before the write finishes, and its stderr is the
        // useful diagnostic, so a broken pipe here is reported only if pack itself
        // succeeded.
        match initial_block {
            Some(initial_block) => {
                modify_initial_block(&mut data, initial_block);
                serde_yaml::to_writer(&mut stdin, &data).into_diagnostic()
            }
            None => stdin
                .write_all(content.as_bytes())
                .into_diagnostic(),
        }
    };

    let output = child
        .wait_with_output()
        .into_diagnostic()?;

    ensure!(
        output.status.success(),
        "Substreams pack command failed. Ensure that the wasm target was built.\n{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    piped.wrap_err("Failed to pipe the manifest into the substreams pack command")?;

    debug!("Spkg built successfully: {}", spkg_name);

    Ok(spkg_name)
}

/// Extract the lowest start block of any module of a Substreams manifest.
///
/// A module's start block comes from `networks.<network>.initialBlock` when that network declares
/// one for it — those entries take precedence over an inline `initialBlock` — and from the module's
/// own declaration otherwise. The network is the one the manifest selects with `network:`, or the
/// only one it defines.
///
/// The manifest is parsed as YAML, so anchors and aliases are resolved: both
/// `initialBlock: &initial_block 123` and `initialBlock: *initial_block` yield `123`.
/// Fails if the manifest cannot be parsed or if no module has a start block.
pub fn extract_initial_block(yaml: &str) -> miette::Result<u64> {
    #[derive(Deserialize)]
    struct Manifest {
        modules: Vec<Module>,
        network: Option<String>,
        #[serde(default)]
        networks: HashMap<String, Network>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Module {
        name: String,
        initial_block: Option<u64>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Network {
        #[serde(default)]
        initial_block: HashMap<String, u64>,
    }

    let manifest: Manifest = serde_yaml::from_str(yaml)
        .into_diagnostic()
        .wrap_err("Failed to parse Substreams manifest")?;

    let network = manifest
        .network
        .as_ref()
        .and_then(|name| manifest.networks.get(name))
        .or_else(|| match manifest.networks.len() {
            1 => manifest.networks.values().next(),
            _ => None,
        });

    manifest
        .modules
        .iter()
        .filter_map(|module| {
            network
                .and_then(|network| {
                    network
                        .initial_block
                        .get(&module.name)
                        .copied()
                })
                .or(module.initial_block)
        })
        .min()
        .ok_or_else(|| {
            miette!(
                "No module has an `initialBlock`, inline or under `networks`. Please specify one \
                 explicitly with --initial-block."
            )
        })
}

/// Update the initial block of every module that declares one in a parsed Substreams manifest.
///
/// A module's start block can be declared on the module itself or per module under
/// `networks.<network>.initialBlock`; both are rewritten. Every network is rewritten rather than
/// only the one named by `network:`, because the chain under test comes from the command line
/// rather than from the manifest.
///
/// Modules leaving `initialBlock` implicit keep it that way: Substreams derives theirs from their
/// inputs, which land on `start_block` anyway.
pub fn modify_initial_block(manifest: &mut serde_yaml::Value, start_block: u64) {
    if let Some(modules) = manifest
        .get_mut("modules")
        .and_then(|modules| modules.as_sequence_mut())
    {
        for module in modules {
            if let Some(initial_block) = module.get_mut("initialBlock") {
                *initial_block = start_block.into();
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
                *initial_block = start_block.into();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_MANIFEST: &str = include_str!("assets/substreams_example.yaml");
    const ANCHORED_MANIFEST: &str = include_str!("assets/substreams_example_anchors.yaml");

    #[test]
    fn test_modify_initial_block_normal_case() {
        let mut manifest: serde_yaml::Value =
            serde_yaml::from_str(EXAMPLE_MANIFEST).expect("Failed to parse YAML");
        let new_block = 12345;

        modify_initial_block(&mut manifest, new_block);

        let modules = manifest["modules"]
            .as_sequence()
            .expect("modules not found or has wrong type");
        assert!(!modules.is_empty());
        for module in modules {
            assert_eq!(module["initialBlock"].as_u64(), Some(new_block));
        }
    }

    #[test]
    fn test_modify_initial_block_leaves_implicit_modules_alone() {
        let mut manifest: serde_yaml::Value = serde_yaml::from_str(
            r"
modules:
  - name: map_a
    initialBlock: 100
  - name: store_b
",
        )
        .expect("Failed to parse YAML");

        modify_initial_block(&mut manifest, 12345);

        let modules = manifest["modules"]
            .as_sequence()
            .expect("modules not found or has wrong type");
        assert_eq!(modules[0]["initialBlock"].as_u64(), Some(12345));
        assert!(
            modules[1].get("initialBlock").is_none(),
            "store_b gained an initialBlock it did not declare"
        );
    }

    // Shape used by ethereum-fluid, ethereum-ekubo-v2/v3 and ethereum-template-singleton: no module
    // declares a block inline, so an override that only looked at the modules did nothing.
    #[test]
    fn test_modify_initial_block_rewrites_network_blocks() {
        let mut manifest: serde_yaml::Value = serde_yaml::from_str(
            r"
network: mainnet
networks:
  mainnet:
    initialBlock:
      map_dex_deployed: 19239106
    params:
      map_dex_deployed: liquidity_contract=0x52aa
modules:
  - name: map_dex_deployed
    kind: map
  - name: store_dexes
    kind: store
",
        )
        .expect("Failed to parse YAML");

        modify_initial_block(&mut manifest, 21609290);

        assert_eq!(
            manifest["networks"]["mainnet"]["initialBlock"]["map_dex_deployed"].as_u64(),
            Some(21609290)
        );
        assert_eq!(
            manifest["networks"]["mainnet"]["params"]["map_dex_deployed"].as_str(),
            Some("liquidity_contract=0x52aa"),
            "params must not be touched"
        );
        let modules = manifest["modules"]
            .as_sequence()
            .expect("modules not found or has wrong type");
        for module in modules {
            assert!(
                module.get("initialBlock").is_none(),
                "modules must not gain an initialBlock they did not declare"
            );
        }
    }

    #[test]
    fn test_extract_initial_block() {
        let block =
            extract_initial_block(EXAMPLE_MANIFEST).expect("Failed to extract initialBlock");

        assert_eq!(block, 1000000);
    }

    #[test]
    fn test_extract_initial_block_returns_lowest_across_modules() {
        let manifest = r"
modules:
  - name: map_a
    initialBlock: 3000000
  - name: store_b
  - name: map_c
    initialBlock: 1500000
  - name: map_d
    initialBlock: 2000000
";

        let block = extract_initial_block(manifest).expect("Failed to extract initialBlock");

        assert_eq!(block, 1500000);
    }

    #[test]
    fn test_extract_initial_block_with_anchors() {
        let block =
            extract_initial_block(ANCHORED_MANIFEST).expect("Failed to extract initialBlock");

        assert_eq!(block, 1000000);
    }

    // The anchor sits under `defaults` rather than on a module so that the alias is the only path
    // to 1000000. Anchoring on a module's own `initialBlock`, as real manifests do, would let the
    // anchor site supply that value directly and the test would pass even if aliases were skipped.
    #[test]
    fn test_extract_initial_block_resolves_aliases() {
        let manifest = r"
defaults:
  initialBlock: &initial_block 1000000
modules:
  - name: map_a
    initialBlock: 2000000
  - name: map_b
    initialBlock: *initial_block
";

        let block = extract_initial_block(manifest).expect("Failed to extract initialBlock");

        assert_eq!(block, 1000000);
    }

    #[test]
    fn test_extract_initial_block_from_network_blocks() {
        let manifest = r"
network: mainnet
networks:
  mainnet:
    initialBlock:
      map_dex_deployed: 19239106
modules:
  - name: map_dex_deployed
    kind: map
  - name: store_dexes
    kind: store
";

        let block = extract_initial_block(manifest).expect("Failed to extract initialBlock");

        assert_eq!(block, 19239106);
    }

    // Packing a manifest that declares both shows the network entry winning, so the lowest start
    // block here is map_b's inline 300 rather than map_a's shadowed 100.
    #[test]
    fn test_extract_initial_block_prefers_network_over_inline() {
        let manifest = r"
network: mainnet
networks:
  mainnet:
    initialBlock:
      map_a: 500
modules:
  - name: map_a
    initialBlock: 100
  - name: map_b
    initialBlock: 300
";

        let block = extract_initial_block(manifest).expect("Failed to extract initialBlock");

        assert_eq!(block, 300);
    }

    #[test]
    fn test_extract_initial_block_ignores_unselected_networks() {
        let manifest = r"
network: mainnet
networks:
  mainnet:
    initialBlock:
      map_a: 5000
  base:
    initialBlock:
      map_a: 10
modules:
  - name: map_a
    kind: map
";

        let block = extract_initial_block(manifest).expect("Failed to extract initialBlock");

        assert_eq!(block, 5000);
    }

    #[test]
    fn test_extract_initial_block_missing() {
        let manifest = r"
modules:
  - name: map_protocol_changes
    kind: map
";

        let err = extract_initial_block(manifest).expect_err("Expected missing initialBlock error");

        assert!(err
            .to_string()
            .contains("has an `initialBlock`"));
    }
}
