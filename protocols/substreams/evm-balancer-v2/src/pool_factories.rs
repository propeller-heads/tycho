use crate::{abi, modules::VAULT_ADDRESS};
use substreams::hex;
use substreams_ethereum::{
    pb::eth::v2::{Call, Log, TransactionTrace},
    Event, Function,
};
use tycho_substreams::{
    attributes::{json_serialize_address_list, json_serialize_bigint_list},
    prelude::*,
};

/// Helper function to get pool_registered event
fn get_pool_registered(
    tx: &TransactionTrace,
    pool_address: &Vec<u8>,
) -> abi::vault::events::PoolRegistered {
    tx.logs_with_calls()
        .filter(|(log, _)| log.address == VAULT_ADDRESS)
        .filter_map(|(log, _)| abi::vault::events::PoolRegistered::match_and_decode(log))
        .find(|pool| pool.pool_address == *pool_address)
        .unwrap()
        .clone()
}

// TODO: add this back if we need to track BPT
// fn get_token_registered(
//     tx: &TransactionTrace,
//     pool_id: &[u8],
// ) -> abi::vault::events::TokensRegistered {
//     tx.logs_with_calls()
//         .filter(|(log, _)| log.address == VAULT_ADDRESS)
//         .filter_map(|(log, _)| abi::vault::events::TokensRegistered::match_and_decode(log))
//         .find(|ev| ev.pool_id == pool_id)
//         .unwrap()
//         .clone()
// }

// This is the main function that handles the creation of `ProtocolComponent`s with `Attribute`s
//  based on the specific factory address. There's 3 factory groups that are represented here:
//  - Weighted Pool Factories
//  - Linear Pool Factories
//  - Stable Pool Factories
// (Balancer does have a bit more (esp. in the deprecated section) that could be implemented as
//  desired.)
// We use the specific ABIs to decode both the log event and corresponding call to gather
//  `PoolCreated` event information alongside the `Create` call data that provide us details to
//  fulfill both the required details + any extra `Attributes`
// Ref: https://docs.balancer.fi/reference/contracts/deployment-addresses/mainnet.html
pub fn address_map(
    pool_factory_address: &[u8],
    log: &Log,
    call: &Call,
    tx: &TransactionTrace,
) -> Option<ProtocolComponent> {
    match *pool_factory_address {
        hex!("8E9aa87E45e92bad84D5F8DD1bff34Fb92637dE9") => {
            let create_call =
                abi::weighted_pool_factory_v1::functions::Create::match_and_decode(call)?;
            let pool_created =
                abi::weighted_pool_factory_v1::events::PoolCreated::match_and_decode(log)?;
            let pool_registered = get_pool_registered(tx, &pool_created.pool);

            Some(
                ProtocolComponent::new(&format!("0x{}", hex::encode(pool_registered.pool_id)))
                    .with_contracts(&[pool_created.pool, VAULT_ADDRESS.to_vec()])
                    .with_tokens(&create_call.tokens)
                    .with_attributes(&[
                        ("pool_type", "WeightedPoolFactoryV1".as_bytes()),
                        ("normalized_weights", &json_serialize_bigint_list(&create_call.weights)),
                        ("manual_updates", &[1u8]),
                    ])
                    .as_swap_type("balancer_v2_pool", ImplementationType::Vm),
            )
        }
        hex!("cC508a455F5b0073973107Db6a878DdBDab957bC") => {
            let create_call =
                abi::weighted_pool_factory_v2::functions::Create::match_and_decode(call)?;
            let pool_created =
                abi::weighted_pool_factory_v2::events::PoolCreated::match_and_decode(log)?;
            let pool_registered = get_pool_registered(tx, &pool_created.pool);

            Some(
                ProtocolComponent::new(&format!("0x{}", hex::encode(pool_registered.pool_id)))
                    .with_contracts(&[pool_created.pool, VAULT_ADDRESS.to_vec()])
                    .with_tokens(&create_call.tokens)
                    .with_attributes(&[
                        ("pool_type", "WeightedPoolFactoryV2".as_bytes()),
                        (
                            "normalized_weights",
                            &json_serialize_bigint_list(&create_call.normalized_weights),
                        ),
                        (
                            "rate_providers",
                            &json_serialize_address_list(&create_call.rate_providers),
                        ),
                        ("manual_updates", &[1u8]),
                    ])
                    .as_swap_type("balancer_v2_pool", ImplementationType::Vm),
            )
        }
        hex!("5Dd94Da3644DDD055fcf6B3E1aa310Bb7801EB8b") => {
            let create_call =
                abi::weighted_pool_factory_v3::functions::Create::match_and_decode(call)?;
            let pool_created =
                abi::weighted_pool_factory_v3::events::PoolCreated::match_and_decode(log)?;
            let pool_registered = get_pool_registered(tx, &pool_created.pool);

            Some(
                ProtocolComponent::new(&format!("0x{}", hex::encode(pool_registered.pool_id)))
                    .with_contracts(&[pool_created.pool, VAULT_ADDRESS.to_vec()])
                    .with_tokens(&create_call.tokens)
                    .with_attributes(&[
                        ("pool_type", "WeightedPoolFactoryV3".as_bytes()),
                        (
                            "normalized_weights",
                            &json_serialize_bigint_list(&create_call.normalized_weights),
                        ),
                        (
                            "rate_providers",
                            &json_serialize_address_list(&create_call.rate_providers),
                        ),
                        ("manual_updates", &[1u8]),
                    ])
                    .as_swap_type("balancer_v2_pool", ImplementationType::Vm),
            )
        }
        hex!("897888115Ada5773E02aA29F775430BFB5F34c51") => {
            let create_call =
                abi::weighted_pool_factory_v4::functions::Create::match_and_decode(call)?;
            let pool_created =
                abi::weighted_pool_factory_v4::events::PoolCreated::match_and_decode(log)?;
            let pool_registered = get_pool_registered(tx, &pool_created.pool);

            Some(
                ProtocolComponent::new(&format!("0x{}", hex::encode(pool_registered.pool_id)))
                    .with_contracts(&[pool_created.pool, VAULT_ADDRESS.to_vec()])
                    .with_tokens(&create_call.tokens)
                    .with_attributes(&[
                        ("pool_type", "WeightedPoolFactoryV4".as_bytes()),
                        (
                            "normalized_weights",
                            &json_serialize_bigint_list(&create_call.normalized_weights),
                        ),
                        (
                            "rate_providers",
                            &json_serialize_address_list(&create_call.rate_providers),
                        ),
                        ("manual_updates", &[1u8]),
                    ])
                    .as_swap_type("balancer_v2_pool", ImplementationType::Vm),
            )
        }
        hex!("DB8d758BCb971e482B2C45f7F8a7740283A1bd3A") => {
            let create_call =
                abi::composable_stable_pool_factory::functions::Create::match_and_decode(call)?;
            let pool_created =
                abi::composable_stable_pool_factory::events::PoolCreated::match_and_decode(log)?;
            let pool_registered = get_pool_registered(tx, &pool_created.pool);
            // let tokens_registered = get_token_registered(tx, &pool_registered.pool_id);

            Some(
                ProtocolComponent::new(&format!("0x{}", hex::encode(pool_registered.pool_id)))
                    .with_contracts(&[pool_created.pool.clone(), VAULT_ADDRESS.to_vec()])
                    // .with_tokens(&tokens_registered.tokens) // TODO: add this back if we need to
                    // track BPT
                    .with_tokens(&create_call.tokens)
                    .with_attributes(&[
                        ("pool_type", "ComposableStablePoolFactory".as_bytes()),
                        ("bpt", &pool_created.pool),
                        (
                            "rate_providers",
                            &json_serialize_address_list(&create_call.rate_providers),
                        ),
                        ("manual_updates", &[1u8]),
                    ])
                    .as_swap_type("balancer_v2_pool", ImplementationType::Vm),
            )
        }
        hex!("813EE7a840CE909E7Fea2117A44a90b8063bd4fd") => {
            let create_call =
                abi::erc_linear_pool_factory::functions::Create::match_and_decode(call)?;
            let pool_created =
                abi::erc_linear_pool_factory::events::PoolCreated::match_and_decode(log)?;
            let pool_registered = get_pool_registered(tx, &pool_created.pool);
            // let tokens_registered = get_token_registered(tx, &pool_registered.pool_id);

            Some(
                ProtocolComponent::new(&format!("0x{}", hex::encode(pool_registered.pool_id)))
                    .with_contracts(&[pool_created.pool.clone(), VAULT_ADDRESS.to_vec()])
                    // .with_tokens(&tokens_registered.tokens) // TODO: add this back if we need to
                    // track BPT
                    .with_tokens(&[
                        create_call.main_token.clone(),
                        create_call.wrapped_token.clone(),
                    ])
                    .with_attributes(&[
                        ("pool_type", "ERC4626LinearPoolFactory".as_bytes()),
                        (
                            "upper_target",
                            &create_call
                                .upper_target
                                .to_signed_bytes_be(),
                        ),
                        ("manual_updates", &[1u8]),
                        ("bpt", &pool_created.pool),
                        ("main_token", &create_call.main_token),
                        ("wrapped_token", &create_call.wrapped_token),
                    ])
                    .as_swap_type("balancer_v2_pool", ImplementationType::Vm),
            )
        }
        // The `WeightedPool2TokenFactory` is a deprecated contract, but we've included
        // it to be able to track one of the highest TVL pools: 80BAL-20WETH.
        hex!("A5bf2ddF098bb0Ef6d120C98217dD6B141c74EE0") => {
            let create_call =
                abi::weighted_pool_tokens_factory::functions::Create::match_and_decode(call)?;
            let pool_created =
                abi::weighted_pool_tokens_factory::events::PoolCreated::match_and_decode(log)?;
            let pool_registered = get_pool_registered(tx, &pool_created.pool);

            Some(
                ProtocolComponent::new(&format!("0x{}", hex::encode(pool_registered.pool_id)))
                    .with_contracts(&[pool_created.pool, VAULT_ADDRESS.to_vec()])
                    .with_tokens(&create_call.tokens)
                    .with_attributes(&[
                        ("pool_type", "WeightedPool2TokensFactory".as_bytes()),
                        ("weights", &json_serialize_bigint_list(&create_call.weights)),
                        ("manual_updates", &[1u8]),
                    ])
                    .as_swap_type("balancer_v2_pool", ImplementationType::Vm),
            )
        }

        // ❌ EulerLinearPoolFactory: factory is disabled and no existing pools have relevant
        // liquidity hex!("5F43FBa61f63Fa6bFF101a0A0458cEA917f6B347") => {
        //     let create_call =
        //         abi::euler_linear_pool_factory::functions::Create::match_and_decode(call)?;
        //     let pool_created =
        //         abi::euler_linear_pool_factory::events::PoolCreated::match_and_decode(log)?;
        //     let pool_registered = get_pool_registered(tx, &pool_created.pool);
        //     let tokens_registered = get_token_registered(tx, &pool_registered.pool_id);

        //     Some(
        //         ProtocolComponent::new(&format!("0x{}", hex::encode(pool_registered.pool_id)))
        //             .with_contracts(&[pool_created.pool.clone(), VAULT_ADDRESS.to_vec()])
        //             // .with_tokens(&tokens_registered.tokens)
        //             .with_tokens(&[
        //                 create_call.main_token.clone(),
        //                 create_call.wrapped_token.clone(),
        //             ])
        //             .with_attributes(&[
        //                 ("pool_type", "EulerLinearPoolFactory".as_bytes()),
        //                 (
        //                     "upper_target",
        //                     &create_call
        //                         .upper_target
        //                         .to_signed_bytes_be(),
        //                 ),
        //                 ("manual_updates", &[1u8]),
        //                 ("bpt", &pool_created.pool),
        //                 ("main_token", &create_call.main_token),
        //                 ("wrapped_token", &create_call.wrapped_token),
        //                 (
        //                     "fee",
        //                     &create_call
        //                         .swap_fee_percentage
        //                         .to_signed_bytes_be(),
        //                 ),
        //             ])
        //             .as_swap_type("balancer_v2_pool", ImplementationType::Vm),
        //     )
        // }

        // ❌ Reading the deployed factory for Gearbox showcases that it's currently disabled
        // hex!("39A79EB449Fc05C92c39aA6f0e9BfaC03BE8dE5B") => {
        //     let create_call =
        //         abi::gearbox_linear_pool_factory::functions::Create::match_and_decode(call)?;
        //     let pool_created =
        //         abi::gearbox_linear_pool_factory::events::PoolCreated::match_and_decode(log)?;

        //     Some(tycho::ProtocolComponent {
        //         id: hex::encode(&pool_created.pool),
        //         tokens: vec![create_call.main_token, create_call.wrapped_token],
        //         contracts: vec![pool_addr.into(), pool_created.pool],
        //         static_att: vec![
        //             tycho::Attribute {
        //                 name: "pool_type".into(),
        //                 value: "GearboxLinearPoolFactory".into(),
        //                 change: tycho::ChangeType::Creation.into(),
        //             },
        //             tycho::Attribute {
        //                 name: "upper_target".into(),
        //                 value: create_call.upper_target.to_signed_bytes_be(),
        //                 change: tycho::ChangeType::Creation.into(),
        //             },
        //         ],
        //         change: tycho::ChangeType::Creation.into(),
        //     })
        // }

        // ❌ The `ManagedPoolFactory` is a bit ✨ unique ✨, so we'll leave it commented out for
        // now. Take a look at it's `Create` call to see how the params are structured.
        // hex!("BF904F9F340745B4f0c4702c7B6Ab1e808eA6b93") => {
        //     let create_call =
        // abi::managed_pool_factory::functions::Create::match_and_decode(call)?;
        //     let pool_created =
        //         abi::managed_pool_factory::events::PoolCreated::match_and_decode(log)?;

        //     Some(tycho::ProtocolComponent {
        //         id: hex::encode(&pool_created.pool),
        //         tokens: create_call.tokens,
        //         contracts: vec![pool_addr.into(), pool_created.pool],
        //         static_att: vec![
        //             tycho::Attribute {
        //                 name: "pool_type".into(),
        //                 value: "ManagedPoolFactory".into(),
        //                 change: tycho::ChangeType::Creation.into(),
        //             },
        //         ],
        //         change: tycho::ChangeType::Creation.into(),
        //     })
        // }

        // ❌ SiloLinearPoolFactory: factory is disabled and no existing pools have relevant
        // liquidity hex!("4E11AEec21baF1660b1a46472963cB3DA7811C89") => {
        //     let create_call =
        //         abi::silo_linear_pool_factory::functions::Create::match_and_decode(call)?;
        //     let pool_created =
        //         abi::silo_linear_pool_factory::events::PoolCreated::match_and_decode(log)?;
        //     let pool_registered = get_pool_registered(tx, &pool_created.pool);
        //     let tokens_registered = get_token_registered(tx, &pool_registered.pool_id);

        //     Some(
        //         ProtocolComponent::new(&format!("0x{}", hex::encode(pool_registered.pool_id)))
        //             .with_contracts(&[pool_created.pool.clone(), VAULT_ADDRESS.to_vec()])
        //             .with_tokens(&tokens_registered.tokens)
        //             .with_attributes(&[
        //                 ("pool_type", "SiloLinearPoolFactory".as_bytes()),
        //                 (
        //                     "upper_target",
        //                     &create_call
        //                         .upper_target
        //                         .to_signed_bytes_be(),
        //                 ),
        //                 ("manual_updates", &[1u8]),
        //                 ("bpt", &pool_created.pool),
        //                 ("main_token", &create_call.main_token),
        //                 ("wrapped_token", &create_call.wrapped_token),
        //                 (
        //                     "fee",
        //                     &create_call
        //                         .swap_fee_percentage
        //                         .to_signed_bytes_be(),
        //                 ),
        //             ])
        //             .as_swap_type("balancer_v2_pool", ImplementationType::Vm),
        //     )
        // }

        // ❌ YearnLinearPoolFactory: factory is disabled and no existing pools have relevant
        // liquidity hex!("5F5222Ffa40F2AEd6380D022184D6ea67C776eE0") => {
        //     let create_call =
        //         abi::yearn_linear_pool_factory::functions::Create::match_and_decode(call)?;
        //     let pool_created =
        //         abi::yearn_linear_pool_factory::events::PoolCreated::match_and_decode(log)?;
        //     let pool_registered = get_pool_registered(tx, &pool_created.pool);
        //     let tokens_registered = get_token_registered(tx, &pool_registered.pool_id);

        //     Some(
        //         ProtocolComponent::new(&format!("0x{}", hex::encode(pool_registered.pool_id)))
        //             .with_contracts(&[pool_created.pool.clone(), VAULT_ADDRESS.to_vec()])
        //             .with_tokens(&tokens_registered.tokens)
        //             .with_attributes(&[
        //                 ("pool_type", "YearnLinearPoolFactory".as_bytes()),
        //                 (
        //                     "upper_target",
        //                     &create_call
        //                         .upper_target
        //                         .to_signed_bytes_be(),
        //                 ),
        //                 ("manual_updates", &[1u8]),
        //                 ("bpt", &pool_created.pool),
        //                 ("main_token", &create_call.main_token),
        //                 ("wrapped_token", &create_call.wrapped_token),
        //                 (
        //                     "fee",
        //                     &create_call
        //                         .swap_fee_percentage
        //                         .to_signed_bytes_be(),
        //                 ),
        //             ])
        //             .as_swap_type("balancer_v2_pool", ImplementationType::Vm),
        //     )
        // }
        _ => None,
    }
}
