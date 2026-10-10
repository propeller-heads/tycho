use std::collections::HashMap;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use tycho_substreams::models::{Attribute, ChangeType};

use substreams::scalar::BigInt;

use crate::{
    constants::{
        TrackedProxy, TrackedSlot, BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_KEY,
        BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_SLOT,
        CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_KEY,
        CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_SLOT, STAKING_STATE_SLOT,
        TOTAL_AND_EXTERNAL_SHARES_KEY, TOTAL_AND_EXTERNAL_SHARES_SLOT, TRACKED_PROXIES,
        WSTETH_SHARES_SLOT,
    },
    utils::{attribute_with_bytes, bytes_from_hex},
};

#[derive(Clone, Debug, Deserialize)]
pub struct InitialState {
    pub start_block: u64,
    pub total_and_external_shares: String,
    pub buffered_ether_and_deposited_post_report: String,
    pub cl_validators_balance_and_cl_pending_balance: String,
    pub staking_state: String,
    pub wsteth_shares: String,
    pub creation_tx: String,
    /// The implementation behind each tracked proxy at `start_block`, keyed by
    /// [`TrackedProxy::label`]. The slots above were verified against these and no others.
    pub implementations: HashMap<String, String>,
}

impl InitialState {
    /// Parses the manifest params, requiring an implementation for every tracked proxy: a proxy
    /// with none recorded could never be found upgraded.
    pub fn parse(params: &str) -> Result<Self> {
        let state: Self = serde_json::from_str(params)
            .map_err(|e| anyhow!("Failed to parse Lido V4 initial state: {e}"))?;
        for proxy in TRACKED_PROXIES.iter() {
            state.implementation_of(proxy)?;
        }
        for (name, word) in [
            ("total_and_external_shares", &state.total_and_external_shares),
            (
                "buffered_ether_and_deposited_post_report",
                &state.buffered_ether_and_deposited_post_report,
            ),
            (
                "cl_validators_balance_and_cl_pending_balance",
                &state.cl_validators_balance_and_cl_pending_balance,
            ),
            ("staking_state", &state.staking_state),
            ("wsteth_shares", &state.wsteth_shares),
        ] {
            let bytes = bytes_from_hex(word).map_err(|error| anyhow!("{name}: {error}"))?;
            if bytes.is_empty() || bytes.len() > 32 {
                return Err(anyhow!("{name} must contain 1 to 32 bytes, got {}", bytes.len()));
            }
        }
        Ok(state)
    }

    /// The implementation `proxy` delegated to when the snapshot was taken.
    pub fn implementation_of(&self, proxy: &TrackedProxy) -> Result<[u8; 20]> {
        let hex = self
            .implementations
            .get(proxy.label)
            .ok_or_else(|| {
                anyhow!("no implementation recorded for tracked proxy {}", proxy.label)
            })?;
        let bytes = bytes_from_hex(hex)?;
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| {
                anyhow!(
                    "implementation of {} is {} bytes, not an address",
                    proxy.label,
                    bytes.len()
                )
            })
    }

    /// Every tracked slot, unpacked the same way the update path unpacks a storage write. One
    /// component serves every direction, so it carries all of them.
    pub fn creation_attributes(&self) -> Result<Vec<Attribute>> {
        let words = [
            (&TOTAL_AND_EXTERNAL_SHARES_SLOT, &self.total_and_external_shares),
            (
                &BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_SLOT,
                &self.buffered_ether_and_deposited_post_report,
            ),
            (
                &CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_SLOT,
                &self.cl_validators_balance_and_cl_pending_balance,
            ),
            (&STAKING_STATE_SLOT, &self.staking_state),
            (&WSTETH_SHARES_SLOT, &self.wsteth_shares),
        ];

        let mut attributes = Vec::new();
        for (slot, word) in words {
            attributes.extend(unpack_fields(slot, &bytes_from_hex(word)?, ChangeType::Creation));
        }
        Ok(attributes)
    }

    /// The balance inputs carried by the snapshot, used to seed the store and to report the
    /// component balance on the activation block.
    pub fn balance_state(&self) -> Result<BalanceState> {
        Ok(BalanceState {
            total_and_external_shares: big_int_from_hex(&self.total_and_external_shares)?,
            buffered_ether_and_deposited_post_report: big_int_from_hex(
                &self.buffered_ether_and_deposited_post_report,
            )?,
            cl_validators_balance_and_cl_pending_balance: big_int_from_hex(
                &self.cl_validators_balance_and_cl_pending_balance,
            )?,
        })
    }
}

/// Reports each value packed into `word` as its own attribute.
///
/// The packing rule lives here and nowhere else: consumers read `total_shares` and the other
/// names Lido gives these values.
pub fn unpack_fields(slot: &TrackedSlot, word: &[u8], change: ChangeType) -> Vec<Attribute> {
    let word = BigInt::from_unsigned_bytes_be(word);
    slot.fields
        .iter()
        .map(|field| {
            let value = if field.width >= 256 {
                word.clone()
            } else {
                let mask = (BigInt::one() << field.width) - BigInt::one();
                (word.clone() >> field.offset) & mask
            };
            attribute_with_bytes(field.attribute, &value.to_bytes_be().1, change)
        })
        .collect()
}

/// Decodes a hex-encoded raw slot value into an unsigned `BigInt`.
pub fn big_int_from_hex(value: &str) -> Result<BigInt> {
    Ok(BigInt::from_unsigned_bytes_be(&bytes_from_hex(value)?))
}

/// The raw slot values that determine the component's reported balance.
///
/// stETH packs two scalars per slot: `buffered_ether` / `deposited_post_report` in one,
/// `cl_validators_balance` / `cl_pending_balance` in another, and `total_shares` /
/// `external_shares` in a third.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BalanceState {
    pub total_and_external_shares: BigInt,
    pub buffered_ether_and_deposited_post_report: BigInt,
    pub cl_validators_balance_and_cl_pending_balance: BigInt,
}

impl BalanceState {
    /// `Lido.getTotalPooledEther()`: the ether the protocol holds itself plus the ether backing
    /// the external shares (stVaults), which Lido values at the internal share rate.
    pub fn total_pooled_ether(&self) -> BigInt {
        let internal_ether = self.internal_ether();
        let (total_shares, external_shares) = split_low_high_u128(&self.total_and_external_shares);
        let internal_shares = total_shares.saturating_sub(external_shares);
        if internal_shares == 0 {
            return internal_ether;
        }
        let external_ether = big_int_from_u128(external_shares) * internal_ether.clone() /
            big_int_from_u128(internal_shares);
        internal_ether + external_ether
    }

    /// `Lido._getInternalEther()`: buffered ether plus every balance counted on the consensus
    /// layer - active validators, deposits pending activation, and deposits made since the last
    /// oracle report. Since Lido v4 these are tracked as balances, so there is no per-validator
    /// stake to multiply by.
    fn internal_ether(&self) -> BigInt {
        let (buffered_ether, deposited_post_report) =
            split_low_high_u128(&self.buffered_ether_and_deposited_post_report);
        let (cl_validators_balance, cl_pending_balance) =
            split_low_high_u128(&self.cl_validators_balance_and_cl_pending_balance);
        big_int_from_u128(buffered_ether) +
            big_int_from_u128(cl_validators_balance) +
            big_int_from_u128(cl_pending_balance) +
            big_int_from_u128(deposited_post_report)
    }

    /// Applies a newly observed raw slot value, keyed as in the store.
    pub fn apply(&mut self, key: &str, value: BigInt) {
        if key == TOTAL_AND_EXTERNAL_SHARES_KEY {
            self.total_and_external_shares = value;
        } else if key == BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_KEY {
            self.buffered_ether_and_deposited_post_report = value;
        } else if key == CL_VALIDATORS_BALANCE_AND_CL_PENDING_BALANCE_KEY {
            self.cl_validators_balance_and_cl_pending_balance = value;
        }
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

    /// The snapshot the manifest ships, read from stETH storage at the Lido v4 migration block
    /// 25603297.
    fn snapshot() -> InitialState {
        InitialState {
            start_block: 25_603_297,
            total_and_external_shares:
                "0x00000000000000c9b9001a2e4d7ccd3f00000000000639d56fd47ec2536471cb".to_string(),
            buffered_ether_and_deposited_post_report:
                "0x000000000000a13dbebf95fa7d800000000000000000001d4009424f9565a013".to_string(),
            cl_validators_balance_and_cl_pending_balance:
                "0x00000000000000000000000000000000000000000007162e4f16cc0af69a3200".to_string(),
            staking_state: "0x00001fc3842bd1f071c000000000190000001fc383c40d61b0f6f0000186acdf"
                .to_string(),
            wsteth_shares: "0x000000000000000000000000000000000000000000030059cedfb0543bebad72"
                .to_string(),
            creation_tx: "0x297042b4e5fd41399634f124beec7afc37712bc3374c3419b72932caf52714aa"
                .to_string(),
            implementations: HashMap::from([(
                "steth".to_string(),
                "0x028271e30a695c0527a0c50ca30603fed004cdb0".to_string(),
            )]),
        }
    }

    /// A proxy with no recorded implementation could never be found upgraded, so the params are
    /// refused rather than indexed without the guard.
    #[test]
    fn params_without_an_implementation_are_rejected() {
        let mut state = snapshot();
        state.implementations.clear();
        let json = serde_json::to_string(&serde_json::json!({
            "start_block": state.start_block,
            "total_and_external_shares": state.total_and_external_shares,
            "buffered_ether_and_deposited_post_report": state.buffered_ether_and_deposited_post_report,
            "cl_validators_balance_and_cl_pending_balance": state.cl_validators_balance_and_cl_pending_balance,
            "staking_state": state.staking_state,
            "wsteth_shares": state.wsteth_shares,
            "creation_tx": state.creation_tx,
            "implementations": {},
        }))
        .expect("json");

        let err = InitialState::parse(&json).unwrap_err();
        assert!(err.to_string().contains("steth"), "{err}");
    }

    #[test]
    fn malformed_snapshot_words_are_rejected() {
        let manifest = include_str!("../substreams.yaml");
        let json = manifest
            .split("store_balance_slots: &initial_state |\n")
            .nth(1)
            .unwrap()
            .split("  map_protocol_components:")
            .next()
            .unwrap();
        let original: serde_json::Value = serde_json::from_str(json).unwrap();
        for name in [
            "total_and_external_shares",
            "buffered_ether_and_deposited_post_report",
            "cl_validators_balance_and_cl_pending_balance",
            "staking_state",
            "wsteth_shares",
        ] {
            for invalid in [
                "0x".to_string(),
                format!("0x01{}", &original[name].as_str().unwrap()[2..]),
                "0xzz".to_string(),
            ] {
                let mut params = original.clone();
                params[name] = serde_json::Value::String(invalid);
                let error = InitialState::parse(&params.to_string()).expect_err(name);
                assert!(error.to_string().contains(name), "{error}");
            }
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
        let (total_shares, external_shares) = split_low_high_u128(&state.total_and_external_shares);

        // stETH.getTotalShares() and the stVaults' share of them, at block 25603297.
        assert_eq!(big_int_from_u128(total_shares), big("7526667021904051320418763"));
        assert_eq!(big_int_from_u128(external_shares), big("3721126242498807385407"));
    }

    /// The snapshot's creation attributes must decode to the same values the chain holds, and
    /// carry them under the names the simulation reads.
    #[test]
    fn creation_attributes_decode_to_the_chain_values() {
        let attributes = snapshot()
            .creation_attributes()
            .expect("attributes");
        let by_name: std::collections::HashMap<_, _> = attributes
            .iter()
            .map(|a| (a.name.as_str(), BigInt::from_unsigned_bytes_be(&a.value)))
            .collect();

        assert_eq!(by_name.len(), 11);
        // stETH.getTotalShares() and the stVaults' share of it, at block 25603297.
        assert_eq!(by_name["total_shares"], big("7526667021904051320418763"));
        assert_eq!(by_name["external_shares"], big("3721126242498807385407"));
        // stETH.getBufferedEther() and the deposits since the last oracle report.
        assert_eq!(by_name["buffered_ether"], big("539569870340371095571"));
        assert_eq!(by_name["deposited_post_report"], big("761440000000000000000000"));
        assert_eq!(by_name["cl_validators_balance"], big("8567227049119653000000000"));
        assert_eq!(by_name["cl_pending_balance"], big("0"));
        // stETH.sharesOf(wstETH).
        assert_eq!(by_name["wsteth_shares"], big("3628434125893615122886002"));
        // getCurrentStakeLimit() is 150,000 ETH there, which is also the configured maximum.
        assert_eq!(by_name["max_stake_limit"], big("150000000000000000000000"));
    }

    /// The snapshot names a word for every slot in `TRACKED_SLOTS`.
    #[test]
    fn creation_attributes_cover_every_tracked_slot() {
        let names: std::collections::HashSet<_> = snapshot()
            .creation_attributes()
            .expect("attributes")
            .into_iter()
            .map(|a| a.name)
            .collect();

        for slot in crate::constants::TRACKED_SLOTS.iter() {
            for field in slot.fields {
                assert!(
                    names.contains(field.attribute),
                    "{} is not in the snapshot",
                    field.attribute
                );
            }
        }
    }

    #[test]
    fn total_pooled_ether_matches_chain() {
        let state = snapshot()
            .balance_state()
            .expect("balance state");

        // stETH.getTotalPooledEther() at block 25603297.
        assert_eq!(state.total_pooled_ether(), big("9333821188342623875037049"));
    }

    #[test]
    fn total_pooled_ether_counts_the_ether_behind_external_shares() {
        let mut state = snapshot()
            .balance_state()
            .expect("balance state");
        let with_external = state.total_pooled_ether();

        // Keep totalShares, drop externalShares: what is left is the internal ether alone.
        let (total_shares, _) = split_low_high_u128(&state.total_and_external_shares);
        state.apply(TOTAL_AND_EXTERNAL_SHARES_KEY, big_int_from_u128(total_shares));

        // bufferedEther + clValidatorsBalance + clPendingBalance + depositedPostReport at
        // block 25603297; the ~4,615 ETH gap is the stVaults' share of the pool.
        assert_eq!(state.total_pooled_ether(), big("9329206618989993371095571"));
        assert!(with_external > state.total_pooled_ether());
    }

    #[test]
    fn apply_updates_only_the_keyed_slot() {
        let mut state = snapshot()
            .balance_state()
            .expect("balance state");
        let before = state.total_pooled_ether();

        state.apply(BUFFERED_ETHER_AND_DEPOSITED_POST_REPORT_KEY, BigInt::zero());

        // Dropping the buffered/deposited half lowers the pool ...
        assert!(state.total_pooled_ether() < before);
        // ... and leaves the consensus-layer half, which still carries most of it.
        assert!(state.total_pooled_ether() > BigInt::zero());
    }
}
