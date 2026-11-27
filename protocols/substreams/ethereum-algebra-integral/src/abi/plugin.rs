const INTERNAL_ERR: &'static str = "`ethabi_derive` internal error";
/// Contract's functions.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod functions {
    use super::INTERNAL_ERR;
    #[derive(Debug, Clone, PartialEq)]
    pub struct BeforeSwap {
        pub sender: Vec<u8>,
        pub recipient: Vec<u8>,
        pub zero_to_one: bool,
        pub amount_required: substreams::scalar::BigInt,
        pub limit_sqrt_price: substreams::scalar::BigInt,
        pub with_payment_in_advance: bool,
        pub data: Vec<u8>,
    }
    impl BeforeSwap {
        const METHOD_ID: [u8; 4] = [2u8, 156u8, 28u8, 183u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                    &[
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Bool,
                        ethabi::ParamType::Int(256usize),
                        ethabi::ParamType::Uint(160usize),
                        ethabi::ParamType::Bool,
                        ethabi::ParamType::Bytes,
                    ],
                    maybe_data.unwrap(),
                )
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                sender: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                recipient: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                zero_to_one: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
                amount_required: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
                limit_sqrt_price: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                with_payment_in_advance: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
                data: values.pop().expect(INTERNAL_ERR).into_bytes().expect(INTERNAL_ERR),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(
                &[
                    ethabi::Token::Address(ethabi::Address::from_slice(&self.sender)),
                    ethabi::Token::Address(ethabi::Address::from_slice(&self.recipient)),
                    ethabi::Token::Bool(self.zero_to_one.clone()),
                    {
                        let non_full_signed_bytes = self
                            .amount_required
                            .to_signed_bytes_be();
                        let full_signed_bytes_init = if non_full_signed_bytes[0] & 0x80
                            == 0x80
                        {
                            0xff
                        } else {
                            0x00
                        };
                        let mut full_signed_bytes = [full_signed_bytes_init as u8; 32];
                        non_full_signed_bytes
                            .into_iter()
                            .rev()
                            .enumerate()
                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                        ethabi::Token::Int(
                            ethabi::Int::from_big_endian(full_signed_bytes.as_ref()),
                        )
                    },
                    ethabi::Token::Uint(
                        ethabi::Uint::from_big_endian(
                            match self.limit_sqrt_price.clone().to_bytes_be() {
                                (num_bigint::Sign::Plus, bytes) => bytes,
                                (num_bigint::Sign::NoSign, bytes) => bytes,
                                (num_bigint::Sign::Minus, _) => {
                                    panic!("negative numbers are not supported")
                                }
                            }
                                .as_slice(),
                        ),
                    ),
                    ethabi::Token::Bool(self.with_payment_in_advance.clone()),
                    ethabi::Token::Bytes(self.data.clone()),
                ],
            );
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<
            ([u8; 4usize], substreams::scalar::BigInt, substreams::scalar::BigInt),
            String,
        > {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<
            ([u8; 4usize], substreams::scalar::BigInt, substreams::scalar::BigInt),
            String,
        > {
            let mut values = ethabi::decode(
                    &[
                        ethabi::ParamType::FixedBytes(4usize),
                        ethabi::ParamType::Uint(24usize),
                        ethabi::ParamType::Uint(24usize),
                    ],
                    data.as_ref(),
                )
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            values.reverse();
            Ok((
                {
                    let mut result = [0u8; 4];
                    let v = values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_fixed_bytes()
                        .expect(INTERNAL_ERR);
                    result.copy_from_slice(&v);
                    result
                },
                {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
            ))
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(
            &self,
            address: Vec<u8>,
        ) -> Option<
            ([u8; 4usize], substreams::scalar::BigInt, substreams::scalar::BigInt),
        > {
            use substreams_ethereum::pb::eth::rpc;
            let rpc_calls = rpc::RpcCalls {
                calls: vec![rpc::RpcCall { to_addr : address, data : self.encode(), }],
            };
            let responses = substreams_ethereum::rpc::eth_call(&rpc_calls).responses;
            let response = responses.get(0).expect("one response should have existed");
            if response.failed {
                return None;
            }
            match Self::output(response.raw.as_ref()) {
                Ok(data) => Some(data),
                Err(err) => {
                    use substreams_ethereum::Function;
                    substreams::log::info!(
                        "Call output for function `{}` failed to decode with error: {}",
                        Self::NAME, err
                    );
                    None
                }
            }
        }
    }
    impl substreams_ethereum::Function for BeforeSwap {
        const NAME: &'static str = "beforeSwap";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    impl substreams_ethereum::rpc::RPCDecodable<
        ([u8; 4usize], substreams::scalar::BigInt, substreams::scalar::BigInt),
    > for BeforeSwap {
        fn output(
            data: &[u8],
        ) -> Result<
            ([u8; 4usize], substreams::scalar::BigInt, substreams::scalar::BigInt),
            String,
        > {
            Self::output(data)
        }
    }
}
/// Contract's events.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod events {
    use super::INTERNAL_ERR;
    #[derive(Debug, Clone, PartialEq)]
    pub struct CallGasBudgetSet {
        pub old_call_gas_budget: substreams::scalar::BigInt,
        pub new_call_gas_budget: substreams::scalar::BigInt,
    }
    impl CallGasBudgetSet {
        const TOPIC_ID: [u8; 32] = [
            169u8,
            28u8,
            26u8,
            109u8,
            236u8,
            108u8,
            73u8,
            71u8,
            155u8,
            19u8,
            186u8,
            213u8,
            44u8,
            168u8,
            241u8,
            141u8,
            116u8,
            106u8,
            166u8,
            0u8,
            18u8,
            31u8,
            0u8,
            58u8,
            125u8,
            156u8,
            23u8,
            23u8,
            41u8,
            90u8,
            50u8,
            88u8,
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
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                old_call_gas_budget: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                new_call_gas_budget: {
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
    impl substreams_ethereum::Event for CallGasBudgetSet {
        const NAME: &'static str = "CallGasBudgetSet";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Incentive {
        pub new_incentive: Vec<u8>,
    }
    impl Incentive {
        const TOPIC_ID: [u8; 32] = [
            145u8,
            92u8,
            83u8,
            105u8,
            230u8,
            88u8,
            7u8,
            51u8,
            115u8,
            93u8,
            28u8,
            46u8,
            48u8,
            202u8,
            32u8,
            220u8,
            170u8,
            57u8,
            86u8,
            151u8,
            160u8,
            65u8,
            3u8,
            60u8,
            159u8,
            53u8,
            248u8,
            15u8,
            83u8,
            82u8,
            94u8,
            132u8,
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
                new_incentive: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
    }
    impl substreams_ethereum::Event for Incentive {
        const NAME: &'static str = "Incentive";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct MevxRouterSet {
        pub old_mevx_router: Vec<u8>,
        pub new_mevx_router: Vec<u8>,
    }
    impl MevxRouterSet {
        const TOPIC_ID: [u8; 32] = [
            240u8,
            145u8,
            158u8,
            209u8,
            168u8,
            0u8,
            139u8,
            152u8,
            237u8,
            223u8,
            84u8,
            252u8,
            66u8,
            104u8,
            212u8,
            134u8,
            185u8,
            103u8,
            15u8,
            213u8,
            183u8,
            57u8,
            16u8,
            16u8,
            118u8,
            181u8,
            215u8,
            27u8,
            136u8,
            89u8,
            177u8,
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
                    &[ethabi::ParamType::Address, ethabi::ParamType::Address],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                old_mevx_router: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                new_mevx_router: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
    }
    impl substreams_ethereum::Event for MevxRouterSet {
        const NAME: &'static str = "MevxRouterSet";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct MinGasLeftSet {
        pub old_min_gas_left: substreams::scalar::BigInt,
        pub new_min_gas_left: substreams::scalar::BigInt,
    }
    impl MinGasLeftSet {
        const TOPIC_ID: [u8; 32] = [
            40u8,
            222u8,
            47u8,
            233u8,
            201u8,
            49u8,
            113u8,
            105u8,
            246u8,
            146u8,
            107u8,
            140u8,
            240u8,
            173u8,
            41u8,
            202u8,
            208u8,
            25u8,
            10u8,
            65u8,
            14u8,
            59u8,
            173u8,
            42u8,
            231u8,
            80u8,
            131u8,
            38u8,
            3u8,
            254u8,
            237u8,
            114u8,
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
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                old_min_gas_left: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                new_min_gas_left: {
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
    impl substreams_ethereum::Event for MinGasLeftSet {
        const NAME: &'static str = "MinGasLeftSet";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
}