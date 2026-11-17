pub mod display;
pub mod dto;
pub mod hex_bytes;
pub mod models;
pub mod serde_primitives;
pub mod simulation;
pub mod storage;
#[cfg(feature = "test-utils")]
pub mod testing;
pub mod traits;

#[cfg(test)]
#[macro_use]
extern crate pretty_assertions;

pub use hex_bytes::Bytes;
use tiny_keccak::{Hasher, Keccak};

/// Compute the Keccak-256 hash of input bytes.
///
/// Note that strings are interpreted as UTF-8 bytes,
pub fn keccak256<T: AsRef<[u8]>>(bytes: T) -> [u8; 32] {
    let mut output = [0u8; 32];

    let mut hasher = Keccak::v256();
    hasher.update(bytes.as_ref());
    hasher.finalize(&mut output);

    output
}
