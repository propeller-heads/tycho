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
    pub struct BlacklistFeeMultiplierSet {
        pub multiplier: substreams::scalar::BigInt,
    }
    impl BlacklistFeeMultiplierSet {
        const TOPIC_ID: [u8; 32] = [
            161u8,
            80u8,
            87u8,
            136u8,
            110u8,
            110u8,
            188u8,
            223u8,
            71u8,
            41u8,
            75u8,
            203u8,
            9u8,
            29u8,
            104u8,
            96u8,
            49u8,
            18u8,
            77u8,
            16u8,
            65u8,
            202u8,
            254u8,
            0u8,
            116u8,
            14u8,
            147u8,
            102u8,
            123u8,
            172u8,
            209u8,
            134u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
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
                    &[ethabi::ParamType::Uint(256usize)],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                multiplier: {
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
    impl substreams_ethereum::Event for BlacklistFeeMultiplierSet {
        const NAME: &'static str = "BlacklistFeeMultiplierSet";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct BlockDelaySet {
        pub block_delay: substreams::scalar::BigInt,
    }
    impl BlockDelaySet {
        const TOPIC_ID: [u8; 32] = [
            103u8,
            63u8,
            146u8,
            128u8,
            70u8,
            126u8,
            241u8,
            214u8,
            119u8,
            237u8,
            214u8,
            162u8,
            22u8,
            48u8,
            207u8,
            50u8,
            128u8,
            104u8,
            161u8,
            220u8,
            141u8,
            166u8,
            66u8,
            5u8,
            193u8,
            188u8,
            121u8,
            133u8,
            92u8,
            107u8,
            35u8,
            7u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
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
                    &[ethabi::ParamType::Uint(48usize)],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                block_delay: {
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
    impl substreams_ethereum::Event for BlockDelaySet {
        const NAME: &'static str = "BlockDelaySet";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct MaxPunishmentX24Set {
        pub max_punishment_x24: substreams::scalar::BigInt,
    }
    impl MaxPunishmentX24Set {
        const TOPIC_ID: [u8; 32] = [
            224u8,
            46u8,
            195u8,
            69u8,
            135u8,
            10u8,
            57u8,
            113u8,
            249u8,
            42u8,
            127u8,
            105u8,
            172u8,
            169u8,
            224u8,
            242u8,
            214u8,
            229u8,
            86u8,
            122u8,
            115u8,
            142u8,
            37u8,
            120u8,
            213u8,
            174u8,
            112u8,
            96u8,
            5u8,
            100u8,
            85u8,
            205u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
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
                max_punishment_x24: {
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
    impl substreams_ethereum::Event for MaxPunishmentX24Set {
        const NAME: &'static str = "MaxPunishmentX24Set";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Paused {
        pub account: Vec<u8>,
    }
    impl Paused {
        const TOPIC_ID: [u8; 32] = [
            98u8,
            231u8,
            140u8,
            234u8,
            1u8,
            190u8,
            227u8,
            32u8,
            205u8,
            78u8,
            66u8,
            2u8,
            112u8,
            181u8,
            234u8,
            116u8,
            0u8,
            13u8,
            17u8,
            176u8,
            201u8,
            247u8,
            71u8,
            84u8,
            235u8,
            219u8,
            252u8,
            84u8,
            75u8,
            5u8,
            162u8,
            88u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
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
                    &[ethabi::ParamType::Address],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                account: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
    }
    impl substreams_ethereum::Event for Paused {
        const NAME: &'static str = "Paused";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct PunishmentApplied {
        pub x_to_y: bool,
        pub punishment_x24: substreams::scalar::BigInt,
        pub fee_ask_x24: substreams::scalar::BigInt,
        pub fee_bid_x24: substreams::scalar::BigInt,
    }
    impl PunishmentApplied {
        const TOPIC_ID: [u8; 32] = [
            84u8,
            210u8,
            146u8,
            28u8,
            89u8,
            179u8,
            111u8,
            196u8,
            241u8,
            110u8,
            186u8,
            29u8,
            56u8,
            1u8,
            94u8,
            27u8,
            94u8,
            248u8,
            7u8,
            31u8,
            126u8,
            175u8,
            242u8,
            183u8,
            164u8,
            15u8,
            39u8,
            99u8,
            46u8,
            163u8,
            136u8,
            51u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 2usize {
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
                        ethabi::ParamType::Uint(24usize),
                        ethabi::ParamType::Uint(24usize),
                        ethabi::ParamType::Uint(24usize),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                x_to_y: ethabi::decode(
                        &[ethabi::ParamType::Bool],
                        log.topics[1usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'x_to_y' from topic of type 'bool': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
                punishment_x24: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                fee_ask_x24: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                fee_bid_x24: {
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
    impl substreams_ethereum::Event for PunishmentApplied {
        const NAME: &'static str = "PunishmentApplied";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct StateUpdated {
        pub anchor_price: substreams::scalar::BigInt,
        pub fee_ask_x24: substreams::scalar::BigInt,
        pub fee_bid_x24: substreams::scalar::BigInt,
    }
    impl StateUpdated {
        const TOPIC_ID: [u8; 32] = [
            138u8,
            203u8,
            129u8,
            29u8,
            44u8,
            81u8,
            6u8,
            120u8,
            95u8,
            132u8,
            127u8,
            175u8,
            3u8,
            206u8,
            22u8,
            13u8,
            46u8,
            177u8,
            36u8,
            184u8,
            99u8,
            46u8,
            180u8,
            45u8,
            70u8,
            111u8,
            70u8,
            192u8,
            135u8,
            3u8,
            61u8,
            97u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
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
                        ethabi::ParamType::Uint(160usize),
                        ethabi::ParamType::Uint(24usize),
                        ethabi::ParamType::Uint(24usize),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                anchor_price: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                fee_ask_x24: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                fee_bid_x24: {
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
    impl substreams_ethereum::Event for StateUpdated {
        const NAME: &'static str = "StateUpdated";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Sync {
        pub reserve_x: substreams::scalar::BigInt,
        pub reserve_y: substreams::scalar::BigInt,
    }
    impl Sync {
        const TOPIC_ID: [u8; 32] = [
            153u8,
            233u8,
            63u8,
            217u8,
            74u8,
            81u8,
            184u8,
            13u8,
            125u8,
            215u8,
            236u8,
            63u8,
            105u8,
            196u8,
            240u8,
            154u8,
            67u8,
            231u8,
            82u8,
            63u8,
            90u8,
            69u8,
            202u8,
            9u8,
            184u8,
            138u8,
            23u8,
            141u8,
            157u8,
            170u8,
            237u8,
            30u8,
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
                        ethabi::ParamType::Uint(128usize),
                        ethabi::ParamType::Uint(128usize),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                reserve_x: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                reserve_y: {
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
    impl substreams_ethereum::Event for Sync {
        const NAME: &'static str = "Sync";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Unpaused {
        pub account: Vec<u8>,
    }
    impl Unpaused {
        const TOPIC_ID: [u8; 32] = [
            93u8,
            185u8,
            238u8,
            10u8,
            73u8,
            91u8,
            242u8,
            230u8,
            255u8,
            156u8,
            145u8,
            167u8,
            131u8,
            76u8,
            27u8,
            164u8,
            253u8,
            210u8,
            68u8,
            165u8,
            232u8,
            170u8,
            78u8,
            83u8,
            123u8,
            211u8,
            138u8,
            234u8,
            228u8,
            176u8,
            115u8,
            170u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
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
                    &[ethabi::ParamType::Address],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                account: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
    }
    impl substreams_ethereum::Event for Unpaused {
        const NAME: &'static str = "Unpaused";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct WhitelistSet {
        pub account: Vec<u8>,
        pub whitelisted: bool,
    }
    impl WhitelistSet {
        const TOPIC_ID: [u8; 32] = [
            10u8,
            165u8,
            236u8,
            95u8,
            253u8,
            199u8,
            246u8,
            249u8,
            196u8,
            208u8,
            221u8,
            237u8,
            72u8,
            157u8,
            116u8,
            80u8,
            41u8,
            113u8,
            85u8,
            203u8,
            47u8,
            113u8,
            203u8,
            119u8,
            30u8,
            2u8,
            66u8,
            127u8,
            125u8,
            255u8,
            79u8,
            81u8,
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
                    &[ethabi::ParamType::Bool],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                account: ethabi::decode(
                        &[ethabi::ParamType::Address],
                        log.topics[1usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'account' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                whitelisted: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
    }
    impl substreams_ethereum::Event for WhitelistSet {
        const NAME: &'static str = "WhitelistSet";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
}