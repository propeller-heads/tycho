use substreams_ethereum::{
    pb::eth::v2::{Call, Log, TransactionTrace},
    Event, Function,
};

use crate::{
    abi,
    consts::{ETH_ADDRESS, ZERO_ADDRESS},
    params::CurveParams,
};
use tycho_substreams::{
    attributes::{json_serialize_address_list, json_serialize_bigint_list, json_serialize_value},
    prelude::*,
};

use substreams::scalar::BigInt;

/// This trait defines some helpers for serializing and deserializing `Vec<BigInt>` which is needed
///  to be able to encode some of the `Attribute`s. This should also be handled by any downstream
///  application.
#[allow(dead_code)]
trait SerializableVecBigInt {
    fn serialize_bytes(&self) -> Vec<u8>;
    fn deserialize_bytes(bytes: &[u8]) -> Vec<BigInt>;
}

impl SerializableVecBigInt for Vec<BigInt> {
    fn serialize_bytes(&self) -> Vec<u8> {
        self.iter()
            .flat_map(|big_int| big_int.to_signed_bytes_be())
            .collect()
    }
    fn deserialize_bytes(bytes: &[u8]) -> Vec<BigInt> {
        bytes
            .as_chunks::<32>()
            .0
            .iter()
            .map(|chunk| BigInt::from_signed_bytes_be(chunk))
            .collect::<Vec<BigInt>>()
    }
}

/// Converts address bytes into a Vec<u8> containing a leading `0x`.
fn address_to_bytes_with_0x(address: &[u8; 20]) -> Vec<u8> {
    address_to_string_with_0x(address).into_bytes()
}

/// Converts address bytes into a string containing a leading `0x`.
fn address_to_string_with_0x(address: &[u8]) -> String {
    format!("0x{}", hex::encode(address))
}

/// Function that swaps `WETH` addresses for `ETH` address for specific factory types that decide
///  to use `WETH` address even though native `ETH` is stored. This is also extra weird bc ETH
///  doesn't even have a real address, so we use the standard `0xEEEee...`.
fn swap_weth_for_eth(weth: [u8; 20], tokens: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    if tokens.contains(&weth.into()) {
        tokens
            .into_iter()
            .map(|token| if token == weth { ETH_ADDRESS.into() } else { token })
            .collect::<Vec<_>>()
    } else {
        tokens
    }
}

/// This massive function matches factory address to specific logic to construct
///  `ProtocolComponent`s and their related `EntityChanges` at creation. While, most of the logic is
/// readily replicable, several factories differ  in information density resulting in needing other
/// information sources such as decoding calls  or even making RPC calls to provide extra details.
///
/// Each `ProtocolComponent` contains the following static attributes:
/// - `pool_type`: The type of pool, such as `crypto_pool`, `plain_pool`, `metapool`, etc.
/// - `name`: The name of the pool.
/// - `factory_name`: The name of the factory that created the pool.
/// - `factory`: The address of the factory that created the pool.
///
/// The basic flow of this function is as follows:
/// - Match the factory address
/// - Decode the relevant event from the log
/// - Attempt to decode the corresponding function call (based on the permutation of the ABI)
/// - Optionally make an RPC call to produce further information (see metapools)
/// - Construct the corresponding `ProtocolComponent`
pub fn address_map(
    curve_params: &CurveParams,
    call_address: &[u8; 20],
    log: &Log,
    call: &Call,
    tx: &TransactionTrace,
) -> Option<(ProtocolComponent, Vec<EntityChanges>)> {
    if *call_address == ZERO_ADDRESS {
        return None;
    }
    match *call_address {
        addr if addr ==
            curve_params
                .protocol_params
                .crypto_pool_factory =>
        {
            let pool_added =
                abi::crypto_pool_factory::events::CryptoPoolDeployed::match_and_decode(log)?;

            let pool_name = abi::crypto_pool_factory::functions::DeployPool::match_and_decode(call)
                .map_or("none".to_string(), |call| call.name);

            let tokens =
                swap_weth_for_eth(curve_params.protocol_params.weth, pool_added.coins.into());

            let component_id = &call.return_data[12..];

            let token_implementation = extract_proxy_impl(call, tx, 0).unwrap_or([1u8; 20]);
            let pool_implementation = extract_proxy_impl(call, tx, 1).unwrap_or([1u8; 20]);

            Some((
                ProtocolComponent {
                    id: address_to_string_with_0x(component_id),
                    tokens: tokens.clone(),
                    contracts: vec![
                        component_id.into(),
                        pool_added.token.clone(),
                        curve_params
                            .protocol_params
                            .crypto_pool_factory
                            .into(),
                    ],
                    static_att: vec![
                        Attribute {
                            name: "pool_type".into(),
                            value: "crypto_pool".into(),
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "name".into(),
                            value: pool_name.into(),
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "factory_name".into(),
                            value: "crypto_pool_factory".into(),
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "factory".into(),
                            value: address_to_bytes_with_0x(
                                &curve_params
                                    .protocol_params
                                    .crypto_pool_factory,
                            ),
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "lp_token".into(),
                            value: pool_added.token,
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "coins".into(),
                            value: json_serialize_address_list(&tokens),
                            change: ChangeType::Creation.into(),
                        },
                    ],
                    change: ChangeType::Creation.into(),
                    protocol_type: Some(ProtocolType {
                        name: "curve_pool".into(),
                        financial_type: FinancialType::Swap.into(),
                        attribute_schema: Vec::new(),
                        implementation_type: ImplementationType::Vm.into(),
                    }),
                },
                vec![EntityChanges {
                    component_id: address_to_string_with_0x(component_id),
                    attributes: vec![
                        Attribute {
                            name: "stateless_contract_addr_0".into(),
                            value: address_to_bytes_with_0x(&pool_implementation),
                            change: ChangeType::Creation.into(),
                        },
                        Attribute {
                            name: "stateless_contract_addr_1".into(),
                            value: address_to_bytes_with_0x(&token_implementation),
                            change: ChangeType::Creation.into(),
                        },
                    ],
                }],
            ))
        }
        addr if addr ==
            curve_params
                .protocol_params
                .meta_pool_factory =>
        {
            if let Some(pool_added) =
                abi::meta_pool_factory::events::PlainPoolDeployed::match_and_decode(log)
            {
                let add_pool =
                    abi::meta_pool_factory::functions::DeployPlainPool1::match_and_decode(call)
                        .map(|add_pool| abi::meta_pool_factory::functions::DeployPlainPool3 {
                            name: add_pool.name,
                            symbol: add_pool.symbol,
                            coins: add_pool.coins,
                            a: add_pool.a,
                            fee: add_pool.fee,
                            asset_type: BigInt::from(0),
                            implementation_idx: BigInt::from(0),
                        })
                        .or_else(|| {
                            abi::meta_pool_factory::functions::DeployPlainPool2::match_and_decode(
                                call,
                            )
                            .map(|add_pool| {
                                abi::meta_pool_factory::functions::DeployPlainPool3 {
                                    name: add_pool.name,
                                    symbol: add_pool.symbol,
                                    coins: add_pool.coins,
                                    a: add_pool.a,
                                    fee: add_pool.fee,
                                    asset_type: add_pool.asset_type,
                                    implementation_idx: BigInt::from(0),
                                }
                            })
                        })
                        .or_else(|| {
                            abi::meta_pool_factory::functions::DeployPlainPool3::match_and_decode(
                                call,
                            )
                        })?;

                // The return data of several of these calls contain the actual component id
                let component_id = &call.return_data[12..];
                let tokens: Vec<_> = pool_added
                    .coins
                    .into_iter()
                    .filter(|token| *token != [0; 20])
                    .collect();
                let use_set_oracle = tokens.len() == 2 &&
                    add_pool
                        .implementation_idx
                        .eq(&BigInt::from(5));
                let mut static_attrs = vec![
                    Attribute {
                        name: "pool_type".into(),
                        value: "plain_pool".into(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "name".into(),
                        value: add_pool.name.into(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "factory_name".into(),
                        value: "meta_pool_factory".into(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "factory".into(),
                        value: address_to_bytes_with_0x(
                            &curve_params
                                .protocol_params
                                .meta_pool_factory,
                        ),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "coins".into(),
                        value: json_serialize_address_list(&tokens),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "set_oracle".into(),
                        value: vec![use_set_oracle as u8],
                        change: ChangeType::Creation.into(),
                    },
                ];
                // Pool implementation contracts that support rebase tokens:
                // n_coins=2: implementation_idx=[1, 5]
                // n_coins=3: implementation_idx=[1]
                // n_coins=4: implementation_idx=[1]
                let is_rebasing = match tokens.len() {
                    2 => {
                        add_pool.implementation_idx == BigInt::from(1) ||
                            add_pool.implementation_idx == BigInt::from(5)
                    }
                    3 | 4 => add_pool.implementation_idx == BigInt::from(1),
                    _ => false,
                };

                if is_rebasing {
                    let rebase_tokens: Vec<Vec<u8>> = tokens
                        .iter()
                        .filter(|coin| coin.as_slice() != ETH_ADDRESS)
                        .cloned()
                        .collect();

                    if !rebase_tokens.is_empty() {
                        static_attrs.push(Attribute {
                            name: "rebase_tokens".to_string(),
                            value: json_serialize_address_list(&rebase_tokens),
                            change: ChangeType::Creation.into(),
                        });
                    }
                }
                let pool_implementation = extract_proxy_impl(call, tx, 0).unwrap_or([1u8; 20]);
                Some((
                    ProtocolComponent {
                        id: address_to_string_with_0x(component_id),
                        tokens: tokens.clone(),
                        contracts: vec![component_id.into()],
                        static_att: static_attrs,
                        change: ChangeType::Creation.into(),
                        protocol_type: Some(ProtocolType {
                            name: "curve_pool".into(),
                            financial_type: FinancialType::Swap.into(),
                            attribute_schema: Vec::new(),
                            implementation_type: ImplementationType::Vm.into(),
                        }),
                    },
                    vec![EntityChanges {
                        component_id: address_to_string_with_0x(component_id),
                        attributes: vec![Attribute {
                            name: "stateless_contract_addr_0".into(),
                            value: address_to_bytes_with_0x(&pool_implementation),
                            change: ChangeType::Creation.into(),
                        }],
                    }],
                ))
            }
            // else if let Some(pool_added) =
            //     abi::meta_pool_factory::events::MetaPoolDeployed::match_and_decode(log)
            // {
            //     let add_pool =
            //         abi::meta_pool_factory::functions::DeployMetapool1::match_and_decode(call)
            //             .map(|add_pool| abi::meta_pool_factory::functions::DeployMetapool2 {
            //                 base_pool: add_pool.base_pool,
            //                 name: add_pool.name,
            //                 symbol: add_pool.symbol,
            //                 coin: add_pool.coin,
            //                 a: add_pool.a,
            //                 fee: add_pool.fee,
            //                 implementation_idx: BigInt::from(0),
            //             })
            //             .or_else(|| {
            //                 abi::meta_pool_factory::functions::DeployMetapool2::match_and_decode(
            //                     call,
            //                 )
            //             })?;

            //     let component_id = &call.return_data[12..];

            //     // The `add_pool.base_pool` may only refer to the contract of the base pool and
            // not     //  the token itself. This means we **have** to make an RPC call
            // to the     //  `meta_registry` in order to get the real LP token address.
            //     let get_lp_token =
            //         abi::meta_registry::functions::GetLpToken1 { pool: add_pool.base_pool.clone()
            // };     let lp_token = get_lp_token.call(META_REGISTRY.to_vec())?;

            //     let pool_implementation = extract_proxy_impl(call, tx, 0).unwrap_or([1u8; 20]);

            //     Some((
            //         ProtocolComponent {
            //             id: hex::encode(component_id),
            //             tx: Some(Transaction {
            //                 to: tx.to.clone(),
            //                 from: tx.from.clone(),
            //                 hash: tx.hash.clone(),
            //                 index: tx.index.into(),
            //             }),
            //             tokens: vec![pool_added.coin, lp_token],
            //             contracts: vec![component_id.into(), add_pool.base_pool.clone()],
            //             static_att: vec![
            //                 Attribute {
            //                     name: "pool_type".into(),
            //                     value: "metapool".into(),
            //                     change: ChangeType::Creation.into(),
            //                 },
            //                 Attribute {
            //                     name: "name".into(),
            //                     value: add_pool.name.into(),
            //                     change: ChangeType::Creation.into(),
            //                 },
            //                 Attribute {
            //                     name: "factory_name".into(),
            //                     value: "meta_pool_factory".into(),
            //                     change: ChangeType::Creation.into(),
            //                 },
            //                 Attribute {
            //                     name: "factory".into(),
            //                     value: address_to_bytes_with_0x(&META_POOL_FACTORY),
            //                     change: ChangeType::Creation.into(),
            //                 },
            //                 Attribute {
            //                     name: "base_pool".into(),
            //                     value: address_to_bytes_with_0x(
            //                         &add_pool.base_pool.try_into().unwrap(),
            //                     ),
            //                     change: ChangeType::Creation.into(),
            //                 },
            //                 TODO:ADD COINS
            //             ],
            //             change: ChangeType::Creation.into(),
            //             protocol_type: Some(ProtocolType {
            //                 name: "curve_pool".into(),
            //                 financial_type: FinancialType::Swap.into(),
            //                 attribute_schema: Vec::new(),
            //                 implementation_type: ImplementationType::Vm.into(),
            //             }),
            //         },
            //         vec![EntityChanges {
            //             component_id: address_to_string_with_0x(component_id),
            //             attributes: vec![Attribute {
            //                 name: "stateless_contract_addr_0".into(),
            //                 value: address_to_bytes_with_0x(&pool_implementation),
            //                 change: ChangeType::Creation.into(),
            //             }],
            //         }],
            //     ))
            // }
            else {
                None
            }
        }
        // META_POOL_FACTORY_OLD => {
        //     if let Some(pool_added) =
        //         abi::meta_pool_factory::events::MetaPoolDeployed::match_and_decode(log)
        //     {
        //         let add_pool =
        //             abi::meta_pool_factory::functions::DeployMetapool1::match_and_decode(call)
        //                 .map(|add_pool| abi::meta_pool_factory::functions::DeployMetapool2 {
        //                     base_pool: add_pool.base_pool,
        //                     name: add_pool.name,
        //                     symbol: add_pool.symbol,
        //                     coin: add_pool.coin,
        //                     a: add_pool.a,
        //                     fee: add_pool.fee,
        //                     implementation_idx: BigInt::from(0),
        //                 })
        //                 .or_else(|| {
        //                     abi::meta_pool_factory::functions::DeployMetapool2::match_and_decode(
        //                         call,
        //                     )
        //                 })?;

        //         let pool_implementation = extract_proxy_impl(call, tx, 0).unwrap_or([1u8; 20]);

        //         let component_id = &call.return_data[12..];
        //         let lp_token = get_token_from_pool(&pool_added.base_pool);

        //         Some((
        //             ProtocolComponent {
        //                 id: hex::encode(component_id),
        //                 tx: Some(Transaction {
        //                     to: tx.to.clone(),
        //                     from: tx.from.clone(),
        //                     hash: tx.hash.clone(),
        //                     index: tx.index.into(),
        //                 }),
        //                 tokens: vec![pool_added.coin, lp_token],
        //                 contracts: vec![component_id.into()],
        //                 static_att: vec![
        //                     Attribute {
        //                         name: "pool_type".into(),
        //                         value: "metapool".into(),
        //                         change: ChangeType::Creation.into(),
        //                     },
        //                     Attribute {
        //                         name: "name".into(),
        //                         value: add_pool.name.into(),
        //                         change: ChangeType::Creation.into(),
        //                     },
        //                     Attribute {
        //                         name: "factory_name".into(),
        //                         value: "meta_pool_factory_old".into(),
        //                         change: ChangeType::Creation.into(),
        //                     },
        //                     Attribute {
        //                         name: "factory".into(),
        //                         value: address_to_bytes_with_0x(&META_POOL_FACTORY_OLD),
        //                         change: ChangeType::Creation.into(),
        //                     },
        //                     Attribute {
        //                         name: "base_pool".into(),
        //                         value: address_to_bytes_with_0x(
        //                             &add_pool.base_pool.try_into().unwrap(),
        //                         ),
        //                         change: ChangeType::Creation.into(),
        //                     },
        //                 TODO:ADD COINS
        //                 ],
        //                 change: ChangeType::Creation.into(),
        //                 protocol_type: Some(ProtocolType {
        //                     name: "curve_pool".into(),
        //                     financial_type: FinancialType::Swap.into(),
        //                     attribute_schema: Vec::new(),
        //                     implementation_type: ImplementationType::Vm.into(),
        //                 }),
        //             },
        //             vec![EntityChanges {
        //                 component_id: address_to_string_with_0x(component_id),
        //                 attributes: vec![Attribute {
        //                     name: "stateless_contract_addr_0".into(),
        //                     value: address_to_bytes_with_0x(&pool_implementation),
        //                     change: ChangeType::Creation.into(),
        //                 }],
        //             }],
        //         ))
        //     } else {
        //         None
        //     }
        // }
        addr if addr ==
            curve_params
                .protocol_params
                .crypto_swap_ng_factory =>
        {
            if let Some(pool_added) =
                abi::crypto_swap_ng_factory::events::PlainPoolDeployed::match_and_decode(log)
            {
                let add_pool =
                    abi::crypto_swap_ng_factory::functions::DeployPlainPool::match_and_decode(
                        call,
                    )?;
                let component_id = &call.return_data[12..];
                let mut static_attrs = vec![
                    Attribute {
                        name: "pool_type".into(),
                        value: "plain_pool".into(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "name".into(),
                        value: add_pool.name.into(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "factory_name".into(),
                        value: "crypto_swap_ng_factory".into(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "factory".into(),
                        value: address_to_bytes_with_0x(
                            &curve_params
                                .protocol_params
                                .crypto_swap_ng_factory,
                        ),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "asset_types".into(),
                        value: json_serialize_bigint_list(&add_pool.asset_types),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "oracles".into(),
                        value: json_serialize_address_list(&add_pool.oracles),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "method_ids".into(),
                        value: json_serialize_value(
                            add_pool
                                .method_ids
                                .iter()
                                .map(|id| format!("0x{}", hex::encode(id)))
                                .collect::<Vec<_>>(),
                        ),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "coins".into(),
                        value: json_serialize_address_list(&pool_added.coins),
                        change: ChangeType::Creation.into(),
                    },
                ];
                let rebase_tokens: Vec<Vec<u8>> = add_pool
                    .asset_types
                    .iter()
                    .enumerate()
                    .filter_map(|(i, t)| {
                        if *t == BigInt::from(2) {
                            pool_added.coins.get(i).cloned()
                        } else {
                            None
                        }
                    })
                    .collect();
                if !rebase_tokens.is_empty() {
                    static_attrs.push(Attribute {
                        name: "rebase_tokens".to_string(),
                        value: json_serialize_address_list(&rebase_tokens),
                        change: ChangeType::Creation.into(),
                    });
                }
                Some((
                    ProtocolComponent {
                        id: address_to_string_with_0x(component_id),
                        tokens: pool_added.coins.clone(),
                        contracts: vec![
                            component_id.into(),
                            curve_params
                                .protocol_params
                                .crypto_swap_ng_factory
                                .into(),
                        ],
                        static_att: static_attrs,
                        change: ChangeType::Creation.into(),
                        protocol_type: Some(ProtocolType {
                            name: "curve_pool".into(),
                            financial_type: FinancialType::Swap.into(),
                            attribute_schema: Vec::new(),
                            implementation_type: ImplementationType::Vm.into(),
                        }),
                    },
                    vec![EntityChanges {
                        component_id: address_to_string_with_0x(component_id),
                        attributes: vec![Attribute {
                            name: "stateless_contract_addr_0".into(),
                            // Call views_implementation() on
                            // curve_params.protocol_params.crypto_swap_ng_factory
                            value: format!(
                                "call:0x{}:views_implementation()",
                                hex::encode(
                                    curve_params
                                        .protocol_params
                                        .crypto_swap_ng_factory
                                )
                            )
                            .into(),
                            change: ChangeType::Creation.into(),
                        }],
                    }],
                ))
            } else if let Some(pool_added) =
                abi::crypto_swap_ng_factory::events::MetaPoolDeployed::match_and_decode(log)
            {
                let add_pool =
                    abi::crypto_swap_ng_factory::functions::DeployMetapool::match_and_decode(call)?;
                let component_id = &call.return_data[12..];
                let lp_token = get_token_from_pool(
                    curve_params
                        .protocol_params
                        .meta_registry,
                    &pool_added.base_pool,
                );
                let mut static_attrs = vec![
                    Attribute {
                        name: "pool_type".into(),
                        value: "metapool".into(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "name".into(),
                        value: add_pool.name.into(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "factory_name".into(),
                        value: "crypto_swap_ng_factory".into(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "factory".into(),
                        value: address_to_bytes_with_0x(
                            &curve_params
                                .protocol_params
                                .crypto_swap_ng_factory,
                        ),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "base_pool".into(),
                        value: address_to_bytes_with_0x(
                            &pool_added
                                .base_pool
                                .clone()
                                .try_into()
                                .unwrap(),
                        ),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "implementation_idx".into(),
                        value: add_pool
                            .implementation_idx
                            .to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "asset_type".into(),
                        value: add_pool.asset_type.to_signed_bytes_be(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "coins".into(),
                        value: json_serialize_address_list(&[
                            pool_added.coin.clone(),
                            lp_token.clone(),
                        ]),
                        change: ChangeType::Creation.into(),
                    },
                ];
                if add_pool.asset_type.eq(&BigInt::from(2)) {
                    static_attrs.push(Attribute {
                        name: "rebase_tokens".to_string(),
                        value: json_serialize_address_list(&[
                            pool_added.coin.clone(),
                            lp_token.clone(),
                        ]),
                        change: ChangeType::Creation.into(),
                    })
                }
                Some((
                    ProtocolComponent {
                        id: address_to_string_with_0x(component_id),
                        tokens: vec![pool_added.coin.clone(), lp_token.clone()],
                        contracts: vec![
                            component_id.into(),
                            curve_params
                                .protocol_params
                                .crypto_swap_ng_factory
                                .into(),
                            pool_added.base_pool.clone(),
                        ],
                        static_att: static_attrs,
                        change: ChangeType::Creation.into(),
                        protocol_type: Some(ProtocolType {
                            name: "curve_pool".into(),
                            financial_type: FinancialType::Swap.into(),
                            attribute_schema: Vec::new(),
                            implementation_type: ImplementationType::Vm.into(),
                        }),
                    },
                    vec![EntityChanges {
                        component_id: address_to_string_with_0x(component_id),
                        attributes: vec![
                            Attribute {
                                name: "stateless_contract_addr_0".into(),
                                // Call views_implementation() on CRYPTO_SWAP_NG_FACTORY
                                value: format!(
                                    "call:0x{}:views_implementation()",
                                    hex::encode(
                                        curve_params
                                            .protocol_params
                                            .crypto_swap_ng_factory
                                    )
                                )
                                .into(),
                                change: ChangeType::Creation.into(),
                            },
                            Attribute {
                                name: "stateless_contract_addr_1".into(),
                                // Call math_implementation() on CRYPTO_SWAP_NG_FACTORY
                                value: format!(
                                    "call:0x{}:math_implementation()",
                                    hex::encode(
                                        curve_params
                                            .protocol_params
                                            .crypto_swap_ng_factory
                                    )
                                )
                                .into(),
                                change: ChangeType::Creation.into(),
                            },
                        ],
                    }],
                ))
            } else {
                None
            }
        }
        addr if addr ==
            curve_params
                .protocol_params
                .tricrypto_factory =>
        {
            if let Some(pool_added) =
                abi::tricrypto_factory::events::TricryptoPoolDeployed::match_and_decode(log)
            {
                let tokens =
                    swap_weth_for_eth(curve_params.protocol_params.weth, pool_added.coins.into());
                let id = hex::encode(&pool_added.pool);

                Some((
                    ProtocolComponent {
                        id: format!("0x{}", id),
                        tokens: tokens.clone(),
                        contracts: vec![
                            pool_added.pool,
                            curve_params
                                .protocol_params
                                .tricrypto_factory
                                .into(),
                        ],
                        static_att: vec![
                            Attribute {
                                name: "pool_type".into(),
                                value: "tricrypto".into(),
                                change: ChangeType::Creation.into(),
                            },
                            Attribute {
                                name: "name".into(),
                                value: pool_added.name.into(),
                                change: ChangeType::Creation.into(),
                            },
                            Attribute {
                                name: "factory_name".into(),
                                value: "tricrypto_factory".into(),
                                change: ChangeType::Creation.into(),
                            },
                            Attribute {
                                name: "factory".into(),
                                value: address_to_bytes_with_0x(
                                    &curve_params
                                        .protocol_params
                                        .tricrypto_factory,
                                ),
                                change: ChangeType::Creation.into(),
                            },
                            Attribute {
                                name: "coins".into(),
                                value: json_serialize_address_list(&tokens),
                                change: ChangeType::Creation.into(),
                            },
                        ],
                        change: ChangeType::Creation.into(),
                        protocol_type: Some(ProtocolType {
                            name: "curve_pool".into(),
                            financial_type: FinancialType::Swap.into(),
                            attribute_schema: Vec::new(),
                            implementation_type: ImplementationType::Vm.into(),
                        }),
                    },
                    vec![EntityChanges {
                        component_id: format!("0x{id}"),
                        attributes: vec![
                            Attribute {
                                name: "stateless_contract_addr_0".into(),
                                // Call views_implementation() on TRICRYPTO_FACTORY
                                value: format!(
                                    "call:0x{}:views_implementation()",
                                    hex::encode(
                                        curve_params
                                            .protocol_params
                                            .tricrypto_factory
                                    )
                                )
                                .into(),
                                change: ChangeType::Creation.into(),
                            },
                            Attribute {
                                name: "stateless_contract_addr_1".into(),
                                // Call math_implementation() on TRICRYPTO_FACTORY
                                value: format!(
                                    "call:0x{}:math_implementation()",
                                    hex::encode(
                                        curve_params
                                            .protocol_params
                                            .tricrypto_factory
                                    )
                                )
                                .into(),
                                change: ChangeType::Creation.into(),
                            },
                        ],
                    }],
                ))
            } else {
                None
            }
        }
        addr if addr ==
            curve_params
                .protocol_params
                .stableswap_factory =>
        {
            if let Some(pool_added) =
                abi::stableswap_factory::events::PlainPoolDeployed::match_and_decode(log)
            {
                let add_pool = if let Some(pool) =
                    abi::stableswap_factory::functions::DeployPlainPool1::match_and_decode(call)
                {
                    abi::stableswap_factory::functions::DeployPlainPool3 {
                        name: pool.name,
                        symbol: pool.symbol,
                        coins: pool.coins,
                        a: pool.a,
                        fee: pool.fee,
                        asset_type: BigInt::from(0),
                        implementation_idx: BigInt::from(0),
                    }
                } else if let Some(pool) =
                    abi::stableswap_factory::functions::DeployPlainPool2::match_and_decode(call)
                {
                    abi::stableswap_factory::functions::DeployPlainPool3 {
                        name: pool.name,
                        symbol: pool.symbol,
                        coins: pool.coins,
                        a: pool.a,
                        fee: pool.fee,
                        asset_type: BigInt::from(0),
                        implementation_idx: BigInt::from(0),
                    }
                } else {
                    abi::stableswap_factory::functions::DeployPlainPool3::match_and_decode(call)?
                };
                let component_id = &call.return_data[12..];

                let tokens: Vec<_> = pool_added
                    .coins
                    .into_iter()
                    .filter(|token| *token != [0; 20])
                    .collect();

                let pool_implementation = extract_proxy_impl(call, tx, 0).unwrap_or([1u8; 20]);
                let mut static_attrs = vec![
                    Attribute {
                        name: "pool_type".into(),
                        value: "plain_pool".into(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "name".into(),
                        value: add_pool.name.into(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "factory_name".into(),
                        value: "stable_swap_factory".into(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "factory".into(),
                        value: address_to_bytes_with_0x(
                            &curve_params
                                .protocol_params
                                .stableswap_factory,
                        ),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "coins".into(),
                        value: json_serialize_address_list(&tokens),
                        change: ChangeType::Creation.into(),
                    },
                ];
                if tokens.len() == 2 &&
                    add_pool
                        .implementation_idx
                        .eq(&BigInt::from(1))
                {
                    let rebase_tokens: Vec<Vec<u8>> = tokens
                        .iter()
                        .filter(|coin| coin.as_slice() != ETH_ADDRESS)
                        .cloned()
                        .collect();

                    if !rebase_tokens.is_empty() {
                        static_attrs.push(Attribute {
                            name: "rebase_tokens".to_string(),
                            value: json_serialize_address_list(&rebase_tokens),
                            change: ChangeType::Creation.into(),
                        });
                    }
                }
                Some((
                    ProtocolComponent {
                        id: address_to_string_with_0x(component_id),
                        tokens: tokens.clone(),
                        contracts: vec![component_id.into()],
                        static_att: static_attrs,
                        change: ChangeType::Creation.into(),
                        protocol_type: Some(ProtocolType {
                            name: "curve_pool".into(),
                            financial_type: FinancialType::Swap.into(),
                            attribute_schema: Vec::new(),
                            implementation_type: ImplementationType::Vm.into(),
                        }),
                    },
                    vec![EntityChanges {
                        component_id: address_to_string_with_0x(component_id),
                        attributes: vec![Attribute {
                            name: "stateless_contract_addr_0".into(),
                            value: address_to_bytes_with_0x(&pool_implementation),
                            change: ChangeType::Creation.into(),
                        }],
                    }],
                ))
            }
            // else if let Some(pool_added) =
            //     abi::stableswap_factory::events::MetaPoolDeployed::match_and_decode(log)
            // {
            //     let add_pool = if let Some(pool) =
            //         abi::stableswap_factory::functions::DeployMetapool1::match_and_decode(call)
            //     {
            //         abi::stableswap_factory::functions::DeployMetapool2 {
            //             base_pool: pool.base_pool,
            //             name: pool.name,
            //             symbol: pool.symbol,
            //             coin: pool.coin,
            //             a: pool.a,
            //             fee: pool.fee,
            //             implementation_idx: BigInt::from(0),
            //         }
            //     } else if let Some(pool) =
            //         abi::stableswap_factory::functions::DeployMetapool2::match_and_decode(call)
            //     {
            //         pool
            //     } else {
            //         return None;
            //     };
            //     let component_id = &call.return_data[12..];

            //     let get_lp_token =
            //         abi::meta_registry::functions::GetLpToken1 { pool: add_pool.base_pool.clone()
            // };     let lp_token = get_lp_token.call(META_REGISTRY.to_vec())?;

            //     Some((
            //         ProtocolComponent {
            //             id: hex::encode(component_id),
            //             tx: Some(Transaction {
            //                 to: tx.to.clone(),
            //                 from: tx.from.clone(),
            //                 hash: tx.hash.clone(),
            //                 index: tx.index.into(),
            //             }),
            //             tokens: vec![pool_added.coin, lp_token],
            //             contracts: vec![component_id.into()],
            //             static_att: vec![
            //                 Attribute {
            //                     name: "pool_type".into(),
            //                     value: "metapool".into(),
            //                     change: ChangeType::Creation.into(),
            //                 },
            //                 Attribute {
            //                     name: "name".into(),
            //                     value: add_pool.name.into(),
            //                     change: ChangeType::Creation.into(),
            //                 },
            //                 Attribute {
            //                     name: "factory_name".into(),
            //                     value: "stable_swap_factory".into(),
            //                     change: ChangeType::Creation.into(),
            //                 },
            //                 Attribute {
            //                     name: "factory".into(),
            //                     value: address_to_bytes_with_0x(&STABLESWAP_FACTORY),
            //                     change: ChangeType::Creation.into(),
            //                 },
            //                 Attribute {
            //                     name: "base_pool".into(),
            //                     value: address_to_bytes_with_0x(
            //                         &pool_added.base_pool.try_into().unwrap(),
            //                     ),
            //                     change: ChangeType::Creation.into(),
            //                 },
            //             ],
            //             change: ChangeType::Creation.into(),
            //             protocol_type: Some(ProtocolType {
            //                 name: "curve_pool".into(),
            //                 financial_type: FinancialType::Swap.into(),
            //                 attribute_schema: Vec::new(),
            //                 implementation_type: ImplementationType::Vm.into(),
            //             }),
            //         },
            //         vec![],
            //     ))
            // }
            else {
                None
            }
        }
        addr if addr ==
            curve_params
                .protocol_params
                .core_stableswap_factory =>
        {
            let pool_added =
                abi::curve_core_stableswap_factory::events::PlainPoolDeployed::match_and_decode(
                    log,
                )?;
            let add_pool =
                abi::curve_core_stableswap_factory::functions::DeployPlainPool::match_and_decode(
                    call,
                )?;
            let component_id = &call.return_data[12..];

            let tokens: Vec<_> = pool_added
                .coins
                .into_iter()
                .filter(|token| *token != [0; 20])
                .collect();

            let mut static_attrs = vec![
                Attribute {
                    name: "pool_type".into(),
                    value: "plain_pool".into(),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "name".into(),
                    value: add_pool.name.into(),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "asset_types".into(),
                    value: json_serialize_bigint_list(&add_pool.asset_types),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "oracles".into(),
                    value: json_serialize_address_list(&add_pool.oracles),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "method_ids".into(),
                    value: json_serialize_value(
                        add_pool
                            .method_ids
                            .iter()
                            .map(|id| format!("0x{}", hex::encode(id)))
                            .collect::<Vec<_>>(),
                    ),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "factory_name".into(),
                    value: "core_stable_swap_factory".into(),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "factory".into(),
                    value: address_to_bytes_with_0x(
                        &curve_params
                            .protocol_params
                            .core_stableswap_factory,
                    ),
                    change: ChangeType::Creation.into(),
                },
                Attribute {
                    name: "coins".into(),
                    value: json_serialize_address_list(&tokens),
                    change: ChangeType::Creation.into(),
                },
            ];
            if tokens.len() == 2 &&
                add_pool
                    .implementation_idx
                    .eq(&BigInt::from(1))
            {
                let rebase_tokens: Vec<Vec<u8>> = tokens
                    .iter()
                    .filter(|coin| coin.as_slice() != ETH_ADDRESS)
                    .cloned()
                    .collect();

                if !rebase_tokens.is_empty() {
                    static_attrs.push(Attribute {
                        name: "rebase_tokens".to_string(),
                        value: json_serialize_address_list(&rebase_tokens),
                        change: ChangeType::Creation.into(),
                    });
                }
            }
            Some((
                ProtocolComponent {
                    id: address_to_string_with_0x(component_id),
                    tokens: tokens.clone(),
                    contracts: vec![
                        component_id.into(),
                        curve_params
                            .protocol_params
                            .core_stableswap_factory
                            .into(),
                    ],
                    static_att: static_attrs,
                    change: ChangeType::Creation.into(),
                    protocol_type: Some(ProtocolType {
                        name: "curve_pool".into(),
                        financial_type: FinancialType::Swap.into(),
                        attribute_schema: Vec::new(),
                        implementation_type: ImplementationType::Vm.into(),
                    }),
                },
                vec![EntityChanges {
                    component_id: address_to_string_with_0x(component_id),
                    attributes: vec![Attribute {
                        name: "stateless_contract_addr_0".into(),
                        // Call views_implementation() on core_stableswap_factory
                        value: format!(
                            "call:0x{}:views_implementation()",
                            hex::encode(
                                curve_params
                                    .protocol_params
                                    .core_stableswap_factory
                            )
                        )
                        .into(),
                        change: ChangeType::Creation.into(),
                    }],
                }],
            ))
        }
        addr if addr ==
            curve_params
                .protocol_params
                .twocrypto_factory =>
        {
            if let Some(pool_added) =
                abi::twocrypto_factory::events::TwocryptoPoolDeployed::match_and_decode(log)
            {
                let mut attributes = vec![
                    Attribute {
                        name: "stateless_contract_addr_0".into(),
                        // Call views_implementation() on TWOCRYPTO_FACTORY
                        value: format!(
                            "call:0x{}:views_implementation()",
                            hex::encode(
                                curve_params
                                    .protocol_params
                                    .twocrypto_factory
                            )
                        )
                        .into(),
                        change: ChangeType::Creation.into(),
                    },
                    Attribute {
                        name: "stateless_contract_addr_1".into(),
                        value: address_to_bytes_with_0x(
                            &pool_added
                                .math
                                .try_into()
                                .unwrap_or([1u8; 20]), // Unexpected issue marker
                        ),
                        change: ChangeType::Creation.into(),
                    },
                ];
                if let Some(deploy_pool) =
                    abi::twocrypto_factory::functions::DeployPool::match_and_decode(call)
                {
                    let impl_id = deploy_pool
                        .implementation_id
                        .to_string();
                    if curve_params
                        .protocol_params
                        .twocrypto_custom_implementation_ids
                        .contains(&impl_id)
                    {
                        attributes.push(Attribute {
                            name: "stateless_contract_addr_2".into(),
                            value: address_to_bytes_with_0x(
                                &curve_params
                                    .protocol_params
                                    .twocrypto_custom_view,
                            ),
                            change: ChangeType::Creation.into(),
                        });
                        attributes.push(Attribute {
                            name: "stateless_contract_addr_3".into(),
                            value: address_to_bytes_with_0x(
                                &curve_params
                                    .protocol_params
                                    .twocrypto_custom_math,
                            ),
                            change: ChangeType::Creation.into(),
                        });
                    } else {
                        let view_addr =
                            (abi::twocrypto_pool::functions::View {}).call(pool_added.pool.clone());
                        let math_addr =
                            (abi::twocrypto_pool::functions::Math {}).call(pool_added.pool.clone());
                        if let (Some(view), Some(math)) = (view_addr, math_addr) {
                            let view: [u8; 20] = view.try_into().unwrap_or([1u8; 20]);
                            let math: [u8; 20] = math.try_into().unwrap_or([1u8; 20]);
                            attributes.push(Attribute {
                                name: "stateless_contract_addr_2".into(),
                                value: address_to_bytes_with_0x(&view),
                                change: ChangeType::Creation.into(),
                            });
                            attributes.push(Attribute {
                                name: "stateless_contract_addr_3".into(),
                                value: address_to_bytes_with_0x(&math),
                                change: ChangeType::Creation.into(),
                            });
                        }
                    }
                }
                let id = hex::encode(&pool_added.pool);

                Some((
                    ProtocolComponent {
                        id: format!("0x{id}"),
                        tokens: pool_added.coins.clone().into(),
                        contracts: vec![
                            pool_added.pool,
                            curve_params
                                .protocol_params
                                .twocrypto_factory
                                .into(),
                        ],
                        static_att: vec![
                            Attribute {
                                name: "pool_type".into(),
                                value: "twocrypto".into(),
                                change: ChangeType::Creation.into(),
                            },
                            Attribute {
                                name: "name".into(),
                                value: pool_added.name.into(),
                                change: ChangeType::Creation.into(),
                            },
                            Attribute {
                                name: "factory_name".into(),
                                value: "twocrypto_factory".into(),
                                change: ChangeType::Creation.into(),
                            },
                            Attribute {
                                name: "factory".into(),
                                value: address_to_bytes_with_0x(
                                    &curve_params
                                        .protocol_params
                                        .twocrypto_factory,
                                ),
                                change: ChangeType::Creation.into(),
                            },
                            Attribute {
                                name: "coins".into(),
                                value: json_serialize_address_list(&pool_added.coins),
                                change: ChangeType::Creation.into(),
                            },
                        ],
                        change: ChangeType::Creation.into(),
                        protocol_type: Some(ProtocolType {
                            name: "curve_pool".into(),
                            financial_type: FinancialType::Swap.into(),
                            attribute_schema: Vec::new(),
                            implementation_type: ImplementationType::Vm.into(),
                        }),
                    },
                    vec![EntityChanges { component_id: format!("0x{id}"), attributes }],
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// This function makes 3 attempts to confirm / get the LP token address from a pool address.
///
/// 1. We attempt to see if the pool address is a token address itself by calling an ERC 20 func.
///  - Some pools may not be the token themselves
/// 2. Then, we try to ping the `META_REGISTRY` address to see if it has a record of the pool.
///  - Older pools might have been created before the `META_REGISTRY` was created and therefore
///    would have registered much later
/// 3. Finally, we have a hardcoded map of pool address -> token address for some pools.
///
/// If all else fails, we force an `unwrap` to trigger a `panic` so that we can resolve this by
///  adding onto our map of `pool` -> `token` addresses.
fn get_token_from_pool(meta_registry: [u8; 20], pool: &Vec<u8>) -> Vec<u8> {
    abi::erc20::functions::Name {}
        .call(pool.clone())
        .and(Some(pool.clone()))
        .or_else(|| {
            abi::meta_registry::functions::GetLpToken1 { pool: pool.clone() }
                .call(meta_registry.to_vec())
        })
        .or_else(|| {
            match hex::encode(pool).as_str() {
                // Curve.fi DAI/USDC/USDT (3Crv)
                "bebc44782c7db0a1a60cb6fe97d0b483032ff1c7" => {
                    hex::decode("6c3F90f043a72FA612cbac8115EE7e52BDe6E490").ok()
                }
                // Curve.fi renBTC/wBTC/sBTC (crvRenWSBTC)
                "7fc77b5c7614e1533320ea6ddc2eb61fa00a9714" => {
                    hex::decode("075b1bb99792c9e1041ba13afef80c91a1e70fb3").ok()
                }
                // Placeholder if we can't find the token. It will help us to detect these missing
                // token easily with a SQL query.
                _ => hex::decode("1111111111111111111111111111111111111111").ok(),
            }
        })
        .unwrap()
}

fn extract_eip1167_target_from_code(code: &[u8]) -> [u8; 20] {
    let mut target = [0u8; 20];

    // Depending on the Vyper version, they use different implementations of EIP1167.
    // We use the first 10 bytes of the code to make a clear distinction.
    match code.get(0..10) {
        Some([54, 61, 61, 55, 61, 61, 61, 54, 61, 115]) => target.copy_from_slice(&code[10..30]),
        Some([54, 96, 0, 96, 0, 55, 97, 16, 0, 96]) => target.copy_from_slice(&code[15..35]),
        _ => target = [1u8; 20], // Placeholder for unexpected values
    }

    target
}

fn extract_proxy_impl(call: &Call, tx: &TransactionTrace, index: usize) -> Option<[u8; 20]> {
    let code_change = tx
        .calls
        .iter()
        .filter(|c| !c.code_changes.is_empty() && c.parent_index == call.index)
        .nth(index)?
        .code_changes
        .first()?;
    Some(extract_eip1167_target_from_code(&code_change.new_code))
}
