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
    pub struct FlipOrderCreated {
        pub order_id: substreams::scalar::BigInt,
        pub flipped_id: substreams::scalar::BigInt,
        pub owner: Vec<u8>,
        pub size: substreams::scalar::BigInt,
        pub price: substreams::scalar::BigInt,
        pub flipped_price: substreams::scalar::BigInt,
        pub is_buy: bool,
    }
    impl FlipOrderCreated {
        const TOPIC_ID: [u8; 32] = [
            248u8,
            29u8,
            63u8,
            222u8,
            168u8,
            167u8,
            238u8,
            156u8,
            92u8,
            9u8,
            23u8,
            152u8,
            130u8,
            225u8,
            188u8,
            225u8,
            197u8,
            19u8,
            167u8,
            65u8,
            20u8,
            110u8,
            69u8,
            49u8,
            103u8,
            217u8,
            232u8,
            145u8,
            44u8,
            142u8,
            179u8,
            170u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
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
                        ethabi::ParamType::Uint(40usize),
                        ethabi::ParamType::Uint(40usize),
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Uint(96usize),
                        ethabi::ParamType::Uint(32usize),
                        ethabi::ParamType::Uint(32usize),
                        ethabi::ParamType::Bool,
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                order_id: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                flipped_id: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                owner: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                size: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                price: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                flipped_price: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                is_buy: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
    }
    impl substreams_ethereum::Event for FlipOrderCreated {
        const NAME: &'static str = "FlipOrderCreated";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct FlipOrderUpdated {
        pub order_id: substreams::scalar::BigInt,
        pub size: substreams::scalar::BigInt,
    }
    impl FlipOrderUpdated {
        const TOPIC_ID: [u8; 32] = [
            183u8,
            78u8,
            150u8,
            107u8,
            200u8,
            115u8,
            184u8,
            193u8,
            68u8,
            250u8,
            179u8,
            156u8,
            153u8,
            129u8,
            33u8,
            15u8,
            80u8,
            19u8,
            8u8,
            133u8,
            232u8,
            156u8,
            175u8,
            69u8,
            86u8,
            192u8,
            132u8,
            12u8,
            236u8,
            116u8,
            29u8,
            205u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 64usize {
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
                        ethabi::ParamType::Uint(40usize),
                        ethabi::ParamType::Uint(96usize),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                order_id: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                size: {
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
    impl substreams_ethereum::Event for FlipOrderUpdated {
        const NAME: &'static str = "FlipOrderUpdated";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct FlippedOrderCreated {
        pub order_id: substreams::scalar::BigInt,
        pub flipped_id: substreams::scalar::BigInt,
        pub owner: Vec<u8>,
        pub size: substreams::scalar::BigInt,
        pub price: substreams::scalar::BigInt,
        pub flipped_price: substreams::scalar::BigInt,
        pub is_buy: bool,
    }
    impl FlippedOrderCreated {
        const TOPIC_ID: [u8; 32] = [
            73u8,
            73u8,
            106u8,
            65u8,
            185u8,
            34u8,
            189u8,
            186u8,
            63u8,
            247u8,
            245u8,
            123u8,
            176u8,
            153u8,
            42u8,
            177u8,
            161u8,
            163u8,
            238u8,
            149u8,
            181u8,
            174u8,
            91u8,
            215u8,
            39u8,
            28u8,
            103u8,
            134u8,
            31u8,
            1u8,
            131u8,
            82u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
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
                        ethabi::ParamType::Uint(40usize),
                        ethabi::ParamType::Uint(40usize),
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Uint(96usize),
                        ethabi::ParamType::Uint(32usize),
                        ethabi::ParamType::Uint(32usize),
                        ethabi::ParamType::Bool,
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                order_id: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                flipped_id: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                owner: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                size: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                price: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                flipped_price: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                is_buy: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
    }
    impl substreams_ethereum::Event for FlippedOrderCreated {
        const NAME: &'static str = "FlippedOrderCreated";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct OrderCanceled {
        pub order_id: substreams::scalar::BigInt,
        pub owner: Vec<u8>,
        pub price: substreams::scalar::BigInt,
        pub size: substreams::scalar::BigInt,
        pub is_buy: bool,
    }
    impl OrderCanceled {
        const TOPIC_ID: [u8; 32] = [
            217u8,
            192u8,
            137u8,
            129u8,
            142u8,
            242u8,
            35u8,
            98u8,
            156u8,
            42u8,
            245u8,
            52u8,
            136u8,
            220u8,
            71u8,
            207u8,
            40u8,
            103u8,
            241u8,
            87u8,
            202u8,
            202u8,
            119u8,
            140u8,
            231u8,
            122u8,
            170u8,
            116u8,
            43u8,
            140u8,
            16u8,
            121u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
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
                        ethabi::ParamType::Uint(40usize),
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Uint(32usize),
                        ethabi::ParamType::Uint(96usize),
                        ethabi::ParamType::Bool,
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                order_id: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                owner: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                price: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                size: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                is_buy: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
    }
    impl substreams_ethereum::Event for OrderCanceled {
        const NAME: &'static str = "OrderCanceled";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct OrderCreated {
        pub order_id: substreams::scalar::BigInt,
        pub owner: Vec<u8>,
        pub size: substreams::scalar::BigInt,
        pub price: substreams::scalar::BigInt,
        pub is_buy: bool,
    }
    impl OrderCreated {
        const TOPIC_ID: [u8; 32] = [
            184u8,
            27u8,
            186u8,
            241u8,
            80u8,
            70u8,
            126u8,
            243u8,
            9u8,
            110u8,
            214u8,
            34u8,
            12u8,
            185u8,
            99u8,
            171u8,
            193u8,
            105u8,
            191u8,
            253u8,
            46u8,
            184u8,
            143u8,
            38u8,
            196u8,
            115u8,
            89u8,
            239u8,
            52u8,
            76u8,
            249u8,
            76u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
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
                        ethabi::ParamType::Uint(40usize),
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Uint(96usize),
                        ethabi::ParamType::Uint(32usize),
                        ethabi::ParamType::Bool,
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                order_id: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                owner: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                size: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                price: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                is_buy: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
    }
    impl substreams_ethereum::Event for OrderCreated {
        const NAME: &'static str = "OrderCreated";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Trade {
        pub order_id: substreams::scalar::BigInt,
        pub maker_address: Vec<u8>,
        pub is_buy: bool,
        pub price: substreams::scalar::BigInt,
        pub updated_size: substreams::scalar::BigInt,
        pub taker_address: Vec<u8>,
        pub tx_origin: Vec<u8>,
        pub filled_size: substreams::scalar::BigInt,
    }
    impl Trade {
        const TOPIC_ID: [u8; 32] = [
            241u8,
            105u8,
            36u8,
            251u8,
            161u8,
            193u8,
            140u8,
            16u8,
            137u8,
            18u8,
            252u8,
            172u8,
            170u8,
            199u8,
            69u8,
            12u8,
            152u8,
            235u8,
            63u8,
            45u8,
            140u8,
            10u8,
            60u8,
            223u8,
            61u8,
            247u8,
            6u8,
            108u8,
            8u8,
            242u8,
            21u8,
            129u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 256usize {
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
                        ethabi::ParamType::Uint(40usize),
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Bool,
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(96usize),
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Uint(96usize),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                order_id: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                maker_address: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                is_buy: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
                price: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                updated_size: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                taker_address: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                tx_origin: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                filled_size: {
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
    impl substreams_ethereum::Event for Trade {
        const NAME: &'static str = "Trade";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
}