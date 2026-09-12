use anyhow::{anyhow, Result};
use serde::Deserialize;
use substreams::scalar::BigInt;
use tycho_substreams::models::{Attribute, ChangeType};

use crate::storage::{read_bytes, StorageLocation};

/// Raw chain state at `start_block`, carried in the module params.
///
/// Both EtherFi contracts predate any block we want to index from, and the component attributes
/// are read from storage *changes* - a slot that does not move in the indexed range would never
/// be reported. Seeding the components from a snapshot lets the package start at a recent block
/// instead of replaying from mid-2023.
///
/// Every field is the raw 32-byte storage word (or, for the balances, a plain integer), so the
/// snapshot decodes through the same [`StorageLocation`] definitions the update path uses and
/// cannot drift from it.
#[derive(Clone, Debug, Deserialize)]
pub struct InitialState {
    pub start_block: u64,
    /// LiquidityPool slot holding `totalValueOutOfLp` and `totalValueInLp`.
    pub liquidity_pool_value_slot: String,
    /// LiquidityPool slot holding `ethAmountLockedForWithdrawl`.
    pub liquidity_pool_locked_slot: String,
    /// eETH slot holding `totalShares`.
    pub eeth_total_shares_slot: String,
    /// RedemptionManager slot holding `ethBucketLimiter`.
    pub eth_bucket_limiter_slot: String,
    /// RedemptionManager slot holding `ethRedemptionInfo`.
    pub eth_redemption_info_slot: String,
    /// The LiquidityPool's native ETH balance, which backs the eETH component.
    pub liquidity_pool_native_balance: String,
    /// The eETH the weETH wrapper holds, which backs the weETH component.
    pub weeth_eeth_balance: String,
}

impl InitialState {
    pub fn parse(params: &str) -> Result<Self> {
        serde_json::from_str(params)
            .map_err(|e| anyhow!("Failed to parse EtherFi initial state: {e}"))
    }

    /// Creation attributes for the given tracked slots, decoded exactly as
    /// [`crate::storage::get_changed_attributes`] decodes an update.
    pub fn creation_attributes(&self, locations: &[StorageLocation]) -> Result<Vec<Attribute>> {
        locations
            .iter()
            .map(|location| {
                let word = self.word_for(location.name)?;
                let data = read_bytes(&word, location.offset, location.number_of_bytes);
                let value = if location.signed {
                    BigInt::from_signed_bytes_be(data)
                } else {
                    BigInt::from_unsigned_bytes_be(data)
                };
                Ok(Attribute {
                    name: location.name.to_string(),
                    value: value.to_signed_bytes_be(),
                    change: ChangeType::Creation.into(),
                })
            })
            .collect()
    }

    /// The LiquidityPool's native balance, which the eETH component publishes as an attribute
    /// rather than reading from storage.
    pub fn liquidity_pool_native_balance_attribute(&self) -> Result<Attribute> {
        Ok(Attribute {
            name: "liquidityPoolNativeBalance".to_string(),
            value: self
                .liquidity_pool_native_balance()?
                .to_signed_bytes_be(),
            change: ChangeType::Creation.into(),
        })
    }

    pub fn liquidity_pool_native_balance(&self) -> Result<BigInt> {
        decimal(&self.liquidity_pool_native_balance)
    }

    pub fn weeth_eeth_balance(&self) -> Result<BigInt> {
        decimal(&self.weeth_eeth_balance)
    }

    /// A tracked slot without a snapshot value is a hard error: it would otherwise seed as zero
    /// and the component would decode against state that never existed.
    fn word_for(&self, attribute: &str) -> Result<Vec<u8>> {
        let hex = match attribute {
            "totalValueOutOfLp" | "totalValueInLp" => &self.liquidity_pool_value_slot,
            "ethAmountLockedForWithdrawl" => &self.liquidity_pool_locked_slot,
            "totalShares" => &self.eeth_total_shares_slot,
            "ethBucketLimiter" => &self.eth_bucket_limiter_slot,
            "ethRedemptionInfo" => &self.eth_redemption_info_slot,
            other => return Err(anyhow!("no snapshot value for tracked slot {other}")),
        };
        storage_word(hex)
    }
}

/// Decodes a hex-encoded storage word, left-padded to the full 32 bytes the slot layouts assume.
fn storage_word(value: &str) -> Result<Vec<u8>> {
    let value = value
        .strip_prefix("0x")
        .unwrap_or(value);
    let bytes = hex::decode(value).map_err(|e| anyhow!("Failed to decode storage word: {e}"))?;
    if bytes.len() > 32 {
        return Err(anyhow!("storage word longer than 32 bytes: {} bytes", bytes.len()));
    }
    let mut word = vec![0u8; 32 - bytes.len()];
    word.extend_from_slice(&bytes);
    Ok(word)
}

fn decimal(value: &str) -> Result<BigInt> {
    value
        .parse::<BigInt>()
        .map_err(|e| anyhow!("Failed to parse decimal {value}: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Chain state at block 25940000.
    fn snapshot() -> InitialState {
        InitialState {
            start_block: 25_940_000,
            liquidity_pool_value_slot:
                "0x00000000000000390066975346099296000000000001d354d821f08629f66d6a".to_string(),
            liquidity_pool_locked_slot:
                "0x0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            eeth_total_shares_slot:
                "0x00000000000000000000000000000000000000000001a7c7a0870fdc1d9275d1".to_string(),
            eth_bucket_limiter_slot:
                "0x0000000000005a6c000000006a8413270000000077307d560000000077359400".to_string(),
            eth_redemption_info_slot:
                "0x00000000000000000000000000000000000000000000000000000064001e03e8".to_string(),
            liquidity_pool_native_balance: "1051493289032982041238".to_string(),
            weeth_eeth_balance: "2134355669936453442791966".to_string(),
        }
    }

    fn attribute_value(attributes: &[Attribute], name: &str) -> BigInt {
        let raw = attributes
            .iter()
            .find(|attribute| attribute.name == name)
            .unwrap_or_else(|| panic!("missing attribute {name}"))
            .value
            .clone();
        BigInt::from_signed_bytes_be(&raw)
    }

    #[test]
    fn packed_slots_decode_to_the_chain_values() {
        let attributes = snapshot()
            .creation_attributes(&crate::storage::EETH_POOL_TRACKED_SLOTS)
            .expect("attributes");

        // LiquidityPool.totalValueOutOfLp / totalValueInLp share one slot; the latter equals the
        // pool's native balance at this block, which is read independently.
        assert_eq!(
            attribute_value(&attributes, "totalValueOutOfLp"),
            decimal("2206910247995761361317226").unwrap()
        );
        assert_eq!(
            attribute_value(&attributes, "totalValueInLp"),
            decimal("1051493289032982041238").unwrap()
        );
        assert_eq!(
            attribute_value(&attributes, "totalValueInLp"),
            snapshot()
                .liquidity_pool_native_balance()
                .unwrap()
        );
        assert_eq!(
            attribute_value(&attributes, "totalShares"),
            decimal("2001243491556134113932753").unwrap()
        );
        // Offset 1 into the slot, so the trailing byte belongs to another variable.
        assert_eq!(attribute_value(&attributes, "ethAmountLockedForWithdrawl"), BigInt::from(0));
    }

    #[test]
    fn weeth_snapshot_covers_only_its_tracked_slots() {
        let attributes = snapshot()
            .creation_attributes(&crate::storage::WEETH_POOL_TRACKED_SLOTS)
            .expect("attributes");

        assert_eq!(attributes.len(), crate::storage::WEETH_POOL_TRACKED_SLOTS.len());
        assert!(attributes
            .iter()
            .all(|attribute| attribute.change == ChangeType::Creation as i32));
    }

    #[test]
    fn a_tracked_slot_without_a_snapshot_value_is_rejected() {
        let unknown = [StorageLocation {
            name: "somethingNew",
            slot: [0u8; 32],
            offset: 0,
            number_of_bytes: 32,
            signed: false,
        }];

        assert!(snapshot()
            .creation_attributes(&unknown)
            .is_err());
    }
}
