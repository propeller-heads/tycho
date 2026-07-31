const INTERNAL_ERR: &'static str = "`ethabi_derive` internal error";
/// Contract's functions.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod functions {
    use super::INTERNAL_ERR;
    #[derive(Debug, Clone, PartialEq)]
    pub struct AddPricer {
        pub who: Vec<u8>,
    }
    impl AddPricer {
        const METHOD_ID: [u8; 4] = [69u8, 26u8, 94u8, 101u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                who: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data =
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.who))]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
    }
    impl substreams_ethereum::Function for AddPricer {
        const NAME: &'static str = "addPricer";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetFairPriceE8 {
        pub base_asset: Vec<u8>,
        pub quote_asset: Vec<u8>,
    }
    impl GetFairPriceE8 {
        const METHOD_ID: [u8; 4] = [250u8, 235u8, 152u8, 69u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Address, ethabi::ParamType::Address],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                base_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                quote_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Address(ethabi::Address::from_slice(&self.base_asset)),
                ethabi::Token::Address(ethabi::Address::from_slice(&self.quote_asset)),
            ]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<(substreams::scalar::BigInt, substreams::scalar::BigInt), String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<(substreams::scalar::BigInt, substreams::scalar::BigInt), String> {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Uint(256usize), ethabi::ParamType::Uint(64usize)],
                data.as_ref(),
            )
            .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            values.reverse();
            Ok((
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
        ) -> Option<(substreams::scalar::BigInt, substreams::scalar::BigInt)> {
            use substreams_ethereum::pb::eth::rpc;
            let rpc_calls = rpc::RpcCalls {
                calls: vec![rpc::RpcCall { to_addr: address, data: self.encode() }],
            };
            let responses = substreams_ethereum::rpc::eth_call(&rpc_calls).responses;
            let response = responses
                .get(0)
                .expect("one response should have existed");
            if response.failed {
                return None;
            }
            match Self::output(response.raw.as_ref()) {
                Ok(data) => Some(data),
                Err(err) => {
                    use substreams_ethereum::Function;
                    substreams::log::info!(
                        "Call output for function `{}` failed to decode with error: {}",
                        Self::NAME,
                        err
                    );
                    None
                }
            }
        }
    }
    impl substreams_ethereum::Function for GetFairPriceE8 {
        const NAME: &'static str = "getFairPriceE8";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    impl
        substreams_ethereum::rpc::RPCDecodable<(
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
        )> for GetFairPriceE8
    {
        fn output(
            data: &[u8],
        ) -> Result<(substreams::scalar::BigInt, substreams::scalar::BigInt), String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetPairParams {
        pub base_asset: Vec<u8>,
        pub quote_asset: Vec<u8>,
    }
    impl GetPairParams {
        const METHOD_ID: [u8; 4] = [195u8, 58u8, 103u8, 186u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Address, ethabi::ParamType::Address],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                base_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                quote_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Address(ethabi::Address::from_slice(&self.base_asset)),
                ethabi::Token::Address(ethabi::Address::from_slice(&self.quote_asset)),
            ]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                Vec<(
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                )>,
                Vec<(
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                )>,
            ),
            String,
        > {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                Vec<(
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                )>,
                Vec<(
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                )>,
            ),
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Uint(128usize),
                    ethabi::ParamType::Uint(16usize),
                    ethabi::ParamType::Uint(128usize),
                    ethabi::ParamType::Uint(128usize),
                    ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Int(128usize),
                        ethabi::ParamType::Int(128usize),
                        ethabi::ParamType::Int(128usize),
                        ethabi::ParamType::Int(128usize),
                        ethabi::ParamType::Int(128usize),
                        ethabi::ParamType::Int(128usize),
                    ]))),
                    ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Int(128usize),
                        ethabi::ParamType::Int(128usize),
                        ethabi::ParamType::Int(128usize),
                        ethabi::ParamType::Int(128usize),
                        ethabi::ParamType::Int(128usize),
                        ethabi::ParamType::Int(128usize),
                    ]))),
                ])],
                data.as_ref(),
            )
            .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok({
                let tuple_elements = values
                    .pop()
                    .expect("one output data should have existed")
                    .into_tuple()
                    .expect(INTERNAL_ERR);
                (
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[0usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[1usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[2usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[3usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
                    tuple_elements[4usize]
                        .clone()
                        .into_array()
                        .expect(INTERNAL_ERR)
                        .into_iter()
                        .map(|inner| {
                            let tuple_elements = inner.into_tuple().expect(INTERNAL_ERR);
                            (
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[0usize]
                                        .clone()
                                        .into_int()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[1usize]
                                        .clone()
                                        .into_int()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[2usize]
                                        .clone()
                                        .into_int()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[3usize]
                                        .clone()
                                        .into_int()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[4usize]
                                        .clone()
                                        .into_int()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[5usize]
                                        .clone()
                                        .into_int()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                },
                            )
                        })
                        .collect(),
                    tuple_elements[5usize]
                        .clone()
                        .into_array()
                        .expect(INTERNAL_ERR)
                        .into_iter()
                        .map(|inner| {
                            let tuple_elements = inner.into_tuple().expect(INTERNAL_ERR);
                            (
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[0usize]
                                        .clone()
                                        .into_int()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[1usize]
                                        .clone()
                                        .into_int()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[2usize]
                                        .clone()
                                        .into_int()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[3usize]
                                        .clone()
                                        .into_int()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[4usize]
                                        .clone()
                                        .into_int()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[5usize]
                                        .clone()
                                        .into_int()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                },
                            )
                        })
                        .collect(),
                )
            })
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
        ) -> Option<(
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            Vec<(
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            )>,
            Vec<(
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            )>,
        )> {
            use substreams_ethereum::pb::eth::rpc;
            let rpc_calls = rpc::RpcCalls {
                calls: vec![rpc::RpcCall { to_addr: address, data: self.encode() }],
            };
            let responses = substreams_ethereum::rpc::eth_call(&rpc_calls).responses;
            let response = responses
                .get(0)
                .expect("one response should have existed");
            if response.failed {
                return None;
            }
            match Self::output(response.raw.as_ref()) {
                Ok(data) => Some(data),
                Err(err) => {
                    use substreams_ethereum::Function;
                    substreams::log::info!(
                        "Call output for function `{}` failed to decode with error: {}",
                        Self::NAME,
                        err
                    );
                    None
                }
            }
        }
    }
    impl substreams_ethereum::Function for GetPairParams {
        const NAME: &'static str = "getPairParams";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    impl
        substreams_ethereum::rpc::RPCDecodable<(
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            Vec<(
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            )>,
            Vec<(
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            )>,
        )> for GetPairParams
    {
        fn output(
            data: &[u8],
        ) -> Result<
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                Vec<(
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                )>,
                Vec<(
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                )>,
            ),
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetPairs {}
    impl GetPairs {
        const METHOD_ID: [u8; 4] = [118u8, 126u8, 181u8, 239u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Ok(Self {})
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Vec<(Vec<u8>, Vec<u8>, bool)>, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>, bool)>, String> {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Bool,
                ])))],
                data.as_ref(),
            )
            .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok(values
                .pop()
                .expect("one output data should have existed")
                .into_array()
                .expect(INTERNAL_ERR)
                .into_iter()
                .map(|inner| {
                    let tuple_elements = inner.into_tuple().expect(INTERNAL_ERR);
                    (
                        tuple_elements[0usize]
                            .clone()
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec(),
                        tuple_elements[1usize]
                            .clone()
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec(),
                        tuple_elements[2usize]
                            .clone()
                            .into_bool()
                            .expect(INTERNAL_ERR),
                    )
                })
                .collect())
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<Vec<(Vec<u8>, Vec<u8>, bool)>> {
            use substreams_ethereum::pb::eth::rpc;
            let rpc_calls = rpc::RpcCalls {
                calls: vec![rpc::RpcCall { to_addr: address, data: self.encode() }],
            };
            let responses = substreams_ethereum::rpc::eth_call(&rpc_calls).responses;
            let response = responses
                .get(0)
                .expect("one response should have existed");
            if response.failed {
                return None;
            }
            match Self::output(response.raw.as_ref()) {
                Ok(data) => Some(data),
                Err(err) => {
                    use substreams_ethereum::Function;
                    substreams::log::info!(
                        "Call output for function `{}` failed to decode with error: {}",
                        Self::NAME,
                        err
                    );
                    None
                }
            }
        }
    }
    impl substreams_ethereum::Function for GetPairs {
        const NAME: &'static str = "getPairs";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    impl substreams_ethereum::rpc::RPCDecodable<Vec<(Vec<u8>, Vec<u8>, bool)>> for GetPairs {
        fn output(data: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>, bool)>, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct IsActive {
        pub base_asset: Vec<u8>,
        pub quote_asset: Vec<u8>,
    }
    impl IsActive {
        const METHOD_ID: [u8; 4] = [174u8, 19u8, 29u8, 235u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Address, ethabi::ParamType::Address],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                base_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                quote_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Address(ethabi::Address::from_slice(&self.base_asset)),
                ethabi::Token::Address(ethabi::Address::from_slice(&self.quote_asset)),
            ]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<bool, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<bool, String> {
            let mut values = ethabi::decode(&[ethabi::ParamType::Bool], data.as_ref())
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok(values
                .pop()
                .expect("one output data should have existed")
                .into_bool()
                .expect(INTERNAL_ERR))
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<bool> {
            use substreams_ethereum::pb::eth::rpc;
            let rpc_calls = rpc::RpcCalls {
                calls: vec![rpc::RpcCall { to_addr: address, data: self.encode() }],
            };
            let responses = substreams_ethereum::rpc::eth_call(&rpc_calls).responses;
            let response = responses
                .get(0)
                .expect("one response should have existed");
            if response.failed {
                return None;
            }
            match Self::output(response.raw.as_ref()) {
                Ok(data) => Some(data),
                Err(err) => {
                    use substreams_ethereum::Function;
                    substreams::log::info!(
                        "Call output for function `{}` failed to decode with error: {}",
                        Self::NAME,
                        err
                    );
                    None
                }
            }
        }
    }
    impl substreams_ethereum::Function for IsActive {
        const NAME: &'static str = "isActive";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    impl substreams_ethereum::rpc::RPCDecodable<bool> for IsActive {
        fn output(data: &[u8]) -> Result<bool, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Quote {
        pub token_in: Vec<u8>,
        pub token_out: Vec<u8>,
        pub amount_specified: substreams::scalar::BigInt,
        pub sender: Vec<u8>,
    }
    impl Quote {
        const METHOD_ID: [u8; 4] = [201u8, 226u8, 112u8, 208u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Int(256usize),
                    ethabi::ParamType::Address,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                token_in: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                token_out: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                amount_specified: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
                sender: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Address(ethabi::Address::from_slice(&self.token_in)),
                ethabi::Token::Address(ethabi::Address::from_slice(&self.token_out)),
                {
                    let non_full_signed_bytes = self
                        .amount_specified
                        .to_signed_bytes_be();
                    let full_signed_bytes_init =
                        if non_full_signed_bytes[0] & 0x80 == 0x80 { 0xff } else { 0x00 };
                    let mut full_signed_bytes = [full_signed_bytes_init as u8; 32];
                    non_full_signed_bytes
                        .into_iter()
                        .rev()
                        .enumerate()
                        .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                    ethabi::Token::Int(ethabi::Int::from_big_endian(full_signed_bytes.as_ref()))
                },
                ethabi::Token::Address(ethabi::Address::from_slice(&self.sender)),
            ]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<(substreams::scalar::BigInt, substreams::scalar::BigInt), String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<(substreams::scalar::BigInt, substreams::scalar::BigInt), String> {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Uint(256usize), ethabi::ParamType::Uint(256usize)],
                data.as_ref(),
            )
            .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            values.reverse();
            Ok((
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
        ) -> Option<(substreams::scalar::BigInt, substreams::scalar::BigInt)> {
            use substreams_ethereum::pb::eth::rpc;
            let rpc_calls = rpc::RpcCalls {
                calls: vec![rpc::RpcCall { to_addr: address, data: self.encode() }],
            };
            let responses = substreams_ethereum::rpc::eth_call(&rpc_calls).responses;
            let response = responses
                .get(0)
                .expect("one response should have existed");
            if response.failed {
                return None;
            }
            match Self::output(response.raw.as_ref()) {
                Ok(data) => Some(data),
                Err(err) => {
                    use substreams_ethereum::Function;
                    substreams::log::info!(
                        "Call output for function `{}` failed to decode with error: {}",
                        Self::NAME,
                        err
                    );
                    None
                }
            }
        }
    }
    impl substreams_ethereum::Function for Quote {
        const NAME: &'static str = "quote";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    impl
        substreams_ethereum::rpc::RPCDecodable<(
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
        )> for Quote
    {
        fn output(
            data: &[u8],
        ) -> Result<(substreams::scalar::BigInt, substreams::scalar::BigInt), String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct RegisterPair {
        pub base_asset: Vec<u8>,
        pub quote_asset: Vec<u8>,
        pub p: (
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            Vec<(
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            )>,
            Vec<(
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            )>,
        ),
    }
    impl RegisterPair {
        const METHOD_ID: [u8; 4] = [41u8, 74u8, 143u8, 23u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(128usize),
                        ethabi::ParamType::Uint(16usize),
                        ethabi::ParamType::Uint(128usize),
                        ethabi::ParamType::Uint(128usize),
                        ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                        ]))),
                        ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                        ]))),
                    ]),
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                base_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                quote_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                p: {
                    let tuple_elements = values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_tuple()
                        .expect(INTERNAL_ERR);
                    (
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[0usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[1usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[2usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[3usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                        tuple_elements[4usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                let tuple_elements = inner.into_tuple().expect(INTERNAL_ERR);
                                (
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[0usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[1usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[2usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[3usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[4usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[5usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                )
                            })
                            .collect(),
                        tuple_elements[5usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                let tuple_elements = inner.into_tuple().expect(INTERNAL_ERR);
                                (
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[0usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[1usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[2usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[3usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[4usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[5usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                )
                            })
                            .collect(),
                    )
                },
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Address(ethabi::Address::from_slice(&self.base_asset)),
                ethabi::Token::Address(ethabi::Address::from_slice(&self.quote_asset)),
                ethabi::Token::Tuple(vec![
                    ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                        match self.p.0.clone().to_bytes_be() {
                            (num_bigint::Sign::Plus, bytes) => bytes,
                            (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                                panic!("negative numbers are not supported")
                            }
                        }
                        .as_slice(),
                    )),
                    ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                        match self.p.1.clone().to_bytes_be() {
                            (num_bigint::Sign::Plus, bytes) => bytes,
                            (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                                panic!("negative numbers are not supported")
                            }
                        }
                        .as_slice(),
                    )),
                    ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                        match self.p.2.clone().to_bytes_be() {
                            (num_bigint::Sign::Plus, bytes) => bytes,
                            (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                                panic!("negative numbers are not supported")
                            }
                        }
                        .as_slice(),
                    )),
                    ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                        match self.p.3.clone().to_bytes_be() {
                            (num_bigint::Sign::Plus, bytes) => bytes,
                            (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                                panic!("negative numbers are not supported")
                            }
                        }
                        .as_slice(),
                    )),
                    {
                        let v = self
                            .p
                            .4
                            .iter()
                            .map(|inner| {
                                ethabi::Token::Tuple(vec![
                                    {
                                        let non_full_signed_bytes = inner.0.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.1.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.2.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.3.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.4.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.5.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                ])
                            })
                            .collect();
                        ethabi::Token::Array(v)
                    },
                    {
                        let v = self
                            .p
                            .5
                            .iter()
                            .map(|inner| {
                                ethabi::Token::Tuple(vec![
                                    {
                                        let non_full_signed_bytes = inner.0.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.1.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.2.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.3.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.4.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.5.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                ])
                            })
                            .collect();
                        ethabi::Token::Array(v)
                    },
                ]),
            ]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
    }
    impl substreams_ethereum::Function for RegisterPair {
        const NAME: &'static str = "registerPair";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct RemovePricer {
        pub who: Vec<u8>,
    }
    impl RemovePricer {
        const METHOD_ID: [u8; 4] = [121u8, 212u8, 69u8, 115u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                who: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data =
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.who))]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
    }
    impl substreams_ethereum::Function for RemovePricer {
        const NAME: &'static str = "removePricer";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct SetFairPriceE8 {
        pub k: [u8; 32usize],
        pub p: substreams::scalar::BigInt,
    }
    impl SetFairPriceE8 {
        const METHOD_ID: [u8; 4] = [172u8, 238u8, 138u8, 171u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[ethabi::ParamType::FixedBytes(32usize), ethabi::ParamType::Uint(256usize)],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                k: {
                    let mut result = [0u8; 32];
                    let v = values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_fixed_bytes()
                        .expect(INTERNAL_ERR);
                    result.copy_from_slice(&v);
                    result
                },
                p: {
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
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::FixedBytes(self.k.as_ref().to_vec()),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.p.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
            ]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
    }
    impl substreams_ethereum::Function for SetFairPriceE8 {
        const NAME: &'static str = "setFairPriceE8";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct SetFairPricesE8 {
        pub u: Vec<([u8; 32usize], substreams::scalar::BigInt)>,
    }
    impl SetFairPricesE8 {
        const METHOD_ID: [u8; 4] = [127u8, 225u8, 1u8, 88u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::FixedBytes(32usize),
                    ethabi::ParamType::Uint(256usize),
                ])))],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                u: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_array()
                    .expect(INTERNAL_ERR)
                    .into_iter()
                    .map(|inner| {
                        let tuple_elements = inner.into_tuple().expect(INTERNAL_ERR);
                        (
                            {
                                let mut result = [0u8; 32];
                                let v = tuple_elements[0usize]
                                    .clone()
                                    .into_fixed_bytes()
                                    .expect(INTERNAL_ERR);
                                result.copy_from_slice(&v);
                                result
                            },
                            {
                                let mut v = [0 as u8; 32];
                                tuple_elements[1usize]
                                    .clone()
                                    .into_uint()
                                    .expect(INTERNAL_ERR)
                                    .to_big_endian(v.as_mut_slice());
                                substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                            },
                        )
                    })
                    .collect(),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[{
                let v = self
                    .u
                    .iter()
                    .map(|inner| {
                        ethabi::Token::Tuple(vec![
                            ethabi::Token::FixedBytes(inner.0.as_ref().to_vec()),
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                                match inner.1.clone().to_bytes_be() {
                                    (num_bigint::Sign::Plus, bytes) => bytes,
                                    (num_bigint::Sign::NoSign, bytes) => bytes,
                                    (num_bigint::Sign::Minus, _) => {
                                        panic!("negative numbers are not supported")
                                    }
                                }
                                .as_slice(),
                            )),
                        ])
                    })
                    .collect();
                ethabi::Token::Array(v)
            }]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
    }
    impl substreams_ethereum::Function for SetFairPricesE8 {
        const NAME: &'static str = "setFairPricesE8";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct SetPairActive {
        pub base_asset: Vec<u8>,
        pub quote_asset: Vec<u8>,
        pub active: bool,
    }
    impl SetPairActive {
        const METHOD_ID: [u8; 4] = [214u8, 195u8, 201u8, 223u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Address, ethabi::ParamType::Address, ethabi::ParamType::Bool],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                base_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                quote_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                active: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Address(ethabi::Address::from_slice(&self.base_asset)),
                ethabi::Token::Address(ethabi::Address::from_slice(&self.quote_asset)),
                ethabi::Token::Bool(self.active.clone()),
            ]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
    }
    impl substreams_ethereum::Function for SetPairActive {
        const NAME: &'static str = "setPairActive";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct SetPairParams {
        pub base_asset: Vec<u8>,
        pub quote_asset: Vec<u8>,
        pub p: (
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            Vec<(
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            )>,
            Vec<(
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            )>,
        ),
    }
    impl SetPairParams {
        const METHOD_ID: [u8; 4] = [24u8, 175u8, 194u8, 116u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(128usize),
                        ethabi::ParamType::Uint(16usize),
                        ethabi::ParamType::Uint(128usize),
                        ethabi::ParamType::Uint(128usize),
                        ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                        ]))),
                        ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                            ethabi::ParamType::Int(128usize),
                        ]))),
                    ]),
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                base_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                quote_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                p: {
                    let tuple_elements = values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_tuple()
                        .expect(INTERNAL_ERR);
                    (
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[0usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[1usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[2usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[3usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                        tuple_elements[4usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                let tuple_elements = inner.into_tuple().expect(INTERNAL_ERR);
                                (
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[0usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[1usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[2usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[3usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[4usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[5usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                )
                            })
                            .collect(),
                        tuple_elements[5usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                let tuple_elements = inner.into_tuple().expect(INTERNAL_ERR);
                                (
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[0usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[1usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[2usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[3usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[4usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[5usize]
                                            .clone()
                                            .into_int()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                    },
                                )
                            })
                            .collect(),
                    )
                },
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Address(ethabi::Address::from_slice(&self.base_asset)),
                ethabi::Token::Address(ethabi::Address::from_slice(&self.quote_asset)),
                ethabi::Token::Tuple(vec![
                    ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                        match self.p.0.clone().to_bytes_be() {
                            (num_bigint::Sign::Plus, bytes) => bytes,
                            (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                                panic!("negative numbers are not supported")
                            }
                        }
                        .as_slice(),
                    )),
                    ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                        match self.p.1.clone().to_bytes_be() {
                            (num_bigint::Sign::Plus, bytes) => bytes,
                            (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                                panic!("negative numbers are not supported")
                            }
                        }
                        .as_slice(),
                    )),
                    ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                        match self.p.2.clone().to_bytes_be() {
                            (num_bigint::Sign::Plus, bytes) => bytes,
                            (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                                panic!("negative numbers are not supported")
                            }
                        }
                        .as_slice(),
                    )),
                    ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                        match self.p.3.clone().to_bytes_be() {
                            (num_bigint::Sign::Plus, bytes) => bytes,
                            (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                                panic!("negative numbers are not supported")
                            }
                        }
                        .as_slice(),
                    )),
                    {
                        let v = self
                            .p
                            .4
                            .iter()
                            .map(|inner| {
                                ethabi::Token::Tuple(vec![
                                    {
                                        let non_full_signed_bytes = inner.0.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.1.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.2.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.3.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.4.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.5.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                ])
                            })
                            .collect();
                        ethabi::Token::Array(v)
                    },
                    {
                        let v = self
                            .p
                            .5
                            .iter()
                            .map(|inner| {
                                ethabi::Token::Tuple(vec![
                                    {
                                        let non_full_signed_bytes = inner.0.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.1.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.2.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.3.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.4.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                    {
                                        let non_full_signed_bytes = inner.5.to_signed_bytes_be();
                                        let full_signed_bytes_init =
                                            if non_full_signed_bytes[0] & 0x80 == 0x80 {
                                                0xff
                                            } else {
                                                0x00
                                            };
                                        let mut full_signed_bytes =
                                            [full_signed_bytes_init as u8; 32];
                                        non_full_signed_bytes
                                            .into_iter()
                                            .rev()
                                            .enumerate()
                                            .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                                        ethabi::Token::Int(ethabi::Int::from_big_endian(
                                            full_signed_bytes.as_ref(),
                                        ))
                                    },
                                ])
                            })
                            .collect();
                        ethabi::Token::Array(v)
                    },
                ]),
            ]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
    }
    impl substreams_ethereum::Function for SetPairParams {
        const NAME: &'static str = "setPairParams";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Swap {
        pub token_in: Vec<u8>,
        pub token_out: Vec<u8>,
        pub amount_specified: substreams::scalar::BigInt,
        pub sender: Vec<u8>,
    }
    impl Swap {
        const METHOD_ID: [u8; 4] = [214u8, 176u8, 189u8, 213u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Int(256usize),
                    ethabi::ParamType::Address,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                token_in: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                token_out: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                amount_specified: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
                sender: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Address(ethabi::Address::from_slice(&self.token_in)),
                ethabi::Token::Address(ethabi::Address::from_slice(&self.token_out)),
                {
                    let non_full_signed_bytes = self
                        .amount_specified
                        .to_signed_bytes_be();
                    let full_signed_bytes_init =
                        if non_full_signed_bytes[0] & 0x80 == 0x80 { 0xff } else { 0x00 };
                    let mut full_signed_bytes = [full_signed_bytes_init as u8; 32];
                    non_full_signed_bytes
                        .into_iter()
                        .rev()
                        .enumerate()
                        .for_each(|(i, byte)| full_signed_bytes[31 - i] = byte);
                    ethabi::Token::Int(ethabi::Int::from_big_endian(full_signed_bytes.as_ref()))
                },
                ethabi::Token::Address(ethabi::Address::from_slice(&self.sender)),
            ]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<(substreams::scalar::BigInt, substreams::scalar::BigInt), String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<(substreams::scalar::BigInt, substreams::scalar::BigInt), String> {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Uint(256usize), ethabi::ParamType::Uint(256usize)],
                data.as_ref(),
            )
            .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            values.reverse();
            Ok((
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
        ) -> Option<(substreams::scalar::BigInt, substreams::scalar::BigInt)> {
            use substreams_ethereum::pb::eth::rpc;
            let rpc_calls = rpc::RpcCalls {
                calls: vec![rpc::RpcCall { to_addr: address, data: self.encode() }],
            };
            let responses = substreams_ethereum::rpc::eth_call(&rpc_calls).responses;
            let response = responses
                .get(0)
                .expect("one response should have existed");
            if response.failed {
                return None;
            }
            match Self::output(response.raw.as_ref()) {
                Ok(data) => Some(data),
                Err(err) => {
                    use substreams_ethereum::Function;
                    substreams::log::info!(
                        "Call output for function `{}` failed to decode with error: {}",
                        Self::NAME,
                        err
                    );
                    None
                }
            }
        }
    }
    impl substreams_ethereum::Function for Swap {
        const NAME: &'static str = "swap";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    impl
        substreams_ethereum::rpc::RPCDecodable<(
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
        )> for Swap
    {
        fn output(
            data: &[u8],
        ) -> Result<(substreams::scalar::BigInt, substreams::scalar::BigInt), String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct TraderVault {}
    impl TraderVault {
        const METHOD_ID: [u8; 4] = [81u8, 237u8, 14u8, 227u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Ok(Self {})
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Vec<u8>, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<Vec<u8>, String> {
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], data.as_ref())
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok(values
                .pop()
                .expect("one output data should have existed")
                .into_address()
                .expect(INTERNAL_ERR)
                .as_bytes()
                .to_vec())
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<Vec<u8>> {
            use substreams_ethereum::pb::eth::rpc;
            let rpc_calls = rpc::RpcCalls {
                calls: vec![rpc::RpcCall { to_addr: address, data: self.encode() }],
            };
            let responses = substreams_ethereum::rpc::eth_call(&rpc_calls).responses;
            let response = responses
                .get(0)
                .expect("one response should have existed");
            if response.failed {
                return None;
            }
            match Self::output(response.raw.as_ref()) {
                Ok(data) => Some(data),
                Err(err) => {
                    use substreams_ethereum::Function;
                    substreams::log::info!(
                        "Call output for function `{}` failed to decode with error: {}",
                        Self::NAME,
                        err
                    );
                    None
                }
            }
        }
    }
    impl substreams_ethereum::Function for TraderVault {
        const NAME: &'static str = "traderVault";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    impl substreams_ethereum::rpc::RPCDecodable<Vec<u8>> for TraderVault {
        fn output(data: &[u8]) -> Result<Vec<u8>, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Unlocked {
        pub token_in: Vec<u8>,
        pub token_out: Vec<u8>,
    }
    impl Unlocked {
        const METHOD_ID: [u8; 4] = [90u8, 51u8, 115u8, 60u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Address, ethabi::ParamType::Address],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                token_in: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                token_out: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Address(ethabi::Address::from_slice(&self.token_in)),
                ethabi::Token::Address(ethabi::Address::from_slice(&self.token_out)),
            ]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<bool, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<bool, String> {
            let mut values = ethabi::decode(&[ethabi::ParamType::Bool], data.as_ref())
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok(values
                .pop()
                .expect("one output data should have existed")
                .into_bool()
                .expect(INTERNAL_ERR))
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<bool> {
            use substreams_ethereum::pb::eth::rpc;
            let rpc_calls = rpc::RpcCalls {
                calls: vec![rpc::RpcCall { to_addr: address, data: self.encode() }],
            };
            let responses = substreams_ethereum::rpc::eth_call(&rpc_calls).responses;
            let response = responses
                .get(0)
                .expect("one response should have existed");
            if response.failed {
                return None;
            }
            match Self::output(response.raw.as_ref()) {
                Ok(data) => Some(data),
                Err(err) => {
                    use substreams_ethereum::Function;
                    substreams::log::info!(
                        "Call output for function `{}` failed to decode with error: {}",
                        Self::NAME,
                        err
                    );
                    None
                }
            }
        }
    }
    impl substreams_ethereum::Function for Unlocked {
        const NAME: &'static str = "unlocked";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
    impl substreams_ethereum::rpc::RPCDecodable<bool> for Unlocked {
        fn output(data: &[u8]) -> Result<bool, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct UnregisterPair {
        pub base_asset: Vec<u8>,
        pub quote_asset: Vec<u8>,
    }
    impl UnregisterPair {
        const METHOD_ID: [u8; 4] = [23u8, 208u8, 23u8, 22u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Address, ethabi::ParamType::Address],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                base_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                quote_asset: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Address(ethabi::Address::from_slice(&self.base_asset)),
                ethabi::Token::Address(ethabi::Address::from_slice(&self.quote_asset)),
            ]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
    }
    impl substreams_ethereum::Function for UnregisterPair {
        const NAME: &'static str = "unregisterPair";
        fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            Self::match_call(call)
        }
        fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            Self::decode(call)
        }
        fn encode(&self) -> Vec<u8> {
            self.encode()
        }
    }
}
/// Contract's events.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod events {
    use super::INTERNAL_ERR;
    #[derive(Debug, Clone, PartialEq)]
    pub struct PairActiveSet {
        pub base_asset: Vec<u8>,
        pub quote_asset: Vec<u8>,
        pub active: bool,
    }
    impl PairActiveSet {
        const TOPIC_ID: [u8; 32] = [
            192u8, 152u8, 119u8, 91u8, 3u8, 25u8, 29u8, 222u8, 242u8, 122u8, 11u8, 11u8, 152u8,
            107u8, 24u8, 84u8, 131u8, 223u8, 147u8, 236u8, 101u8, 131u8, 76u8, 253u8, 92u8, 239u8,
            171u8, 132u8, 29u8, 197u8, 86u8, 169u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 3usize {
                return false;
            }
            if log.data.len() != 32usize {
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
            let mut values = ethabi::decode(&[ethabi::ParamType::Bool], log.data.as_ref())
                .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                base_asset: ethabi::decode(
                    &[ethabi::ParamType::Address],
                    log.topics[1usize].as_ref(),
                )
                .map_err(|e| {
                    format!(
                        "unable to decode param 'base_asset' from topic of type 'address': {:?}",
                        e
                    )
                })?
                .pop()
                .expect(INTERNAL_ERR)
                .into_address()
                .expect(INTERNAL_ERR)
                .as_bytes()
                .to_vec(),
                quote_asset: ethabi::decode(
                    &[ethabi::ParamType::Address],
                    log.topics[2usize].as_ref(),
                )
                .map_err(|e| {
                    format!(
                        "unable to decode param 'quote_asset' from topic of type 'address': {:?}",
                        e
                    )
                })?
                .pop()
                .expect(INTERNAL_ERR)
                .into_address()
                .expect(INTERNAL_ERR)
                .as_bytes()
                .to_vec(),
                active: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
    }
    impl substreams_ethereum::Event for PairActiveSet {
        const NAME: &'static str = "PairActiveSet";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct PairRegistered {
        pub base_asset: Vec<u8>,
        pub quote_asset: Vec<u8>,
    }
    impl PairRegistered {
        const TOPIC_ID: [u8; 32] = [
            4u8, 168u8, 196u8, 164u8, 7u8, 1u8, 201u8, 51u8, 188u8, 152u8, 118u8, 42u8, 207u8,
            32u8, 126u8, 123u8, 176u8, 204u8, 85u8, 135u8, 43u8, 82u8, 148u8, 78u8, 163u8, 246u8,
            121u8, 191u8, 157u8, 65u8, 222u8, 129u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 3usize {
                return false;
            }
            if log.data.len() != 0usize {
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
            Ok(Self {
                base_asset: ethabi::decode(
                    &[ethabi::ParamType::Address],
                    log.topics[1usize].as_ref(),
                )
                .map_err(|e| {
                    format!(
                        "unable to decode param 'base_asset' from topic of type 'address': {:?}",
                        e
                    )
                })?
                .pop()
                .expect(INTERNAL_ERR)
                .into_address()
                .expect(INTERNAL_ERR)
                .as_bytes()
                .to_vec(),
                quote_asset: ethabi::decode(
                    &[ethabi::ParamType::Address],
                    log.topics[2usize].as_ref(),
                )
                .map_err(|e| {
                    format!(
                        "unable to decode param 'quote_asset' from topic of type 'address': {:?}",
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
    impl substreams_ethereum::Event for PairRegistered {
        const NAME: &'static str = "PairRegistered";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct PairUnregistered {
        pub base_asset: Vec<u8>,
        pub quote_asset: Vec<u8>,
    }
    impl PairUnregistered {
        const TOPIC_ID: [u8; 32] = [
            199u8, 110u8, 91u8, 206u8, 152u8, 3u8, 218u8, 162u8, 110u8, 95u8, 139u8, 5u8, 12u8,
            238u8, 65u8, 111u8, 140u8, 9u8, 70u8, 128u8, 249u8, 155u8, 44u8, 102u8, 58u8, 46u8,
            164u8, 79u8, 13u8, 229u8, 213u8, 70u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 3usize {
                return false;
            }
            if log.data.len() != 0usize {
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
            Ok(Self {
                base_asset: ethabi::decode(
                    &[ethabi::ParamType::Address],
                    log.topics[1usize].as_ref(),
                )
                .map_err(|e| {
                    format!(
                        "unable to decode param 'base_asset' from topic of type 'address': {:?}",
                        e
                    )
                })?
                .pop()
                .expect(INTERNAL_ERR)
                .into_address()
                .expect(INTERNAL_ERR)
                .as_bytes()
                .to_vec(),
                quote_asset: ethabi::decode(
                    &[ethabi::ParamType::Address],
                    log.topics[2usize].as_ref(),
                )
                .map_err(|e| {
                    format!(
                        "unable to decode param 'quote_asset' from topic of type 'address': {:?}",
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
    impl substreams_ethereum::Event for PairUnregistered {
        const NAME: &'static str = "PairUnregistered";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
}
