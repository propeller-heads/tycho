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
}