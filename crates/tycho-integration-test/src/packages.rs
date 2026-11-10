//! The strict-mode declarations of the packages tested on this chain, from the yamls embedded at
//! build time.

use std::collections::HashMap;

use miette::{ensure, Result};
use tycho_common::models::Chain;
use tycho_test::expectations::{load_packages_from, Harness, PackageExpectations};

include!(concat!(env!("OUT_DIR"), "/package_yamls.rs"));

/// The packages of one chain that opted into strict mode, by protocol system. A protocol without
/// a package here, legacy or without a yaml, is tested as before.
pub struct StrictPackages {
    by_protocol: HashMap<String, PackageExpectations>,
}

impl StrictPackages {
    /// Reads every embedded yaml and keeps the strict packages of `chain`.
    ///
    /// # Errors
    /// When no yaml was embedded, which means the build could not see `protocols/substreams`, or
    /// when a yaml is invalid, as `load_packages` reports it.
    pub fn load(chain: Chain) -> Result<Self> {
        Self::from_yamls(
            PACKAGE_YAMLS
                .iter()
                .map(|(path, text)| (path.to_string(), text.to_string())),
            chain,
        )
    }

    fn from_yamls(yamls: impl IntoIterator<Item = (String, String)>, chain: Chain) -> Result<Self> {
        let yamls: Vec<(String, String)> = yamls.into_iter().collect();
        ensure!(
            !yamls.is_empty(),
            "no package yamls were embedded at build time: the build needs \
             protocols/substreams/**/integration_test*.tycho.yaml (see .dockerignore)"
        );
        let by_protocol = load_packages_from(yamls, chain)?
            .into_iter()
            .filter(|(_, package)| package.harness == Harness::Strict)
            .collect();
        Ok(Self { by_protocol })
    }

    /// The strict packages of one Ethereum `yaml`, for tests elsewhere in the crate.
    #[cfg(test)]
    pub fn from_yamls_for_tests(yaml: String) -> Self {
        Self::from_yamls([("test".to_string(), yaml)], Chain::Ethereum).unwrap()
    }

    /// The declarations of `protocol_system`, when its package is strict.
    pub fn get(&self, protocol_system: &str) -> Option<&PackageExpectations> {
        self.by_protocol.get(protocol_system)
    }

    /// The protocol systems with a strict package.
    pub fn protocols(&self) -> impl Iterator<Item = &str> {
        self.by_protocol
            .keys()
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn every_package_yaml_in_the_repository_is_embedded() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocols/substreams");
        let on_disk: Vec<String> = tycho_test::expectations::test_yamls(&root, false)
            .unwrap()
            .into_iter()
            .map(|path| {
                path.strip_prefix(&root)
                    .unwrap()
                    .display()
                    .to_string()
            })
            .collect();
        let embedded: Vec<&str> = PACKAGE_YAMLS
            .iter()
            .map(|(path, _)| *path)
            .collect();
        assert!(!embedded.is_empty());
        assert_eq!(embedded, on_disk);
    }

    #[test]
    fn only_strict_packages_are_kept() {
        let yaml = |system: &str, harness: &str| {
            (
                system.to_string(),
                format!(
                    "protocol_system: {system}\n{harness}substreams_yaml_path: ./substreams.yaml\n\
                     tests: []\n"
                ),
            )
        };
        let packages = StrictPackages::from_yamls(
            [yaml("vault", "harness: strict\n"), yaml("pool", "")],
            Chain::Ethereum,
        )
        .unwrap();
        assert!(packages.get("vault").is_some());
        assert!(packages.get("pool").is_none());
    }

    #[test]
    fn no_embedded_yaml_is_an_error() {
        let err = StrictPackages::from_yamls(Vec::new(), Chain::Ethereum)
            .err()
            .unwrap();
        assert!(
            err.to_string()
                .contains("no package yamls"),
            "{err}"
        );
    }
}
