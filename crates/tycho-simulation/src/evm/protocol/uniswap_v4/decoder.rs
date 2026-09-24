use std::collections::HashMap;

use alloy::primitives::{Address, U256};
use itertools::Itertools;
use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim, Bytes};

use super::state::UniswapV4State;
use crate::{
    evm::protocol::{
        uniswap_v4::{
            hooks::{
                hook_handler_creator::{instantiate_hook_handler, HookCreationParams},
                utils::{has_permission, HookOptions},
            },
            state::UniswapV4Fees,
        },
        utils::uniswap::{i24_be_bytes_to_i32, tick_list::TickInfo},
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

        let ticks: Vec<_> = snapshot
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
            .try_collect()?;

        let hook_bytes: [u8; 20] = snapshot
            .component
            .static_attributes
            .get("hooks")
            .ok_or_else(|| InvalidSnapshotError::MissingAttribute("hooks".to_string()))?
            .as_ref()
            .try_into()
            .map_err(|_| {
                InvalidSnapshotError::ValueError(
                    "hooks attribute must be a 20-byte address".to_string(),
                )
            })?;
        let hook_address = Address::from(hook_bytes);

        // A hook can reshape swaps or manage the pool's liquidity only if it exposes a
        // before/after-swap permission — which also covers the *ReturnsDelta variants (v4 requires
        // the base swap flag to be set for those) and any protocol-specific handler (e.g. Angstrom,
        // Euler), since those hooks necessarily intercept swaps.
        let hook_affects_swaps = has_permission(hook_address, HookOptions::BeforeSwap) ||
            has_permission(hook_address, HookOptions::AfterSwap);

        let ticks = if ticks.is_empty() {
            // An empty tick set is legitimate when the pool holds no active liquidity (a
            // freshly-created or fully-drained pool) or when a hook manages swaps/liquidity.
            // Active liquidity with no tick data, on the other hand, is a malformed snapshot.
            if hook_affects_swaps || liquidity == 0 {
                Vec::new()
            } else {
                return Err(InvalidSnapshotError::MissingAttribute("tick_liquidities".to_string()));
            }
        } else {
            ticks
                .into_iter()
                .filter(|t| t.net_liquidity != 0)
                .sorted_unstable_by_key(|t| t.index)
                .collect()
        };

        let mut state = UniswapV4State::new(liquidity, sqrt_price, fees, tick, tick_spacing, ticks)
            .map_err(|err| {
                tracing::error!(
                    pool_id = %snapshot.component.id,
                    error = %err,
                    "Failed to create UniswapV4State"
                );
                InvalidSnapshotError::ValueError(err.to_string())
            })?;

        if hook_affects_swaps {
            // Merge state attributes into static_attributes for hook creation
            let mut merged_attributes = snapshot
                .component
                .static_attributes
                .clone();
            merged_attributes.extend(snapshot.state.attributes.clone());

            let hook_params = HookCreationParams::new(
                hook_address,
                account_balances,
                all_tokens,
                state.clone(),
                &merged_attributes,
                &snapshot.state.balances,
                decoder_context.vm_traces,
            );

            let hook_handler = instantiate_hook_handler(&hook_address, hook_params)?;
            state.set_hook_handler(hook_handler);
        }

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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use chrono::DateTime;
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
            ("hooks".to_string(), Bytes::from(vec![0u8; 20])),
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

    #[tokio::test]
    #[rstest]
    // The zero address (no-op hook) and any non-zero address without before/after-swap permission
    // bits must NOT get a hook handler attached: the pool prices via the sqrt-price branch.
    #[case::zero_address(vec![0u8; 20])]
    #[case::non_zero_without_swap_permissions({
        let mut a = vec![0u8; 20];
        a[19] = 0x01; // low permission byte with BeforeSwap (bit 7) / AfterSwap (bit 6) clear
        a
    })]
    async fn test_usv4_no_handler_for_permissionless_hook(#[case] hook_bytes: Vec<u8>) {
        let mut component = usv4_component();
        component
            .static_attributes
            .insert("hooks".to_string(), Bytes::from(hook_bytes));

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes: usv4_attributes(),
                balances: HashMap::new(),
            },
            component,
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<UniswapV4State>(snapshot)
            .await
            .unwrap();

        assert!(result.hook.is_none());
    }

    #[tokio::test]
    async fn test_usv4_tick_decode_error_propagates() {
        // A malformed tick attribute must surface as a decoding (ValueError) error, not be
        // masked as missing/empty tick liquidity.
        let mut attributes = usv4_attributes();
        attributes.insert(
            "ticks/not_an_int/net_liquidity".to_string(),
            Bytes::from(400_i128.to_be_bytes().to_vec()),
        );

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes,
                balances: HashMap::new(),
            },
            component: usv4_component(),
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<UniswapV4State>(snapshot).await;
        assert!(matches!(result, Err(InvalidSnapshotError::ValueError(_))));
    }

    #[tokio::test]
    async fn test_usv4_malformed_hooks_length_errors() {
        // A `hooks` attribute that is not a 20-byte address must error, not panic.
        let mut component = usv4_component();
        component
            .static_attributes
            .insert("hooks".to_string(), Bytes::from(vec![0u8; 19]));

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes: usv4_attributes(),
                balances: HashMap::new(),
            },
            component,
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<UniswapV4State>(snapshot).await;
        assert!(matches!(result, Err(InvalidSnapshotError::ValueError(_))));
    }

    #[tokio::test]
    async fn test_usv4_empty_hookless_pool_decodes() {
        // A hookless pool with zero active liquidity and no tick data is valid (e.g. a freshly
        // created or fully-drained pool) and must decode as an empty pool, not be rejected.
        let mut attributes = usv4_attributes();
        attributes.insert("liquidity".to_string(), Bytes::from(0_u64.to_be_bytes().to_vec()));
        attributes.remove("ticks/60/net_liquidity");

        let snapshot = ComponentWithState {
            state: ProtocolComponentState {
                component_id: "State1".to_owned(),
                attributes,
                balances: HashMap::new(),
            },
            component: usv4_component(), // hooks = zero address
            component_tvl: None,
            entrypoints: Vec::new(),
        };

        let result = try_decode_snapshot_with_defaults::<UniswapV4State>(snapshot)
            .await
            .unwrap();

        let expected = UniswapV4State::new(
            0,
            U256::from(79228162514264337593543950336_u128),
            UniswapV4Fees::new(0, 0, 500),
            300,
            60,
            vec![],
        )
        .unwrap();
        assert_eq!(result, expected);
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
    #[case::missing_hooks("hooks")]
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

        if missing_attribute == "hooks" {
            component
                .static_attributes
                .remove("hooks");
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
}
