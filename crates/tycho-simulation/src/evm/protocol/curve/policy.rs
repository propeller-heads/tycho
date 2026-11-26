//! The policy contract of a Twocrypto pool, as the indexer reports it in state attributes.
use std::collections::HashMap;

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tycho_common::Bytes;

/// Name of the state attribute that holds the pool's policy contract.
pub const POLICY_ATTRIBUTE: &str = "policy";
/// Name of the state attribute that holds the version of the policy's grant rules.
pub const POLICY_VERSION_ATTRIBUTE: &str = "policy_version";
/// Name of the state attribute that holds the key that signs the policy's grants.
pub const POLICY_CONTROLLER_ATTRIBUTE: &str = "policy_controller";
/// Name of the state attribute that holds the only caller that can submit a grant.
pub const POLICY_ROUTER_ATTRIBUTE: &str = "policy_router";
/// Name of the state attribute that is `1` while the policy refuses grants.
pub const POLICY_KILLED_ATTRIBUTE: &str = "policy_killed";
/// Name of the state attribute that holds the lowest fee of a granted swap, in units of 1e-10.
pub const POLICY_MIN_FEE_ATTRIBUTE: &str = "policy_min_fee";

/// A policy attribute whose value is malformed.
#[derive(Debug, Error)]
#[error("curve `{name}` attribute is {value}, expected {expected}")]
pub struct PolicyDecodeError {
    name: String,
    value: Bytes,
    expected: &'static str,
}

impl PolicyDecodeError {
    fn new(name: &str, value: &Bytes, expected: &'static str) -> Self {
        Self { name: name.to_string(), value: value.clone(), expected }
    }
}

/// The policy contract of a pool and the terms of its grants. A zero attribute value means unset:
/// a pool without a grant policy has version 0.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurvePolicy {
    address: Option<Bytes>,
    version: U256,
    controller: Option<Bytes>,
    router: Option<Bytes>,
    is_killed: bool,
    min_fee: U256,
}

impl CurvePolicy {
    /// Decodes the policy from a pool's state attributes. Absent attributes stay unset.
    pub fn from_attributes(attributes: &HashMap<String, Bytes>) -> Result<Self, PolicyDecodeError> {
        let mut policy = Self::default();
        for (name, value) in attributes {
            policy.update(name, value)?;
        }
        Ok(policy)
    }

    /// Sets the value of a policy attribute. Ignores other attributes.
    pub fn update(&mut self, name: &str, value: &Bytes) -> Result<(), PolicyDecodeError> {
        match name {
            POLICY_ATTRIBUTE => self.address = decode_address(name, value)?,
            POLICY_VERSION_ATTRIBUTE => self.version = decode_uint(name, value)?,
            POLICY_CONTROLLER_ATTRIBUTE => self.controller = decode_address(name, value)?,
            POLICY_ROUTER_ATTRIBUTE => self.router = decode_address(name, value)?,
            POLICY_KILLED_ATTRIBUTE => self.is_killed = !decode_uint(name, value)?.is_zero(),
            POLICY_MIN_FEE_ATTRIBUTE => self.min_fee = decode_uint(name, value)?,
            _ => {}
        }
        Ok(())
    }

    /// Unsets a deleted policy attribute. Ignores other attributes.
    pub fn delete(&mut self, name: &str) {
        match name {
            POLICY_ATTRIBUTE => self.address = None,
            POLICY_VERSION_ATTRIBUTE => self.version = U256::ZERO,
            POLICY_CONTROLLER_ATTRIBUTE => self.controller = None,
            POLICY_ROUTER_ATTRIBUTE => self.router = None,
            POLICY_KILLED_ATTRIBUTE => self.is_killed = false,
            POLICY_MIN_FEE_ATTRIBUTE => self.min_fee = U256::ZERO,
            _ => {}
        }
    }

    /// The policy contract. `None` if the pool has no policy.
    pub fn address(&self) -> Option<&Bytes> {
        self.address.as_ref()
    }

    /// The version of the grant rules. 0 if the policy has no grants.
    pub fn version(&self) -> U256 {
        self.version
    }

    /// The key that signs the policy's grants. `None` if unset.
    pub fn controller(&self) -> Option<&Bytes> {
        self.controller.as_ref()
    }

    /// The only caller that can submit a grant. `None` if unset.
    pub fn router(&self) -> Option<&Bytes> {
        self.router.as_ref()
    }

    /// Whether the policy refuses grants.
    pub fn is_killed(&self) -> bool {
        self.is_killed
    }

    /// The lowest fee of a granted swap, in units of 1e-10.
    pub fn min_fee(&self) -> U256 {
        self.min_fee
    }
}

/// Decodes an address attribute. The zero address means unset.
fn decode_address(name: &str, value: &Bytes) -> Result<Option<Bytes>, PolicyDecodeError> {
    if value.len() != 20 {
        return Err(PolicyDecodeError::new(name, value, "a 20-byte address"));
    }
    if value.iter().all(|byte| *byte == 0) {
        return Ok(None);
    }
    Ok(Some(value.clone()))
}

/// Decodes a big-endian unsigned integer attribute.
fn decode_uint(name: &str, value: &Bytes) -> Result<U256, PolicyDecodeError> {
    U256::try_from_be_slice(value)
        .ok_or_else(|| PolicyDecodeError::new(name, value, "at most 32 bytes"))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn attributes(entries: &[(&str, Vec<u8>)]) -> HashMap<String, Bytes> {
        entries
            .iter()
            .map(|(name, value)| (name.to_string(), Bytes::from(value.clone())))
            .collect()
    }

    #[test]
    fn test_from_attributes() {
        let policy = CurvePolicy::from_attributes(&attributes(&[
            (POLICY_ATTRIBUTE, vec![1; 20]),
            (POLICY_VERSION_ATTRIBUTE, vec![1]),
            (POLICY_CONTROLLER_ATTRIBUTE, vec![2; 20]),
            (POLICY_ROUTER_ATTRIBUTE, vec![3; 20]),
            (POLICY_KILLED_ATTRIBUTE, vec![1]),
            (POLICY_MIN_FEE_ATTRIBUTE, vec![0x01, 0x86, 0xa0]),
            ("balances", vec![9; 64]),
        ]))
        .expect("decode");

        let expected = CurvePolicy {
            address: Some(Bytes::from(vec![1; 20])),
            version: U256::from(1),
            controller: Some(Bytes::from(vec![2; 20])),
            router: Some(Bytes::from(vec![3; 20])),
            is_killed: true,
            min_fee: U256::from(100_000),
        };
        assert_eq!(policy, expected);
    }

    #[test]
    fn test_from_attributes_zero_values() {
        let policy = CurvePolicy::from_attributes(&attributes(&[
            (POLICY_ATTRIBUTE, vec![0; 20]),
            (POLICY_VERSION_ATTRIBUTE, vec![0]),
            (POLICY_CONTROLLER_ATTRIBUTE, vec![0; 20]),
            (POLICY_ROUTER_ATTRIBUTE, vec![0; 20]),
            (POLICY_KILLED_ATTRIBUTE, vec![0]),
            (POLICY_MIN_FEE_ATTRIBUTE, vec![]),
        ]))
        .expect("decode");

        assert_eq!(policy, CurvePolicy::default());
    }

    #[rstest]
    #[case::short_address(POLICY_ATTRIBUTE, vec![7; 19])]
    #[case::empty_address(POLICY_CONTROLLER_ATTRIBUTE, vec![])]
    #[case::wide_uint(POLICY_MIN_FEE_ATTRIBUTE, vec![1; 33])]
    fn test_update_malformed_value(#[case] name: &str, #[case] value: Vec<u8>) {
        let mut policy = CurvePolicy::default();

        assert!(policy
            .update(name, &Bytes::from(value))
            .is_err());
    }

    #[test]
    fn test_delete() {
        let mut policy = CurvePolicy::from_attributes(&attributes(&[
            (POLICY_ATTRIBUTE, vec![1; 20]),
            (POLICY_MIN_FEE_ATTRIBUTE, vec![5]),
        ]))
        .expect("decode");

        policy.delete(POLICY_ATTRIBUTE);
        policy.delete(POLICY_MIN_FEE_ATTRIBUTE);

        assert_eq!(policy, CurvePolicy::default());
    }
}
