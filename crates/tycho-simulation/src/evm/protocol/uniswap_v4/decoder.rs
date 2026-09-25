use std::collections::HashMap;

use alloy::primitives::U256;
use itertools::Itertools;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim, Bytes};

use super::state::UniswapV4State;
use crate::{
    evm::protocol::{
        uniswap_v4::{
            hooks::hook_handler_creator::{instantiate_hook_handler, HookCreationParams},
            state::UniswapV4Fees,
        },
        utils::{
            bytes_to_address,
            uniswap::{i24_be_bytes_to_i32, tick_list::TickInfo},
        },
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

impl TryFromWithBlock<ComponentWithState, BlockHeader> for UniswapV4State {
    type Error = InvalidSnapshotError;

    /// Decodes a `ComponentWithState` into a `UniswapV4State`. Errors with a `InvalidSnapshotError`
    /// if the snapshot is missing any required attributes.
    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let liq = snapshot
            .state
            .attributes
            .get("liquidity")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("liquidity".to_string()))?
            .clone();

        let liquidity = u128::from(liq);

        let sqrt_price = U256::from_be_slice(
            snapshot
                .state
                .attributes
                .get("sqrt_price_x96")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("sqrt_price".to_string()))?,
        );

        let lp_fee = u32::from(
            snapshot
                .component
                .static_attributes
                .get("key_lp_fee")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("key_lp_fee".to_string()))?
                .clone(),
        );

        let zero2one_protocol_fee = u32::from(
            snapshot
                .state
                .attributes
                .get("protocol_fees/zero2one")
                .ok_or_else(|| {
                    InvalidSnapshotError::MissingAttribute("protocol_fees/zero2one".to_string())
                })?
                .clone(),
        );
        let one2zero_protocol_fee = u32::from(
            snapshot
                .state
                .attributes
                .get("protocol_fees/one2zero")
                .ok_or_else(|| {
                    InvalidSnapshotError::MissingAttribute("protocol_fees/one2zero".to_string())
                })?
                .clone(),
        );

        let fees: UniswapV4Fees =
            UniswapV4Fees::new(zero2one_protocol_fee, one2zero_protocol_fee, lp_fee);

        let tick_spacing: i32 = i32::from(
            snapshot
                .component
                .static_attributes
                .get("tick_spacing")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("tick_spacing".to_string()))?
                .clone(),
        );

        let tick = i24_be_bytes_to_i32(
            snapshot
                .state
                .attributes
                .get("tick")
                .ok_or_else(|| InvalidSnapshotError::MissingAttribute("tick".to_string()))?,
        );

        let ticks: Result<Vec<_>, _> = snapshot
            .state
            .attributes
            .iter()
            .filter_map(|(key, value)| {
                if key.starts_with("ticks/") {
                    Some(
                        key.split('/')
                            .nth(1)?
                            .parse::<i32>()
                            .map_err(|err| InvalidSnapshotError::ValueError(err.to_string()))
                            .and_then(|tick_index| {
                                TickInfo::new(tick_index, i128::from(value.clone())).map_err(
                                    |err| InvalidSnapshotError::ValueError(err.to_string()),
                                )
                            }),
                    )
                } else {
                    None
                }
            })
            .collect();

        let hook_attribute = snapshot
            .component
            .static_attributes
            .get("hooks");

        let mut ticks = match ticks {
            Ok(ticks) if !ticks.is_empty() => ticks
                .into_iter()
                .filter(|t| t.net_liquidity != 0)
                .collect::<Vec<_>>(),
            _ => {
                // there might be pools where the liquidity is managed by the hook
                //
                // Keyed on the attribute being present rather than on the address being non-zero:
                // a freshly initialised hookless pool has no ticks yet and must still decode.
                if hook_attribute.is_some() {
                    Vec::new()
                } else {
                    return Err(InvalidSnapshotError::MissingAttribute(
                        "tick_liquidities".to_string(),
                    ));
                }
            }
        };

        ticks.sort_by_key(|tick| tick.index);

        let mut state = UniswapV4State::new(liquidity, sqrt_price, fees, tick, tick_spacing, ticks)
            .map_err(|err| {
                tracing::error!(
                    pool_id = %snapshot.component.id,
                    error = %err,
                    "Failed to create UniswapV4State"
                );
                InvalidSnapshotError::ValueError(err.to_string())
            })?;

        // Both substreams variants emit `hooks` for every pool, so the attribute's presence says
        // nothing about whether a hook exists — the zero address is what "no hook" looks like on
        // the wire. Such a pool is a plain V4 pool on every chain: no handler, no chain needed.
        let hook_address = hook_attribute
            .map(bytes_to_address)
            .transpose()
            .map_err(|err| {
                InvalidSnapshotError::ValueError(format!(
                    "hooks attribute is not a 20-byte address: {err}"
                ))
            })?
            .filter(|address| !address.is_zero());

        if let Some(hook_address) = hook_address {
            // Merge state attributes into static_attributes for hook creation
            let mut merged_attributes = snapshot
                .component
                .static_attributes
                .clone();
            merged_attributes.extend(snapshot.state.attributes.clone());

            let chain = decoder_context.chain.ok_or_else(|| {
                InvalidSnapshotError::ValueError(
                    "uniswap v4 hook pools require DecoderContext.chain; register the decoder \
                     through TychoStreamDecoder or set DecoderContext::chain"
                        .to_string(),
                )
            })?;

            let hook_params = HookCreationParams::new(
                hook_address,
                account_balances,
                all_tokens,
                state.clone(),
                &merged_attributes,
                &snapshot.state.balances,
                decoder_context.vm_traces,
            );

            let hook_handler = instantiate_hook_handler(chain, &hook_address, hook_params)?;
            state.set_hook_handler(hook_handler);
        };

        for tokens in snapshot
            .component
            .tokens
            .iter()
            .permutations(2)
        {
            let (t0, t1) = (tokens[0], tokens[1]);
            let token_in = all_tokens.get(t0).ok_or_else(|| {
                InvalidSnapshotError::ValueError("Failed to get token".to_string())
            })?;
            let token_out = all_tokens.get(t1).ok_or_else(|| {
                InvalidSnapshotError::ValueError("Failed to get token".to_string())
            })?;
            state.spot_price(token_in, token_out)?;
        }

        Ok(state)
    }
}

/// The recorded Robinhood Pons V2 pool `0xc96847cc…` (XLG/NVDA), shared with the stream-decoder
/// tests in [`crate::evm::decoder`].
///
/// Every value comes from the chain: `tests/assets/decoder/uniswap_v4_pons_snapshot_robinhood.json`
/// is the pool at the end of the block that created it, and the sibling `_sources.json` names the
/// log or storage read each of its fields was taken from.
#[cfg(test)]
pub(crate) mod pons_fixture {
    use std::{collections::HashMap, fs, path::Path, str::FromStr};

    use num_bigint::BigUint;
    use tycho_client::feed::{dto, synchronizer::ComponentWithState, BlockHeader};
    use tycho_common::{
        models::{token::Token, Chain},
        Bytes,
    };

    use super::UniswapV4State;
    use crate::protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    };

    pub(crate) const POOL_ID: &str =
        "0xc96847cc43f7595aafcbc1c99d335cb91be7ce1107524c87030f48a716a5f289";
    const XLG_ADDRESS: &str = "0xab5983fe30f186055095305c862b0e097dab3b52";
    const NVDA_ADDRESS: &str = "0xd0601ce157db5bdc3162bbac2a2c8af5320d9eec";

    /// The terms `registerPool` froze for this pool, read out of `launches[poolId]`.
    pub(crate) const HOOK_FEE_BPS: u32 = 100;
    pub(crate) const CREATOR_TAX_BPS: u32 = 100;

    /// The block the pool was created in, and its timestamp.
    const BLOCK_NUMBER: u64 = 58_759_099;
    const BLOCK_TIMESTAMP: u64 = 1_788_978_331;

    pub(crate) fn snapshot() -> ComponentWithState {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/assets/decoder/uniswap_v4_pons_snapshot_robinhood.json");
        let raw =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        serde_json::from_str::<dto::ComponentWithState>(&raw)
            .expect("the Pons snapshot fixture should match ComponentWithState")
            .into()
    }

    /// The same pool with everything the hook contributes stripped off the wire, which is what a
    /// pool of this shape looks like with no hook at all. Its quotes are the core swap math the
    /// hook's cut is measured against.
    pub(crate) fn hookless_snapshot() -> ComponentWithState {
        let mut snapshot = snapshot();
        snapshot.component.protocol_system = "uniswap_v4".to_string();
        for attribute in ["hooks", "hook_identifier", "pons_hook_fee_bps", "pons_creator_tax_bps"] {
            snapshot
                .component
                .static_attributes
                .remove(attribute);
        }
        snapshot
    }

    /// The same pool behind an address no native handler is registered for on any chain.
    pub(crate) fn unknown_hook_snapshot() -> ComponentWithState {
        let mut snapshot = snapshot();
        snapshot
            .component
            .static_attributes
            .insert(
                "hooks".to_string(),
                Bytes::from_str("0x14bcc18fdb0e7a427122b9c2f1a40ff7d63eaacc")
                    .expect("the literal address parses"),
            );
        snapshot
    }

    fn token(address: &str, symbol: &str) -> Token {
        Token::new(
            &Bytes::from_str(address).expect("the fixture token addresses parse"),
            symbol,
            18,
            0,
            &[Some(10_000)],
            Chain::Robinhood,
            100,
        )
    }

    /// `currency0` of the pool: the lower of the two addresses, so a swap out of it is
    /// zero-for-one.
    pub(crate) fn xlg() -> Token {
        token(XLG_ADDRESS, "XLG")
    }

    pub(crate) fn nvda() -> Token {
        token(NVDA_ADDRESS, "NVDA")
    }

    pub(crate) fn tokens() -> HashMap<Bytes, Token> {
        [xlg(), nvda()]
            .into_iter()
            .map(|token| (token.address.clone(), token))
            .collect()
    }

    pub(crate) fn header() -> BlockHeader {
        BlockHeader {
            number: BLOCK_NUMBER,
            hash: Bytes::from([1u8; 32]),
            parent_hash: Bytes::from([0u8; 32]),
            revert: false,
            timestamp: BLOCK_TIMESTAMP,
            partial_block_index: None,
        }
    }

    /// Decodes `snapshot` as a component of `chain`, the way a decoder registered for that chain
    /// would.
    pub(crate) async fn decode_on(
        snapshot: ComponentWithState,
        chain: Chain,
    ) -> Result<UniswapV4State, InvalidSnapshotError> {
        UniswapV4State::try_from_with_header(
            snapshot,
            header(),
            &HashMap::default(),
            &tokens(),
            &DecoderContext::new().chain(chain),
        )
        .await
    }

    /// What the PoolManager holds of `token` for this pool, straight from the snapshot.
    ///
    /// Trade sizes are taken as fractions of this rather than of `get_limits`. A full-range
    /// position can absorb an unbounded amount of one side while giving up a bounded amount of
    /// the other, so even a millionth of the limit on the `currency0` side drains the pool.
    pub(crate) fn reserve(token: &Token) -> BigUint {
        let snapshot = snapshot();
        let balance = snapshot
            .state
            .balances
            .get(&token.address)
            .unwrap_or_else(|| panic!("the fixture carries a balance for {}", token.symbol));
        BigUint::from_bytes_be(balance)
    }

    /// What is left of a `core` output once the hook has taken its fee and its tax, each floored
    /// on its own exactly as `_afterSwap` computes them.
    pub(crate) fn net_of_hook_take(core: &BigUint) -> BigUint {
        let denominator = BigUint::from(10_000u32);
        let fee = core * BigUint::from(HOOK_FEE_BPS) / &denominator;
        let tax = core * BigUint::from(CREATOR_TAX_BPS) / &denominator;
        core - fee - tax
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use chrono::DateTime;
    use num_bigint::BigUint;
    use rstest::rstest;
    use tycho_common::models::{
        protocol::{ProtocolComponent, ProtocolComponentState},
        Chain, ChangeType,
    };

    use super::*;
    use crate::evm::protocol::test_utils::try_decode_snapshot_with_defaults;

    fn usv4_component() -> ProtocolComponent {
        let creation_time = DateTime::from_timestamp(1622526000, 0)
            .unwrap()
            .naive_utc();

        let static_attributes: HashMap<String, Bytes> = HashMap::from([
            ("key_lp_fee".to_string(), Bytes::from(500_i32.to_be_bytes().to_vec())),
            ("tick_spacing".to_string(), Bytes::from(60_i32.to_be_bytes().to_vec())),
        ]);

        ProtocolComponent {
            id: "State1".to_string(),
            protocol_system: "system1".to_string(),
            protocol_type_name: "typename1".to_string(),
            chain: Chain::Ethereum,
            tokens: Vec::new(),
            contract_addresses: Vec::new(),
            static_attributes,
            change: ChangeType::Creation,
            creation_tx: Bytes::from_str("0x0000").unwrap(),
            created_at: creation_time,
        }
    }

    fn usv4_attributes() -> HashMap<String, Bytes> {
        HashMap::from([
            ("liquidity".to_string(), Bytes::from(100_u64.to_be_bytes().to_vec())),
            ("tick".to_string(), Bytes::from(300_i32.to_be_bytes().to_vec())),
            (
                "sqrt_price_x96".to_string(),
                Bytes::from(
                    79228162514264337593543950336_u128
                        .to_be_bytes()
                        .to_vec(),
                ),
            ),
            ("protocol_fees/zero2one".to_string(), Bytes::from(0_u32.to_be_bytes().to_vec())),
            ("protocol_fees/one2zero".to_string(), Bytes::from(0_u32.to_be_bytes().to_vec())),
            ("ticks/60/net_liquidity".to_string(), Bytes::from(400_i128.to_be_bytes().to_vec())),
        ])
    }

    #[tokio::test]
    async fn test_usv4_try_from() {
        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes: usv4_attributes(),
                balances: HashMap::new(),
            },
            component: usv4_component(),
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<UniswapV4State>(snapshot)
            .await
            .unwrap();

        let fees = UniswapV4Fees::new(0, 0, 500);
        let expected = UniswapV4State::new(
            100,
            U256::from(79228162514264337593543950336_u128),
            fees,
            300,
            60,
            vec![TickInfo::new(60, 400).unwrap()],
        )
        .unwrap();
        assert_eq!(result, expected);
    }

    /// What a pool with no hook looks like on the wire: both substreams variants emit the `hooks`
    /// attribute for every pool, zero-valued when there is no hook.
    const ZERO_HOOK: &str = "0x0000000000000000000000000000000000000000";
    /// A hook address with no native handler registered for it on any chain.
    const UNKNOWN_HOOK: &str = "0x00000000000000000000000000000000000000c4";

    fn hookless_token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(
            &Bytes::from_str(address).unwrap(),
            symbol,
            decimals,
            0,
            &[Some(10_000)],
            Chain::Ethereum,
            100,
        )
    }

    fn token0() -> Token {
        hookless_token("0x0000000000000000000000000000000000000001", "T0", 18)
    }

    /// Deliberately a different decimal count from [`token0`], so the two quote orderings differ
    /// and an ordering bug cannot hide behind a symmetric result.
    fn token1() -> Token {
        hookless_token("0x0000000000000000000000000000000000000002", "T1", 6)
    }

    /// A symmetric, liquid pool, so quoting succeeds in both directions.
    fn hookless_pool_attributes() -> HashMap<String, Bytes> {
        let liquidity = 1_000_000_000_000_000_000_i128;
        HashMap::from([
            (
                "liquidity".to_string(),
                Bytes::from(
                    (liquidity as u128)
                        .to_be_bytes()
                        .to_vec(),
                ),
            ),
            ("tick".to_string(), Bytes::from(0_i32.to_be_bytes().to_vec())),
            (
                "sqrt_price_x96".to_string(),
                Bytes::from(
                    79228162514264337593543950336_u128
                        .to_be_bytes()
                        .to_vec(),
                ),
            ),
            ("protocol_fees/zero2one".to_string(), Bytes::from(0_u32.to_be_bytes().to_vec())),
            ("protocol_fees/one2zero".to_string(), Bytes::from(0_u32.to_be_bytes().to_vec())),
            ("ticks/-60/net_liquidity".to_string(), Bytes::from(liquidity.to_be_bytes().to_vec())),
            (
                "ticks/60/net_liquidity".to_string(),
                Bytes::from((-liquidity).to_be_bytes().to_vec()),
            ),
        ])
    }

    /// The very same pool, built directly with no hook — the reference every hookless-decoding
    /// assertion compares against.
    fn reference_hookless_state() -> UniswapV4State {
        let liquidity = 1_000_000_000_000_000_000_i128;
        UniswapV4State::new(
            liquidity as u128,
            U256::from(79228162514264337593543950336_u128),
            UniswapV4Fees::new(0, 0, 500),
            0,
            60,
            vec![TickInfo::new(-60, liquidity).unwrap(), TickInfo::new(60, -liquidity).unwrap()],
        )
        .unwrap()
    }

    fn hookless_snapshot_with_hook(hook: &str) -> ComponentWithState {
        let mut component = usv4_component();
        component
            .static_attributes
            .insert("hooks".to_string(), Bytes::from_str(hook).unwrap());
        component.tokens = vec![token0().address, token1().address];

        ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes: hookless_pool_attributes(),
                balances: HashMap::new(),
            },
            component,
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    /// The snapshot the original hook tests used: minimal attributes, no tokens.
    fn usv4_snapshot_with_hook(hook: &str) -> ComponentWithState {
        let mut component = usv4_component();
        component
            .static_attributes
            .insert("hooks".to_string(), Bytes::from_str(hook).unwrap());

        ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes: usv4_attributes(),
                balances: HashMap::new(),
            },
            component,
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    async fn decode_with_chain(
        snapshot: ComponentWithState,
        chain: Option<Chain>,
    ) -> Result<UniswapV4State, InvalidSnapshotError> {
        let all_tokens =
            HashMap::from([(token0().address, token0()), (token1().address, token1())]);
        let context = match chain {
            Some(chain) => DecoderContext::new().chain(chain),
            None => DecoderContext::new(),
        };
        UniswapV4State::try_from_with_header(
            snapshot,
            Default::default(),
            &HashMap::default(),
            &all_tokens,
            &context,
        )
        .await
    }

    /// A zero `hooks` address is not a hook. The pool decodes on every chain, with or without a
    /// chain in the context, and behaves exactly like a `UniswapV4State` built with no hook:
    /// analytic `spot_price`, tick-walk `get_limits`, unchanged `get_amount_out`. It must never
    /// route through `GenericVMHookHandler`.
    #[tokio::test]
    #[rstest]
    #[case::no_chain(None)]
    #[case::robinhood(Some(Chain::Robinhood))]
    #[case::base(Some(Chain::Base))]
    #[case::ethereum(Some(Chain::Ethereum))]
    async fn zero_hook_address_decodes_as_hookless(#[case] chain: Option<Chain>) {
        let decoded = decode_with_chain(hookless_snapshot_with_hook(ZERO_HOOK), chain)
            .await
            .expect("a pool with a zero hook address must decode on every chain");

        assert!(decoded.hook.is_none(), "a zero hook address must not install a hook handler");

        let reference = reference_hookless_state();
        let (t0, t1) = (token0(), token1());

        // spot_price, both orderings
        assert_eq!(decoded.spot_price(&t0, &t1).unwrap(), reference.spot_price(&t0, &t1).unwrap());
        assert_eq!(decoded.spot_price(&t1, &t0).unwrap(), reference.spot_price(&t1, &t0).unwrap());

        // get_limits, both directions
        assert_eq!(
            decoded
                .get_limits(t0.address.clone(), t1.address.clone())
                .unwrap(),
            reference
                .get_limits(t0.address.clone(), t1.address.clone())
                .unwrap()
        );
        assert_eq!(
            decoded
                .get_limits(t1.address.clone(), t0.address.clone())
                .unwrap(),
            reference
                .get_limits(t1.address.clone(), t0.address.clone())
                .unwrap()
        );

        // get_amount_out, one amount, both directions
        let amount_in = BigUint::from(1_000_000_000_000_000_u64);
        for (token_in, token_out) in [(&t0, &t1), (&t1, &t0)] {
            let from_decoded = decoded
                .get_amount_out(amount_in.clone(), token_in, token_out)
                .unwrap();
            let from_reference = reference
                .get_amount_out(amount_in.clone(), token_in, token_out)
                .unwrap();
            assert_eq!(from_decoded.amount, from_reference.amount);
            assert_eq!(from_decoded.gas, from_reference.gas);
        }
    }

    /// A non-zero hook with no registered handler is rejected rather than quoted as if the hook
    /// were absent, and it still needs a chain to decide that.
    #[tokio::test]
    #[rstest]
    #[case::robinhood_fails_closed(Some(Chain::Robinhood), "unsupported uniswap v4 hook")]
    #[case::no_chain_is_rejected(None, "DecoderContext.chain")]
    async fn nonzero_unsupported_hook_is_rejected(
        #[case] chain: Option<Chain>,
        #[case] expected_message: &str,
    ) {
        let result = decode_with_chain(hookless_snapshot_with_hook(UNKNOWN_HOOK), chain).await;

        let Err(InvalidSnapshotError::ValueError(message)) = result else {
            panic!("an unregistered non-zero hook must not decode");
        };
        assert!(message.contains(expected_message), "{message}");
    }

    /// The `hooks` attribute arrives from the stream, so a value that is not a 20-byte address
    /// must be reported as a bad snapshot rather than panic the decoding task.
    #[tokio::test]
    async fn malformed_hooks_attribute_is_rejected_without_panicking() {
        let mut snapshot = hookless_snapshot_with_hook(ZERO_HOOK);
        snapshot
            .component
            .static_attributes
            .insert("hooks".to_string(), Bytes::from(vec![0x11_u8; 19]));

        let result = decode_with_chain(snapshot, Some(Chain::Ethereum)).await;

        let Err(InvalidSnapshotError::ValueError(message)) = result else {
            panic!("a 19-byte hooks attribute must not decode");
        };
        assert!(message.contains("hooks"), "{message}");
    }

    #[tokio::test]
    async fn test_usv4_zero_hook_pool_with_no_ticks_still_decodes() {
        // Tick leniency is keyed on the `hooks` attribute being present, not on the address being
        // non-zero: a freshly initialised hookless pool has no ticks yet and must still decode.
        let mut snapshot = usv4_snapshot_with_hook(ZERO_HOOK);
        snapshot
            .state
            .attributes
            .remove("ticks/60/net_liquidity");

        let decoded = decode_with_chain(snapshot, None)
            .await
            .expect("a hookless pool with no ticks must decode");

        assert!(decoded.hook.is_none());
    }

    #[tokio::test]
    #[rstest]
    #[case::missing_liquidity("liquidity")]
    #[case::missing_sqrt_price("sqrt_price")]
    #[case::missing_tick("tick")]
    #[case::missing_tick_liquidity("tick_liquidities")]
    #[case::missing_fee("key_lp_fee")]
    #[case::missing_fee("protocol_fees/one2zero")]
    #[case::missing_fee("protocol_fees/zero2one")]
    async fn test_usv4_try_from_invalid(#[case] missing_attribute: String) {
        // remove missing attribute
        let mut component = usv4_component();
        let mut attributes = usv4_attributes();
        attributes.remove(&missing_attribute);

        if missing_attribute == "tick_liquidities" {
            attributes.remove("ticks/60/net_liquidity");
        }

        if missing_attribute == "sqrt_price" {
            attributes.remove("sqrt_price_x96");
        }

        if missing_attribute == "key_lp_fee" {
            component
                .static_attributes
                .remove("key_lp_fee");
        }

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes,
                balances: HashMap::new(),
            },
            component,
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<UniswapV4State>(snapshot).await;

        assert!(result.is_err());
        assert!(matches!(
            result.err().unwrap(),
            InvalidSnapshotError::MissingAttribute(attr) if attr == missing_attribute
        ));
    }

    // Pons V2 on Robinhood, decoded through this very `TryFromWithBlock`.

    use std::{fs, path::Path};

    use serde::Deserialize;

    use super::pons_fixture;
    use crate::evm::protocol::uniswap_v4::hooks::{
        hook_handler::HookHandler,
        hook_handler_creator::initialize_hook_handlers,
        pons_v2::hook_handler::{PonsV2HookHandler, PONS_V2_HOOK_ROBINHOOD},
    };

    /// The native Pons handler a decoded state installed, or a panic naming what it installed
    /// instead.
    fn pons_handler(state: &UniswapV4State) -> &PonsV2HookHandler {
        let hook = state
            .hook
            .as_ref()
            .expect("a Pons pool must decode with a hook handler");
        hook.as_any()
            .downcast_ref::<PonsV2HookHandler>()
            .unwrap_or_else(|| panic!("expected the native Pons handler at {}", hook.address()))
    }

    /// Whether `state` quotes through the native Pons handler.
    fn is_pons(state: &UniswapV4State) -> bool {
        state.hook.as_ref().is_some_and(|hook| {
            hook.as_any()
                .downcast_ref::<PonsV2HookHandler>()
                .is_some()
        })
    }

    /// A millionth, a ten-thousandth and a hundredth of what the pool holds of the input token,
    /// so one test covers three trade sizes two orders of magnitude apart without hard-coding an
    /// amount per direction.
    fn probe_amounts(reserve_in: &BigUint) -> [BigUint; 3] {
        [
            reserve_in / BigUint::from(1_000_000u32),
            reserve_in / BigUint::from(10_000u32),
            reserve_in / BigUint::from(100u32),
        ]
    }

    /// A Pons pool decoded on Robinhood charges the fee and the tax `registerPool` froze for it,
    /// and nothing else: what it quotes is the hookless output less two independently floored
    /// cuts, and what it prices is the hookless price marked up by the same take.
    #[tokio::test]
    async fn pons_pool_decodes_on_robinhood_and_charges_hook_fee() {
        initialize_hook_handlers().expect("hook handler registration should succeed");

        let hooked = pons_fixture::decode_on(pons_fixture::snapshot(), Chain::Robinhood)
            .await
            .expect("the recorded Pons pool must decode on Robinhood");
        let core = pons_fixture::decode_on(pons_fixture::hookless_snapshot(), Chain::Robinhood)
            .await
            .expect("the same pool with no hook must decode too");

        let handler = pons_handler(&hooked);
        assert_eq!(u32::from(handler.hook_fee_bps()), pons_fixture::HOOK_FEE_BPS);
        assert_eq!(u32::from(handler.creator_tax_bps()), pons_fixture::CREATOR_TAX_BPS);
        assert_eq!(handler.address(), PONS_V2_HOOK_ROBINHOOD);
        assert!(core.hook.is_none(), "the reference pool must carry no hook");

        let (xlg, nvda) = (pons_fixture::xlg(), pons_fixture::nvda());
        for (token_in, token_out) in [(&xlg, &nvda), (&nvda, &xlg)] {
            let (limit_in, core_limit_out) = core
                .get_limits(token_in.address.clone(), token_out.address.clone())
                .expect("a pool holding a full range position has limits");

            for amount_in in probe_amounts(&pons_fixture::reserve(token_in)) {
                let core_out = core
                    .get_amount_out(amount_in.clone(), token_in, token_out)
                    .expect("the reference pool quotes every probe")
                    .amount;
                let hooked_out = hooked
                    .get_amount_out(amount_in.clone(), token_in, token_out)
                    .expect("the hooked pool quotes every probe")
                    .amount;

                let expected = pons_fixture::net_of_hook_take(&core_out);
                assert!(expected < core_out, "the hook must take something out of {core_out}");
                assert_eq!(
                    hooked_out, expected,
                    "{} -> {}: {amount_in} in",
                    token_in.symbol, token_out.symbol
                );
            }

            // `get_limits` reports what a swapper receives, so the hook's cut comes off the
            // output while the input the pool can absorb is unchanged.
            let (hooked_limit_in, hooked_limit_out) = hooked
                .get_limits(token_in.address.clone(), token_out.address.clone())
                .expect("the hooked pool has limits too");
            assert_eq!(hooked_limit_in, limit_in);
            assert_eq!(hooked_limit_out, pons_fixture::net_of_hook_take(&core_limit_out));
        }

        // Pons takes its cut out of the output, so buying one `base` costs `core / (1 - rate)`.
        // At 100 + 100 bps that is `1 / 0.98` of the hookless price, in both orderings.
        for (base, quote) in [(&xlg, &nvda), (&nvda, &xlg)] {
            let hooked_price = hooked
                .spot_price(base, quote)
                .expect("the hook prices its own fee");
            let core_price = core
                .spot_price(base, quote)
                .expect("a hookless pool always prices");
            let ratio = hooked_price / core_price;
            assert!((ratio * 0.98 - 1.0).abs() < 1e-9, "hooked/core is {ratio}, not 1/0.98");
        }
    }

    /// What a tampered Pons snapshot has to do.
    #[derive(Debug, Clone, Copy)]
    enum Outcome {
        /// Decodes, carrying these terms.
        Decodes(u16, u16),
        /// Rejected as a missing attribute of this name.
        Missing(&'static str),
        /// Rejected as a value error naming this attribute.
        Value(&'static str),
    }

    /// The substreams module writes each bps with `BigInt::to_signed_bytes_be`, which is
    /// minimal-width: one byte below 256, two above.
    const BPS_0: &[u8] = &[0x00];
    const BPS_1: &[u8] = &[0x01];
    const BPS_100: &[u8] = &[0x64];
    const BPS_1001: &[u8] = &[0x03, 0xe9];
    const BPS_2000: &[u8] = &[0x07, 0xd0];
    /// 1,000,001: in range for the three bytes it occupies, far past the `uint16` the contract
    /// keeps the rate in.
    const BPS_WIDE: &[u8] = &[0x0f, 0x42, 0x41];

    /// A Pons snapshot whose terms disagree with what `_registerPool` could have written is
    /// rejected rather than quoted at whatever the attributes happen to say. The contract caps
    /// `hookFeeBps` at 1,000 and the sum at 2,000, with no separate cap on the creator tax.
    #[tokio::test]
    #[rstest]
    #[case::missing_hook_fee(None, Some(BPS_100), Outcome::Missing("pons_hook_fee_bps"))]
    #[case::missing_creator_tax(Some(BPS_100), None, Outcome::Missing("pons_creator_tax_bps"))]
    #[case::hook_fee_over_cap(Some(BPS_1001), Some(BPS_0), Outcome::Value("pons_hook_fee_bps"))]
    #[case::sum_over_cap(Some(BPS_1), Some(BPS_2000), Outcome::Value("pons_creator_tax_bps"))]
    #[case::tax_may_take_the_whole_sum(Some(BPS_0), Some(BPS_2000), Outcome::Decodes(0, 2_000))]
    #[case::three_bytes_past_uint16(
        Some(BPS_WIDE),
        Some(BPS_0),
        Outcome::Value("pons_hook_fee_bps")
    )]
    async fn pons_snapshot_fails_closed_on_bad_attributes(
        #[case] hook_fee_bps: Option<&[u8]>,
        #[case] creator_tax_bps: Option<&[u8]>,
        #[case] expected: Outcome,
    ) {
        initialize_hook_handlers().expect("hook handler registration should succeed");

        let mut snapshot = pons_fixture::snapshot();
        for (name, value) in
            [("pons_hook_fee_bps", hook_fee_bps), ("pons_creator_tax_bps", creator_tax_bps)]
        {
            let attributes = &mut snapshot.component.static_attributes;
            match value {
                Some(value) => attributes.insert(name.to_string(), Bytes::from(value.to_vec())),
                None => attributes.remove(name),
            };
        }

        let result = pons_fixture::decode_on(snapshot, Chain::Robinhood).await;

        match expected {
            Outcome::Decodes(hook_fee, creator_tax) => {
                let decoded = result.expect("terms the contract could have written must decode");
                let handler = pons_handler(&decoded);
                assert_eq!(handler.hook_fee_bps(), hook_fee);
                assert_eq!(handler.creator_tax_bps(), creator_tax);
            }
            Outcome::Missing(name) => {
                let Err(InvalidSnapshotError::MissingAttribute(reported)) = result else {
                    panic!("expected a missing-attribute error, got {result:?}");
                };
                assert_eq!(reported, name);
            }
            Outcome::Value(name) => {
                let Err(InvalidSnapshotError::ValueError(message)) = result else {
                    panic!("expected a value error, got {result:?}");
                };
                assert!(message.contains(name), "{message}");
            }
        }
    }

    /// The same pool behind a hook this crate does not model fails to decode on Robinhood, so a
    /// hook that could move the price is never quoted as if it were absent.
    #[tokio::test]
    async fn unknown_hook_on_robinhood_fails_decode() {
        initialize_hook_handlers().expect("hook handler registration should succeed");

        let result =
            pons_fixture::decode_on(pons_fixture::unknown_hook_snapshot(), Chain::Robinhood).await;

        let Err(InvalidSnapshotError::ValueError(message)) = result else {
            panic!("an unregistered hook must not decode, got {result:?}");
        };
        assert!(message.contains("unsupported uniswap v4 hook"), "{message}");
        assert!(message.contains("robinhood"), "{message}");
    }

    /// Pons is deployed on Robinhood alone, so the same address on Ethereum is a different
    /// deployment and the native handler must not serve it there. Whether the generic VM path
    /// then manages to build a handler is beside the point; it must not be the Pons one.
    #[tokio::test]
    async fn pons_same_address_on_ethereum_is_not_native() {
        initialize_hook_handlers().expect("hook handler registration should succeed");

        match pons_fixture::decode_on(pons_fixture::snapshot(), Chain::Ethereum).await {
            Ok(decoded) => assert!(
                !is_pons(&decoded),
                "the Pons handler must not serve its address off Robinhood"
            ),
            Err(error) => {
                // What actually happens today: the generic VM handler is built and then fails
                // for want of VM prerequisites this snapshot does not carry. Either way the
                // failure must not come from the Pons creator, and Ethereum being a generic-VM
                // chain it must not be the fail-closed rejection Robinhood gives either.
                let message = error.to_string();
                assert!(
                    !message.contains("pons_"),
                    "the Pons creator ran off Robinhood: {message}"
                );
                assert!(
                    !message.contains("unsupported uniswap v4 hook"),
                    "ethereum runs the generic VM handler rather than failing closed: {message}"
                );
            }
        }
    }

    /// A second quote taken on the state the first one returned sees the pool the first swap
    /// left behind. The hook moves nothing: the pool it hands back is the hookless pool's, swap
    /// for swap, and only the amounts differ, by the hook's cut.
    #[tokio::test]
    #[rstest]
    #[case::zero_for_one(true)]
    #[case::one_for_zero(false)]
    async fn pons_sequential_swaps_use_returned_state(#[case] zero_for_one: bool) {
        initialize_hook_handlers().expect("hook handler registration should succeed");

        let hooked = pons_fixture::decode_on(pons_fixture::snapshot(), Chain::Robinhood)
            .await
            .expect("the recorded Pons pool must decode on Robinhood");
        let core = pons_fixture::decode_on(pons_fixture::hookless_snapshot(), Chain::Robinhood)
            .await
            .expect("the same pool with no hook must decode too");

        let (xlg, nvda) = (pons_fixture::xlg(), pons_fixture::nvda());
        let (token_in, token_out) = if zero_for_one { (&xlg, &nvda) } else { (&nvda, &xlg) };
        let amount_in = pons_fixture::reserve(token_in) / BigUint::from(100u32);

        let hooked_first = hooked
            .get_amount_out(amount_in.clone(), token_in, token_out)
            .expect("the first hooked quote fits the pool");
        let core_first = core
            .get_amount_out(amount_in.clone(), token_in, token_out)
            .expect("the first hookless quote fits the pool");

        // The hook only accrues accounting balances, so the pool a hooked swap leaves behind is
        // the one a hookless swap of the same size leaves: same price, tick and liquidity.
        assert_eq!(
            format!("{:?}", hooked_first.new_state),
            format!("{:?}", core_first.new_state),
            "the hook must not move the pool"
        );

        let hooked_second = hooked_first
            .new_state
            .get_amount_out(amount_in.clone(), token_in, token_out)
            .expect("the second hooked quote fits the pool");
        let core_second = core_first
            .new_state
            .get_amount_out(amount_in, token_in, token_out)
            .expect("the second hookless quote fits the pool");

        assert!(
            core_second.amount < core_first.amount,
            "the second swap must execute at a worse price, or the state was not carried over"
        );
        assert_eq!(hooked_second.amount, pons_fixture::net_of_hook_take(&core_second.amount));
        assert_eq!(
            format!("{:?}", hooked_second.new_state),
            format!("{:?}", core_second.new_state)
        );
    }

    #[derive(Deserialize)]
    struct RecordedSwaps {
        swaps: Vec<RecordedSwap>,
    }

    #[derive(Deserialize)]
    struct RecordedSwap {
        label: String,
        currency0: String,
        currency1: String,
        tick_spacing: i32,
        hook_fee_bps: u16,
        creator_tax_bps: u16,
        pre_state: RecordedPoolState,
        position: RecordedPosition,
        swap: RecordedSwapLog,
        hook_fee: RecordedHookFee,
    }

    #[derive(Deserialize)]
    struct RecordedPoolState {
        sqrt_price_x96: String,
        liquidity: String,
        tick: i32,
    }

    #[derive(Deserialize)]
    struct RecordedPosition {
        tick_lower: i32,
        tick_upper: i32,
        liquidity: String,
    }

    #[derive(Deserialize)]
    struct RecordedSwapLog {
        zero_for_one: bool,
        amount0: String,
        amount1: String,
        sqrt_price_x96: String,
        liquidity: String,
        tick: i32,
    }

    #[derive(Deserialize)]
    struct RecordedHookFee {
        fee_amount: String,
        tax_amount: String,
    }

    fn recorded_swaps() -> Vec<RecordedSwap> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/assets/hooks/pons_v2/recorded_swaps.json");
        let raw =
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        serde_json::from_str::<RecordedSwaps>(&raw)
            .expect("the recorded swap fixture should parse")
            .swaps
    }

    fn recorded_token(address: &str) -> Token {
        Token::new(
            &Bytes::from_str(address).expect("recorded currency addresses parse"),
            "RECORDED",
            18,
            0,
            &[Some(10_000)],
            Chain::Robinhood,
            100,
        )
    }

    fn u256_from_decimal(value: &str, what: &str) -> U256 {
        U256::from_str_radix(value, 10).unwrap_or_else(|e| panic!("{what} = {value}: {e}"))
    }

    /// Rebuilds the pool a recorded swap ran against: one full-range position and nothing else,
    /// no LP fee and no protocol fee, which is what a Pons `PoolKey` carries.
    fn recorded_pool(recorded: &RecordedSwap) -> UniswapV4State {
        let position = recorded
            .position
            .liquidity
            .parse::<i128>()
            .unwrap_or_else(|e| panic!("{}: position liquidity: {e}", recorded.label));

        let mut pool = UniswapV4State::new(
            recorded
                .pre_state
                .liquidity
                .parse::<u128>()
                .unwrap_or_else(|e| panic!("{}: pre-state liquidity: {e}", recorded.label)),
            u256_from_decimal(&recorded.pre_state.sqrt_price_x96, "pre-state sqrt price"),
            UniswapV4Fees::new(0, 0, 0),
            recorded.pre_state.tick,
            recorded.tick_spacing,
            vec![
                TickInfo::new(recorded.position.tick_lower, position).expect("lower tick is valid"),
                TickInfo::new(recorded.position.tick_upper, -position)
                    .expect("upper tick is valid"),
            ],
        )
        .unwrap_or_else(|e| panic!("{}: rebuilding the pool: {e:?}", recorded.label));

        pool.set_hook_handler(Box::new(PonsV2HookHandler::new(
            PONS_V2_HOOK_ROBINHOOD,
            recorded.hook_fee_bps,
            recorded.creator_tax_bps,
        )));
        pool
    }

    /// Every recorded swap is a real Robinhood transaction. Quoting its input against the state
    /// the previous swap on that pool left behind has to reproduce to the wei what the swapper
    /// received once the hook had taken its cut, and leave the pool where the Swap log says it
    /// ended up.
    #[tokio::test]
    async fn pons_quotes_match_recorded_swaps() {
        let swaps = recorded_swaps();
        assert!(swaps.len() >= 6, "the fixture should cover both directions on several pools");

        for recorded in swaps {
            let pool = recorded_pool(&recorded);
            let (currency0, currency1) =
                (recorded_token(&recorded.currency0), recorded_token(&recorded.currency1));
            let (token_in, token_out) = if recorded.swap.zero_for_one {
                (&currency0, &currency1)
            } else {
                (&currency1, &currency0)
            };

            let parse = |value: &str, what: &str| {
                value
                    .parse::<i128>()
                    .unwrap_or_else(|e| panic!("{}: {what} = {value}: {e}", recorded.label))
            };
            let (amount0, amount1) = (
                parse(&recorded.swap.amount0, "amount0"),
                parse(&recorded.swap.amount1, "amount1"),
            );
            // The Swap log is the swapper's balance delta, so the leg they paid is negative.
            let (paid, received) =
                if recorded.swap.zero_for_one { (amount0, amount1) } else { (amount1, amount0) };
            assert!(paid < 0 && received > 0, "{}: not an exact-input swap", recorded.label);

            let take = parse(&recorded.hook_fee.fee_amount, "fee_amount") +
                parse(&recorded.hook_fee.tax_amount, "tax_amount");
            let expected = BigUint::from(
                u128::try_from(received - take).expect("what the swapper kept is positive"),
            );

            let quote = pool
                .get_amount_out(BigUint::from(paid.unsigned_abs()), token_in, token_out)
                .unwrap_or_else(|e| panic!("{}: {e:?}", recorded.label));

            assert_eq!(quote.amount, expected, "{}: amount out", recorded.label);

            // The Swap log also carries the state the swap left the pool in. A `UniswapV4State`
            // prints its liquidity, price, tick, fees and tick spacing and nothing else, so
            // building the logged state and comparing the two covers every one of them.
            let logged = UniswapV4State::new(
                recorded
                    .swap
                    .liquidity
                    .parse::<u128>()
                    .unwrap_or_else(|e| panic!("{}: post-swap liquidity: {e}", recorded.label)),
                u256_from_decimal(&recorded.swap.sqrt_price_x96, "post-swap sqrt price"),
                UniswapV4Fees::new(0, 0, 0),
                recorded.swap.tick,
                recorded.tick_spacing,
                Vec::new(),
            )
            .unwrap_or_else(|e| panic!("{}: building the logged state: {e:?}", recorded.label));

            assert_eq!(
                format!("{:?}", quote.new_state),
                format!("{logged:?}"),
                "{}: the pool the quote leaves behind",
                recorded.label
            );
        }
    }
}
