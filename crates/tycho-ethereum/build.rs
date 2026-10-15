//! Computes the digest behind `SERVICES_SOURCE_DIGEST`: the sources of the RPC services the
//! indexer's extractors call, the RPC client they call through, the ERC-20 bindings they use and
//! the crate root, whose `BytesCodec` they all use. The indexer's recorded-extractor tests mock
//! these, so its delta format files cannot show a change to them; the digest is for the indexer's
//! extractor fingerprints instead.
//!
//! `build/source_digest.rs` says what the digest ignores. It reads only this crate's own files,
//! so it works from the published package too.

#[path = "build/source_digest.rs"]
mod source_digest;

use std::{env, path::PathBuf};

/// The hashed sources, relative to the crate root. Directories are hashed recursively.
const HASHED_PATHS: [&str; 4] = ["src/services", "src/rpc", "src/erc20.rs", "src/lib.rs"];

/// The environment variable `SERVICES_SOURCE_DIGEST` is read from.
const DIGEST_ENV: &str = "TYCHO_ETHEREUM_SERVICES_SOURCE_DIGEST";

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
