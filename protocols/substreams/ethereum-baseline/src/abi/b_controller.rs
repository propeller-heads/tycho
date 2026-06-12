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
    pub struct ApprovedCreditDeployerSet {
        pub user: Vec<u8>,
        pub approved: bool,
    }
    impl ApprovedCreditDeployerSet {
        const TOPIC_ID: [u8; 32] = [
            146u8,
            217u8,
            93u8,
            29u8,
            16u8,
            16u8,
            106u8,
            69u8,
            166u8,
            54u8,
            152u8,
            91u8,
            185u8,
            90u8,
            214u8,
            241u8,
            171u8,
            63u8,
            54u8,
            184u8,
            224u8,
            70u8,
            250u8,
            29u8,
            66u8,
            99u8,
            225u8,
            185u8,
            159u8,
            68u8,
            133u8,
            249u8,
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
                    &[ethabi::ParamType::Address, ethabi::ParamType::Bool],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                user: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                approved: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
    }
    impl substreams_ethereum::Event for ApprovedCreditDeployerSet {
        const NAME: &'static str = "ApprovedCreditDeployerSet";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct CreatorFeePctSet {
        pub b_token: Vec<u8>,
        pub creator_fee_pct: substreams::scalar::BigInt,
    }
    impl CreatorFeePctSet {
        const TOPIC_ID: [u8; 32] = [
            209u8,
            103u8,
            144u8,
            186u8,
            46u8,
            19u8,
            150u8,
            8u8,
            119u8,
            69u8,
            154u8,
            42u8,
            75u8,
            192u8,
            140u8,
            21u8,
            103u8,
            221u8,
            179u8,
            23u8,
            61u8,
            39u8,
            83u8,
            110u8,
            186u8,
            226u8,
            236u8,
            16u8,
            132u8,
            90u8,
            106u8,
            128u8,
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
                    &[ethabi::ParamType::Address, ethabi::ParamType::Uint(256usize)],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                b_token: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                creator_fee_pct: {
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
    impl substreams_ethereum::Event for CreatorFeePctSet {
        const NAME: &'static str = "CreatorFeePctSet";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct CreatorTransferred {
        pub b_token: Vec<u8>,
        pub new_creator: Vec<u8>,
    }
    impl CreatorTransferred {
        const TOPIC_ID: [u8; 32] = [
            127u8,
            17u8,
            254u8,
            12u8,
            112u8,
            152u8,
            225u8,
            167u8,
            106u8,
            252u8,
            198u8,
            143u8,
            53u8,
            171u8,
            170u8,
            7u8,
            239u8,
            21u8,
            111u8,
            167u8,
            214u8,
            221u8,
            45u8,
            57u8,
            102u8,
            72u8,
            18u8,
            254u8,
            55u8,
            52u8,
            141u8,
            22u8,
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
                    &[ethabi::ParamType::Address, ethabi::ParamType::Address],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                b_token: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                new_creator: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
    }
    impl substreams_ethereum::Event for CreatorTransferred {
        const NAME: &'static str = "CreatorTransferred";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct DeployerProfileSet {
        pub deployer: Vec<u8>,
        pub profile: (
            bool,
            bool,
            bool,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
        ),
    }
    impl DeployerProfileSet {
        const TOPIC_ID: [u8; 32] = [
            104u8,
            238u8,
            17u8,
            85u8,
            199u8,
            23u8,
            203u8,
            102u8,
            135u8,
            190u8,
            155u8,
            105u8,
            251u8,
            24u8,
            54u8,
            131u8,
            126u8,
            143u8,
            175u8,
            130u8,
            66u8,
            225u8,
            115u8,
            25u8,
            49u8,
            1u8,
            1u8,
            106u8,
            229u8,
            21u8,
            128u8,
            162u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 192usize {
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
                        ethabi::ParamType::Tuple(
                            vec![
                                ethabi::ParamType::Bool, ethabi::ParamType::Bool,
                                ethabi::ParamType::Bool, ethabi::ParamType::Uint(64usize),
                                ethabi::ParamType::Uint(64usize)
                            ],
                        ),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                deployer: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                profile: {
                    let tuple_elements = values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_tuple()
                        .expect(INTERNAL_ERR);
                    (
                        tuple_elements[0usize].clone().into_bool().expect(INTERNAL_ERR),
                        tuple_elements[1usize].clone().into_bool().expect(INTERNAL_ERR),
                        tuple_elements[2usize].clone().into_bool().expect(INTERNAL_ERR),
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[3usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[4usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                    )
                },
            })
        }
    }
    impl substreams_ethereum::Event for DeployerProfileSet {
        const NAME: &'static str = "DeployerProfileSet";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct DeployerSet {
        pub b_token: Vec<u8>,
        pub deployer: Vec<u8>,
    }
    impl DeployerSet {
        const TOPIC_ID: [u8; 32] = [
            159u8,
            0u8,
            71u8,
            122u8,
            114u8,
            36u8,
            221u8,
            106u8,
            106u8,
            66u8,
            27u8,
            77u8,
            27u8,
            54u8,
            32u8,
            30u8,
            204u8,
            143u8,
            115u8,
            150u8,
            227u8,
            210u8,
            18u8,
            81u8,
            41u8,
            219u8,
            95u8,
            104u8,
            152u8,
            188u8,
            55u8,
            186u8,
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
                    &[ethabi::ParamType::Address, ethabi::ParamType::Address],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                b_token: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                deployer: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
    }
    impl substreams_ethereum::Event for DeployerSet {
        const NAME: &'static str = "DeployerSet";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct FeeRecipientSet {
        pub b_token: Vec<u8>,
        pub fee_recipient: Vec<u8>,
    }
    impl FeeRecipientSet {
        const TOPIC_ID: [u8; 32] = [
            21u8,
            216u8,
            10u8,
            1u8,
            63u8,
            34u8,
            21u8,
            27u8,
            199u8,
            36u8,
            110u8,
            59u8,
            193u8,
            50u8,
            225u8,
            40u8,
            40u8,
            205u8,
            225u8,
            157u8,
            233u8,
            136u8,
            112u8,
            71u8,
            94u8,
            63u8,
            167u8,
            8u8,
            64u8,
            21u8,
            39u8,
            33u8,
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
                    &[ethabi::ParamType::Address, ethabi::ParamType::Address],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                b_token: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                fee_recipient: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
    }
    impl substreams_ethereum::Event for FeeRecipientSet {
        const NAME: &'static str = "FeeRecipientSet";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct FeesClaimed {
        pub b_token: Vec<u8>,
        pub reserve: Vec<u8>,
        pub creator_amount: substreams::scalar::BigInt,
        pub protocol_amount: substreams::scalar::BigInt,
    }
    impl FeesClaimed {
        const TOPIC_ID: [u8; 32] = [
            145u8,
            109u8,
            96u8,
            207u8,
            83u8,
            96u8,
            192u8,
            88u8,
            114u8,
            44u8,
            1u8,
            61u8,
            244u8,
            233u8,
            88u8,
            22u8,
            174u8,
            93u8,
            223u8,
            113u8,
            244u8,
            194u8,
            24u8,
            235u8,
            137u8,
            209u8,
            175u8,
            183u8,
            110u8,
            4u8,
            123u8,
            184u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
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
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                b_token: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                reserve: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                creator_amount: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                protocol_amount: {
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
    impl substreams_ethereum::Event for FeesClaimed {
        const NAME: &'static str = "FeesClaimed";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LiquidityFeePctSet {
        pub b_token: Vec<u8>,
        pub liquidity_fee_pct: substreams::scalar::BigInt,
    }
    impl LiquidityFeePctSet {
        const TOPIC_ID: [u8; 32] = [
            206u8,
            81u8,
            112u8,
            22u8,
            165u8,
            40u8,
            186u8,
            215u8,
            67u8,
            225u8,
            177u8,
            176u8,
            86u8,
            255u8,
            122u8,
            17u8,
            12u8,
            17u8,
            152u8,
            75u8,
            69u8,
            243u8,
            54u8,
            73u8,
            117u8,
            123u8,
            249u8,
            219u8,
            22u8,
            82u8,
            60u8,
            214u8,
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
                    &[ethabi::ParamType::Address, ethabi::ParamType::Uint(256usize)],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                b_token: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                liquidity_fee_pct: {
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
    impl substreams_ethereum::Event for LiquidityFeePctSet {
        const NAME: &'static str = "LiquidityFeePctSet";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct ReserveApproved {
        pub reserve: Vec<u8>,
        pub approved: bool,
    }
    impl ReserveApproved {
        const TOPIC_ID: [u8; 32] = [
            54u8,
            69u8,
            18u8,
            147u8,
            155u8,
            53u8,
            195u8,
            99u8,
            106u8,
            4u8,
            252u8,
            46u8,
            160u8,
            215u8,
            223u8,
            245u8,
            30u8,
            142u8,
            252u8,
            66u8,
            233u8,
            131u8,
            50u8,
            48u8,
            121u8,
            106u8,
            176u8,
            2u8,
            102u8,
            30u8,
            46u8,
            24u8,
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
                    &[ethabi::ParamType::Address, ethabi::ParamType::Bool],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                reserve: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                approved: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
    }
    impl substreams_ethereum::Event for ReserveApproved {
        const NAME: &'static str = "ReserveApproved";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
}