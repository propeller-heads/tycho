const INTERNAL_ERR: &'static str = "`ethabi_derive` internal error";
/// Contract's functions.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod functions {
    use super::INTERNAL_ERR;
}
/// Contract's events.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod events {
    use super::INTERNAL_ERR;
    #[derive(Debug, Clone, PartialEq)]
    pub struct Donate {
        pub id: [u8; 32usize],
        pub sender: Vec<u8>,
        pub amount0: substreams::scalar::BigInt,
        pub amount1: substreams::scalar::BigInt,
        pub tick: substreams::scalar::BigInt,
    }
    impl Donate {
        const TOPIC_ID: [u8; 32] = [
            190u8,
            112u8,
            137u8,
            17u8,
            101u8,
            106u8,
            225u8,
            134u8,
            172u8,
            63u8,
            194u8,
            106u8,
            121u8,
            78u8,
            95u8,
            19u8,
            25u8,
            96u8,
            156u8,
            227u8,
            64u8,
            161u8,
            76u8,
            99u8,
            82u8,
            79u8,
            152u8,
            95u8,
            238u8,
            75u8,
            200u8,
            65u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 3usize {
                return false;
            }
            if log.data.len() != 96usize {
                return false;
            }
            return log.topics.get(0).expect("bounds already checked").as_ref()
                == Self::TOPIC_ID;
        }
        pub fn decode(
            log: &substreams_ethereum::pb::eth::v2::Log,
        ) -> Result<Self, String> {
            let mut values = ethabi::decode(
                    &[
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Int(24usize),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                id: {
                    let mut result = [0u8; 32];
                    let v = ethabi::decode(
                            &[ethabi::ParamType::FixedBytes(32usize)],
                            log.topics[1usize].as_ref(),
                        )
                        .map_err(|e| {
                            format!(
                                "unable to decode param 'id' from topic of type 'bytes32': {:?}",
                                e
                            )
                        })?
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_fixed_bytes()
                        .expect(INTERNAL_ERR);
                    result.copy_from_slice(&v);
                    result
                },
                sender: ethabi::decode(
                        &[ethabi::ParamType::Address],
                        log.topics[2usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'sender' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                amount0: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                amount1: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                tick: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
            })
        }
    }
    impl substreams_ethereum::Event for Donate {
        const NAME: &'static str = "Donate";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Initialize {
        pub id: [u8; 32usize],
        pub currency0: Vec<u8>,
        pub currency1: Vec<u8>,
        pub hooks: Vec<u8>,
        pub fee: substreams::scalar::BigInt,
        pub parameters: [u8; 32usize],
        pub sqrt_price_x96: substreams::scalar::BigInt,
        pub tick: substreams::scalar::BigInt,
    }
    impl Initialize {
        const TOPIC_ID: [u8; 32] = [
            66u8,
            108u8,
            198u8,
            47u8,
            230u8,
            163u8,
            58u8,
            64u8,
            186u8,
            39u8,
            136u8,
            194u8,
            200u8,
            122u8,
            156u8,
            52u8,
            238u8,
            69u8,
            130u8,
            185u8,
            91u8,
            201u8,
            250u8,
            90u8,
            123u8,
            183u8,
            174u8,
            112u8,
            183u8,
            80u8,
            185u8,
            156u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 4usize {
                return false;
            }
            if log.data.len() != 160usize {
                return false;
            }
            return log.topics.get(0).expect("bounds already checked").as_ref()
                == Self::TOPIC_ID;
        }
        pub fn decode(
            log: &substreams_ethereum::pb::eth::v2::Log,
        ) -> Result<Self, String> {
            let mut values = ethabi::decode(
                    &[
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Uint(24usize),
                        ethabi::ParamType::FixedBytes(32usize),
                        ethabi::ParamType::Uint(160usize),
                        ethabi::ParamType::Int(24usize),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                id: {
                    let mut result = [0u8; 32];
                    let v = ethabi::decode(
                            &[ethabi::ParamType::FixedBytes(32usize)],
                            log.topics[1usize].as_ref(),
                        )
                        .map_err(|e| {
                            format!(
                                "unable to decode param 'id' from topic of type 'bytes32': {:?}",
                                e
                            )
                        })?
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_fixed_bytes()
                        .expect(INTERNAL_ERR);
                    result.copy_from_slice(&v);
                    result
                },
                currency0: ethabi::decode(
                        &[ethabi::ParamType::Address],
                        log.topics[2usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'currency0' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                currency1: ethabi::decode(
                        &[ethabi::ParamType::Address],
                        log.topics[3usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'currency1' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                hooks: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                fee: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                parameters: {
                    let mut result = [0u8; 32];
                    let v = values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_fixed_bytes()
                        .expect(INTERNAL_ERR);
                    result.copy_from_slice(&v);
                    result
                },
                sqrt_price_x96: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                tick: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
            })
        }
    }
    impl substreams_ethereum::Event for Initialize {
        const NAME: &'static str = "Initialize";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct ModifyLiquidity {
        pub id: [u8; 32usize],
        pub sender: Vec<u8>,
        pub tick_lower: substreams::scalar::BigInt,
        pub tick_upper: substreams::scalar::BigInt,
        pub liquidity_delta: substreams::scalar::BigInt,
        pub salt: [u8; 32usize],
    }
    impl ModifyLiquidity {
        const TOPIC_ID: [u8; 32] = [
            242u8,
            8u8,
            244u8,
            145u8,
            39u8,
            130u8,
            253u8,
            37u8,
            199u8,
            241u8,
            20u8,
            202u8,
            55u8,
            35u8,
            162u8,
            213u8,
            221u8,
            111u8,
            59u8,
            204u8,
            58u8,
            200u8,
            219u8,
            90u8,
            246u8,
            59u8,
            170u8,
            133u8,
            247u8,
            17u8,
            213u8,
            236u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 3usize {
                return false;
            }
            if log.data.len() != 128usize {
                return false;
            }
            return log.topics.get(0).expect("bounds already checked").as_ref()
                == Self::TOPIC_ID;
        }
        pub fn decode(
            log: &substreams_ethereum::pb::eth::v2::Log,
        ) -> Result<Self, String> {
            let mut values = ethabi::decode(
                    &[
                        ethabi::ParamType::Int(24usize),
                        ethabi::ParamType::Int(24usize),
                        ethabi::ParamType::Int(256usize),
                        ethabi::ParamType::FixedBytes(32usize),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                id: {
                    let mut result = [0u8; 32];
                    let v = ethabi::decode(
                            &[ethabi::ParamType::FixedBytes(32usize)],
                            log.topics[1usize].as_ref(),
                        )
                        .map_err(|e| {
                            format!(
                                "unable to decode param 'id' from topic of type 'bytes32': {:?}",
                                e
                            )
                        })?
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_fixed_bytes()
                        .expect(INTERNAL_ERR);
                    result.copy_from_slice(&v);
                    result
                },
                sender: ethabi::decode(
                        &[ethabi::ParamType::Address],
                        log.topics[2usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'sender' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                tick_lower: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
                tick_upper: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
                liquidity_delta: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
                salt: {
                    let mut result = [0u8; 32];
                    let v = values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_fixed_bytes()
                        .expect(INTERNAL_ERR);
                    result.copy_from_slice(&v);
                    result
                },
            })
        }
    }
    impl substreams_ethereum::Event for ModifyLiquidity {
        const NAME: &'static str = "ModifyLiquidity";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct ProtocolFeeUpdated {
        pub id: [u8; 32usize],
        pub protocol_fee: substreams::scalar::BigInt,
    }
    impl ProtocolFeeUpdated {
        const TOPIC_ID: [u8; 32] = [
            233u8,
            196u8,
            37u8,
            147u8,
            231u8,
            31u8,
            132u8,
            64u8,
            59u8,
            132u8,
            53u8,
            44u8,
            209u8,
            104u8,
            214u8,
            147u8,
            226u8,
            201u8,
            252u8,
            209u8,
            253u8,
            188u8,
            195u8,
            254u8,
            178u8,
            29u8,
            146u8,
            180u8,
            62u8,
            102u8,
            150u8,
            249u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 2usize {
                return false;
            }
            if log.data.len() != 32usize {
                return false;
            }
            return log.topics.get(0).expect("bounds already checked").as_ref()
                == Self::TOPIC_ID;
        }
        pub fn decode(
            log: &substreams_ethereum::pb::eth::v2::Log,
        ) -> Result<Self, String> {
            let mut values = ethabi::decode(
                    &[ethabi::ParamType::Uint(24usize)],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                id: {
                    let mut result = [0u8; 32];
                    let v = ethabi::decode(
                            &[ethabi::ParamType::FixedBytes(32usize)],
                            log.topics[1usize].as_ref(),
                        )
                        .map_err(|e| {
                            format!(
                                "unable to decode param 'id' from topic of type 'bytes32': {:?}",
                                e
                            )
                        })?
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_fixed_bytes()
                        .expect(INTERNAL_ERR);
                    result.copy_from_slice(&v);
                    result
                },
                protocol_fee: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
            })
        }
    }
    impl substreams_ethereum::Event for ProtocolFeeUpdated {
        const NAME: &'static str = "ProtocolFeeUpdated";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Swap {
        pub id: [u8; 32usize],
        pub sender: Vec<u8>,
        pub amount0: substreams::scalar::BigInt,
        pub amount1: substreams::scalar::BigInt,
        pub sqrt_price_x96: substreams::scalar::BigInt,
        pub liquidity: substreams::scalar::BigInt,
        pub tick: substreams::scalar::BigInt,
        pub fee: substreams::scalar::BigInt,
        pub protocol_fee: substreams::scalar::BigInt,
    }
    impl Swap {
        const TOPIC_ID: [u8; 32] = [
            4u8,
            32u8,
            106u8,
            210u8,
            183u8,
            192u8,
            244u8,
            99u8,
            191u8,
            243u8,
            221u8,
            79u8,
            51u8,
            197u8,
            115u8,
            91u8,
            15u8,
            41u8,
            87u8,
            163u8,
            81u8,
            228u8,
            247u8,
            151u8,
            99u8,
            164u8,
            250u8,
            158u8,
            119u8,
            93u8,
            210u8,
            55u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 3usize {
                return false;
            }
            if log.data.len() != 224usize {
                return false;
            }
            return log.topics.get(0).expect("bounds already checked").as_ref()
                == Self::TOPIC_ID;
        }
        pub fn decode(
            log: &substreams_ethereum::pb::eth::v2::Log,
        ) -> Result<Self, String> {
            let mut values = ethabi::decode(
                    &[
                        ethabi::ParamType::Int(128usize),
                        ethabi::ParamType::Int(128usize),
                        ethabi::ParamType::Uint(160usize),
                        ethabi::ParamType::Uint(128usize),
                        ethabi::ParamType::Int(24usize),
                        ethabi::ParamType::Uint(24usize),
                        ethabi::ParamType::Uint(16usize),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                id: {
                    let mut result = [0u8; 32];
                    let v = ethabi::decode(
                            &[ethabi::ParamType::FixedBytes(32usize)],
                            log.topics[1usize].as_ref(),
                        )
                        .map_err(|e| {
                            format!(
                                "unable to decode param 'id' from topic of type 'bytes32': {:?}",
                                e
                            )
                        })?
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_fixed_bytes()
                        .expect(INTERNAL_ERR);
                    result.copy_from_slice(&v);
                    result
                },
                sender: ethabi::decode(
                        &[ethabi::ParamType::Address],
                        log.topics[2usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'sender' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                amount0: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
                amount1: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
                sqrt_price_x96: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                liquidity: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                tick: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
                fee: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                protocol_fee: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
            })
        }
    }
    impl substreams_ethereum::Event for Swap {
        const NAME: &'static str = "Swap";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
}