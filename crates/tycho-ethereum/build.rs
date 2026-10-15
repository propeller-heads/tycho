//! Computes the digest behind `SERVICES_SOURCE_DIGEST`: the sources of the RPC services the
//! indexer's extractors call, the RPC client they call through, the ERC-20 bindings they use and
//! the crate root, whose `BytesCodec` they all use. The indexer's recorded-extractor tests mock
//! these, so its delta format files cannot show a change to them; the digest is for the indexer's
//! extractor fingerprints instead.
//!
//! Reads only this crate's own files, so it works from the published package too.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use tiny_keccak::{Hasher, Keccak};

/// The hashed sources, relative to the crate root. Directories are hashed recursively.
const HASHED_PATHS: [&str; 4] = ["src/services", "src/rpc", "src/erc20.rs", "src/lib.rs"];

/// The environment variable `SERVICES_SOURCE_DIGEST` is read from.
const DIGEST_ENV: &str = "TYCHO_ETHEREUM_SERVICES_SOURCE_DIGEST";

fn main() {
    let root =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"));

    println!("cargo:rerun-if-changed=build.rs");
    let mut files = Vec::new();
    for path in HASHED_PATHS {
        println!("cargo:rerun-if-changed={path}");
        collect_files(&root, path, &mut files);
    }
    files.sort();

    let mut hasher = Keccak::v256();
    for relative in &files {
        let path = root.join(relative);
        let contents =
            fs::read(&path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));
        update_framed(&mut hasher, relative.as_bytes());
        update_framed(&mut hasher, &normalize_line_endings(contents));
    }
    let mut digest = [0u8; 32];
    hasher.finalize(&mut digest);

    let hex: String = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!("cargo:rustc-env={DIGEST_ENV}={hex}");
}

/// Appends the files at `relative` (a file, or a directory walked recursively) to `files`, as
/// paths relative to `root` joined with `/` on every platform. Skips hidden files and editor
/// backups (names starting with `.` or ending with `~`), which are not sources.
///
/// # Panics
/// If `relative` does not exist, or holds a name that is not UTF-8.
fn collect_files(root: &Path, relative: &str, files: &mut Vec<String>) {
    let path = root.join(relative);
    let metadata = fs::metadata(&path).unwrap_or_else(|err| {
        panic!("cannot read {}: {err}. Update HASHED_PATHS in build.rs.", path.display())
    });
    if !metadata.is_dir() {
        files.push(relative.to_owned());
        return;
    }
    let entries =
        fs::read_dir(&path).unwrap_or_else(|err| panic!("cannot list {}: {err}", path.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|err| panic!("cannot list {}: {err}", path.display()));
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            panic!("{} holds a file name that is not UTF-8: {name:?}", path.display());
        };
        if name.starts_with('.') || name.ends_with('~') {
            continue;
        }
        collect_files(root, &format!("{relative}/{name}"), files);
    }
}

/// Hashes `bytes` prefixed with their length, so moving bytes from one field to the next changes
/// the digest.
fn update_framed(hasher: &mut Keccak, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// `contents` with every CRLF replaced by LF when it is UTF-8 text: a checkout that converts line
/// endings (Git on Windows) must give the same digest as one that does not. Anything else, such as
/// compiled bytecode, is hashed as it is.
fn normalize_line_endings(contents: Vec<u8>) -> Vec<u8> {
    if std::str::from_utf8(&contents).is_err() {
        return contents;
    }
    let mut normalized = Vec::with_capacity(contents.len());
    for (index, byte) in contents.iter().enumerate() {
        if *byte == b'\r' && contents.get(index + 1) == Some(&b'\n') {
            continue;
        }
        normalized.push(*byte);
    }
    normalized
}
