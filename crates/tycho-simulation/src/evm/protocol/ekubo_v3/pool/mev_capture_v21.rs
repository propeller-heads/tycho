use std::collections::{HashMap, HashSet};

use ekubo_sdk::{
    chain::evm::{EvmPoolKey, EvmTokenAmount},
    math::tick::to_sqrt_ratio,
    quoting::{
        pools::mev_capture_v21_main::{
            MevCaptureV21MainAnchor, MevCaptureV21MainConfig, MevCaptureV21MainCoreState,
            MevCaptureV21MainPool, MevCaptureV21MainPoolKey, MevCaptureV21MainPoolResources,
            MevCaptureV21MainPoolState,
        },
        types::{Pool, QuoteParams, Tick},
    },
    U256,
};
use revm::primitives::Address;
use serde::{Deserialize, Serialize};
use tycho_common::{
    simulation::errors::{SimulationError, TransitionError},
    Bytes,
};

use super::{timed::estimate_block_timestamp, EkuboPool, EkuboPoolQuote};
use crate::{
    evm::protocol::ekubo_v3::attributes::ticks_from_attributes,
    protocol::errors::InvalidSnapshotError,
};

/// Name of the state attribute holding the extension's packed per-pool state word (the storage
/// slot keyed by the pool ID).
pub const ANCHOR_STATE_ATTRIBUTE: &str = "anchor_state";

// Gas of the executor's Ekubo v3 flow around the forwarded swap (same as for other v3 pools).
const TRANSFER_FROM_USER_GAS_COST: u64 = 40_000;
const START_PAYMENT_GAS_COST: u64 = 6_000;
const WITHDRAW_GAS_COST: u64 = 37_500;
const COMPLETE_PAYMENT_GAS_COST: u64 = 2_000;

// Base gas of one Core swap that the forge-fitted constants below are added to, and the cold
// access of the extension through `Core.forward`.
const BASE_CORE_SWAP_GAS_COST: u64 = 7_900;
const COLD_EXTENSION_ACCESS_GAS_COST: u64 = 2_500;

// Fitted with forge on 62 swaps against the same swaps on pools without extension, with the
// deployed configuration: per forwarded swap, Core call, extra Core swap step, initialized tick
// crossed, `accumulateAsFees` and first-of-timestamp anchor update (including the state write).
const EXTRA_BASE_GAS_COST: u64 = 8_250;
const GAS_COST_OF_ONE_CORE_CALL: u64 = 12_350;
const GAS_COST_OF_ONE_EXTRA_CORE_STEP: u64 = 2_850;
const GAS_COST_OF_ONE_TICK_CROSSED: u64 = 13_150;
const GAS_COST_OF_ONE_FEE_ACCUMULATION: u64 = 7_950;
const GAS_COST_OF_ONE_ANCHOR_UPDATE: u64 = 4_650;

/// A concentrated pool behind MEVCapture v2.1, quoted exactly with the SDK's
/// [`MevCaptureV21MainPool`], which reproduces each Core call the extension makes in
/// `Core.forward`.
#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
#[serde(try_from = "MevCaptureV21PoolRepr", into = "MevCaptureV21PoolRepr")]
pub struct MevCaptureV21Pool {
    imp: MevCaptureV21MainPool,
    config: MevCaptureV21MainConfig,
    state: MevCaptureV21MainPoolState,
    /// Whether this state results from a quote in the block being simulated, in which case the
    /// anchor's last update time is that block's timestamp.
    swapped_this_block: bool,
}

#[derive(Serialize, Deserialize)]
struct MevCaptureV21PoolRepr {
    key: MevCaptureV21MainPoolKey,
    config: MevCaptureV21MainConfig,
    state: MevCaptureV21MainPoolState,
    ticks: Vec<Tick>,
    swapped_this_block: bool,
}

impl TryFrom<MevCaptureV21PoolRepr> for MevCaptureV21Pool {
    type Error = String;

    fn try_from(repr: MevCaptureV21PoolRepr) -> Result<Self, Self::Error> {
        let mut pool = Self::new(repr.key, repr.config, repr.state, repr.ticks)
            .map_err(|err| err.to_string())?;
        pool.swapped_this_block = repr.swapped_this_block;
        Ok(pool)
    }
}

impl From<MevCaptureV21Pool> for MevCaptureV21PoolRepr {
    fn from(pool: MevCaptureV21Pool) -> Self {
        Self {
            key: pool.imp.key(),
            config: pool.config,
            state: pool.state,
            ticks: pool.imp.ticks().to_vec(),
            swapped_this_block: pool.swapped_this_block,
        }
    }
}

impl MevCaptureV21Pool {
    /// `config` is the extension's `getConfig`, `state.anchor` the pool's `getAnchor` and
    /// `ticks` all initialized ticks, including those whose net liquidity delta is zero.
    pub fn new(
        key: MevCaptureV21MainPoolKey,
        config: MevCaptureV21MainConfig,
        state: MevCaptureV21MainPoolState,
        ticks: Vec<Tick>,
    ) -> Result<Self, InvalidSnapshotError> {
        Ok(Self {
            imp: MevCaptureV21MainPool::new(key, config, state, ticks).map_err(|err| {
                InvalidSnapshotError::ValueError(format!("creating MEVCapture v2.1 pool: {err}"))
            })?,
            config,
            state,
            swapped_this_block: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn sdk_pool(&self) -> &MevCaptureV21MainPool {
        &self.imp
    }

    fn timestamp(&self) -> Result<u32, SimulationError> {
        // Timestamps are truncated to 32 bits by the extension.
        estimate_block_timestamp(
            self.swapped_this_block,
            u64::from(self.state.anchor.last_update_time),
        )
        .map(|timestamp| timestamp as u32)
    }

    fn rebuild(&mut self, ticks: Vec<Tick>) -> Result<(), TransitionError> {
        self.imp = MevCaptureV21MainPool::new(self.imp.key(), self.config, self.state, ticks)
            .map_err(|err| {
                TransitionError::SimulationError(SimulationError::RecoverableError(format!(
                    "reinstantiate MEVCapture v2.1 pool: {err}"
                )))
            })?;
        Ok(())
    }
}

impl EkuboPool for MevCaptureV21Pool {
    fn key(&self) -> EvmPoolKey {
        self.imp.key().map_into_config()
    }

    fn sqrt_ratio(&self) -> U256 {
        self.state.core.sqrt_ratio
    }

    fn set_sqrt_ratio(&mut self, sqrt_ratio: U256) {
        self.state.core.sqrt_ratio = sqrt_ratio;
    }

    fn set_liquidity(&mut self, liquidity: u128) {
        self.state.core.liquidity = liquidity;
    }

    fn quote(
        &self,
        token_amount: EvmTokenAmount,
        sqrt_ratio_limit: Option<U256>,
    ) -> Result<EkuboPoolQuote, SimulationError> {
        let quote = self
            .imp
            .quote(QuoteParams {
                token_amount,
                sqrt_ratio_limit,
                override_state: Some(self.state),
                meta: u64::from(self.timestamp()?),
            })
            .map_err(|err| SimulationError::RecoverableError(format!("quoting error: {err}")))?;

        Ok(EkuboPoolQuote {
            consumed_amount: quote.consumed_amount,
            calculated_amount: quote.calculated_amount,
            gas: gas_costs(quote.execution_resources),
            new_state: Self {
                imp: self.imp.clone(),
                config: self.config,
                state: quote.state_after,
                swapped_this_block: true,
            }
            .into(),
        })
    }

    fn get_limit(&self, token_in: Address) -> Result<i128, SimulationError> {
        let key = self.imp.key();
        let ticks = self.imp.ticks();
        let sqrt_ratio = self.sqrt_ratio();

        let tick_limit = if token_in == key.token0 { ticks.first() } else { ticks.last() };
        let sqrt_ratio_limit = match tick_limit {
            None => sqrt_ratio,
            Some(tick) => {
                let limit =
                    to_sqrt_ratio::<ekubo_sdk::chain::evm::Evm>(tick.index).ok_or_else(|| {
                        SimulationError::FatalError(
                            "sqrt_ratio should be computable from tick index".to_string(),
                        )
                    })?;
                if token_in == key.token0 {
                    limit.min(sqrt_ratio)
                } else {
                    limit.max(sqrt_ratio)
                }
            }
        };

        // The extension rejects amounts whose Core deltas overflow, so probe with the largest
        // amount Core accepts and let the price limit stop the swap.
        let quote = self
            .imp
            .quote(QuoteParams {
                token_amount: EvmTokenAmount { token: token_in, amount: i128::MAX },
                sqrt_ratio_limit: Some(sqrt_ratio_limit),
                override_state: Some(self.state),
                meta: u64::from(self.timestamp()?),
            })
            .map_err(|err| SimulationError::RecoverableError(format!("quoting error: {err}")))?;

        Ok(quote.consumed_amount.max(0))
    }

    fn finish_transition(
        &mut self,
        mut updated_attributes: HashMap<String, Bytes>,
        deleted_attributes: HashSet<String>,
    ) -> Result<(), TransitionError> {
        if let Some(tick) = updated_attributes.remove("tick") {
            self.state.core.tick = tick.into();
        }

        if let Some(anchor_state) = updated_attributes.remove(ANCHOR_STATE_ATTRIBUTE) {
            self.state.anchor =
                anchor_from_bytes(&anchor_state).map_err(TransitionError::DecodeError)?;
        }

        // A state received from the indexer starts a new block.
        self.swapped_this_block = false;

        // Updated ticks stay initialized even when their net liquidity delta is zero; only
        // deleted ticks are uninitialized.
        let changed_ticks = ticks_from_attributes(
            updated_attributes
                .iter()
                .map(|(key, value)| (key, std::borrow::Cow::Borrowed(value))),
        )
        .map_err(TransitionError::DecodeError)?;
        let deleted_ticks = ticks_from_attributes(
            deleted_attributes
                .iter()
                .map(|key| (key, std::borrow::Cow::Owned(Bytes::new()))),
        )
        .map_err(TransitionError::DecodeError)?;

        let mut ticks = self.imp.ticks().to_vec();
        for tick in changed_ticks {
            match ticks.binary_search_by_key(&tick.index, |t| t.index) {
                Ok(idx) => ticks[idx] = tick,
                Err(idx) => ticks.insert(idx, tick),
            }
        }
        for tick in deleted_ticks {
            if let Ok(idx) = ticks.binary_search_by_key(&tick.index, |t| t.index) {
                ticks.remove(idx);
            }
        }

        self.rebuild(ticks)
    }
}

impl PartialEq for MevCaptureV21Pool {
    fn eq(&self, other: &Self) -> bool {
        self.imp.key() == other.imp.key() &&
            self.imp.ticks() == other.imp.ticks() &&
            self.config == other.config &&
            self.state == other.state
    }
}

/// Decodes the extension's packed per-pool state word (`MEVCaptureV21PoolState`).
pub fn anchor_from_bytes(bytes: &[u8]) -> Result<MevCaptureV21MainAnchor, String> {
    if bytes.len() > 32 {
        return Err(format!("anchor state is {} bytes long", bytes.len()));
    }
    let mut word = [0u8; 32];
    word[32 - bytes.len()..].copy_from_slice(bytes);

    let u32_at = |offset: usize| {
        u32::from_be_bytes(
            word[offset..offset + 4]
                .try_into()
                .unwrap(),
        )
    };

    Ok(MevCaptureV21MainAnchor {
        last_update_time: u32_at(0),
        l_ref_time: u32_at(4),
        last_pos_time: u32_at(8),
        l_ref_bits: word[12],
        snap_bits: word[13],
        l_ref_raise_time: u32_at(14),
        anchor_x16: i64::from_be_bytes(word[24..32].try_into().unwrap()),
    })
}

/// Builds the core state from the indexed Core pool state.
pub fn core_state(sqrt_ratio: U256, tick: i32, liquidity: u128) -> MevCaptureV21MainCoreState {
    MevCaptureV21MainCoreState { sqrt_ratio, tick, liquidity }
}

fn gas_costs(
    MevCaptureV21MainPoolResources {
        core_calls,
        core_steps,
        initialized_ticks_crossed,
        bridges: _,
        tick_event_searches: _,
        accumulate_as_fees,
        anchor_updates,
        state_writes: _,
    }: MevCaptureV21MainPoolResources,
) -> u64 {
    TRANSFER_FROM_USER_GAS_COST +
        START_PAYMENT_GAS_COST +
        WITHDRAW_GAS_COST +
        COMPLETE_PAYMENT_GAS_COST +
        BASE_CORE_SWAP_GAS_COST +
        COLD_EXTENSION_ACCESS_GAS_COST +
        EXTRA_BASE_GAS_COST +
        GAS_COST_OF_ONE_CORE_CALL * u64::from(core_calls) +
        GAS_COST_OF_ONE_EXTRA_CORE_STEP * u64::from(core_steps.saturating_sub(core_calls)) +
        GAS_COST_OF_ONE_TICK_CROSSED * u64::from(initialized_ticks_crossed) +
        GAS_COST_OF_ONE_FEE_ACCUMULATION * u64::from(accumulate_as_fees) +
        GAS_COST_OF_ONE_ANCHOR_UPDATE * u64::from(anchor_updates)
}

/// Parses the extension's `getConfig` from the component's static attributes.
pub fn config_from_attributes(
    attrs: &HashMap<String, Bytes>,
) -> Result<MevCaptureV21MainConfig, InvalidSnapshotError> {
    fn get(attrs: &HashMap<String, Bytes>, key: &str) -> Result<u64, InvalidSnapshotError> {
        let bytes = attrs
            .get(key)
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute(key.to_string()))?;
        if bytes.len() > 8 {
            return Err(InvalidSnapshotError::ValueError(format!("{key} is too long")));
        }
        Ok(bytes
            .iter()
            .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte)))
    }
    fn narrow<T: TryFrom<u64>>(value: u64, key: &str) -> Result<T, InvalidSnapshotError> {
        T::try_from(value)
            .map_err(|_| InvalidSnapshotError::ValueError(format!("{key} is out of range")))
    }

    Ok(MevCaptureV21MainConfig {
        half_life: narrow(get(attrs, "half_life")?, "half_life")?,
        slope_k: narrow(get(attrs, "slope_k")?, "slope_k")?,
        segment_exp: narrow(get(attrs, "segment_exp")?, "segment_exp")?,
        j_lin: narrow(get(attrs, "j_lin")?, "j_lin")?,
        max_fee: get(attrs, "max_fee")?,
        clamp_ticks: narrow(get(attrs, "clamp_ticks")?, "clamp_ticks")?,
        m_gate: narrow(get(attrs, "m_gate")?, "m_gate")?,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy::primitives::aliases::B32;
    use ekubo_sdk::{
        chain::evm::EvmPoolTypeConfig,
        quoting::{
            pools::{
                concentrated::TickSpacing,
                mev_capture_v21_main::{
                    sqrt_ratio_from_raw, sqrt_ratio_to_raw, MevCaptureV21MainCallKind,
                    MevCaptureV21MainSwapParams,
                },
            },
            types::{PoolConfig, PoolKey},
        },
    };
    use revm::primitives::address;
    use serde_json::Value;
    use tycho_client::feed::synchronizer::ComponentWithState;
    use tycho_common::models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        Chain,
    };

    use super::*;
    use crate::evm::protocol::{
        ekubo_v3::{addresses::MEV_CAPTURE_V21_ETHEREUM_ADDRESS, state::EkuboV3State},
        test_utils::try_decode_snapshot_with_defaults,
    };

    const TOKEN0: Address = address!("0x0000000000000000000000000000000000000001");
    const TOKEN1: Address = address!("0x0000000000000000000000000000000000000002");
    const S: i32 = 4096;
    const L66: i128 = 1 << 66;

    fn parse<T: std::str::FromStr>(value: &Value) -> T
    where
        T::Err: std::fmt::Debug,
    {
        match value {
            Value::String(s) => s.parse().unwrap(),
            other => other.to_string().parse().unwrap(),
        }
    }

    fn vectors() -> Vec<Value> {
        let file: Value =
            serde_json::from_str(include_str!("../assets/mev_capture_v21_vectors.json")).unwrap();
        file["vectors"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// Positions minted by the contract test that traced each vector (`test_vectors_*` in
    /// `MEVCaptureV21Rules.t.sol`); the vectors carry the pool state but not the positions.
    fn positions(name: &str) -> Vec<(i32, i32, i128)> {
        let wide = (-40 * S, 40 * S, L66);
        match name {
            "inc_exact_in_cut_bridge_rate" |
            "dec_toward_then_away_exact_in" |
            "inc_exact_out_after_anchor_update" |
            "dec_exact_out_partial" |
            "inc_exact_in_partial_mid_segment" => {
                vec![wide, (3 * S, 7 * S, L66), (-7 * S, -3 * S, L66)]
            }
            "exact_tick_state_dec_from_tick_T" |
            "inc_through_T_toward_uncut" |
            "dec_onto_T_tick_T_minus_1" |
            "exact_tick_state_inc_from_tick_T_minus_1" |
            "dec_onto_T_again" |
            "fee_only_1_wei_exact_in_at_T" => vec![wide, (3 * S, 7 * S, L66)],
            "bridge_25pct_exact_in" | "bridge_25pct_exact_out_dec" | "bridge_1ulp_exact_in" => {
                vec![wide, (2 * S, 3 * S, L66)]
            }
            "empty_words_skip0" | "return_toward_skip0" | "empty_words_skip3" => {
                vec![(-S, S, L66), (1000 * S, 1001 * S, L66)]
            }
            "cap_tail_to_max" => {
                vec![(-20_000 * S, 20_000 * S, 1 << 40), (3000 * S, 3001 * S, 1_000_000_000)]
            }
            "segexp2_offgrid_ticks_inc" => {
                vec![wide, (3 * S, 7 * S, L66 * 2), (8 * S, 9 * S, L66)]
            }
            "spacing3000_negative_anchor_dec" | "spacing3000_negative_anchor_inc" => {
                vec![(-200 * 3000, 100 * 3000, L66), (-50 * 3000, -30 * 3000, L66)]
            }
            _ => panic!("no position layout for {name}"),
        }
    }

    fn ticks(name: &str) -> BTreeMap<i32, i128> {
        let mut net = BTreeMap::new();
        for (lower, upper, liquidity) in positions(name) {
            *net.entry(lower).or_insert(0) += liquidity;
            *net.entry(upper).or_insert(0) -= liquidity;
        }
        net
    }

    fn core(value: &Value) -> MevCaptureV21MainCoreState {
        core_state(
            sqrt_ratio_from_raw(parse(&value["sqrtRatio"])),
            parse(&value["tick"]),
            parse(&value["liquidity"]),
        )
    }

    fn anchor_word(anchor: MevCaptureV21MainAnchor) -> Vec<u8> {
        let mut word = [0u8; 32];
        word[0..4].copy_from_slice(&anchor.last_update_time.to_be_bytes());
        word[4..8].copy_from_slice(&anchor.l_ref_time.to_be_bytes());
        word[8..12].copy_from_slice(&anchor.last_pos_time.to_be_bytes());
        word[12] = anchor.l_ref_bits;
        word[13] = anchor.snap_bits;
        word[14..18].copy_from_slice(&anchor.l_ref_raise_time.to_be_bytes());
        word[24..32].copy_from_slice(&anchor.anchor_x16.to_be_bytes());
        word.to_vec()
    }

    /// The component and state attributes the indexer emits for the vector's pool, with the
    /// vector's anchor already updated at `TIMESTAMP`.
    fn snapshot(vector: &Value, timestamp: u32) -> ComponentWithState {
        let name = vector["name"].as_str().unwrap();
        let cfg = &vector["cfg"];
        let before = core(&vector["before"]);
        let fee: u64 = parse(&vector["pool"]["fee"]);
        let spacing: u32 = parse(&vector["pool"]["tickSpacing"]);

        let mut static_attributes: HashMap<String, Bytes> = [
            ("token0", TOKEN0.to_vec()),
            ("token1", TOKEN1.to_vec()),
            ("extension", MEV_CAPTURE_V21_ETHEREUM_ADDRESS.to_vec()),
            ("fee", fee.to_be_bytes().to_vec()),
            (
                "pool_type_config",
                B32::from(EvmPoolTypeConfig::Concentrated(TickSpacing(spacing))).to_vec(),
            ),
            (
                "half_life",
                parse::<u32>(&cfg["tau"])
                    .to_be_bytes()
                    .to_vec(),
            ),
            ("slope_k", vec![parse::<u8>(&cfg["slopeK"])]),
            ("segment_exp", vec![parse::<u8>(&cfg["segExp"])]),
            ("j_lin", vec![parse::<u8>(&cfg["jLin"])]),
            (
                "max_fee",
                parse::<u64>(&cfg["maxFee"])
                    .to_be_bytes()
                    .to_vec(),
            ),
            (
                "clamp_ticks",
                parse::<u32>(&cfg["clampTicks"])
                    .to_be_bytes()
                    .to_vec(),
            ),
            ("m_gate", vec![parse::<u8>(&cfg["mGate"])]),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.into()))
        .collect();
        static_attributes.insert("extension_id".to_string(), 0_i32.to_be_bytes().into());

        let anchor = MevCaptureV21MainAnchor {
            last_update_time: timestamp,
            l_ref_time: timestamp,
            last_pos_time: 0,
            l_ref_bits: 0,
            snap_bits: 0,
            l_ref_raise_time: timestamp,
            anchor_x16: parse(&vector["anchorX16"]),
        };

        let mut attributes: HashMap<String, Bytes> = [
            ("sqrt_ratio".to_string(), before.sqrt_ratio.to_be_bytes_vec()),
            ("tick".to_string(), before.tick.to_be_bytes().to_vec()),
            ("liquidity".to_string(), before.liquidity.to_be_bytes().to_vec()),
            (ANCHOR_STATE_ATTRIBUTE.to_string(), anchor_word(anchor)),
        ]
        .into_iter()
        .map(|(key, value)| (key, value.into()))
        .collect();
        for (index, delta) in ticks(name) {
            attributes.insert(format!("tick/{index}"), delta.to_be_bytes().to_vec().into());
        }

        ComponentWithState {
            state: ProtocolComponentState::new(name, attributes, HashMap::new()),
            component: ProtocolComponent {
                static_attributes,
                chain: Chain::Ethereum,
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    async fn decode(vector: &Value, timestamp: u32) -> MevCaptureV21Pool {
        match try_decode_snapshot_with_defaults::<EkuboV3State>(snapshot(vector, timestamp))
            .await
            .expect("decoding snapshot")
        {
            EkuboV3State::MevCaptureV21(pool) => pool,
            other => panic!("decoded {other:?}"),
        }
    }

    /// Every contract-traced vector: the quote through the integration matches the traced
    /// totals, and the SDK pool built from the indexed attributes reproduces every Core call.
    #[tokio::test]
    async fn test_golden_vectors() {
        const TIMESTAMP: u32 = 1_000_000;

        let vectors = vectors();
        assert_eq!(vectors.len(), 21);

        let mut calls_checked = 0;
        for vector in &vectors {
            let name = vector["name"].as_str().unwrap();
            let mut pool = decode(vector, TIMESTAMP).await;
            // The anchor was updated at `TIMESTAMP` by an earlier swap in the simulated block.
            pool.swapped_this_block = true;

            let p = &vector["params"];
            let params = MevCaptureV21MainSwapParams {
                is_token1: p["isToken1"].as_bool().unwrap(),
                amount: parse(&p["amount"]),
                sqrt_ratio_limit: Some(sqrt_ratio_from_raw(parse(&p["sqrtRatioLimit"]))),
                skip_ahead: parse(&p["skipAhead"]),
            };
            let (total0, total1): (i128, i128) =
                (parse(&vector["total"]["delta0"]), parse(&vector["total"]["delta1"]));

            // Per-call breakdown, with the vector's skipAhead.
            let swap = pool
                .sdk_pool()
                .swap_through_segments(core(&vector["before"]), params, parse(&vector["anchorX16"]))
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            let expected = vector["calls"].as_array().unwrap();
            assert_eq!(swap.calls.len(), expected.len(), "{name}: call count");
            for (i, (call, e)) in swap
                .calls
                .iter()
                .zip(expected)
                .enumerate()
            {
                let ctx = format!("{name} call {i}");
                let (kind, surcharge) = match call.kind {
                    MevCaptureV21MainCallKind::Charged { surcharge, .. } => ("charged", surcharge),
                    MevCaptureV21MainCallKind::Toward => ("toward", 0),
                    MevCaptureV21MainCallKind::Bridge => ("bridge", 0),
                    MevCaptureV21MainCallKind::PassThrough => ("passThrough", 0),
                };
                assert_eq!(kind, e["kind"].as_str().unwrap(), "{ctx}: kind");
                assert_eq!(surcharge, parse::<u64>(&e["surcharge"]), "{ctx}: surcharge");
                assert_eq!(
                    sqrt_ratio_to_raw(call.sqrt_ratio_limit),
                    parse::<U256>(&e["limit"]),
                    "{ctx}: limit"
                );
                assert_eq!(call.amount, parse::<i128>(&e["amount"]), "{ctx}: amount");
                assert_eq!(call.state_before, core(&e["before"]), "{ctx}: before");
                assert_eq!(call.state_after, core(&e["after"]), "{ctx}: after");
                assert_eq!(call.delta0, parse::<i128>(&e["delta0"]), "{ctx}: delta0");
                assert_eq!(call.delta1, parse::<i128>(&e["delta1"]), "{ctx}: delta1");
                assert_eq!(call.surcharge0, parse::<u128>(&e["fee0"]), "{ctx}: fee0");
                assert_eq!(call.surcharge1, parse::<u128>(&e["fee1"]), "{ctx}: fee1");
                calls_checked += 1;
            }
            assert_eq!((swap.delta0, swap.delta1), (total0, total1), "{name}: totals");
            assert_eq!(swap.state_after.core, core(&vector["after"]), "{name}: state after");

            // The integration quotes with skipAhead 0, as the executor swaps.
            if params.skip_ahead != 0 {
                continue;
            }
            let token = if params.is_token1 { TOKEN1 } else { TOKEN0 };
            let quote = pool
                .quote(EvmTokenAmount { token, amount: params.amount }, params.sqrt_ratio_limit)
                .unwrap_or_else(|err| panic!("{name}: {err}"));
            let (specified, calculated) =
                if params.is_token1 { (total1, total0) } else { (total0, total1) };
            assert_eq!(quote.consumed_amount, specified, "{name}: consumed");
            assert_eq!(quote.calculated_amount, calculated.unsigned_abs(), "{name}: calculated");

            let EkuboV3State::MevCaptureV21(after) = quote.new_state else {
                panic!("{name}: unexpected state after quote");
            };
            assert_eq!(after.state.core, core(&vector["after"]), "{name}: quoted state after");
            assert_eq!(after.state.anchor.anchor_x16, parse::<i64>(&vector["anchorX16"]));
        }
        assert_eq!(calls_checked, 266);
    }

    /// The first swap in a new block updates the anchor before swapping; later quotes in the
    /// same simulated block reuse that block's timestamp.
    #[tokio::test]
    async fn test_first_swap_of_block_updates_anchor() {
        let vector = vectors()
            .into_iter()
            .find(|v| v["name"] == "dec_toward_then_away_exact_in")
            .unwrap();
        let last_update_time =
            (crate::evm::protocol::ekubo_v3::test_cases::TEST_TIMESTAMP as u32) - 5;
        let pool = decode(&vector, last_update_time).await;
        let token = TOKEN0;

        let first = pool
            .quote(EvmTokenAmount { token, amount: 1_000_000_000 }, None)
            .unwrap();
        let EkuboV3State::MevCaptureV21(after_first) = first.new_state else { panic!() };
        // The block timestamp is estimated as one slot after the last update.
        assert_eq!(
            after_first
                .state
                .anchor
                .last_update_time,
            last_update_time + 12
        );
        assert!(after_first.swapped_this_block);
        assert_ne!(after_first.state.anchor, pool.state.anchor);

        let second = after_first
            .quote(EvmTokenAmount { token, amount: 1_000_000_000 }, None)
            .unwrap();
        let EkuboV3State::MevCaptureV21(after_second) = second.new_state else { panic!() };
        assert_eq!(
            after_second
                .state
                .anchor
                .last_update_time,
            last_update_time + 12
        );
        assert_eq!(after_second.state.anchor.anchor_x16, after_first.state.anchor.anchor_x16);
        // Only the first swap of the block pays for the anchor update.
        assert!(first.gas > second.gas);
    }

    #[tokio::test]
    async fn test_transition_keeps_zero_delta_ticks_initialized() {
        let vector = vectors()
            .into_iter()
            .find(|v| v["name"] == "bridge_25pct_exact_in")
            .unwrap();
        let mut pool = decode(&vector, 1_000).await;
        let ticks_before = pool.imp.ticks().to_vec();
        let far = 39 * S;

        pool.finish_transition(
            [(format!("tick/{far}"), 0_i128.to_be_bytes().to_vec().into())].into(),
            HashSet::new(),
        )
        .unwrap();
        assert!(pool
            .imp
            .ticks()
            .iter()
            .any(|t| t.index == far && t.liquidity_delta == 0));

        pool.finish_transition(HashMap::new(), [format!("tick/{far}")].into())
            .unwrap();
        assert_eq!(pool.imp.ticks(), ticks_before);
    }

    #[tokio::test]
    async fn test_transition_updates_anchor_and_tick() {
        let vector = vectors()
            .into_iter()
            .find(|v| v["name"] == "bridge_25pct_exact_in")
            .unwrap();
        let mut pool = decode(&vector, 1_000).await;
        pool.swapped_this_block = true;

        let anchor = MevCaptureV21MainAnchor { anchor_x16: 5 << 16, ..pool.state.anchor };
        pool.finish_transition(
            [
                (ANCHOR_STATE_ATTRIBUTE.to_string(), anchor_word(anchor).into()),
                ("tick".to_string(), (-1_i32).to_be_bytes().to_vec().into()),
            ]
            .into(),
            HashSet::new(),
        )
        .unwrap();

        assert_eq!(pool.state.anchor, anchor);
        assert_eq!(pool.state.core.tick, -1);
        assert!(!pool.swapped_this_block);
    }

    /// The swaps of `EkuboV3MevCaptureV21Test` (tycho-execution), executed through the executor
    /// and `Core.forward` against the extension on an Ethereum fork, on a pool the test creates.
    /// The executed amounts and resulting states, logged by that test, equal these quotes.
    #[test]
    fn test_fork_route_quotes() {
        const USDC: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        const USDT: Address = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
        const L: i128 = 40_000_000_000_000;

        fn word(hex: &str) -> MevCaptureV21MainAnchor {
            anchor_from_bytes(&alloy::hex::decode(hex).unwrap()).unwrap()
        }
        fn core_from_raw(raw: &str, tick: i32, liquidity: u128) -> MevCaptureV21MainCoreState {
            core_state(sqrt_ratio_from_raw(raw.parse().unwrap()), tick, liquidity)
        }

        // `getConfig` of the deployed extension.
        let config = MevCaptureV21MainConfig {
            half_life: 120,
            slope_k: 4,
            segment_exp: 0,
            j_lin: 16,
            max_fee: 1 << 63,
            clamp_ticks: 2500,
            m_gate: 3,
        };
        let ticks = [
            (-50_000, L),
            (-3000, L),
            (-1000, -L),
            (1000, L),
            (2000, L / 2),
            (3000, -L),
            (4000, 0),
            (6000, -L / 2),
            (50_000, -L),
        ]
        .map(|(index, liquidity_delta)| Tick { index, liquidity_delta })
        .to_vec();

        let seeded = MevCaptureV21Pool::new(
            PoolKey {
                token0: USDC,
                token1: USDT,
                config: PoolConfig {
                    extension: MEV_CAPTURE_V21_ETHEREUM_ADDRESS,
                    fee: 0x0020_c49b_a5e3_53f7,
                    pool_type_config: TickSpacing(1000),
                },
            },
            config,
            MevCaptureV21MainPoolState {
                core: core_from_raw("39614081261743854815199363072", 0, 40_000_000_000_000),
                anchor: word("6ada581f6ada581f6ada581f00006ada57a70000000000000000000000000000"),
            },
            ticks,
        )
        .unwrap();

        let swap = |pool: &MevCaptureV21Pool, token: Address, amount: i128| {
            let quote = pool
                .quote(EvmTokenAmount { token, amount }, None)
                .unwrap();
            assert_eq!(quote.consumed_amount, amount);
            let EkuboV3State::MevCaptureV21(next) = quote.new_state else { panic!() };
            (quote.calculated_amount, quote.gas, next)
        };

        // Block 1 (one slot later): away from the anchor, then back toward it.
        let (away_dec, away_dec_gas, pool) = swap(&seeded, USDC, 50_000_000_000);
        let (toward_inc, toward_inc_gas, mut pool) = swap(&pool, USDT, 10_000_000_000);
        assert_eq!((away_dec, toward_inc), (49_868_947_729, 10_011_242_024));
        assert_eq!(
            pool.state,
            MevCaptureV21MainPoolState {
                core: core_from_raw("39599244835603686790750605543", -1499, 80_000_000_000_000),
                anchor: word("6ada582b6ada582b6ada581f01006ada582b0000000000000000000000000000"),
            }
        );

        // Block 2: the indexer delivers the new state.
        pool.finish_transition(HashMap::new(), HashSet::new())
            .unwrap();
        let (away_inc, away_inc_gas, pool) = swap(&pool, USDT, 150_000_000_000);
        let (toward_dec, toward_dec_gas, pool) = swap(&pool, USDC, 5_000_000_000);
        assert_eq!((away_inc, toward_dec), (149_559_555_721, 5_012_253_996));
        assert_eq!(
            pool.state,
            MevCaptureV21MainPoolState {
                core: core_from_raw("39614081261750541708812399991", 2897, 100_000_000_000_000),
                anchor: word("6ada58376ada58376ada581f01006ada582b000000000000ffffffffff9b9dcf"),
            }
        );

        println!(
            "gas estimates: away_dec {away_dec_gas}, toward_inc {toward_inc_gas}, \
             away_inc {away_inc_gas}, toward_dec {toward_dec_gas}"
        );
    }

    #[test]
    fn test_anchor_from_bytes() {
        let anchor = MevCaptureV21MainAnchor {
            last_update_time: 0x0102_0304,
            l_ref_time: 0x0506_0708,
            last_pos_time: 0x090a_0b0c,
            l_ref_bits: 0x0d,
            snap_bits: 0x0e,
            l_ref_raise_time: 0x0f10_1112,
            anchor_x16: -123_456_789,
        };
        assert_eq!(anchor_from_bytes(&anchor_word(anchor)), Ok(anchor));
        // Leading zero bytes may be trimmed.
        assert_eq!(
            anchor_from_bytes(&[7]),
            Ok(MevCaptureV21MainAnchor {
                last_update_time: 0,
                l_ref_time: 0,
                last_pos_time: 0,
                l_ref_bits: 0,
                snap_bits: 0,
                l_ref_raise_time: 0,
                anchor_x16: 7,
            })
        );
        assert!(anchor_from_bytes(&[0; 33]).is_err());
    }

    #[tokio::test]
    async fn test_serde_round_trip() {
        let vector = vectors().remove(0);
        let pool = decode(&vector, 1_000).await;
        let json = serde_json::to_string(&pool).unwrap();
        let decoded: MevCaptureV21Pool = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, pool);
    }

    #[tokio::test]
    async fn test_get_limit_is_swappable() {
        let vector = vectors()
            .into_iter()
            .find(|v| v["name"] == "dec_toward_then_away_exact_in")
            .unwrap();
        let pool = decode(&vector, 1_000).await;
        for token in [TOKEN0, TOKEN1] {
            let limit = pool.get_limit(token).unwrap();
            assert!(limit > 0);
            let quote = pool
                .quote(EvmTokenAmount { token, amount: limit }, None)
                .unwrap();
            assert_eq!(quote.consumed_amount, limit);
        }
    }

    #[test]
    fn test_ethereum_only() {
        use crate::evm::protocol::ekubo_v3::decoder::{extension_type, ExtensionType};
        assert!(matches!(
            extension_type(MEV_CAPTURE_V21_ETHEREUM_ADDRESS, Chain::Ethereum),
            Some(ExtensionType::MevCaptureV21)
        ));
        assert!(extension_type(MEV_CAPTURE_V21_ETHEREUM_ADDRESS, Chain::Base).is_none());
    }
}
