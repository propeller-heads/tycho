//! Biconomy PropAMM venue: one push-payment `IPropAMM` contract serving many pairs, quoting the
//! boards makers store in the PropAMM executor. The state is the executor's raw board storage,
//! priced at the execution block the way `PropAMMVenue.quote` prices it.

mod decoder;
mod math;
pub mod state;

pub use state::BiconomyState;

pub const PROTOCOL_SYSTEM: &str = "biconomy";

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::primitives::U256;
    use num_bigint::BigUint;
    use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
    use tycho_common::{
        dto::{ProtocolComponent, ProtocolStateDelta, ResponseProtocolState},
        models::{token::Token, Chain},
        simulation::protocol_sim::{
            Balances, BlockContext, Price, ProtocolSim, QueryPoolSwapParams, SwapConstraint,
        },
        Bytes,
    };

    use super::{decoder::decode_biconomy_snapshot, state::BiconomyState};

    const VENUE: &str = "0x0000009e5f98cdb4dd1e1d2e15b220217c88b3b8";
    const EXECUTOR: &str = "0x0000006f9bba066e4efda2e0cabe4ff8cf9030ac";
    const MM: &str = "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc";
    const PROVIDER: &str = "0xa5d389cb45ac9e4c41329f8edca523e3ffcd4ca1";
    const WETH: &str = "0x4200000000000000000000000000000000000006";
    const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(&Bytes::from(address), symbol, decimals, 0, &[Some(100_000)], Chain::Base, 100)
    }

    /// A provider config attribute: kind byte, vault.
    fn provider_config(kind: u8, vault: Option<&str>) -> Bytes {
        let mut out = vec![kind];
        out.extend(
            vault
                .map(|v| Bytes::from(v).to_vec())
                .unwrap_or(vec![0u8; 20]),
        );
        Bytes::from(out)
    }

    fn word(value: U256) -> Bytes {
        Bytes::from(value.to_be_bytes::<32>().to_vec())
    }

    fn address_word(address: &str) -> Bytes {
        word(U256::from_str_radix(&address[2..], 16).unwrap())
    }

    /// One maker with a two-level price board WETH -> USDC: 1 WETH at 2000, then 1 more at 1990.
    fn snapshot() -> ComponentWithState {
        let header = U256::from_str_radix(&PROVIDER[2..], 16).unwrap() |
            (U256::from(10_000u64) << 160) |
            (U256::from(2u8) << 200) |
            (U256::from(1u8) << 208);
        let level = |size: u128, price: u128| U256::from(size) | (U256::from(price) << 128);
        let board = |slot: u8| format!("board/{MM}/{WETH}/{USDC}/{slot}");
        let attributes = HashMap::from([
            ("fee_bps".to_owned(), Bytes::from(10u16.to_be_bytes().to_vec())),
            ("maker_count".to_owned(), Bytes::from(vec![1u8])),
            ("maker/0".to_owned(), Bytes::from(MM)),
            (board(0), word(header)),
            (board(2), word(level(1_000_000_000_000_000_000, 2_000_000_000))),
            (board(3), word(level(2_000_000_000_000_000_000, 1_990_000_000))),
            (format!("provider/{PROVIDER}"), provider_config(0, None)),
            (format!("signer/{PROVIDER}"), address_word(MM)),
            (format!("approved_executor/{PROVIDER}"), address_word(EXECUTOR)),
            (format!("balance/{USDC}/{PROVIDER}"), word(U256::from(5_000_000_000u64))),
        ]);
        ComponentWithState {
            state: ResponseProtocolState {
                component_id: VENUE.to_owned(),
                attributes,
                balances: HashMap::new(),
            }
            .into(),
            component: ProtocolComponent {
                id: VENUE.to_owned(),
                protocol_system: "biconomy".to_owned(),
                tokens: vec![Bytes::from(WETH), Bytes::from(USDC)],
                static_attributes: HashMap::from([
                    ("pamm_address".to_owned(), Bytes::from(VENUE)),
                    ("executor".to_owned(), Bytes::from(EXECUTOR)),
                ]),
                ..Default::default()
            }
            .into(),
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    fn state() -> BiconomyState {
        let header = BlockHeader { number: 99, timestamp: 1_000, ..Default::default() };
        decode_biconomy_snapshot(&snapshot(), &header).unwrap()
    }

    fn delta(updated: Vec<(String, Bytes)>, deleted: Vec<String>) -> ProtocolStateDelta {
        ProtocolStateDelta {
            component_id: VENUE.to_owned(),
            updated_attributes: updated.into_iter().collect(),
            deleted_attributes: deleted.into_iter().collect(),
        }
    }

    #[test]
    fn quotes_across_levels_net_of_fee() {
        let state = state();
        let weth = token(WETH, "WETH", 18);
        let usdc = token(USDC, "USDC", 6);

        let result = state
            .get_amount_out(BigUint::from(1_500_000_000_000_000_000u64), &weth, &usdc)
            .unwrap();

        // 2000 + 0.5 * 1990 = 2995 USDC gross, minus 10 bps.
        assert_eq!(result.amount, BigUint::from(2_995_000_000u64 - 2_995_000u64));
    }

    #[test]
    fn fill_advances_the_meter_and_spends_inventory() {
        let state = state();
        let weth = token(WETH, "WETH", 18);
        let usdc = token(USDC, "USDC", 6);

        let first = state
            .get_amount_out(BigUint::from(1_000_000_000_000_000_000u64), &weth, &usdc)
            .unwrap();
        let next = first
            .new_state
            .get_amount_out(BigUint::from(1_000_000_000_000_000_000u64), &weth, &usdc)
            .unwrap();

        // The second WETH fills on the 1990 level.
        assert_eq!(next.amount, BigUint::from(1_990_000_000u64 - 1_990_000u64));
        // 5000 USDC of inventory less 2000 and 1990 paid leaves 10 USDC, too little for the
        // next wei of input to be worth anything at 1990.
        let (limit_in, _) = next
            .new_state
            .get_limits(Bytes::from(WETH), Bytes::from(USDC))
            .unwrap();
        assert_eq!(limit_in, BigUint::ZERO);
    }

    #[test]
    fn rejects_sizes_the_venue_cannot_cover() {
        let state = state();
        let weth = token(WETH, "WETH", 18);
        let usdc = token(USDC, "USDC", 6);

        assert!(state
            .get_amount_out(BigUint::from(3_000_000_000_000_000_000u64), &weth, &usdc)
            .is_err());
        assert!(state
            .get_amount_out(BigUint::from(1u8), &usdc, &weth)
            .is_err());
    }

    #[test]
    fn board_expires_with_the_execution_block() {
        let mut state = state();
        let weth = token(WETH, "WETH", 18);
        let usdc = token(USDC, "USDC", 6);

        assert!(state.apply_block(&BlockContext::new(200, 10_001)));
        assert!(state
            .get_amount_out(BigUint::from(1_000u64), &weth, &usdc)
            .is_err());
    }

    /// Applies `updated` and checks the board goes dark, then comes back once `restored`.
    fn assert_dark_until_restored(updated: Vec<(String, Bytes)>, restored: Vec<(String, Bytes)>) {
        let mut state = state();
        let weth = token(WETH, "WETH", 18);
        let usdc = token(USDC, "USDC", 6);
        // 0.001 WETH, 2 USDC at the first level.
        let one = BigUint::from(1_000_000_000_000_000u64);
        assert!(state
            .get_amount_out(one.clone(), &weth, &usdc)
            .is_ok());

        for (attributes, live) in [(updated, false), (restored, true)] {
            state
                .delta_transition(delta(attributes, vec![]), &HashMap::new(), &Balances::default())
                .unwrap();
            assert_eq!(
                state
                    .get_amount_out(one.clone(), &weth, &usdc)
                    .is_ok(),
                live
            );
        }
    }

    const OTHER: &str = "0x0000000000000000000000000000000000000001";

    #[test]
    fn board_is_dark_when_the_provider_names_another_signer() {
        assert_dark_until_restored(
            vec![(format!("signer/{PROVIDER}"), address_word(OTHER))],
            vec![(format!("signer/{PROVIDER}"), address_word(MM))],
        );
    }

    #[test]
    fn quote_fails_when_the_provider_approves_another_executor() {
        // The venue still routes to the maker, and the fill reverts with `NotApprovedExecutor`.
        assert_dark_until_restored(
            vec![(format!("approved_executor/{PROVIDER}"), address_word(OTHER))],
            vec![(format!("approved_executor/{PROVIDER}"), address_word(EXECUTOR))],
        );
    }

    #[test]
    fn vault_provider_has_no_inventory_unless_the_vault_routes_to_it() {
        let vault = "0x862ffacd6b13b4734bd8550c720f115f3f31e6ab";
        let vault_provider = vec![
            (format!("provider/{PROVIDER}"), provider_config(1, Some(vault))),
            (format!("balance/{USDC}/{vault}"), word(U256::from(5_000_000_000u64))),
            (format!("allowance/{USDC}/{vault}/{PROVIDER}"), word(U256::MAX)),
            (format!("router_count/{vault}"), word(U256::from(1u8))),
        ];
        let routed = |router: &str| {
            let mut attributes = vault_provider.clone();
            attributes.push((format!("router/{vault}"), address_word(router)));
            attributes
        };

        assert_dark_until_restored(routed(OTHER), routed(PROVIDER));
        // An empty router array: `router()` is zero even with a stale first element.
        let mut emptied = routed(PROVIDER);
        emptied.push((format!("router_count/{vault}"), word(U256::ZERO)));
        assert_dark_until_restored(
            emptied,
            vec![(format!("router_count/{vault}"), word(U256::from(1u8)))],
        );
    }

    #[test]
    fn get_limits_are_the_largest_size_the_venue_fills() {
        let state = state();
        let weth = token(WETH, "WETH", 18);
        let usdc = token(USDC, "USDC", 6);

        let (limit_in, limit_out) = state
            .get_limits(Bytes::from(WETH), Bytes::from(USDC))
            .unwrap();

        // Two levels of 1 WETH each, both within the 5000 USDC of inventory.
        assert_eq!(limit_in, BigUint::from(2_000_000_000_000_000_000u64));
        let at_limit = state
            .get_amount_out(limit_in.clone(), &weth, &usdc)
            .unwrap();
        assert_eq!(at_limit.amount, limit_out);
        assert!(state
            .get_amount_out(limit_in + 1u8, &weth, &usdc)
            .is_err());
        // Every size up to the limit fills, and output grows with input.
        let mut previous = BigUint::ZERO;
        for step in 1u64..=20 {
            let size = BigUint::from(step) * BigUint::from(100_000_000_000_000_000u64);
            let out = state
                .get_amount_out(size, &weth, &usdc)
                .unwrap()
                .amount;
            assert!(out >= previous);
            previous = out;
        }
        assert_eq!(previous, limit_out);
    }

    #[test]
    fn delta_updates_and_deletes_attributes() {
        let mut state = state();

        state
            .delta_transition(
                delta(
                    vec![(format!("paused/{MM}"), Bytes::from(vec![1u8]))],
                    vec![format!("balance/{USDC}/{PROVIDER}")],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();

        assert_eq!(state.paused.len(), 1);
        assert_eq!(state.balances.values().copied().max(), Some(U256::ZERO));
    }

    /// A venue state with one price board WETH -> USDC per maker, each maker with its own
    /// balance provider holding plenty of USDC: `(size, price)` per maker in registry order.
    fn makers_state(levels: &[(u128, u128)], approved: &[bool]) -> BiconomyState {
        let mut attributes = HashMap::from([
            ("fee_bps".to_owned(), Bytes::from(vec![0u8])),
            ("maker_count".to_owned(), Bytes::from(vec![levels.len() as u8])),
        ]);
        for (i, ((size, price), ok)) in levels.iter().zip(approved).enumerate() {
            let mm = format!("0x{:040x}", 0x1000 + i);
            let provider = format!("0x{:040x}", 0x2000 + i);
            let header = U256::from_str_radix(&provider[2..], 16).unwrap() |
                (U256::from(10_000u64) << 160) |
                (U256::from(1u8) << 200) |
                (U256::from(1u8) << 208);
            let board = |slot: u8| format!("board/{mm}/{WETH}/{USDC}/{slot}");
            attributes.insert(format!("maker/{i}"), Bytes::from(mm.as_str()));
            attributes.insert(board(0), word(header));
            attributes.insert(board(2), word(U256::from(*size) | (U256::from(*price) << 128)));
            attributes.insert(format!("provider/{provider}"), provider_config(0, None));
            attributes.insert(format!("signer/{provider}"), address_word(&mm));
            let executor =
                if *ok { EXECUTOR } else { "0x0000000000000000000000000000000000000001" };
            attributes.insert(format!("approved_executor/{provider}"), address_word(executor));
            attributes.insert(format!("balance/{USDC}/{provider}"), word(U256::from(u64::MAX)));
        }
        let mut snap = snapshot();
        snap.state.attributes = attributes;
        decode_biconomy_snapshot(
            &snap,
            &BlockHeader { number: 99, timestamp: 1_000, ..Default::default() },
        )
        .unwrap()
    }

    #[test]
    fn get_limits_reports_what_quote_fillable_reports() {
        let weth = token(WETH, "WETH", 18);
        let usdc = token(USDC, "USDC", 6);
        let limits = |state: &BiconomyState| {
            state
                .get_limits(Bytes::from(WETH), Bytes::from(USDC))
                .unwrap()
        };

        // 100 wei at 1.0 and 1 wei at 0.6: the dust maker floors to zero and drops out, so the
        // first plan covers 100, and the re-plan at 100 covers it again.
        let state = makers_state(
            &[(100, 1_000_000_000_000_000_000), (1, 600_000_000_000_000_000)],
            &[true, true],
        );
        let (limit_in, limit_out) = limits(&state);
        assert_eq!(
            (limit_in.clone(), limit_out.clone()),
            (BigUint::from(100u8), BigUint::from(100u8))
        );
        assert_eq!(
            state
                .get_amount_out(limit_in, &weth, &usdc)
                .unwrap()
                .amount,
            limit_out
        );

        // Add 2 wei at 0.5: the first plan covers 102, but at 102 both dust makers floor to zero
        // and only 100 is covered, so the venue's `quoteFillable` reports (0, 0) and so do we.
        let state = makers_state(
            &[
                (100, 1_000_000_000_000_000_000),
                (1, 600_000_000_000_000_000),
                (2, 500_000_000_000_000_000),
            ],
            &[true, true, true],
        );
        assert_eq!(limits(&state), (BigUint::ZERO, BigUint::ZERO));
        assert!(state
            .get_amount_out(BigUint::from(102u8), &weth, &usdc)
            .is_err());
    }

    #[test]
    fn quote_fails_when_the_venue_would_route_to_a_provider_without_the_executor() {
        // The better-priced maker's provider dropped the executor: the venue still allocates to
        // it first and the swap reverts, rather than routing to the second maker.
        let state = makers_state(
            &[
                (1_000_000_000_000_000_000, 2_000_000_000),
                (1_000_000_000_000_000_000, 1_990_000_000),
            ],
            &[false, true],
        );
        let weth = token(WETH, "WETH", 18);
        let usdc = token(USDC, "USDC", 6);
        assert!(state
            .get_amount_out(BigUint::from(1_000_000_000_000_000u64), &weth, &usdc)
            .is_err());
        assert_eq!(
            state
                .get_limits(Bytes::from(WETH), Bytes::from(USDC))
                .unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
    }

    #[test]
    fn get_limits_stop_where_the_venue_would_route_to_a_provider_without_the_executor() {
        // The worse-priced maker's provider dropped the executor: the venue fills through the
        // better maker up to its size and reverts only beyond it.
        let state = makers_state(
            &[
                (1_000_000_000_000_000_000, 2_000_000_000),
                (1_000_000_000_000_000_000, 1_990_000_000),
            ],
            &[true, false],
        );
        let weth = token(WETH, "WETH", 18);
        let usdc = token(USDC, "USDC", 6);
        let (limit_in, limit_out) = state
            .get_limits(Bytes::from(WETH), Bytes::from(USDC))
            .unwrap();
        assert_eq!(limit_in, BigUint::from(1_000_000_000_000_000_000u64));
        assert_eq!(limit_out, BigUint::from(2_000_000_000u64));
        assert_eq!(
            state
                .get_amount_out(limit_in.clone(), &weth, &usdc)
                .unwrap()
                .amount,
            limit_out
        );
        assert!(state
            .get_amount_out(limit_in + 1u8, &weth, &usdc)
            .is_err());
    }

    #[test]
    fn get_amount_out_rejects_amounts_beyond_uint256() {
        let state = makers_state(&[(1_000_000_000_000_000_000, 2_000_000_000)], &[true]);
        let result = state.get_amount_out(
            BigUint::from(1u8) << 256,
            &token(WETH, "WETH", 18),
            &token(USDC, "USDC", 6),
        );
        assert!(matches!(
            result,
            Err(tycho_common::simulation::errors::SimulationError::InvalidInput(..))
        ));
    }

    /// 1 WETH at 2000 USDC and 1 WETH at 1990 USDC, WETH -> USDC only (no USDC -> WETH boards).
    fn one_sided_book() -> BiconomyState {
        makers_state(
            &[
                (1_000_000_000_000_000_000, 2_000_000_000),
                (1_000_000_000_000_000_000, 1_990_000_000),
            ],
            &[true, true],
        )
    }

    /// `usdc` USDC per `weth` WETH, in raw units.
    fn usdc_per_weth(usdc: u64, weth: u64) -> Price {
        Price::new(BigUint::from(usdc) * 1_000_000u32, BigUint::from(weth) * 10u64.pow(18))
    }

    fn query(
        state: &BiconomyState,
        constraint: SwapConstraint,
    ) -> Result<(BigUint, BigUint), tycho_common::simulation::errors::SimulationError> {
        state
            .query_pool_swap(&QueryPoolSwapParams::new(
                token(WETH, "WETH", 18),
                token(USDC, "USDC", 6),
                constraint,
            ))
            .map(|swap| (swap.amount_in().clone(), swap.amount_out().clone()))
    }

    fn trade_limit(limit: Price) -> SwapConstraint {
        SwapConstraint::TradeLimitPrice {
            limit,
            tolerance: 0.0,
            min_amount_in: None,
            max_amount_in: None,
        }
    }

    fn pool_target(target: Price, tolerance: f64) -> SwapConstraint {
        SwapConstraint::PoolTargetPrice {
            target,
            tolerance,
            min_amount_in: None,
            max_amount_in: None,
        }
    }

    #[test]
    fn trade_limit_price_takes_the_largest_trade_at_or_above_the_limit() {
        let state = one_sided_book();
        let weth = BigUint::from(10u64.pow(18));

        // Both levels average exactly 1995: the whole book.
        let (amount_in, amount_out) = query(&state, trade_limit(usdc_per_weth(1995, 1))).unwrap();
        assert_eq!((amount_in, amount_out), (&weth * 2u8, BigUint::from(3_990_000_000u64)));

        // 1996 on average needs 2000 + 1990 * (x - 1) >= 1996 * x, so x <= 5/3 WETH; the output
        // floors to whole micro-USDC, which costs up to 1e-6 USDC of margin (6 * dx >= 1e-6).
        let limit = usdc_per_weth(1996, 1);
        let (amount_in, amount_out) = query(&state, trade_limit(limit.clone())).unwrap();
        assert!(amount_in <= BigUint::from(1_666_666_666_666_666_666u64));
        assert!(amount_in > BigUint::from(1_666_666_400_000_000_000u64));
        assert!(&amount_out * &limit.denominator >= &amount_in * &limit.numerator);
        let beyond = &amount_in + 1_000_000_000_000u64;
        let beyond_out = state
            .get_amount_out(beyond.clone(), &token(WETH, "WETH", 18), &token(USDC, "USDC", 6))
            .unwrap()
            .amount;
        assert!(beyond_out * &limit.denominator < beyond * &limit.numerator);

        // Above the best level, nothing qualifies.
        assert_eq!(
            query(&state, trade_limit(usdc_per_weth(2001, 1))).unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
    }

    #[test]
    fn pool_target_price_walks_the_forward_levels() {
        let state = one_sided_book();
        let weth = BigUint::from(10u64.pow(18));

        // Already at the target: no trade.
        assert_eq!(
            query(&state, pool_target(usdc_per_weth(2000, 1), 0.0)).unwrap(),
            (BigUint::ZERO, BigUint::ZERO)
        );
        // The second level is exactly the target: fill the first.
        assert_eq!(
            query(&state, pool_target(usdc_per_weth(1990, 1), 0.0)).unwrap(),
            (weth.clone(), BigUint::from(2_000_000_000u64))
        );
        // No level lands in [1995, 1995]: stop on the last level above it, the closest price
        // not below the target.
        let (amount_in, _) = query(&state, pool_target(usdc_per_weth(1995, 1), 0.0)).unwrap();
        assert_eq!(amount_in, &weth - 1u8);
        // A tolerance wide enough to admit the second level fills the first.
        let (amount_in, _) = query(&state, pool_target(usdc_per_weth(1980, 1), 0.01)).unwrap();
        assert_eq!(amount_in, weth.clone());
        // Below the whole book: take all of it.
        let (amount_in, _) = query(&state, pool_target(usdc_per_weth(1000, 1), 0.0)).unwrap();
        assert_eq!(amount_in, &weth * 2u8);
        // Above the marginal price: unreachable by selling WETH.
        assert!(query(&state, pool_target(usdc_per_weth(2001, 1), 0.0)).is_err());
    }

    #[test]
    fn spot_price_is_the_quote_needed_to_buy_one_base_with_the_fee_as_markup() {
        // A USDC -> WETH board paying 1/2000 WETH per USDC: 5e14 raw WETH per 1e6 raw USDC, so
        // 5e26 per 1e18 raw USDC. With the 10 bps fee, one WETH costs 2000 / 0.999 USDC.
        let mut state = state();
        let header = U256::from_str_radix(&PROVIDER[2..], 16).unwrap() |
            (U256::from(10_000u64) << 160) |
            (U256::from(1u8) << 200) |
            (U256::from(1u8) << 208);
        let level = U256::from(1_000_000_000u64) |
            (U256::from(500_000_000_000_000_000_000_000_000u128) << 128);
        state
            .delta_transition(
                delta(
                    vec![
                        (format!("board/{MM}/{USDC}/{WETH}/0"), word(header)),
                        (format!("board/{MM}/{USDC}/{WETH}/2"), word(level)),
                        (format!("balance/{WETH}/{PROVIDER}"), word(U256::from(10u128.pow(20)))),
                    ],
                    vec![],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();
        let price = state
            .spot_price(&token(WETH, "WETH", 18), &token(USDC, "USDC", 6))
            .unwrap();
        assert!((price - 2_000.0 / 0.999).abs() < 1e-6, "{price}");
    }

    /// Rebuilds the state from the executor's and venue's raw storage on Base, the words the
    /// substreams package indexes, and checks every quote against `PropAMMVenue.quote` at the
    /// same block. Needs `BASE_RPC_URL` (an archive node).
    #[cfg(feature = "network_tests")]
    mod network {
        use std::{collections::BTreeMap, env, str::FromStr};

        use alloy::{
            eips::BlockId,
            primitives::{keccak256, Address, U256},
            providers::{Provider, ProviderBuilder},
            rpc::types::TransactionRequest,
            sol,
            sol_types::SolCall,
        };

        use super::*;
        use crate::evm::protocol::biconomy::{
            decoder::{apply_attributes, board_key, hex_address, AttributeChange},
            math::BOARD_WORDS,
            state::BlockEnvState,
        };

        sol! {
            function makers() external view returns (address[] memory);
            function feeBps() external view returns (uint16);
            function quote(address tokenIn, address tokenOut, uint256 amountIn) external view returns (uint256);
            function available(address token) external view returns (uint256);
            function signer() external view returns (address);
            function approvedExecutor() external view returns (address);
            function vault() external view returns (address);
            function router() external view returns (address);
        }

        const CBBTC: &str = "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf";
        // Early blocks with a balance provider's price ladders live in both directions; blocks
        // across the vault provider's onboarding and maker list rewrites; and blocks with fills
        // against the vault provider's boards.
        const BLOCKS: [u64; 13] = [
            52_549_100, 52_549_180, 52_549_230, 52_806_929, 52_808_000, 52_809_300, 52_922_000,
            53_096_813, 53_100_992, 53_103_396, 53_108_702, 53_112_973, 53_114_528,
        ];

        fn slot(key: Address, base: U256) -> U256 {
            let mut buf = [0u8; 64];
            buf[12..32].copy_from_slice(key.as_slice());
            buf[32..].copy_from_slice(&base.to_be_bytes::<32>());
            U256::from_be_bytes(keccak256(buf).0)
        }

        async fn call<P: Provider>(
            provider: &P,
            to: Address,
            data: Vec<u8>,
            block: u64,
        ) -> Option<Vec<u8>> {
            let tx = TransactionRequest::default()
                .to(to)
                .input(data.into());
            provider
                .call(tx)
                .block(BlockId::number(block))
                .await
                .ok()
                .map(|out| out.to_vec())
        }

        /// Balance and allowance mapping slots of the venue's tokens on Base.
        fn token_slots(token: Address) -> (u8, u8) {
            if token == Address::from_str(WETH).unwrap() {
                (3, 4)
            } else {
                (9, 10)
            }
        }

        /// The state the substreams package indexes, rebuilt from storage at `block`: venue
        /// slots, board and anchor words, and token balances and allowances behind each
        /// provider. Provider configs (kind, signer, vault) are what the package takes as params.
        async fn state_at<P: Provider>(
            provider: &P,
            block: u64,
            tokens: &[Address],
        ) -> BiconomyState {
            let venue = Address::from_str(VENUE).unwrap();
            let executor = Address::from_str(EXECUTOR).unwrap();
            let header = provider
                .get_block_by_number(block.into())
                .await
                .unwrap()
                .unwrap()
                .header;
            let storage = |contract: Address, slot: U256| async move {
                provider
                    .get_storage_at(contract, slot)
                    .block_id(BlockId::number(block))
                    .await
                    .unwrap()
            };

            let mut state = BiconomyState {
                executor: executor.0 .0,
                block: BlockEnvState { number: block, timestamp: header.timestamp },
                ..Default::default()
            };
            let mut attributes: Vec<(String, Bytes)> = vec![
                ("fee_bps".to_owned(), word(storage(venue, U256::ZERO).await)),
                ("maker_count".to_owned(), word(storage(venue, U256::from(1u8)).await)),
            ];
            let count = storage(venue, U256::from(1u8))
                .await
                .to::<u64>();
            let first = U256::from_be_bytes(keccak256(U256::from(1u8).to_be_bytes::<32>()).0);
            let mut makers = Vec::new();
            for i in 0..count {
                let value = storage(venue, first + U256::from(i)).await;
                makers.push(Address::from_slice(&value.to_be_bytes::<32>()[12..]));
                attributes.push((format!("maker/{i}"), word(value)));
            }

            for mm in &makers {
                if !storage(executor, slot(*mm, U256::from(2u8)))
                    .await
                    .is_zero()
                {
                    state.paused.insert(mm.0 .0);
                }
                for (i, a) in tokens.iter().enumerate() {
                    for b in &tokens[i + 1..] {
                        let (t0, t1) = if a < b { (*a, *b) } else { (*b, *a) };
                        let anchor =
                            storage(executor, slot(t1, slot(t0, slot(*mm, U256::from(1u8))))).await;
                        state
                            .anchors
                            .insert(board_key(&mm.0 .0, &t0.0 .0, &t1.0 .0), anchor);
                        for (tin, tout) in [(*a, *b), (*b, *a)] {
                            let base = slot(tout, slot(tin, slot(*mm, U256::ZERO)));
                            let mut words = BTreeMap::new();
                            for i in 0..BOARD_WORDS {
                                let value = storage(executor, base + U256::from(i)).await;
                                if !value.is_zero() {
                                    words.insert(i, value);
                                }
                            }
                            let board_provider = Address::from_slice(
                                &words
                                    .get(&0)
                                    .copied()
                                    .unwrap_or_default()
                                    .to_be_bytes::<32>()[12..],
                            );
                            state
                                .boards
                                .insert(board_key(&mm.0 .0, &tin.0 .0, &tout.0 .0), words);
                            if board_provider.is_zero() {
                                continue;
                            }
                            let key = format!("provider/{}", hex_address(&board_provider.0 .0));
                            if attributes
                                .iter()
                                .any(|(name, _)| *name == key)
                            {
                                continue;
                            }
                            let signer =
                                call(provider, board_provider, signerCall {}.abi_encode(), block)
                                    .await
                                    .and_then(|out| signerCall::abi_decode_returns(&out).ok())
                                    .unwrap_or_default();
                            let approved_executor = call(
                                provider,
                                board_provider,
                                approvedExecutorCall {}.abi_encode(),
                                block,
                            )
                            .await
                            .and_then(|out| approvedExecutorCall::abi_decode_returns(&out).ok())
                            .unwrap_or_default();
                            let vault =
                                call(provider, board_provider, vaultCall {}.abi_encode(), block)
                                    .await
                                    .and_then(|out| vaultCall::abi_decode_returns(&out).ok())
                                    .filter(|vault| !vault.is_zero());
                            let available = call(
                                provider,
                                board_provider,
                                availableCall { token: tout }.abi_encode(),
                                block,
                            )
                            .await
                            .and_then(|out| availableCall::abi_decode_returns(&out).ok())
                            .unwrap_or(U256::MAX);
                            let kind: u8 = match vault {
                                Some(_) => 1,
                                None if available >= U256::from(u128::MAX) => 2,
                                None => 0,
                            };
                            let mut config = vec![kind];
                            config.extend(vault.unwrap_or_default().0 .0);
                            attributes.push((key, Bytes::from(config)));
                            let provider_key = hex_address(&board_provider.0 .0);
                            attributes.push((
                                format!("signer/{provider_key}"),
                                Bytes::from(signer.into_word().to_vec()),
                            ));
                            attributes.push((
                                format!("approved_executor/{provider_key}"),
                                Bytes::from(approved_executor.into_word().to_vec()),
                            ));
                            if let Some(vault) = vault {
                                let router =
                                    call(provider, vault, routerCall {}.abi_encode(), block)
                                        .await
                                        .and_then(|out| routerCall::abi_decode_returns(&out).ok())
                                        .unwrap_or_default();
                                let vault_key = hex_address(&vault.0 .0);
                                attributes.push((
                                    format!("router_count/{vault_key}"),
                                    word(U256::from(u8::from(!router.is_zero()))),
                                ));
                                attributes.push((
                                    format!("router/{vault_key}"),
                                    Bytes::from(router.into_word().to_vec()),
                                ));
                            }
                            for token in tokens {
                                let (balance_slot, allowance_slot) = token_slots(*token);
                                let holder = vault.unwrap_or(board_provider);
                                if kind != 2 {
                                    let value =
                                        storage(*token, slot(holder, U256::from(balance_slot)))
                                            .await;
                                    attributes.push((
                                        format!(
                                            "balance/{}/{}",
                                            hex_address(&token.0 .0),
                                            hex_address(&holder.0 .0)
                                        ),
                                        word(value),
                                    ));
                                }
                                if let Some(vault) = vault {
                                    let value = storage(
                                        *token,
                                        slot(
                                            board_provider,
                                            slot(vault, U256::from(allowance_slot)),
                                        ),
                                    )
                                    .await;
                                    attributes.push((
                                        format!(
                                            "allowance/{}/{}/{}",
                                            hex_address(&token.0 .0),
                                            hex_address(&vault.0 .0),
                                            hex_address(&board_provider.0 .0)
                                        ),
                                        word(value),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            apply_attributes(
                &mut state,
                attributes
                    .iter()
                    .map(|(name, value)| AttributeChange::Set(name, value)),
            )
            .unwrap();
            state
        }

        #[tokio::test]
        async fn matches_venue_quote_on_base() {
            let url = env::var("BASE_RPC_URL").expect("BASE_RPC_URL");
            let provider = ProviderBuilder::new().connect_http(url.parse().unwrap());
            let venue = Address::from_str(VENUE).unwrap();
            let weth = token(WETH, "WETH", 18);
            let usdc = token(USDC, "USDC", 6);
            let cbbtc = token(CBBTC, "cbBTC", 8);
            let tokens: Vec<Address> = [&weth, &usdc, &cbbtc]
                .iter()
                .map(|t| Address::from_slice(t.address.as_ref()))
                .collect();

            let mut compared = 0;
            let mut live = 0;
            for block in BLOCKS {
                let state = state_at(&provider, block, &tokens).await;
                for (tin, tout, sizes) in [
                    (
                        &weth,
                        &usdc,
                        vec![
                            1u64,
                            1_000_000_000,
                            100_000_000_000_000,
                            500_000_000_000_000,
                            600_000_000_000_000,
                            10_000_000_000_000_000,
                        ],
                    ),
                    (
                        &usdc,
                        &weth,
                        vec![1u64, 1_000, 1_000_000, 5_000_000, 20_000_000, 1_000_000_000_000],
                    ),
                    (&weth, &cbbtc, vec![1_000_000_000_000_000u64]),
                    (&usdc, &cbbtc, vec![1_000_000u64]),
                ] {
                    for size in sizes {
                        let data = quoteCall {
                            tokenIn: Address::from_slice(tin.address.as_ref()),
                            tokenOut: Address::from_slice(tout.address.as_ref()),
                            amountIn: U256::from(size),
                        }
                        .abi_encode();
                        let chain = call(&provider, venue, data, block)
                            .await
                            .map(|out| quoteCall::abi_decode_returns(&out).unwrap());
                        let sim = state
                            .get_amount_out(BigUint::from(size), tin, tout)
                            .ok()
                            .map(|result| {
                                crate::evm::protocol::u256_num::biguint_to_u256(&result.amount)
                            });
                        assert_eq!(
                            sim, chain,
                            "block {block} {} -> {} size {size}",
                            tin.symbol, tout.symbol
                        );
                        compared += 1;
                        live += usize::from(chain.is_some());
                    }
                }
            }
            println!("compared {compared} quotes, {live} live");
            assert!(live >= compared / 2, "only {live} of {compared} quotes were live");
        }
    }
}
