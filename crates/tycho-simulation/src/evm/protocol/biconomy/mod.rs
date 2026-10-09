//! Biconomy PropAMM venue: one push-payment `IPropAMM` contract serving many pairs, quoting the
//! boards makers store in the PropAMM executor. The state is the executor's raw board storage,
//! priced at the execution block the way `PropAMMVenue.quote` prices it.

use tycho_client::feed::BlockHeader;

use crate::evm::decoder::TychoStreamDecoder;

mod decoder;
mod math;
pub mod state;

pub use state::BiconomyState;

pub const PROTOCOL_SYSTEM: &str = "biconomy";

pub fn register_biconomy_decoder(decoder: &mut TychoStreamDecoder<BlockHeader>) {
    decoder.register_decoder::<BiconomyState>(PROTOCOL_SYSTEM);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::primitives::U256;
    use num_bigint::BigUint;
    use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
    use tycho_common::{
        dto::{ProtocolComponent, ProtocolStateDelta, ResponseProtocolState},
        models::{token::Token, Chain},
        simulation::protocol_sim::{Balances, BlockContext, ProtocolSim},
        Bytes,
    };

    use super::{decoder::decode_biconomy_snapshot, state::BiconomyState};

    const VENUE: &str = "0x000000da21a0f02b2626874870b6447db220c1ef";
    const MM: &str = "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc";
    const PROVIDER: &str = "0xa5d389cb45ac9e4c41329f8edca523e3ffcd4ca1";
    const WETH: &str = "0x8b414ad7005eefd315af2a16538885eae229bab7";
    const USDC: &str = "0xabbdbbbd6d56593a9c5656c06cb30d61e4a544df";

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(&Bytes::from(address), symbol, decimals, 0, &[Some(100_000)], Chain::Base, 100)
    }

    fn word(value: U256) -> Bytes {
        Bytes::from(value.to_be_bytes::<32>().to_vec())
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
            ("makers".to_owned(), Bytes::from(MM)),
            (board(0), word(header)),
            (board(2), word(level(1_000_000_000_000_000_000, 2_000_000_000))),
            (board(3), word(level(2_000_000_000_000_000_000, 1_990_000_000))),
            (format!("inventory/{PROVIDER}/{USDC}"), word(U256::from(5_000_000_000u64))),
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
                static_attributes: HashMap::from([("pamm_address".to_owned(), Bytes::from(VENUE))]),
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
        // 5000 USDC of inventory less 2000 paid covers 1990 more, but not a third WETH.
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

    #[test]
    fn delta_updates_and_deletes_attributes() {
        let mut state = state();
        let delta = ProtocolStateDelta {
            component_id: VENUE.to_owned(),
            updated_attributes: HashMap::from([(format!("paused/{MM}"), Bytes::from(vec![1u8]))]),
            deleted_attributes: [format!("inventory/{PROVIDER}/{USDC}")].into(),
        };

        state
            .delta_transition(delta, &HashMap::new(), &Balances::default())
            .unwrap();

        assert_eq!(state.paused.len(), 1);
        assert!(state.inventory.is_empty());
    }

    /// Rebuilds the state from the executor's and venue's raw storage on Base Sepolia, the words
    /// the substreams package indexes, and checks every quote against `PropAMMVenue.quote` at the
    /// same block. Needs `BASE_SEPOLIA_RPC_URL` (an archive node).
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
            decoder::{board_key, pair_key},
            math::BOARD_WORDS,
            state::BlockEnvState,
        };

        sol! {
            function makers() external view returns (address[] memory);
            function feeBps() external view returns (uint16);
            function quote(address tokenIn, address tokenOut, uint256 amountIn) external view returns (uint256);
            function available(address token) external view returns (uint256);
        }

        const EXECUTOR: &str = "0x000000d4d7CB15E0FA9aB2B1fd49ca8537CDCA26";
        const BLOCKS: [u64; 3] = [47_849_900, 47_849_920, 47_849_940];

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

        async fn state_at<P: Provider>(provider: &P, block: u64) -> BiconomyState {
            let venue = Address::from_str(VENUE).unwrap();
            let executor = Address::from_str(EXECUTOR).unwrap();
            let tokens = [Address::from_str(WETH).unwrap(), Address::from_str(USDC).unwrap()];
            let makers = makersCall::abi_decode_returns(
                &call(provider, venue, makersCall {}.abi_encode(), block)
                    .await
                    .unwrap(),
            )
            .unwrap();
            let fee_bps = feeBpsCall::abi_decode_returns(
                &call(provider, venue, feeBpsCall {}.abi_encode(), block)
                    .await
                    .unwrap(),
            )
            .unwrap();
            let header = provider
                .get_block_by_number(block.into())
                .await
                .unwrap()
                .unwrap()
                .header;

            let mut state = BiconomyState {
                fee_bps,
                makers: makers.iter().map(|m| m.0 .0).collect(),
                block: BlockEnvState { number: block, timestamp: header.timestamp },
                ..Default::default()
            };
            let storage = |slot: U256| async move {
                provider
                    .get_storage_at(executor, slot)
                    .block_id(BlockId::number(block))
                    .await
                    .unwrap()
            };
            for mm in &makers {
                if !storage(slot(*mm, U256::from(2u8)))
                    .await
                    .is_zero()
                {
                    state.paused.insert(mm.0 .0);
                }
                let (t0, t1) = if tokens[0] < tokens[1] {
                    (tokens[0], tokens[1])
                } else {
                    (tokens[1], tokens[0])
                };
                let anchor = storage(slot(t1, slot(t0, slot(*mm, U256::from(1u8))))).await;
                state
                    .anchors
                    .insert(board_key(&mm.0 .0, &t0.0 .0, &t1.0 .0), anchor);
                for (tin, tout) in [(tokens[0], tokens[1]), (tokens[1], tokens[0])] {
                    let base = slot(tout, slot(tin, slot(*mm, U256::ZERO)));
                    let mut words = BTreeMap::new();
                    for i in 0..BOARD_WORDS {
                        let value = storage(base + U256::from(i)).await;
                        if !value.is_zero() {
                            words.insert(i, value);
                        }
                    }
                    let provider_address = Address::from_slice(
                        &words
                            .get(&0)
                            .copied()
                            .unwrap_or_default()
                            .to_be_bytes::<32>()[12..],
                    );
                    // The executor treats a failing `available` call as unbounded.
                    let available = call(
                        provider,
                        provider_address,
                        availableCall { token: tout }.abi_encode(),
                        block,
                    )
                    .await
                    .and_then(|out| availableCall::abi_decode_returns(&out).ok())
                    .unwrap_or(U256::MAX);
                    state
                        .inventory
                        .insert(pair_key(&provider_address.0 .0, &tout.0 .0), available);
                    state
                        .boards
                        .insert(board_key(&mm.0 .0, &tin.0 .0, &tout.0 .0), words);
                }
            }
            state
        }

        #[tokio::test]
        async fn matches_venue_quote_on_base_sepolia() {
            let url = env::var("BASE_SEPOLIA_RPC_URL").expect("BASE_SEPOLIA_RPC_URL");
            let provider = ProviderBuilder::new().connect_http(url.parse().unwrap());
            let venue = Address::from_str(VENUE).unwrap();
            let weth = token(WETH, "WETH", 18);
            let usdc = token(USDC, "USDC", 18);

            let mut compared = 0;
            let mut live = 0;
            for block in BLOCKS {
                let state = state_at(&provider, block).await;
                for (tin, tout, sizes) in [
                    (
                        &weth,
                        &usdc,
                        vec![
                            1u64,
                            1_000_000,
                            1_000_000_000_000,
                            1_000_000_000_000_000,
                            5_000_000_000_000_000,
                            9_000_000_000_000_000,
                            50_000_000_000_000_000,
                        ],
                    ),
                    (
                        &usdc,
                        &weth,
                        vec![
                            1u64,
                            1_000,
                            1_000_000,
                            20_000_000,
                            1_000_000_000,
                            1_000_000_000_000_000_000,
                        ],
                    ),
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
