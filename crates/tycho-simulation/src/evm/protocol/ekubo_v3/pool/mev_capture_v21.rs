use std::collections::{HashMap, HashSet};

use ekubo_sdk::{
    chain::evm::{
        Evm, EvmConcentratedPoolResources, EvmPoolKey, EvmTokenAmount, EVM_MAX_TICK, EVM_MIN_TICK,
    },
    math::tick::to_sqrt_ratio,
    quoting::{
        pools::{
            concentrated::TickSpacing,
            mev_capture_v21_main::{
                MevCaptureV21MainAnchor, MevCaptureV21MainConfig, MevCaptureV21MainCoreState,
                MevCaptureV21MainPool, MevCaptureV21MainPoolKey, MevCaptureV21MainPoolResources,
                MevCaptureV21MainPoolState,
            },
        },
        types::{Pool, Quote, QuoteParams, Tick},
    },
    U256,
};
use revm::primitives::Address;
use serde::{Deserialize, Serialize};
use tycho_common::{
    simulation::errors::{SimulationError, TransitionError},
    Bytes,
};

use super::{concentrated, timed::estimate_block_timestamp, EkuboPool, EkuboPoolQuote};
use crate::{
    evm::protocol::ekubo_v3::attributes::ticks_from_attributes,
    protocol::errors::InvalidSnapshotError,
};

/// Name of the state attribute holding the extension's packed per-pool state word (the storage
/// slot keyed by the pool ID).
pub const ANCHOR_STATE_ATTRIBUTE: &str = "anchor_state";

// The extension's gas on top of the concentrated pool's estimate of the same swap, fitted to the
// difference `EkuboV3MevCaptureV21GasTest` (tycho-execution) measures between pools with and
// without the extension (`tests::test_gas_matches_fork_measurements` checks the fit): per
// forwarded swap, per Core call after the first, per `accumulateAsFees`, once more for the first
// `accumulateAsFees` of a swap, which writes a slot the transaction has not written yet, and per
// anchor update (the first swap of a timestamp), which includes the write of the extension's pool
// state.
const EXTRA_BASE_GAS_COST: u64 = 11_150;
const GAS_COST_OF_ONE_EXTRA_CORE_CALL: u64 = 9_650;
const GAS_COST_OF_ONE_FEE_ACCUMULATION: u64 = 10_350;
const GAS_COST_OF_FIRST_FEE_ACCUMULATION: u64 = 4_650;
const GAS_COST_OF_ONE_ANCHOR_UPDATE: u64 = 6_950;
// Any other write of the extension's pool state, which the swap read before (EIP-2929 and
// EIP-2200: 5,000 gas less the 2,100 of the cold read).
const GAS_COST_OF_ONE_STATE_WRITE: u64 = 2_900;

// The SDK emulates Core's tick bitmap, so a swap takes one step per bitmap word it searches, even
// an empty one: in a sparse pool with a small tick spacing, the work is bounded by the tick range
// instead of by the initialized ticks. Quotes therefore stop where a swap would have searched
// `UNPAYABLE_BITMAP_WORDS` words, which no Ethereum transaction can pay for: Core reads each
// searched word from its own storage slot, at least an access-list storage key (1,900 gas) plus a
// warm read (100 gas), and EIP-7825 caps a transaction at 2^24 gas. Measured with forge on the
// deployed configuration and tick spacing 1, an empty word costs 3.6k gas in a call toward the
// anchor and 11.4k in the calls away from it, so the bound changes no executable swap.
const TRANSACTION_GAS_LIMIT_CAP: i64 = 1 << 24;
const MIN_GAS_PER_SEARCHED_BITMAP_WORD: i64 = 2_000;
const UNPAYABLE_BITMAP_WORDS: i64 =
    TRANSACTION_GAS_LIMIT_CAP / MIN_GAS_PER_SEARCHED_BITMAP_WORD + 1;
const TICK_SPACINGS_PER_BITMAP_WORD: i64 = 256;

/// A concentrated pool behind MEVCapture v2.1, quoted exactly with the SDK's
/// [`MevCaptureV21MainPool`], which reproduces each Core call the extension makes in
/// `Core.forward`.
#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
#[serde(try_from = "MevCaptureV21PoolRepr", into = "MevCaptureV21PoolRepr")]
pub struct MevCaptureV21Pool {
    imp: MevCaptureV21MainPool,
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
            config: pool.imp.model().config(),
            state: pool.state,
            ticks: pool.imp.ticks().to_vec(),
            swapped_this_block: pool.swapped_this_block,
        }
    }
}

impl MevCaptureV21Pool {
    /// Builds the pool from the extension's `getConfig` (`config`), the pool's Core state and
    /// `getAnchor` (`state`), and all its initialized ticks (`ticks`), including those whose net
    /// liquidity delta is zero.
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
            state,
            swapped_this_block: false,
        })
    }

    fn timestamp(&self) -> Result<u32, SimulationError> {
        // Timestamps are truncated to 32 bits by the extension.
        estimate_block_timestamp(
            self.swapped_this_block,
            u64::from(self.state.anchor.last_update_time),
        )
        .map(|timestamp| (timestamp & 0xFFFF_FFFF) as u32)
    }

    /// The sqrt ratio that no executable swap in the given direction reaches: moving the price
    /// there searches at least [`UNPAYABLE_BITMAP_WORDS`] bitmap words.
    fn unreachable_sqrt_ratio(&self, increasing: bool) -> Result<U256, SimulationError> {
        let TickSpacing(spacing) = self.imp.key().config.pool_type_config;
        let distance = UNPAYABLE_BITMAP_WORDS * TICK_SPACINGS_PER_BITMAP_WORD * i64::from(spacing);
        let tick = i64::from(self.state.core.tick);
        let tick = if increasing { tick + distance } else { tick - distance };
        let tick = tick.clamp(i64::from(EVM_MIN_TICK), i64::from(EVM_MAX_TICK));
        let tick = i32::try_from(tick).map_err(|err| {
            SimulationError::FatalError(format!("tick {tick} clamped to the tick range: {err}"))
        })?;
        to_sqrt_ratio::<Evm>(tick).ok_or_else(|| {
            SimulationError::FatalError(format!("no sqrt ratio for tick {tick} in the tick range"))
        })
    }

    /// Quotes with the SDK, stopping at [`Self::unreachable_sqrt_ratio`] unless `sqrt_ratio_limit`
    /// is nearer.
    fn sdk_quote(
        &self,
        token_amount: EvmTokenAmount,
        sqrt_ratio_limit: Option<U256>,
    ) -> Result<Quote<MevCaptureV21MainPoolResources, MevCaptureV21MainPoolState>, SimulationError>
    {
        let key = self.imp.key();
        // A token outside the pair is left to the SDK to reject.
        let sqrt_ratio_limit =
            if token_amount.token == key.token0 || token_amount.token == key.token1 {
                let increasing = (token_amount.token == key.token1) != (token_amount.amount < 0);
                let bound = self.unreachable_sqrt_ratio(increasing)?;
                Some(sqrt_ratio_limit.map_or(bound, |limit| {
                    if increasing {
                        limit.min(bound)
                    } else {
                        limit.max(bound)
                    }
                }))
            } else {
                sqrt_ratio_limit
            };

        self.imp
            .quote(QuoteParams {
                token_amount,
                sqrt_ratio_limit,
                override_state: Some(self.state),
                meta: u64::from(self.timestamp()?),
            })
            .map_err(|err| SimulationError::RecoverableError(format!("quoting error: {err}")))
    }

    fn rebuild(&mut self, ticks: Vec<Tick>) -> Result<(), TransitionError> {
        self.imp = MevCaptureV21MainPool::new(
            self.imp.key(),
            self.imp.model().config(),
            self.state,
            ticks,
        )
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
        let quote = self.sdk_quote(token_amount, sqrt_ratio_limit)?;

        Ok(EkuboPoolQuote {
            consumed_amount: quote.consumed_amount,
            calculated_amount: quote.calculated_amount,
            gas: gas_costs(quote.execution_resources),
            new_state: Self {
                imp: self.imp.clone(),
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

        // There is no liquidity beyond the furthest initialized tick.
        let tick_limit = if token_in == key.token0 { ticks.first() } else { ticks.last() };
        let sqrt_ratio_limit = match tick_limit {
            None => sqrt_ratio,
            Some(tick) => {
                let limit = to_sqrt_ratio::<Evm>(tick.index).ok_or_else(|| {
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
        let quote = self.sdk_quote(
            EvmTokenAmount { token: token_in, amount: i128::MAX },
            Some(sqrt_ratio_limit),
        )?;

        Ok(quote.consumed_amount.max(0))
    }

    fn finish_transition(
        &mut self,
        mut updated_attributes: HashMap<String, Bytes>,
        deleted_attributes: HashSet<String>,
    ) -> Result<(), TransitionError> {
        if let Some(tick) = updated_attributes.remove("tick") {
            self.state.core.tick = i32::try_from(tick)?;
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
            self.imp.model().config() == other.imp.model().config() &&
            self.state == other.state &&
            self.swapped_this_block == other.swapped_this_block
    }
}

/// Decodes the extension's packed per-pool state word (`MEVCaptureV21PoolState`), which may have
/// its leading zero bytes trimmed.
pub fn anchor_from_bytes(bytes: &[u8]) -> Result<MevCaptureV21MainAnchor, String> {
    let word = U256::try_from_be_slice(bytes)
        .ok_or_else(|| format!("anchor state is {} bytes long", bytes.len()))?;

    Ok(MevCaptureV21MainAnchor {
        last_update_time: (word >> 224_usize).wrapping_to::<u32>(),
        l_ref_time: (word >> 192_usize).wrapping_to::<u32>(),
        last_pos_time: (word >> 160_usize).wrapping_to::<u32>(),
        l_ref_bits: (word >> 152_usize).wrapping_to::<u8>(),
        snap_bits: (word >> 144_usize).wrapping_to::<u8>(),
        l_ref_raise_time: (word >> 112_usize).wrapping_to::<u32>(),
        anchor_x16: word.wrapping_to::<i64>(),
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
        bridges,
        tick_event_searches: _,
        accumulate_as_fees,
        anchor_updates,
        state_writes,
    }: MevCaptureV21MainPoolResources,
) -> u64 {
    // Core's swap loop takes a step per call and one more per initialized tick or bitmap word end
    // it crosses within the call. A bridge call crosses its tick in its only step, and a word end
    // away from the anchor ends a call, so those cost no extra step.
    let bitmap_words_crossed_within_calls = core_steps
        .saturating_sub(core_calls)
        .saturating_sub(initialized_ticks_crossed.saturating_sub(bridges));

    concentrated::gas_costs(EvmConcentratedPoolResources {
        no_override_price_change: 0,
        initialized_ticks_crossed,
        extra_distinct_bitmap_lookups: bitmap_words_crossed_within_calls,
    }) + EXTRA_BASE_GAS_COST +
        GAS_COST_OF_ONE_EXTRA_CORE_CALL * u64::from(core_calls.saturating_sub(1)) +
        GAS_COST_OF_ONE_FEE_ACCUMULATION * u64::from(accumulate_as_fees) +
        GAS_COST_OF_FIRST_FEE_ACCUMULATION * u64::from(accumulate_as_fees.min(1)) +
        GAS_COST_OF_ONE_ANCHOR_UPDATE * u64::from(anchor_updates) +
        GAS_COST_OF_ONE_STATE_WRITE * u64::from(state_writes.saturating_sub(anchor_updates))
}

/// Parses the extension's `getConfig` from the component's static attributes.
pub fn config_from_attributes(
    attrs: &HashMap<String, Bytes>,
) -> Result<MevCaptureV21MainConfig, InvalidSnapshotError> {
    fn field<T>(attrs: &HashMap<String, Bytes>, key: &str) -> Result<T, InvalidSnapshotError>
    where
        T: TryFrom<u64>,
        T::Error: std::fmt::Display,
    {
        let bytes = attrs
            .get(key)
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute(key.to_string()))?;
        let value = U256::try_from_be_slice(&bytes[..])
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| {
                InvalidSnapshotError::ValueError(format!("{key} {bytes} does not fit in 64 bits"))
            })?;
        T::try_from(value).map_err(|err| {
            InvalidSnapshotError::ValueError(format!("{key} {value} is out of range: {err}"))
        })
    }

    Ok(MevCaptureV21MainConfig {
        half_life: field(attrs, "half_life")?,
        slope_k: field(attrs, "slope_k")?,
        segment_exp: field(attrs, "segment_exp")?,
        j_lin: field(attrs, "j_lin")?,
        max_fee: field(attrs, "max_fee")?,
        clamp_ticks: field(attrs, "clamp_ticks")?,
        m_gate: field(attrs, "m_gate")?,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy::primitives::aliases::B32;
    use ekubo_sdk::{
        chain::evm::{
            EvmConcentratedPoolConfig, EvmConcentratedPoolKey, EvmConcentratedPoolState,
            EvmPoolTypeConfig,
        },
        quoting::{
            pools::mev_capture_v21_main::{
                sqrt_ratio_from_raw, sqrt_ratio_to_raw, MevCaptureV21MainCallKind,
                MevCaptureV21MainSwapParams,
            },
            types::{PoolConfig, PoolKey},
            util::find_nearest_initialized_tick_index,
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
        ekubo_v3::{
            addresses::MEV_CAPTURE_V21_ETHEREUM_ADDRESS, pool::concentrated::ConcentratedPool,
            state::EkuboV3State,
        },
        test_utils::try_decode_snapshot_with_defaults,
    };

    const TOKEN0: Address = address!("0x0000000000000000000000000000000000000001");
    const TOKEN1: Address = address!("0x0000000000000000000000000000000000000002");
    /// Tick spacing of most contract-traced vectors.
    const S: i32 = 4096;
    /// Liquidity of most of their positions.
    const L66: i128 = 1 << 66;
    /// `getConfig` of the deployed extension.
    const DEPLOYED_CONFIG: MevCaptureV21MainConfig = MevCaptureV21MainConfig {
        half_life: 120,
        slope_k: 4,
        segment_exp: 0,
        j_lin: 16,
        max_fee: 1 << 63,
        clamp_ticks: 2500,
        m_gate: 3,
    };

    fn parse<T: std::str::FromStr>(value: &Value) -> T
    where
        T::Err: std::fmt::Debug,
    {
        if let Value::String(s) = value {
            s.parse().unwrap()
        } else {
            value.to_string().parse().unwrap()
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

    fn tick_deltas(name: &str) -> BTreeMap<i32, i128> {
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
    /// vector's anchor already updated at `timestamp`.
    fn snapshot(vector: &Value, timestamp: u32) -> ComponentWithState {
        let name = vector["name"].as_str().unwrap();
        let cfg = &vector["cfg"];
        let before = core(&vector["before"]);
        let fee: u64 = parse(&vector["pool"]["fee"]);
        let spacing: u32 = parse(&vector["pool"]["tickSpacing"]);

        let static_attributes: HashMap<String, Bytes> = [
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
        for (index, delta) in tick_deltas(name) {
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
        let state = try_decode_snapshot_with_defaults::<EkuboV3State>(snapshot(vector, timestamp))
            .await
            .expect("decoding snapshot");
        let EkuboV3State::MevCaptureV21(pool) = state else {
            panic!("decoded a pool of another type");
        };
        pool
    }

    /// Every contract-traced vector: the SDK pool built from the indexed attributes reproduces
    /// every Core call, and the quote through the integration matches the traced totals for the
    /// 20 vectors with `skipAhead` 0. The vectors start from an anchor already updated in the
    /// block; `test_first_swap_of_block_updates_anchor` covers the update.
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
                .imp
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
            u32::try_from(crate::evm::protocol::ekubo_v3::test_cases::TEST_TIMESTAMP).unwrap() - 5;
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
        assert_ne!(after_first.state.anchor.anchor_x16, pool.state.anchor.anchor_x16);

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
    /// That test asserts that the executed amounts and resulting states equal these quotes.
    #[test]
    fn test_fork_route_quotes() {
        const USDC: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        const USDT: Address = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
        const L: i128 = 40_000_000_000_000;

        fn anchor_from_hex(hex: &str) -> MevCaptureV21MainAnchor {
            anchor_from_bytes(&alloy::hex::decode(hex).unwrap()).unwrap()
        }
        fn core_from_raw(raw: &str, tick: i32, liquidity: u128) -> MevCaptureV21MainCoreState {
            core_state(sqrt_ratio_from_raw(raw.parse().unwrap()), tick, liquidity)
        }

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
            DEPLOYED_CONFIG,
            MevCaptureV21MainPoolState {
                core: core_from_raw("39614081261743854815199363072", 0, 40_000_000_000_000),
                anchor: anchor_from_hex(
                    "6ada581f6ada581f6ada581f00006ada57a70000000000000000000000000000",
                ),
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
            (quote.calculated_amount, next)
        };

        // Block 1 (one slot later): away from the anchor, then back toward it.
        let (away_dec, pool) = swap(&seeded, USDC, 50_000_000_000);
        let (toward_inc, mut pool) = swap(&pool, USDT, 10_000_000_000);
        assert_eq!((away_dec, toward_inc), (49_868_947_729, 10_011_242_024));
        assert_eq!(
            pool.state,
            MevCaptureV21MainPoolState {
                core: core_from_raw("39599244835603686790750605543", -1499, 80_000_000_000_000),
                anchor: anchor_from_hex(
                    "6ada582b6ada582b6ada581f01006ada582b0000000000000000000000000000"
                ),
            }
        );

        // Block 2: the indexer delivers the new state.
        pool.finish_transition(HashMap::new(), HashSet::new())
            .unwrap();
        // Toward and then away from the anchor, increasing price, through the tick at 4000 whose
        // net liquidity delta is zero; then toward the anchor, decreasing price, back across it.
        let (away_inc, pool) = swap(&pool, USDT, 210_000_000_000);
        assert_eq!(pool.state.core.tick, 4991);
        let (toward_dec, pool) = swap(&pool, USDC, 40_000_000_000);
        assert_eq!((away_inc, toward_dec), (209_045_869_607, 40_153_242_269));
        assert_eq!(
            pool.state,
            MevCaptureV21MainPoolState {
                core: core_from_raw("39614081261752292887120781112", 3656, 60_000_000_000_000),
                anchor: anchor_from_hex(
                    "6ada58376ada58376ada581f01006ada582b000000000000ffffffffff9b9dcf"
                ),
            }
        );
    }

    /// The swaps `EkuboV3MevCaptureV21GasTest` (tycho-execution) executes through the executor on
    /// an Ethereum fork, on MEVCapture v2.1 pools and on twin pools without extension, recorded
    /// with both pools' states before each swap. Each quote equals the executed output, and
    /// each gas estimate is within 5% of the concentrated pool's estimate of the twin swap plus
    /// the measured difference between the pools.
    #[test]
    fn test_gas_matches_fork_measurements() {
        let file: Value =
            serde_json::from_str(include_str!("../assets/mev_capture_v21_gas.json")).unwrap();
        let rows = file["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 49);

        for row in rows {
            let name = row["name"].as_str().unwrap();
            let pool = &file["pools"][row["pool"].as_str().unwrap()];
            let fee: u64 = parse(&pool["fee"]);
            let spacing = TickSpacing(parse(&pool["tickSpacing"]));
            let mut net = BTreeMap::new();
            for position in pool["positions"].as_array().unwrap() {
                let liquidity: i128 = parse(&position[2]);
                *net.entry(parse(&position[0]))
                    .or_insert(0) += liquidity;
                *net.entry(parse(&position[1]))
                    .or_insert(0) -= liquidity;
            }
            let ticks: Vec<Tick> = net
                .into_iter()
                .map(|(index, liquidity_delta)| Tick { index, liquidity_delta })
                .collect();
            let token_amount = EvmTokenAmount {
                token: if row["isToken1"].as_bool().unwrap() { TOKEN1 } else { TOKEN0 },
                amount: parse(&row["amount"]),
            };

            let quote = MevCaptureV21MainPool::new(
                PoolKey {
                    token0: TOKEN0,
                    token1: TOKEN1,
                    config: PoolConfig {
                        extension: MEV_CAPTURE_V21_ETHEREUM_ADDRESS,
                        fee,
                        pool_type_config: spacing,
                    },
                },
                DEPLOYED_CONFIG,
                MevCaptureV21MainPoolState {
                    core: core(&row["before"]),
                    anchor: anchor_from_bytes(
                        &alloy::hex::decode(row["anchor"].as_str().unwrap()).unwrap(),
                    )
                    .unwrap(),
                },
                ticks.clone(),
            )
            .unwrap()
            .quote(QuoteParams {
                token_amount,
                sqrt_ratio_limit: None,
                override_state: None,
                meta: parse(&row["now"]),
            })
            .unwrap();
            assert_eq!(quote.consumed_amount, token_amount.amount, "{name}");
            assert_eq!(quote.calculated_amount, parse::<u128>(&row["v21Out"]), "{name}");

            let plain = core(&row["plainBefore"]);
            let plain_estimate = ConcentratedPool::new(
                EvmConcentratedPoolKey {
                    token0: TOKEN0,
                    token1: TOKEN1,
                    config: EvmConcentratedPoolConfig {
                        fee,
                        pool_type_config: spacing,
                        extension: Address::ZERO,
                    },
                },
                EvmConcentratedPoolState {
                    sqrt_ratio: plain.sqrt_ratio,
                    liquidity: plain.liquidity,
                    active_tick_index: find_nearest_initialized_tick_index(&ticks, plain.tick),
                },
                plain.tick,
                ticks,
            )
            .unwrap()
            .quote(token_amount, None)
            .unwrap()
            .gas;

            let measured =
                plain_estimate + parse::<u64>(&row["v21Gas"]) - parse::<u64>(&row["plainGas"]);
            let estimate = gas_costs(quote.execution_resources);
            assert!(
                estimate.abs_diff(measured) * 20 <= measured,
                "{name}: estimated {estimate}, measured {measured}"
            );
        }
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

    /// Tick spacing 1, liquidity around the price and a position at `MIN_TICK`, about 346k empty
    /// bitmap words away.
    fn sparse_pool() -> MevCaptureV21Pool {
        let liquidity: u128 = 1 << 70;
        let delta = i128::try_from(liquidity).unwrap();
        MevCaptureV21Pool::new(
            PoolKey {
                token0: TOKEN0,
                token1: TOKEN1,
                config: PoolConfig {
                    extension: MEV_CAPTURE_V21_ETHEREUM_ADDRESS,
                    fee: 55_340_232_221_128_654,
                    pool_type_config: TickSpacing(1),
                },
            },
            DEPLOYED_CONFIG,
            MevCaptureV21MainPoolState {
                core: core_state(to_sqrt_ratio::<Evm>(0).unwrap(), 0, liquidity),
                anchor: MevCaptureV21MainAnchor {
                    last_update_time: 1_000,
                    l_ref_time: 1_000,
                    last_pos_time: 1_000,
                    l_ref_bits: 0,
                    snap_bits: 0,
                    l_ref_raise_time: 1_000,
                    anchor_x16: 0,
                },
            },
            [(EVM_MIN_TICK, delta), (EVM_MIN_TICK + 1, -delta), (-1000, delta), (1000, -delta)]
                .map(|(index, liquidity_delta)| Tick { index, liquidity_delta })
                .to_vec(),
        )
        .unwrap()
    }

    /// The probe of `get_limit` and a quote without price limit stop where the swap would have
    /// searched `UNPAYABLE_BITMAP_WORDS` bitmap words, instead of walking every empty word.
    #[test]
    fn test_sparse_pool_quotes_stop_at_unpayable_words() {
        let pool = sparse_pool();
        let words = u32::try_from(UNPAYABLE_BITMAP_WORDS).unwrap();

        for (token, increasing) in [(TOKEN0, false), (TOKEN1, true)] {
            let bound = pool
                .unreachable_sqrt_ratio(increasing)
                .unwrap();
            let distance =
                i32::try_from(UNPAYABLE_BITMAP_WORDS * TICK_SPACINGS_PER_BITMAP_WORD).unwrap();
            let bound_tick = if increasing { distance } else { -distance };
            assert_eq!(bound, to_sqrt_ratio::<Evm>(bound_tick).unwrap());

            let quote = pool
                .sdk_quote(EvmTokenAmount { token, amount: i128::MAX }, None)
                .unwrap();
            assert_eq!(quote.state_after.core.sqrt_ratio, bound);
            assert!(quote.consumed_amount < i128::MAX);
            // One step per searched word, plus the rate cuts before the surcharge reaches its cap
            // and the bridge onto the initialized tick at -1000 or 1000.
            let resources = quote.execution_resources;
            assert!(resources.core_steps >= words, "{resources:?}");
            assert!(resources.core_steps <= words + 32, "{resources:?}");

            let limit = pool.get_limit(token).unwrap();
            assert!(limit > 0);
            let quote = pool
                .quote(EvmTokenAmount { token, amount: limit }, None)
                .unwrap();
            assert_eq!(quote.consumed_amount, limit);
        }
    }

    /// Swaps that fit in a transaction are quoted as without the bound. These stay within the
    /// liquidity around the price, which holds about 5.9e17 of either token.
    #[test]
    fn test_sparse_pool_executable_quotes_are_unchanged() {
        let pool = sparse_pool();
        for token in [TOKEN0, TOKEN1] {
            for amount in [100_000_000_000_000_000, -100_000_000_000_000_000] {
                let token_amount = EvmTokenAmount { token, amount };
                let bounded = pool
                    .sdk_quote(token_amount, None)
                    .unwrap();
                let unbounded = pool
                    .imp
                    .quote(QuoteParams {
                        token_amount,
                        sqrt_ratio_limit: None,
                        override_state: Some(pool.state),
                        meta: u64::from(pool.timestamp().unwrap()),
                    })
                    .unwrap();
                assert_eq!(bounded.consumed_amount, amount);
                assert_eq!(bounded.consumed_amount, unbounded.consumed_amount);
                assert_eq!(bounded.calculated_amount, unbounded.calculated_amount);
                assert_eq!(bounded.state_after, unbounded.state_after);
                assert_eq!(bounded.execution_resources, unbounded.execution_resources);
            }
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
