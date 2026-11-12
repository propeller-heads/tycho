//! Embeds every package's integration test yaml, so the binary reads the packages' strict-mode
//! declarations without a checkout of the repository.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let root = manifest_dir.join("../../protocols/substreams");
    println!("cargo:rerun-if-changed={}", root.display());

    let mut yamls = Vec::new();
    if root.is_dir() {
        collect(&root, &mut yamls);
    } else {
        println!(
            "cargo:warning=no package yamls embedded: {} is missing; the binary refuses to start \
             without them",
            root.display()
        );
    }
    yamls.sort();

    let mut source = String::from("/// Every package's integration test yaml: its path under `protocols/substreams`, and its text.\npub static PACKAGE_YAMLS: &[(&str, &str)] = &[\n");
    for path in &yamls {
        let relative = path
            .strip_prefix(&root)
            .expect("found under the root")
            .display()
            .to_string();
        let absolute = fs::canonicalize(path).expect("the yaml exists");
        source.push_str(&format!(
            "    ({relative:?}, include_str!({:?})),\n",
            absolute.display().to_string()
        ));
    }
    source.push_str("];\n");
    let out = PathBuf::from(env::var("OUT_DIR").expect("set by cargo")).join("package_yamls.rs");
    fs::write(out, source).expect("OUT_DIR is writable");
}

/// The yamls `tycho_test::expectations::test_yamls(root, false)` finds: every
/// `integration_test*.tycho.yaml` outside build output and package templates.
fn collect(dir: &Path, yamls: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("a directory entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if path.is_dir() {
            if name != "target" && !name.contains("-template-") {
                collect(&path, yamls);
            }
        } else if name.starts_with("integration_test") && name.ends_with(".tycho.yaml") {
            yamls.push(path);
        }
    }
}
