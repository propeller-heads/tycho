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
    pub struct FyndPolicyCreated {
        pub pool: Vec<u8>,
        pub version: substreams::scalar::BigInt,
    }
    impl FyndPolicyCreated {
        const TOPIC_ID: [u8; 32] = [
            236u8,
            182u8,
            34u8,
            126u8,
            216u8,
            126u8,
            189u8,
            218u8,
            34u8,
            64u8,
            150u8,
            174u8,
            148u8,
            146u8,
            124u8,
            130u8,
            148u8,
            246u8,
            45u8,
            210u8,
            84u8,
            148u8,
            124u8,
            41u8,
            155u8,
            116u8,
            97u8,
            105u8,
            113u8,
            222u8,
            199u8,
            15u8,
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
                    &[ethabi::ParamType::Uint(256usize)],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                pool: ethabi::decode(
                        &[ethabi::ParamType::Address],
                        log.topics[1usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'pool' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                version: {
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
    impl substreams_ethereum::Event for FyndPolicyCreated {
        const NAME: &'static str = "FyndPolicyCreated";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct SetController {
        pub controller: Vec<u8>,
    }
    impl SetController {
        const TOPIC_ID: [u8; 32] = [
            79u8,
            246u8,
            56u8,
            69u8,
            43u8,
            191u8,
            51u8,
            192u8,
            18u8,
            100u8,
            93u8,
            24u8,
            174u8,
            111u8,
            5u8,
            81u8,
            95u8,
            245u8,
            242u8,
            209u8,
            223u8,
            176u8,
            206u8,
            206u8,
            140u8,
            191u8,
            1u8,
            140u8,
            96u8,
            144u8,
            63u8,
            112u8,
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
                controller: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
    }
    impl substreams_ethereum::Event for SetController {
        const NAME: &'static str = "SetController";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct SetKilled {
        pub is_killed: bool,
    }
    impl SetKilled {
        const TOPIC_ID: [u8; 32] = [
            246u8,
            162u8,
            62u8,
            207u8,
            49u8,
            38u8,
            48u8,
            102u8,
            212u8,
            204u8,
            26u8,
            194u8,
            24u8,
            55u8,
            193u8,
            113u8,
            72u8,
            24u8,
            4u8,
            94u8,
            204u8,
            92u8,
            13u8,
            216u8,
            159u8,
            39u8,
            158u8,
            11u8,
            212u8,
            183u8,
            239u8,
            135u8,
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
                    &[ethabi::ParamType::Bool],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                is_killed: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
    }
    impl substreams_ethereum::Event for SetKilled {
        const NAME: &'static str = "SetKilled";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct SetMinFee {
        pub min_fee: substreams::scalar::BigInt,
    }
    impl SetMinFee {
        const TOPIC_ID: [u8; 32] = [
            129u8,
            210u8,
            200u8,
            113u8,
            246u8,
            152u8,
            75u8,
            103u8,
            67u8,
            169u8,
            180u8,
            84u8,
            106u8,
            204u8,
            3u8,
            165u8,
            149u8,
            162u8,
            230u8,
            30u8,
            8u8,
            143u8,
            202u8,
            189u8,
            115u8,
            191u8,
            141u8,
            200u8,
            57u8,
            38u8,
            107u8,
            129u8,
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
                min_fee: {
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
    impl substreams_ethereum::Event for SetMinFee {
        const NAME: &'static str = "SetMinFee";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct SetRouter {
        pub router: Vec<u8>,
    }
    impl SetRouter {
        const TOPIC_ID: [u8; 32] = [
            109u8,
            228u8,
            50u8,
            106u8,
            139u8,
            144u8,
            84u8,
            215u8,
            45u8,
            157u8,
            186u8,
            185u8,
            125u8,
            39u8,
            188u8,
            78u8,
            223u8,
            250u8,
            222u8,
            231u8,
            217u8,
            102u8,
            245u8,
            175u8,
            156u8,
            196u8,
            234u8,
            253u8,
            175u8,
            142u8,
            84u8,
            85u8,
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
                router: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
    }
    impl substreams_ethereum::Event for SetRouter {
        const NAME: &'static str = "SetRouter";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
}