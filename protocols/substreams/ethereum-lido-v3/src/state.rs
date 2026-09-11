use anyhow::{anyhow, Result};
use serde::Deserialize;
use tycho_substreams::models::{Attribute, ChangeType};

use substreams::scalar::BigInt;

use crate::{
    constants::{
        BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_ATTR, BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_KEY,
        CL_BALANCE_AND_CL_VALIDATORS_ATTR, CL_BALANCE_AND_CL_VALIDATORS_KEY, DEPOSIT_SIZE_WEI,
        STAKING_STATE_ATTR, TOTAL_AND_EXTERNAL_SHARES_ATTR, TOTAL_AND_EXTERNAL_SHARES_KEY,
        WSTETH_SHARES_ATTR, WSTETH_SHARES_KEY,
    },
    utils::{attribute_with_bytes, bytes_from_hex},
};

#[derive(Clone, Debug, Deserialize)]
pub struct InitialState {
    pub start_block: u64,
    pub total_and_external_shares: String,
    pub buffered_ether_and_deposited_validators: String,
    pub cl_balance_and_cl_validators: String,
    pub staking_state: String,
    pub wsteth_shares: String,
}

impl InitialState {
    pub fn parse(params: &str) -> Result<Self> {
        serde_json::from_str(params)
            .map_err(|e| anyhow!("Failed to parse Lido V3 initial state: {e}"))
    }

    pub fn steth_creation_attributes(&self) -> Result<Vec<Attribute>> {
        let mut attributes = self.shared_creation_attributes()?;
        attributes.push(attribute_with_bytes(
            STAKING_STATE_ATTR,
            &bytes_from_hex(&self.staking_state)?,
            ChangeType::Creation,
        ));
        Ok(attributes)
    }

    pub fn wsteth_creation_attributes(&self) -> Result<Vec<Attribute>> {
        let mut attributes = self.shared_creation_attributes()?;
        attributes.push(attribute_with_bytes(
            WSTETH_SHARES_ATTR,
            &bytes_from_hex(&self.wsteth_shares)?,
            ChangeType::Creation,
        ));
        Ok(attributes)
    }

    /// The balance inputs carried by the snapshot, used to seed the store and to report the
    /// component balances on the activation block.
    pub fn balance_state(&self) -> Result<BalanceState> {
        Ok(BalanceState {
            total_and_external_shares: big_int_from_hex(&self.total_and_external_shares)?,
            buffered_ether_and_deposited_validators: big_int_from_hex(
                &self.buffered_ether_and_deposited_validators,
            )?,
            cl_balance_and_cl_validators: big_int_from_hex(&self.cl_balance_and_cl_validators)?,
            wsteth_shares: big_int_from_hex(&self.wsteth_shares)?,
        })
    }

    fn shared_creation_attributes(&self) -> Result<Vec<Attribute>> {
        Ok(vec![
            attribute_with_bytes(
                TOTAL_AND_EXTERNAL_SHARES_ATTR,
                &bytes_from_hex(&self.total_and_external_shares)?,
                ChangeType::Creation,
            ),
            attribute_with_bytes(
                BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_ATTR,
                &bytes_from_hex(&self.buffered_ether_and_deposited_validators)?,
                ChangeType::Creation,
            ),
            attribute_with_bytes(
                CL_BALANCE_AND_CL_VALIDATORS_ATTR,
                &bytes_from_hex(&self.cl_balance_and_cl_validators)?,
                ChangeType::Creation,
            ),
        ])
    }
}

/// Decodes a hex-encoded raw slot value into an unsigned `BigInt`.
pub fn big_int_from_hex(value: &str) -> Result<BigInt> {
    Ok(BigInt::from_unsigned_bytes_be(&bytes_from_hex(value)?))
}

/// The raw slot values that determine the two components' balances.
///
/// stETH packs two scalars per slot: `buffered_ether` / `deposited_validators` in one,
/// `cl_balance` / `cl_validators` in another, and `total_shares` / `external_shares` in a third.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BalanceState {
    pub total_and_external_shares: BigInt,
    pub buffered_ether_and_deposited_validators: BigInt,
    pub cl_balance_and_cl_validators: BigInt,
    pub wsteth_shares: BigInt,
}

impl BalanceState {
    /// `buffered + consensus-layer balance + stake of validators already deposited but not yet
    /// counted on the consensus layer`, matching `Lido.getTotalPooledEther()`.
    pub fn total_pooled_ether(&self) -> BigInt {
        let (buffered_ether, deposited_validators) =
            split_low_high_u128(&self.buffered_ether_and_deposited_validators);
        let (cl_balance, cl_validators) = split_low_high_u128(&self.cl_balance_and_cl_validators);
        let transient = deposited_validators.saturating_sub(cl_validators);
        big_int_from_u128(buffered_ether) +
            big_int_from_u128(cl_balance) +
            big_int_from_u128(transient) * big_int_from_u128(DEPOSIT_SIZE_WEI)
    }

    /// Applies a newly observed raw slot value, keyed as in the store.
    pub fn apply(&mut self, key: &str, value: BigInt) {
        if key == TOTAL_AND_EXTERNAL_SHARES_KEY {
            self.total_and_external_shares = value;
        } else if key == BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_KEY {
            self.buffered_ether_and_deposited_validators = value;
        } else if key == CL_BALANCE_AND_CL_VALIDATORS_KEY {
            self.cl_balance_and_cl_validators = value;
        } else if key == WSTETH_SHARES_KEY {
            self.wsteth_shares = value;
        }
    }

    pub fn total_shares(&self) -> BigInt {
        let (total_shares, _external_shares) = split_low_high_u128(&self.total_and_external_shares);
        big_int_from_u128(total_shares)
    }

    /// The stETH locked in the wstETH wrapper, which is the wstETH component's tradable
    /// liquidity: `sharesOf(wstETH) * totalPooledEther / totalShares`, matching
    /// `stETH.balanceOf(wstETH)`.
    pub fn wsteth_steth_balance(&self) -> BigInt {
        let total_shares = self.total_shares();
        if total_shares.clone() == BigInt::zero() {
            return BigInt::zero();
        }
        self.wsteth_shares.clone() * self.total_pooled_ether() / total_shares
    }
}

/// Splits a packed slot into its low and high 128-bit halves.
fn split_low_high_u128(packed: &BigInt) -> (u128, u128) {
    let bytes = packed.to_bytes_be().1;
    let mut padded = [0u8; 32];
    let take = bytes.len().min(32);
    padded[32 - take..].copy_from_slice(&bytes[bytes.len() - take..]);
    let high = u128::from_be_bytes(
        padded[..16]
            .try_into()
            .expect("16 bytes"),
    );
    let low = u128::from_be_bytes(
        padded[16..]
            .try_into()
            .expect("16 bytes"),
    );
    (low, high)
}

/// `substreams::scalar::BigInt` has no `From<u128>`, so widen through big-endian bytes.
fn big_int_from_u128(value: u128) -> BigInt {
    BigInt::from_unsigned_bytes_be(&value.to_be_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The snapshot the manifest ships, read from stETH storage at the Lido V3 migration block
    /// 24083113.
    fn snapshot() -> InitialState {
        InitialState {
            start_block: 24_083_113,
            total_and_external_shares:
                "0x00000000000000000000000000000000000000000005f18d02edc955cbcfc9b0".to_string(),
            buffered_ether_and_deposited_validators:
                "0x00000000000000000000000000065004000000000000002303b296dd9f3631db".to_string(),
            cl_balance_and_cl_validators:
                "0x00000000000000000000000000064d490000000000073f77f7680e9a096c4600".to_string(),
            staking_state: "0x00001fc3842bd1f071c000000000190000001fc37ee5c9f3db1a0000016f7a9f"
                .to_string(),
            wsteth_shares: "0x00000000000000000000000000000000000000000002a0ab5c33b9a953fed49e"
                .to_string(),
        }
    }

    fn big(value: &str) -> BigInt {
        value
            .parse::<BigInt>()
            .expect("decimal BigInt")
    }

    #[test]
    fn packed_slots_decode_to_the_documented_halves() {
        let state = snapshot()
            .balance_state()
            .expect("balance state");

        assert_eq!(state.total_shares(), big("7185320622405251301886384"));
    }

    #[test]
    fn total_pooled_ether_matches_chain() {
        let state = snapshot()
            .balance_state()
            .expect("balance state");

        // stETH.getTotalPooledEther() at block 24083113.
        assert_eq!(state.total_pooled_ether(), big("8785216718266929405655003"));
    }

    #[test]
    fn wsteth_balance_matches_chain() {
        let state = snapshot()
            .balance_state()
            .expect("balance state");

        // stETH.balanceOf(wstETH) at block 24083113 - the stETH locked in the wrapper, well
        // below the pool total.
        assert_eq!(state.wsteth_steth_balance(), big("3883896708543715528581826"));
        assert!(state.wsteth_steth_balance() < state.total_pooled_ether());
    }

    #[test]
    fn apply_updates_the_keyed_slot() {
        let mut state = snapshot()
            .balance_state()
            .expect("balance state");
        let before = state.wsteth_steth_balance();

        state.apply(WSTETH_SHARES_KEY, BigInt::zero());

        assert_eq!(state.wsteth_steth_balance(), BigInt::zero());
        assert!(before > BigInt::zero());
        // Unrelated keys are untouched.
        assert_eq!(state.total_pooled_ether(), big("8785216718266929405655003"));
    }

    #[test]
    fn zero_total_shares_yields_zero_wsteth_balance() {
        let mut state = snapshot()
            .balance_state()
            .expect("balance state");

        state.apply(TOTAL_AND_EXTERNAL_SHARES_KEY, BigInt::zero());

        assert_eq!(state.wsteth_steth_balance(), BigInt::zero());
    }
}
