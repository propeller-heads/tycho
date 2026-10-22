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

    const VENUE: &str = "0x0000009e5f98cdb4dd1e1d2e15b220217c88b3b8";
    const MM: &str = "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc";
    const PROVIDER: &str = "0xa5d389cb45ac9e4c41329f8edca523e3ffcd4ca1";
    const WETH: &str = "0x4200000000000000000000000000000000000006";
    const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";

    fn token(address: &str, symbol: &str, decimals: u32) -> Token {
        Token::new(&Bytes::from(address), symbol, decimals, 0, &[Some(100_000)], Chain::Base, 100)
    }

    /// A provider config attribute: kind byte, signer, vault.
    fn provider_config(kind: u8, signer: &str, vault: Option<&str>) -> Bytes {
        let mut out = vec![kind];
        out.extend(Bytes::from(signer).to_vec());
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
            (format!("provider/{PROVIDER}"), provider_config(0, MM, None)),
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

    #[test]
    fn board_is_dark_when_the_provider_names_another_signer() {
        let mut state = state();
        let weth = token(WETH, "WETH", 18);
        let usdc = token(USDC, "USDC", 6);
        let other = "0x0000000000000000000000000000000000000001";

        state
            .delta_transition(
                delta(
                    vec![(format!("provider/{PROVIDER}"), provider_config(0, other, None))],
                    vec![],
                ),
                &HashMap::new(),
                &Balances::default(),
            )
            .unwrap();

        assert!(state
            .get_amount_out(BigUint::from(1_000u64), &weth, &usdc)
            .is_err());
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
            function vault() external view returns (address);
        }

        const EXECUTOR: &str = "0x0000006f9bBA066e4efda2e0CAbE4FF8cf9030aC";
        const CBBTC: &str = "0xcbb7c0000ab88b473b1f5afd9ef808440eed33bf";
        // Blocks where a maker's WETH/USDC boards were live in both directions.
        const BLOCKS: [u64; 3] = [52_549_100, 52_549_180, 52_549_230];

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
                            config.extend(signer.0 .0);
                            config.extend(vault.unwrap_or_default().0 .0);
                            attributes.push((key, Bytes::from(config)));
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
