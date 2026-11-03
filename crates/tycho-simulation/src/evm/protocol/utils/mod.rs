pub mod slipstreams;
pub(super) mod solidity_math;
pub mod uniswap;

use alloy::primitives::Address;
use tycho_common::{simulation::errors::SimulationError, Bytes};

/// Computes the gross amount needed so that after fees are taken, the net equals `amount`.
///
/// Formula: `amount / (1 - fee)`. Example: with 0.3% fee, 100 → ~100.30.
pub fn add_fee_markup(amount: f64, fee: f64) -> f64 {
    amount / (1.0 - fee)
}

/// Safely converts a `Bytes` object to an `Address` object.
///
/// Checks the length of the `Bytes` before attempting to convert, and returns a `SimulationError`
/// if not 20 bytes long.
pub fn bytes_to_address(address: &Bytes) -> Result<Address, SimulationError> {
    if address.len() == 20 {
        Ok(Address::from_slice(address))
    } else {
        Err(SimulationError::InvalidInput(
            format!("Invalid ERC20 token address: {address:?}"),
            None,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::hexstring_to_vec;
    #[test]
    fn test_bytes_to_address_0x() {
        let address =
            Bytes::from(hexstring_to_vec("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap());
        assert_eq!(
            bytes_to_address(&address).unwrap(),
            Address::from_slice(&hex::decode("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap())
        );
    }

    #[test]
    fn test_bytes_to_address_invalid() {
        let address = Bytes::from(hex::decode("C02aaA").unwrap());
        assert!(bytes_to_address(&address).is_err());
    }
}
