#[cfg(test)]
#[macro_use]
extern crate pretty_assertions;

pub mod erc20;
pub mod gas;
pub mod rpc;
pub mod services;

#[cfg(test)]
pub mod test_fixtures;

use alloy::primitives::{Address, B256, U256};
pub(crate) use rpc::errors::*;
use tycho_common::Bytes;

/// Hex-encoded Keccak-256 digest of this crate's `services`, `rpc` and `erc20` sources and of this
/// file, computed at build time from every file's path and contents (line endings normalized to
/// LF).
///
/// Two builds with the same digest compiled the same sources for these modules. Any edit to them,
/// tests and comments included, changes it.
pub const SERVICES_SOURCE_DIGEST: &str = env!("TYCHO_ETHEREUM_SERVICES_SOURCE_DIGEST");

/// A trait for converting types to and from `Bytes`.
///
/// This trait provides methods to convert a type into a `Bytes` object,
/// as well as reconstruct the original type from a `Bytes` object.
///
/// # Examples
/// ```
/// use alloy::primitives::Address;
/// use tycho_ethereum::BytesCodec;
/// use tycho_common::Bytes;
///
/// let address_value = Address::ZERO;
/// let bytes: Bytes = address_value.to_bytes(); // Converts Address to Bytes
/// let new_address = Address::from_bytes(&bytes);  // Converts Bytes back to Address
/// ```
pub trait BytesCodec {
    /// Converts the current type into a `Bytes` object.
    fn to_bytes(self) -> Bytes;

    /// Reconstructs the type from a `Bytes` object.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The `Bytes` object to convert into the original type.
    ///
    /// # Returns
    ///
    /// The type that was converted from `Bytes`.
    fn from_bytes(bytes: &Bytes) -> Self;
}

// Implementing `BytesCodec` for `Address` (H160 equivalent).
impl BytesCodec for Address {
    /// Converts `Address` to `Bytes`.
    fn to_bytes(self) -> Bytes {
        Bytes::from(self.0.to_vec())
    }

    /// Converts `Bytes` to `Address`.
    ///
    /// # Panics
    ///
    /// Will panic if the length of `Bytes` is not 20 (which is the size of an `Address`).
    fn from_bytes(bytes: &Bytes) -> Self {
        Address::from_slice(bytes.as_ref())
    }
}

// Implementing `BytesCodec` for `B256`
impl BytesCodec for B256 {
    /// Converts `B256` to `Bytes`.
    fn to_bytes(self) -> Bytes {
        Bytes::from(self.0.to_vec())
    }

    /// Converts `Bytes` to `B256`.
    ///
    /// # Panics
    ///
    /// Will panic if the length of `Bytes` is not 32 (which is the size of a `B256`).
    fn from_bytes(bytes: &Bytes) -> Self {
        B256::from_slice(bytes.as_ref())
    }
}

// Implementing `BytesCodec` for `U256`.
impl BytesCodec for U256 {
    /// Converts `U256` to `Bytes`.
    fn to_bytes(self) -> Bytes {
        let buf = self.to_be_bytes::<32>();
        Bytes::from(buf.to_vec())
    }

    /// Converts `Bytes` to `U256` using big-endian.
    ///
    /// # Panics
    ///
    /// Will panic if the length of `Bytes` is larger than 32.
    fn from_bytes(bytes: &Bytes) -> Self {
        let bytes_slice = bytes.as_ref();

        // Create an array with zeros.
        let mut u256_bytes: [u8; 32] = [0; 32];

        // Copy bytes from `bytes_slice` to `u256_bytes`.
        u256_bytes[32 - bytes_slice.len()..].copy_from_slice(bytes_slice);

        // Convert the byte array to `U256` using big-endian.
        U256::from_be_bytes(u256_bytes)
    }
}
