//! The committed delta format files (`delta_format/*.json`) and their digest, which every
//! extractor fingerprint covers.
//!
//! The files hold every message the recorded extractors emit for their recorded streams, and
//! `delta_format_tests.rs` fails while the emitted messages differ from them. A change to the
//! deltas an unchanged extractor emits therefore cannot merge without regenerating the files, and
//! regenerating them changes the digest: there is no version to bump by hand.
//!
//! The digest also covers the sources whose behaviour the files cannot show: the RPC services the
//! recorded extractors mock ([`tycho_ethereum::SERVICES_SOURCE_DIGEST`]) and the extractor modules
//! they do not run ([`UNRECORDED_MODULES_SOURCE_DIGEST`]).

use tiny_keccak::{Hasher, Keccak};

/// Hex-encoded Keccak-256 digest of the sources of the extractor modules the recorded extractors
/// do not run, `extractor/dynamic_contract_indexer/hooks/` (the Uniswap V4 hooks DCI) and
/// `extractor/post_processors/`, and of the code only they use: `extractor/u256_num.rs` and
/// `extractor/dynamic_contract_indexer/mod.rs`. Computed by `build.rs` with tycho-ethereum's
/// `build/source_digest.rs`, so it changes with their code, not with comments, docs, formatting or
/// test-only items.
const UNRECORDED_MODULES_SOURCE_DIGEST: &str =
    env!("TYCHO_INDEXER_UNRECORDED_MODULES_SOURCE_DIGEST");

/// A recorded stream with a committed format file in `delta_format/`.
///
/// A new stream needs its file before this compiles: create it empty, then generate it with
/// `UPDATE_DELTA_FORMAT=1` (see `delta_format_tests.rs`).
#[derive(Debug, Clone, Copy)]
pub(super) enum FormatStream {
    Native,
    /// The native stream's blocks sent as partial blocks, so the merge of partials into
    /// full-block messages and the reverts of pending partials are in the format.
    NativePartialBlocks,
    Dci,
}

/// The name of a file in `delta_format/` with its contents, built into the binary.
macro_rules! committed_file {
    ($name:literal) => {
        ($name, include_bytes!(concat!("delta_format/", $name)).as_slice())
    };
}

impl FormatStream {
    /// Every stream, in the order the digest hashes them. A test fails while a file in
    /// `delta_format/` belongs to none of them.
    pub(super) const ALL: [Self; 3] = [Self::Native, Self::NativePartialBlocks, Self::Dci];

    /// The name of the stream's format file in `delta_format/`.
    pub(super) fn file_name(self) -> &'static str {
        self.committed_file().0
    }

    /// The stream's format file as committed, built into the binary.
    pub(super) fn committed_bytes(self) -> &'static [u8] {
        self.committed_file().1
    }

    fn committed_file(self) -> (&'static str, &'static [u8]) {
        match self {
            Self::Native => committed_file!("native.json"),
            Self::NativePartialBlocks => committed_file!("native_partial_blocks.json"),
            Self::Dci => committed_file!("dci.json"),
        }
    }
}

/// Digest of the committed format files and of the sources whose behaviour they cannot show.
/// Depends only on the build, so it is the same for every extractor and every run of a binary.
pub(super) fn delta_format_digest() -> [u8; 32] {
    let files = FormatStream::ALL.map(|stream| (stream.file_name(), stream.committed_bytes()));
    format_digest(&files, tycho_ethereum::SERVICES_SOURCE_DIGEST, UNRECORDED_MODULES_SOURCE_DIGEST)
}

fn format_digest(
    files: &[(&str, &[u8])],
    services_source_digest: &str,
    unrecorded_modules_source_digest: &str,
) -> [u8; 32] {
    let mut hasher = FieldHasher::new();
    hasher.count(files.len());
    for (name, contents) in files {
        hasher.field(name.as_bytes());
        hasher.field(contents);
    }
    hasher.field(services_source_digest.as_bytes());
    hasher.field(unrecorded_modules_source_digest.as_bytes());
    hasher.finish()
}

/// Keccak-256 over a sequence of fields, each prefixed with its length, so two different
/// sequences never hash the same bytes. Variable-length lists need [`FieldHasher::count`] before
/// their items for the same reason.
pub(super) struct FieldHasher(Keccak);

impl FieldHasher {
    pub(super) fn new() -> Self {
        Self(Keccak::v256())
    }

    /// Hashes `bytes` as the next field.
    pub(super) fn field(&mut self, bytes: &[u8]) {
        self.0
            .update(&(bytes.len() as u64).to_be_bytes());
        self.0.update(bytes);
    }

    /// Hashes the number of items of the list whose fields come next.
    pub(super) fn count(&mut self, items: usize) {
        self.field(&(items as u64).to_be_bytes());
    }

    pub(super) fn finish(self) -> [u8; 32] {
        let mut digest = [0u8; 32];
        self.0.finalize(&mut digest);
        digest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVICES: &str = "services";
    const MODULES: &str = "modules";

    fn digest_of(files: &[(&str, Vec<u8>)], services: &str, modules: &str) -> [u8; 32] {
        let files: Vec<(&str, &[u8])> = files
            .iter()
            .map(|(name, contents)| (*name, contents.as_slice()))
            .collect();
        format_digest(&files, services, modules)
    }

    fn files(entries: &[(&'static str, &str)]) -> Vec<(&'static str, Vec<u8>)> {
        entries
            .iter()
            .map(|(name, contents)| (*name, contents.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn the_digest_changes_with_the_bytes_of_each_committed_file() {
        let committed: Vec<(&str, Vec<u8>)> = FormatStream::ALL
            .iter()
            .map(|stream| (stream.file_name(), stream.committed_bytes().to_vec()))
            .collect();
        let base = digest_of(&committed, SERVICES, MODULES);

        for (index, (name, contents)) in committed.iter().enumerate() {
            for position in [0, contents.len() / 2, contents.len() - 1] {
                let mut changed = committed.clone();
                changed[index].1[position] ^= 1;

                assert_ne!(
                    digest_of(&changed, SERVICES, MODULES),
                    base,
                    "flipping byte {position} of {name} kept the digest"
                );
            }
        }
    }

    #[test]
    fn the_digest_changes_with_every_input() {
        let base = digest_of(&files(&[("a.json", "[]"), ("b.json", "[]")]), SERVICES, MODULES);

        let changes = [
            ("a renamed file", files(&[("a.json", "[]"), ("c.json", "[]")]), SERVICES, MODULES),
            (
                "bytes moved from a name to its contents",
                files(&[("a.jso", "n[]"), ("b.json", "[]")]),
                SERVICES,
                MODULES,
            ),
            ("a dropped file", files(&[("a.json", "[]")]), SERVICES, MODULES),
            (
                "another services digest",
                files(&[("a.json", "[]"), ("b.json", "[]")]),
                "services'",
                MODULES,
            ),
            (
                "another modules digest",
                files(&[("a.json", "[]"), ("b.json", "[]")]),
                SERVICES,
                "modules'",
            ),
        ];
        for (change, files, services, modules) in changes {
            assert_ne!(digest_of(&files, services, modules), base, "{change} kept the digest");
        }
    }
}
