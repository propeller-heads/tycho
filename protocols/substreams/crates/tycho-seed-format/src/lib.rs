//! On-disk layout of a seed file.
//!
//! A seed is the state of one Substreams package's protocol after one block. The package embeds
//! the file and, when it streams that block, emits the state as synthetic events instead of the
//! block's real events, so a manifest whose `initialBlock` is the seed block skips the protocol's
//! history. This crate defines only the header in front of the package-specific body, so the
//! writer of a seed and the package reading it agree on the block and the package without either
//! knowing the other's payload type. It builds for the packages' `wasm32-unknown-unknown` target
//! and for the native writers alike.
//!
//! ```text
//! MAGIC | package name length (u8) | package name | block number (u64 BE) | block hash (32) | body
//! ```
//!
//! The body is the package's own protobuf message. An empty body is a valid seed of nothing, which
//! is what a package embeds by default.

use std::{error, fmt};

use tiny_keccak::{Hasher, Keccak};

/// Identifies a seed file and the version of this layout.
pub const MAGIC: &[u8] = b"tycho-seed-v1";

const BLOCK_HASH_LEN: usize = 32;

/// What a seed describes: the state of `package` after block `block_number`, whose hash is
/// `block_hash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Cargo package name of the Substreams package the body is meant for.
    pub package: String,
    pub block_number: u64,
    pub block_hash: [u8; BLOCK_HASH_LEN],
}

impl Header {
    /// Hash of the synthetic transaction that carries the seeded state in the package's output.
    ///
    /// Deterministic in the package and block so a seeded run can be told apart from, and compared
    /// against, a run over the protocol's history: seeded components carry this hash as their
    /// creation transaction.
    pub fn genesis_transaction_hash(&self) -> [u8; 32] {
        let mut hasher = Keccak::v256();
        hasher.update(MAGIC);
        hasher.update(self.package.as_bytes());
        hasher.update(&self.block_hash);
        let mut hash = [0u8; 32];
        hasher.finalize(&mut hash);
        hash
    }

    fn encoded_len(&self) -> usize {
        MAGIC.len() + 1 + self.package.len() + size_of::<u64>() + BLOCK_HASH_LEN
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Fewer bytes than a header needs; an empty file is the common case.
    TooShort {
        len: usize,
        needed: usize,
    },
    NotASeed,
    PackageNameNotUtf8,
    /// The seed was written for another package.
    WrongPackage {
        expected: String,
        found: String,
    },
    /// The package name does not fit the one-byte length prefix.
    PackageNameTooLong(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooShort { len, needed } => {
                write!(f, "seed holds {len} bytes, less than the {needed}-byte header")
            }
            Error::NotASeed => write!(f, "seed does not start with the `tycho-seed-v1` magic"),
            Error::PackageNameNotUtf8 => write!(f, "seed header carries a non-UTF-8 package name"),
            Error::WrongPackage { expected, found } => {
                write!(f, "seed was written for package `{found}`, not `{expected}`")
            }
            Error::PackageNameTooLong(len) => {
                write!(f, "package name of {len} bytes exceeds the header's limit of 255")
            }
        }
    }
}

impl error::Error for Error {}

/// Serializes `header` followed by `body`.
pub fn encode(header: &Header, body: &[u8]) -> Result<Vec<u8>, Error> {
    let name_len = u8::try_from(header.package.len())
        .map_err(|_| Error::PackageNameTooLong(header.package.len()))?;

    let mut bytes = Vec::with_capacity(header.encoded_len() + body.len());
    bytes.extend_from_slice(MAGIC);
    bytes.push(name_len);
    bytes.extend_from_slice(header.package.as_bytes());
    bytes.extend_from_slice(&header.block_number.to_be_bytes());
    bytes.extend_from_slice(&header.block_hash);
    bytes.extend_from_slice(body);

    Ok(bytes)
}

/// Splits a seed file into its header and body.
pub fn decode(bytes: &[u8]) -> Result<(Header, &[u8]), Error> {
    let too_short = |needed| Error::TooShort { len: bytes.len(), needed };

    let fixed_len = MAGIC.len() + 1;
    if bytes.len() < fixed_len {
        return Err(too_short(fixed_len + size_of::<u64>() + BLOCK_HASH_LEN));
    }
    let (magic, rest) = bytes.split_at(MAGIC.len());
    if magic != MAGIC {
        return Err(Error::NotASeed);
    }

    let (name_len, rest) = rest.split_first().unwrap();
    let name_len = usize::from(*name_len);
    let needed = fixed_len + name_len + size_of::<u64>() + BLOCK_HASH_LEN;
    if bytes.len() < needed {
        return Err(too_short(needed));
    }

    let (package, rest) = rest.split_at(name_len);
    let package = std::str::from_utf8(package)
        .map_err(|_| Error::PackageNameNotUtf8)?
        .to_owned();
    let (block_number, rest) = rest.split_at(size_of::<u64>());
    let block_number = u64::from_be_bytes(block_number.try_into().unwrap());
    let (block_hash, body) = rest.split_at(BLOCK_HASH_LEN);

    Ok((Header { package, block_number, block_hash: block_hash.try_into().unwrap() }, body))
}

/// [`decode`] for a known package, rejecting seeds written for another one.
pub fn decode_for<'a>(bytes: &'a [u8], package: &str) -> Result<(Header, &'a [u8]), Error> {
    let (header, body) = decode(bytes)?;
    if header.package != package {
        return Err(Error::WrongPackage { expected: package.to_owned(), found: header.package });
    }

    Ok((header, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            package: "ethereum-example".to_owned(),
            block_number: 25_000_000,
            block_hash: [7; 32],
        }
    }

    #[test]
    fn round_trips_header_and_body() {
        let bytes = encode(&header(), b"payload").unwrap();

        let (decoded, body) = decode(&bytes).unwrap();

        assert_eq!(decoded, header());
        assert_eq!(body, b"payload");
    }

    #[test]
    fn empty_body_is_a_valid_seed() {
        let bytes = encode(&header(), &[]).unwrap();

        let (decoded, body) = decode(&bytes).unwrap();

        assert_eq!(decoded, header());
        assert!(body.is_empty());
    }

    #[test]
    fn rejects_empty_truncated_and_foreign_input() {
        assert!(matches!(decode(&[]), Err(Error::TooShort { len: 0, .. })));

        let bytes = encode(&header(), &[]).unwrap();
        assert!(matches!(decode(&bytes[..bytes.len() - 1]), Err(Error::TooShort { .. })));

        assert_eq!(decode(&[0; 64]), Err(Error::NotASeed));
    }

    #[test]
    fn checks_the_package_name() {
        let bytes = encode(&header(), &[]).unwrap();

        assert!(decode_for(&bytes, "ethereum-example").is_ok());
        assert_eq!(
            decode_for(&bytes, "ethereum-other"),
            Err(Error::WrongPackage {
                expected: "ethereum-other".to_owned(),
                found: "ethereum-example".to_owned(),
            })
        );
    }

    #[test]
    fn genesis_transaction_hash_depends_on_package_and_block() {
        let base = header();
        let other_package = Header { package: "ethereum-other".to_owned(), ..header() };
        let other_block = Header { block_hash: [8; 32], ..header() };

        assert_eq!(base.genesis_transaction_hash(), header().genesis_transaction_hash());
        assert_ne!(base.genesis_transaction_hash(), other_package.genesis_transaction_hash());
        assert_ne!(base.genesis_transaction_hash(), other_block.genesis_transaction_hash());
    }

    #[test]
    fn error_messages_name_the_cause() {
        assert_eq!(
            Error::TooShort { len: 0, needed: 54 }.to_string(),
            "seed holds 0 bytes, less than the 54-byte header"
        );
        assert_eq!(
            Error::WrongPackage { expected: "a".to_owned(), found: "b".to_owned() }.to_string(),
            "seed was written for package `b`, not `a`"
        );
    }
}
