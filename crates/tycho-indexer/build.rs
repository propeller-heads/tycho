//! Computes the digest behind `UNRECORDED_MODULES_SOURCE_DIGEST` (`extractor/delta_format.rs`):
//! the sources of the extractor modules the recorded-extractor tests do not run, the Uniswap V4
//! hooks DCI and the post-processors, and of the code only they use: `u256_num.rs` and the DCI
//! module root (`PausingReason`). The delta format files cannot show a change to them, so every
//! extractor fingerprint covers this digest instead.
//!
//! The digest comes from tycho-ethereum's `build/source_digest.rs`, which says what it ignores.

#[path = "../tycho-ethereum/build/source_digest.rs"]
mod source_digest;

use std::{env, path::PathBuf};

/// The hashed sources, relative to the crate root. Directories are hashed recursively.
const HASHED_PATHS: [&str; 4] = [
    "src/extractor/dynamic_contract_indexer/hooks",
    "src/extractor/post_processors",
    "src/extractor/u256_num.rs",
    "src/extractor/dynamic_contract_indexer/mod.rs",
];

/// The environment variable `UNRECORDED_MODULES_SOURCE_DIGEST` is read from.
const DIGEST_ENV: &str = "TYCHO_INDEXER_UNRECORDED_MODULES_SOURCE_DIGEST";

fn main() {
    let root =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"));

    println!("cargo:rerun-if-changed=build.rs");
    for path in HASHED_PATHS {
        println!("cargo:rerun-if-changed={path}");
    }
    let digest = source_digest::source_digest(&root, &HASHED_PATHS);
    println!("cargo:rustc-env={DIGEST_ENV}={digest}");
}
