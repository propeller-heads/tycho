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
    pub struct PoolCreated {
        pub token0: Vec<u8>,
        pub token1: Vec<u8>,
        pub fee: substreams::scalar::BigInt,
        pub tick_spacing: substreams::scalar::BigInt,
        pub pool: Vec<u8>,
    }
    impl PoolCreated {
        const TOPIC_ID: [u8; 32] = [
            120u8, 60u8, 202u8, 28u8, 4u8, 18u8, 221u8, 13u8, 105u8, 94u8, 120u8, 69u8, 104u8,
            201u8, 109u8, 162u8, 233u8, 194u8, 47u8, 249u8, 137u8, 53u8, 122u8, 46u8, 139u8, 29u8,
            155u8, 43u8, 78u8, 107u8, 113u8, 24u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 4usize {
                return false;
            }
            if log.data.len() != 64usize {
                return false;
            }
            return log
                .topics
                .get(0)
                .expect("bounds already checked")
                .as_ref() ==
                Self::TOPIC_ID;
        }
        pub fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Int(24usize), ethabi::ParamType::Address],
                log.data.as_ref(),
            )
            .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                token0: ethabi::decode(&[ethabi::ParamType::Address], log.topics[1usize].as_ref())
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'token0' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                token1: ethabi::decode(&[ethabi::ParamType::Address], log.topics[2usize].as_ref())
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'token1' from topic of type 'address': {:?}",
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
                        &[ethabi::ParamType::Uint(24usize)],
                        log.topics[3usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!("unable to decode param 'fee' from topic of type 'uint24': {:?}", e)
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_uint()
                    .expect(INTERNAL_ERR)
                    .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                tick_spacing: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
                pool: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
    }
    impl substreams_ethereum::Event for PoolCreated {
        const NAME: &'static str = "PoolCreated";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
}
