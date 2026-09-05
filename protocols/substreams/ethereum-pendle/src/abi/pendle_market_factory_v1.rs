const INTERNAL_ERR: &'static str = "`ethabi_derive` internal error";
/// Contract's functions.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod functions {
    use super::INTERNAL_ERR;
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetMarketConfig {
        pub router: Vec<u8>,
    }
    impl GetMarketConfig {
        const METHOD_ID: [u8; 4] = [199u8, 191u8, 210u8, 253u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                    &[ethabi::ParamType::Address],
                    maybe_data.unwrap(),
                )
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
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
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(
                &[ethabi::Token::Address(ethabi::Address::from_slice(&self.router))],
            );
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<
            (Vec<u8>, substreams::scalar::BigInt, substreams::scalar::BigInt),
            String,
        > {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<
            (Vec<u8>, substreams::scalar::BigInt, substreams::scalar::BigInt),
            String,
        > {
            let mut values = ethabi::decode(
                    &[
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Uint(80usize),
                        ethabi::ParamType::Uint(8usize),
                    ],
                    data.as_ref(),
                )
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            values.reverse();
            Ok((
                values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
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
        ) -> Option<(Vec<u8>, substreams::scalar::BigInt, substreams::scalar::BigInt)> {
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
    impl substreams_ethereum::Function for GetMarketConfig {
        const NAME: &'static str = "getMarketConfig";
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
        (Vec<u8>, substreams::scalar::BigInt, substreams::scalar::BigInt),
    > for GetMarketConfig {
        fn output(
            data: &[u8],
        ) -> Result<
            (Vec<u8>, substreams::scalar::BigInt, substreams::scalar::BigInt),
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
    pub struct CreateNewMarket {
        pub market: Vec<u8>,
        pub pt: Vec<u8>,
        pub scalar_root: substreams::scalar::BigInt,
        pub initial_anchor: substreams::scalar::BigInt,
    }
    impl CreateNewMarket {
        const TOPIC_ID: [u8; 32] = [
            22u8,
            106u8,
            229u8,
            245u8,
            86u8,
            21u8,
            182u8,
            91u8,
            189u8,
            154u8,
            36u8,
            150u8,
            233u8,
            141u8,
            78u8,
            77u8,
            120u8,
            202u8,
            21u8,
            189u8,
            97u8,
            39u8,
            192u8,
            254u8,
            45u8,
            194u8,
            123u8,
            118u8,
            246u8,
            192u8,
            49u8,
            67u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 3usize {
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
                        ethabi::ParamType::Int(256usize),
                        ethabi::ParamType::Int(256usize),
                    ],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                market: ethabi::decode(
                        &[ethabi::ParamType::Address],
                        log.topics[1usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'market' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                pt: ethabi::decode(
                        &[ethabi::ParamType::Address],
                        log.topics[2usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'pt' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                scalar_root: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
                initial_anchor: {
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
    impl substreams_ethereum::Event for CreateNewMarket {
        const NAME: &'static str = "CreateNewMarket";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct NewMarketConfig {
        pub treasury: Vec<u8>,
        pub ln_fee_rate_root: substreams::scalar::BigInt,
        pub reserve_fee_percent: substreams::scalar::BigInt,
    }
    impl NewMarketConfig {
        const TOPIC_ID: [u8; 32] = [
            231u8,
            179u8,
            156u8,
            51u8,
            162u8,
            136u8,
            171u8,
            222u8,
            73u8,
            184u8,
            216u8,
            49u8,
            178u8,
            18u8,
            91u8,
            34u8,
            219u8,
            177u8,
            81u8,
            118u8,
            109u8,
            106u8,
            165u8,
            43u8,
            21u8,
            17u8,
            179u8,
            64u8,
            146u8,
            61u8,
            69u8,
            135u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 2usize {
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
                    &[ethabi::ParamType::Uint(80usize), ethabi::ParamType::Uint(8usize)],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                treasury: ethabi::decode(
                        &[ethabi::ParamType::Address],
                        log.topics[1usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'treasury' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                ln_fee_rate_root: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                reserve_fee_percent: {
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
    impl substreams_ethereum::Event for NewMarketConfig {
        const NAME: &'static str = "NewMarketConfig";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct SetOverriddenFee {
        pub router: Vec<u8>,
        pub ln_fee_rate_root: substreams::scalar::BigInt,
        pub reserve_fee_percent: substreams::scalar::BigInt,
    }
    impl SetOverriddenFee {
        const TOPIC_ID: [u8; 32] = [
            161u8,
            158u8,
            116u8,
            38u8,
            211u8,
            7u8,
            84u8,
            245u8,
            13u8,
            84u8,
            169u8,
            193u8,
            200u8,
            43u8,
            64u8,
            15u8,
            193u8,
            180u8,
            81u8,
            106u8,
            185u8,
            233u8,
            30u8,
            32u8,
            38u8,
            150u8,
            52u8,
            251u8,
            139u8,
            66u8,
            39u8,
            82u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 2usize {
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
                    &[ethabi::ParamType::Uint(80usize), ethabi::ParamType::Uint(8usize)],
                    log.data.as_ref(),
                )
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                router: ethabi::decode(
                        &[ethabi::ParamType::Address],
                        log.topics[1usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'router' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                ln_fee_rate_root: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                reserve_fee_percent: {
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
    impl substreams_ethereum::Event for SetOverriddenFee {
        const NAME: &'static str = "SetOverriddenFee";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct UnsetOverriddenFee {
        pub router: Vec<u8>,
    }
    impl UnsetOverriddenFee {
        const TOPIC_ID: [u8; 32] = [
            222u8,
            110u8,
            145u8,
            235u8,
            149u8,
            131u8,
            2u8,
            86u8,
            56u8,
            75u8,
            68u8,
            126u8,
            14u8,
            229u8,
            100u8,
            85u8,
            108u8,
            133u8,
            173u8,
            232u8,
            7u8,
            219u8,
            88u8,
            220u8,
            110u8,
            29u8,
            24u8,
            104u8,
            132u8,
            24u8,
            170u8,
            205u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 2usize {
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
                router: ethabi::decode(
                        &[ethabi::ParamType::Address],
                        log.topics[1usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'router' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
    }
    impl substreams_ethereum::Event for UnsetOverriddenFee {
        const NAME: &'static str = "UnsetOverriddenFee";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
}