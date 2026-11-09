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
    pub struct SetCustomFee {
        pub pool: Vec<u8>,
        pub fee: substreams::scalar::BigInt,
    }
    impl SetCustomFee {
        const TOPIC_ID: [u8; 32] = [
            174u8,
            70u8,
            140u8,
            229u8,
            134u8,
            249u8,
            168u8,
            118u8,
            96u8,
            253u8,
            255u8,
            193u8,
            68u8,
            140u8,
            238u8,
            148u8,
            32u8,
            66u8,
            193u8,
            106u8,
            226u8,
            240u8,
            32u8,
            70u8,
            177u8,
            52u8,
            181u8,
            34u8,
            79u8,
            49u8,
            147u8,
            107u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 3usize {
                return false;
            }
            if log.data.len() != 0usize {
                return false;
            }
            return log.topics.get(0).expect("bounds already checked").as_ref()
                == Self::TOPIC_ID;
        }
        pub fn decode(
            log: &substreams_ethereum::pb::eth::v2::Log,
        ) -> Result<Self, String> {
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
                fee: {
                    let mut v = [0 as u8; 32];
                    ethabi::decode(
                            &[ethabi::ParamType::Uint(256usize)],
                            log.topics[2usize].as_ref(),
                        )
                        .map_err(|e| {
                            format!(
                                "unable to decode param 'fee' from topic of type 'uint256': {:?}",
                                e
                            )
                        })?
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
    impl substreams_ethereum::Event for SetCustomFee {
        const NAME: &'static str = "SetCustomFee";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
}