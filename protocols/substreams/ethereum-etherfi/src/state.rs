use std::collections::HashMap;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use substreams::scalar::BigInt;
use tycho_substreams::models::{Attribute, ChangeType};

use crate::{
    constants::{
        Component, TrackedProxy, TrackedSlot, EETH_BURN_LIMIT_SLOT, EETH_MINT_LIMIT_SLOT,
        EETH_TOTAL_SHARES_SLOT, ETH_REDEMPTION_INFO_SLOT, ETH_REDEMPTION_LIMIT_SLOT,
        LIQUIDITY_POOL_VALUE_KEY, LIQUIDITY_POOL_VALUE_SLOT, TOTAL_SHARES_KEY, TRACKED_PROXIES,
        WEETH_SHARES_KEY, WEETH_SHARES_SLOT,
    },
    utils::{attribute_with_bytes, bytes_from_hex},
};

/// Raw chain state at `start_block`, carried in the module params.
///
/// The contracts predate any block worth indexing from and the attributes are read from storage
/// writes, so a slot that does not move inside the indexed range would never be reported. The
/// snapshot seeds every tracked slot once; every field is the raw 32-byte word, decoded through
/// the same [`TrackedSlot`] definitions as a live write.
#[derive(Clone, Debug, Deserialize)]
pub struct InitialState {
    pub start_block: u64,
    /// Transaction in `start_block` the components are anchored to.
    pub creation_tx: String,
    pub liquidity_pool_value: String,
    pub eeth_total_shares: String,
    pub weeth_shares: String,
    pub eth_redemption_limit: String,
    pub eth_redemption_info: String,
    pub eeth_mint_limit: String,
    pub eeth_burn_limit: String,
    /// The implementation behind each tracked proxy at `start_block`, keyed by
    /// [`TrackedProxy::label`]. The slots above were verified against these and no others.
    pub implementations: HashMap<String, String>,
}

impl InitialState {
    /// Parses the manifest params, requiring an implementation for every tracked proxy: a proxy
    /// with none recorded could never be found upgraded.
    pub fn parse(params: &str) -> Result<Self> {
        let state: Self = serde_json::from_str(params)
            .map_err(|e| anyhow!("Failed to parse EtherFi initial state: {e}"))?;
        for proxy in TRACKED_PROXIES.iter() {
            state.implementation_of(proxy)?;
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

    /// Each tracked slot with the snapshot word it decodes from.
    fn words(&self) -> [(&'static TrackedSlot, &str); 7] {
        [
            (&LIQUIDITY_POOL_VALUE_SLOT, &self.liquidity_pool_value),
            (&EETH_TOTAL_SHARES_SLOT, &self.eeth_total_shares),
            (&WEETH_SHARES_SLOT, &self.weeth_shares),
            (&ETH_REDEMPTION_LIMIT_SLOT, &self.eth_redemption_limit),
            (&ETH_REDEMPTION_INFO_SLOT, &self.eth_redemption_info),
            (&EETH_MINT_LIMIT_SLOT, &self.eeth_mint_limit),
            (&EETH_BURN_LIMIT_SLOT, &self.eeth_burn_limit),
        ]
    }

    /// The attributes `component` carries, unpacked the same way a storage write is.
    pub fn creation_attributes(&self, component: Component) -> Result<Vec<Attribute>> {
        let mut attributes = Vec::new();
        for (slot, word) in self.words() {
            if !slot.components.contains(&component) {
                continue;
            }
            attributes.extend(unpack_fields(slot, &bytes_from_hex(word)?, ChangeType::Creation));
        }
        Ok(attributes)
    }

    /// The slot values the component balances are derived from.
    pub fn balance_state(&self) -> Result<BalanceState> {
        Ok(BalanceState {
            liquidity_pool_value: big_int_from_hex(&self.liquidity_pool_value)?,
            total_shares: big_int_from_hex(&self.eeth_total_shares)?,
            weeth_shares: big_int_from_hex(&self.weeth_shares)?,
        })
    }
}

/// Reports each value packed into `word` as its own attribute.
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

/// The raw slot values that determine the two component balances.
///
/// The pool reports the ETH it can pay redemptions from, and the wrapper reports the eETH its
/// shares are worth at the current rate, which moves with every rebase. Both are the protocol's
/// own accounting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BalanceState {
    pub liquidity_pool_value: BigInt,
    pub total_shares: BigInt,
    pub weeth_shares: BigInt,
}

impl BalanceState {
    /// `LiquidityPool.totalValueInLp()`, the high half of the value word.
    pub fn pool_eth_balance(&self) -> BigInt {
        self.liquidity_pool_value.clone() >> 128u32
    }

    /// `LiquidityPool.getTotalPooledEther()`: both halves of the value word.
    fn total_pooled_ether(&self) -> BigInt {
        let low_mask = (BigInt::one() << 128u32) - BigInt::one();
        (self.liquidity_pool_value.clone() & low_mask) + self.pool_eth_balance()
    }

    /// `eETH.balanceOf(weETH)`: the wrapper's shares at the current share rate, rounded down as
    /// `LiquidityPool.amountForShare` rounds.
    pub fn wrapper_eeth_balance(&self) -> BigInt {
        if self.total_shares == BigInt::zero() {
            return BigInt::zero();
        }
        self.weeth_shares.clone() * self.total_pooled_ether() / self.total_shares.clone()
    }

    /// Applies a newly observed raw slot value, keyed as in the store.
    pub fn apply(&mut self, key: &str, value: BigInt) {
        if key == LIQUIDITY_POOL_VALUE_KEY {
            self.liquidity_pool_value = value;
        } else if key == TOTAL_SHARES_KEY {
            self.total_shares = value;
        } else if key == WEETH_SHARES_KEY {
            self.weeth_shares = value;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::constants::TRACKED_SLOTS;

    /// Chain state at block 25940000.
    pub(crate) fn snapshot() -> InitialState {
        InitialState {
            start_block: 25_940_000,
            creation_tx: "0xdc387d47c733ca16693792c12f81b3a038930489b2b89dfcae14b6c4f293fe56"
                .to_string(),
            liquidity_pool_value:
                "0x00000000000000390066975346099296000000000001d354d821f08629f66d6a".to_string(),
            eeth_total_shares: "0x00000000000000000000000000000000000000000001a7c7a0870fdc1d9275d1"
                .to_string(),
            weeth_shares: "0x0000000000000000000000000000000000000000000199a703089f7b8ed2abd1"
                .to_string(),
            eth_redemption_limit:
                "0x0000000000005a6c000000006a8413270000000077307d560000000077359400".to_string(),
            eth_redemption_info:
                "0x00000000000000000000000000000000000000000000000000000064001e03e8".to_string(),
            eeth_mint_limit: "0x000000052c8c338e000000006aa1515300002460bc2c859e0000246139ca8000"
                .to_string(),
            eeth_burn_limit: "0x00000000677af407000000006aa0ce13000016b6f226fb9f000016bcc41e9000"
                .to_string(),
            implementations: recorded_implementations(),
        }
    }

    /// The implementations behind the five proxies at block 25940000.
    pub(crate) fn recorded_implementations() -> HashMap<String, String> {
        [
            ("liquidity_pool", "0x17a16747d03006c9754548ac0d0aff48783a4a45"),
            ("eeth", "0xd1901dd36cbf4a81386d0162df2707f7ddb60527"),
            ("weeth", "0xa6ca0607190d03cf16fe6f2865cf40c3d160ccf3"),
            ("redemption_manager", "0x5d53b303d62a7861f88650045b8d5deb59dfb3dc"),
            ("rate_limiter", "0x9ea4d0fd09b628e23b1998f2153e27e5261b1b67"),
        ]
        .into_iter()
        .map(|(label, implementation)| (label.to_string(), implementation.to_string()))
        .collect()
    }

    /// A proxy with no recorded implementation could never be found upgraded, so the params are
    /// refused rather than indexed without the guard.
    #[test]
    fn params_missing_an_implementation_are_rejected() {
        let state = snapshot();
        let mut implementations = state.implementations.clone();
        implementations.remove("rate_limiter");
        let json = serde_json::to_string(&serde_json::json!({
            "start_block": state.start_block,
            "creation_tx": state.creation_tx,
            "liquidity_pool_value": state.liquidity_pool_value,
            "eeth_total_shares": state.eeth_total_shares,
            "weeth_shares": state.weeth_shares,
            "eth_redemption_limit": state.eth_redemption_limit,
            "eth_redemption_info": state.eth_redemption_info,
            "eeth_mint_limit": state.eeth_mint_limit,
            "eeth_burn_limit": state.eeth_burn_limit,
            "implementations": implementations,
        }))
        .expect("json");

        let err = InitialState::parse(&json).unwrap_err();
        assert!(err.to_string().contains("rate_limiter"), "{err}");
    }

    fn big(value: &str) -> BigInt {
        value
            .parse::<BigInt>()
            .expect("decimal BigInt")
    }

    fn by_name(attributes: &[Attribute]) -> HashMap<&str, BigInt> {
        attributes
            .iter()
            .map(|a| (a.name.as_str(), BigInt::from_unsigned_bytes_be(&a.value)))
            .collect()
    }

    /// The snapshot's creation attributes must decode to the values the chain's getters return
    /// at block 25940000, under the names the simulation reads.
    #[test]
    fn pool_attributes_decode_to_the_chain_values() {
        let attributes = snapshot()
            .creation_attributes(Component::Pool)
            .expect("attributes");
        let values = by_name(&attributes);

        assert_eq!(values.len(), 18);
        // LiquidityPool.totalValueOutOfLp() / totalValueInLp()
        assert_eq!(values["total_value_out_of_lp"], big("2206910247995761361317226"));
        assert_eq!(values["total_value_in_lp"], big("1051493289032982041238"));
        // eETH.totalShares()
        assert_eq!(values["total_shares"], big("2001243491556134113932753"));
        // EtherFiRedemptionManager.tokenToRedemptionInfo(ETH)
        assert_eq!(values["redemption_bucket_capacity"], big("2000000000"));
        assert_eq!(values["redemption_bucket_remaining"], big("1999666518"));
        assert_eq!(values["redemption_bucket_last_refill"], big("1787040551"));
        assert_eq!(values["redemption_bucket_refill_rate"], big("23148"));
        assert_eq!(values["exit_fee_split_to_treasury_bps"], big("1000"));
        assert_eq!(values["exit_fee_bps"], big("30"));
        assert_eq!(values["low_watermark_bps"], big("100"));
        // EtherFiRateLimiter.getLimit(EETH_MINT_LIMIT_ID) / getLimit(EETH_BURN_LIMIT_ID)
        assert_eq!(values["mint_bucket_capacity"], big("40000000000000"));
        assert_eq!(values["mint_bucket_remaining"], big("39997892494750"));
        assert_eq!(values["mint_bucket_last_refill"], big("1788957011"));
        assert_eq!(values["mint_bucket_refill_rate"], big("22222222222"));
        assert_eq!(values["burn_bucket_capacity"], big("25000000000000"));
        assert_eq!(values["burn_bucket_remaining"], big("24975002499999"));
        assert_eq!(values["burn_bucket_last_refill"], big("1788923411"));
        assert_eq!(values["burn_bucket_refill_rate"], big("1736111111"));
    }

    #[test]
    fn wrapper_attributes_decode_to_the_chain_values() {
        let attributes = snapshot()
            .creation_attributes(Component::Wrapper)
            .expect("attributes");
        let values = by_name(&attributes);

        assert_eq!(values.len(), 4);
        assert_eq!(values["total_value_out_of_lp"], big("2206910247995761361317226"));
        assert_eq!(values["total_value_in_lp"], big("1051493289032982041238"));
        assert_eq!(values["total_shares"], big("2001243491556134113932753"));
        // eETH.shares(weETH)
        assert_eq!(values["weeth_shares"], big("1934528716353929340955601"));
    }

    /// Every tracked slot has a snapshot word.
    #[test]
    fn creation_attributes_cover_every_tracked_slot() {
        let state = snapshot();
        let names: HashSet<String> = [Component::Pool, Component::Wrapper]
            .into_iter()
            .flat_map(|component| {
                state
                    .creation_attributes(component)
                    .expect("attributes")
            })
            .map(|a| a.name)
            .collect();

        for slot in TRACKED_SLOTS.iter() {
            for field in slot.fields {
                assert!(
                    names.contains(field.attribute),
                    "{} is not in the snapshot",
                    field.attribute
                );
            }
        }
    }

    /// `LiquidityPool.totalValueInLp()` at block 25940000, which is also
    /// `EtherFiRedemptionManager.getInstantLiquidityAmount(ETH)`.
    #[test]
    fn pool_balance_is_the_liquid_ether() {
        let state = snapshot()
            .balance_state()
            .expect("balance state");

        assert_eq!(state.pool_eth_balance(), big("1051493289032982041238"));
    }

    /// `eETH.balanceOf(weETH)` at block 25940000, to the wei.
    #[test]
    fn wrapper_balance_matches_the_chain() {
        let state = snapshot()
            .balance_state()
            .expect("balance state");

        assert_eq!(state.wrapper_eeth_balance(), big("2134355669936453442791966"));
    }

    /// A rebase moves `totalValueOutOfLp` and nothing else, and the wrapper balance follows
    /// it.
    #[test]
    fn wrapper_balance_follows_a_rebase() {
        let mut state = snapshot()
            .balance_state()
            .expect("balance state");
        let before = state.wrapper_eeth_balance();

        let one_eth = BigInt::from(10u64).pow(18);
        state.apply(LIQUIDITY_POOL_VALUE_KEY, state.liquidity_pool_value.clone() + one_eth);

        assert!(state.wrapper_eeth_balance() > before);
        assert_eq!(state.pool_eth_balance(), big("1051493289032982041238"));
    }

    #[test]
    fn apply_updates_only_the_keyed_slot() {
        let mut state = snapshot()
            .balance_state()
            .expect("balance state");
        let pool_before = state.pool_eth_balance();

        state.apply(WEETH_SHARES_KEY, BigInt::zero());

        assert_eq!(state.wrapper_eeth_balance(), BigInt::zero());
        assert_eq!(state.pool_eth_balance(), pool_before);
    }

    #[test]
    fn wrapper_balance_is_zero_without_shares_outstanding() {
        let state = BalanceState {
            liquidity_pool_value: big("1"),
            total_shares: BigInt::zero(),
            weeth_shares: big("1"),
        };
        assert_eq!(state.wrapper_eeth_balance(), BigInt::zero());
    }
}
