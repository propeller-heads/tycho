const INTERNAL_ERR: &'static str = "`ethabi_derive` internal error";
/// Contract's functions.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod functions {
    use super::INTERNAL_ERR;
    #[derive(Debug, Clone, PartialEq)]
    pub struct EstimateSwapIn {
        pub dex: Vec<u8>,
        pub swap0to1: bool,
        pub amount_in: substreams::scalar::BigInt,
        pub amount_out_min: substreams::scalar::BigInt,
    }
    impl EstimateSwapIn {
        const METHOD_ID: [u8; 4] = [187u8, 57u8, 227u8, 161u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Bool,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                dex: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                swap0to1: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
                amount_in: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                amount_out_min: {
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
                ethabi::Token::Address(ethabi::Address::from_slice(&self.dex)),
                ethabi::Token::Bool(self.swap0to1.clone()),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.amount_in.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self
                        .amount_out_min
                        .clone()
                        .to_bytes_be()
                    {
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
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<substreams::scalar::BigInt, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            let mut values = ethabi::decode(&[ethabi::ParamType::Uint(256usize)], data.as_ref())
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok({
                let mut v = [0 as u8; 32];
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_uint()
                    .expect(INTERNAL_ERR)
                    .to_big_endian(v.as_mut_slice());
                substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
            })
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<substreams::scalar::BigInt> {
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
    impl substreams_ethereum::Function for EstimateSwapIn {
        const NAME: &'static str = "estimateSwapIn";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for EstimateSwapIn {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct EstimateSwapOut {
        pub dex: Vec<u8>,
        pub swap0to1: bool,
        pub amount_out: substreams::scalar::BigInt,
        pub amount_in_max: substreams::scalar::BigInt,
    }
    impl EstimateSwapOut {
        const METHOD_ID: [u8; 4] = [66u8, 87u8, 17u8, 55u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Bool,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                dex: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                swap0to1: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
                amount_out: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                amount_in_max: {
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
                ethabi::Token::Address(ethabi::Address::from_slice(&self.dex)),
                ethabi::Token::Bool(self.swap0to1.clone()),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.amount_out.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.amount_in_max.clone().to_bytes_be() {
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
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<substreams::scalar::BigInt, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            let mut values = ethabi::decode(&[ethabi::ParamType::Uint(256usize)], data.as_ref())
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok({
                let mut v = [0 as u8; 32];
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_uint()
                    .expect(INTERNAL_ERR)
                    .to_big_endian(v.as_mut_slice());
                substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
            })
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<substreams::scalar::BigInt> {
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
    impl substreams_ethereum::Function for EstimateSwapOut {
        const NAME: &'static str = "estimateSwapOut";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for EstimateSwapOut {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Factory {}
    impl Factory {
        const METHOD_ID: [u8; 4] = [45u8, 211u8, 16u8, 0u8];
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
    impl substreams_ethereum::Function for Factory {
        const NAME: &'static str = "FACTORY";
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
    impl substreams_ethereum::rpc::RPCDecodable<Vec<u8>> for Factory {
        fn output(data: &[u8]) -> Result<Vec<u8>, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetAllPoolAddresses {}
    impl GetAllPoolAddresses {
        const METHOD_ID: [u8; 4] = [197u8, 111u8, 27u8, 68u8];
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
        ) -> Result<Vec<Vec<u8>>, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Array(Box::new(ethabi::ParamType::Address))],
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
                    inner
                        .into_address()
                        .expect(INTERNAL_ERR)
                        .as_bytes()
                        .to_vec()
                })
                .collect())
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<Vec<Vec<u8>>> {
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
    impl substreams_ethereum::Function for GetAllPoolAddresses {
        const NAME: &'static str = "getAllPoolAddresses";
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
    impl substreams_ethereum::rpc::RPCDecodable<Vec<Vec<u8>>> for GetAllPoolAddresses {
        fn output(data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetAllPools {}
    impl GetAllPools {
        const METHOD_ID: [u8; 4] = [216u8, 143u8, 241u8, 244u8];
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
        ) -> Result<Vec<(Vec<u8>, Vec<u8>, Vec<u8>, substreams::scalar::BigInt)>, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<Vec<(Vec<u8>, Vec<u8>, Vec<u8>, substreams::scalar::BigInt)>, String> {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Uint(256usize),
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
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec(),
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[3usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
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
        pub fn call(
            &self,
            address: Vec<u8>,
        ) -> Option<Vec<(Vec<u8>, Vec<u8>, Vec<u8>, substreams::scalar::BigInt)>> {
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
    impl substreams_ethereum::Function for GetAllPools {
        const NAME: &'static str = "getAllPools";
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
        substreams_ethereum::rpc::RPCDecodable<
            Vec<(Vec<u8>, Vec<u8>, Vec<u8>, substreams::scalar::BigInt)>,
        > for GetAllPools
    {
        fn output(
            data: &[u8],
        ) -> Result<Vec<(Vec<u8>, Vec<u8>, Vec<u8>, substreams::scalar::BigInt)>, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetAllPoolsReserves {}
    impl GetAllPoolsReserves {
        const METHOD_ID: [u8; 4] = [110u8, 56u8, 192u8, 35u8];
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
        ) -> Result<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
            String,
        > {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                    ]),
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
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec(),
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
                        {
                            let tuple_elements = tuple_elements[5usize]
                                .clone()
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
                            )
                        },
                        {
                            let tuple_elements = tuple_elements[6usize]
                                .clone()
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
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[4usize]
                                        .clone()
                                        .into_uint()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[5usize]
                                        .clone()
                                        .into_uint()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                                },
                            )
                        },
                        {
                            let tuple_elements = tuple_elements[7usize]
                                .clone()
                                .into_tuple()
                                .expect(INTERNAL_ERR);
                            (
                                {
                                    let tuple_elements = tuple_elements[0usize]
                                        .clone()
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
                                    )
                                },
                                {
                                    let tuple_elements = tuple_elements[1usize]
                                        .clone()
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
                                    )
                                },
                                {
                                    let tuple_elements = tuple_elements[2usize]
                                        .clone()
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
                                    )
                                },
                                {
                                    let tuple_elements = tuple_elements[3usize]
                                        .clone()
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
                                    )
                                },
                            )
                        },
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
        pub fn call(
            &self,
            address: Vec<u8>,
        ) -> Option<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
        > {
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
    impl substreams_ethereum::Function for GetAllPoolsReserves {
        const NAME: &'static str = "getAllPoolsReserves";
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
        substreams_ethereum::rpc::RPCDecodable<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
        > for GetAllPoolsReserves
    {
        fn output(
            data: &[u8],
        ) -> Result<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetAllPoolsReservesAdjusted {}
    impl GetAllPoolsReservesAdjusted {
        const METHOD_ID: [u8; 4] = [51u8, 61u8, 1u8, 169u8];
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
        ) -> Result<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
            String,
        > {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                    ]),
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
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec(),
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
                        {
                            let tuple_elements = tuple_elements[5usize]
                                .clone()
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
                            )
                        },
                        {
                            let tuple_elements = tuple_elements[6usize]
                                .clone()
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
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[4usize]
                                        .clone()
                                        .into_uint()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[5usize]
                                        .clone()
                                        .into_uint()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                                },
                            )
                        },
                        {
                            let tuple_elements = tuple_elements[7usize]
                                .clone()
                                .into_tuple()
                                .expect(INTERNAL_ERR);
                            (
                                {
                                    let tuple_elements = tuple_elements[0usize]
                                        .clone()
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
                                    )
                                },
                                {
                                    let tuple_elements = tuple_elements[1usize]
                                        .clone()
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
                                    )
                                },
                                {
                                    let tuple_elements = tuple_elements[2usize]
                                        .clone()
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
                                    )
                                },
                                {
                                    let tuple_elements = tuple_elements[3usize]
                                        .clone()
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
                                    )
                                },
                            )
                        },
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
        pub fn call(
            &self,
            address: Vec<u8>,
        ) -> Option<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
        > {
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
    impl substreams_ethereum::Function for GetAllPoolsReservesAdjusted {
        const NAME: &'static str = "getAllPoolsReservesAdjusted";
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
        substreams_ethereum::rpc::RPCDecodable<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
        > for GetAllPoolsReservesAdjusted
    {
        fn output(
            data: &[u8],
        ) -> Result<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetDexCollateralReserves {
        pub dex: Vec<u8>,
    }
    impl GetDexCollateralReserves {
        const METHOD_ID: [u8; 4] = [149u8, 119u8, 85u8, 230u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                dex: values
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
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.dex))]);
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
            ),
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
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
    impl substreams_ethereum::Function for GetDexCollateralReserves {
        const NAME: &'static str = "getDexCollateralReserves";
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
        )> for GetDexCollateralReserves
    {
        fn output(
            data: &[u8],
        ) -> Result<
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetDexCollateralReservesAdjusted {
        pub dex: Vec<u8>,
    }
    impl GetDexCollateralReservesAdjusted {
        const METHOD_ID: [u8; 4] = [192u8, 238u8, 192u8, 232u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                dex: values
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
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.dex))]);
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
            ),
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
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
    impl substreams_ethereum::Function for GetDexCollateralReservesAdjusted {
        const NAME: &'static str = "getDexCollateralReservesAdjusted";
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
        )> for GetDexCollateralReservesAdjusted
    {
        fn output(
            data: &[u8],
        ) -> Result<
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetDexDebtReserves {
        pub dex: Vec<u8>,
    }
    impl GetDexDebtReserves {
        const METHOD_ID: [u8; 4] = [85u8, 24u8, 31u8, 17u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                dex: values
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
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.dex))]);
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
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
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
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
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
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[4usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[5usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
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
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
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
    impl substreams_ethereum::Function for GetDexDebtReserves {
        const NAME: &'static str = "getDexDebtReserves";
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
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
        )> for GetDexDebtReserves
    {
        fn output(
            data: &[u8],
        ) -> Result<
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetDexDebtReservesAdjusted {
        pub dex: Vec<u8>,
    }
    impl GetDexDebtReservesAdjusted {
        const METHOD_ID: [u8; 4] = [221u8, 224u8, 71u8, 6u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                dex: values
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
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.dex))]);
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
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
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
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
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
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[4usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[5usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
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
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
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
    impl substreams_ethereum::Function for GetDexDebtReservesAdjusted {
        const NAME: &'static str = "getDexDebtReservesAdjusted";
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
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
        )> for GetDexDebtReservesAdjusted
    {
        fn output(
            data: &[u8],
        ) -> Result<
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetDexLimits {
        pub dex: Vec<u8>,
    }
    impl GetDexLimits {
        const METHOD_ID: [u8; 4] = [24u8, 15u8, 76u8, 101u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                dex: values
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
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.dex))]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<
            (
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
            ),
            String,
        > {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<
            (
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
            ),
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
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
                        let tuple_elements = tuple_elements[0usize]
                            .clone()
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
                        )
                    },
                    {
                        let tuple_elements = tuple_elements[1usize]
                            .clone()
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
                        )
                    },
                    {
                        let tuple_elements = tuple_elements[2usize]
                            .clone()
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
                        )
                    },
                    {
                        let tuple_elements = tuple_elements[3usize]
                            .clone()
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
                        )
                    },
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
            (substreams::scalar::BigInt, substreams::scalar::BigInt, substreams::scalar::BigInt),
            (substreams::scalar::BigInt, substreams::scalar::BigInt, substreams::scalar::BigInt),
            (substreams::scalar::BigInt, substreams::scalar::BigInt, substreams::scalar::BigInt),
            (substreams::scalar::BigInt, substreams::scalar::BigInt, substreams::scalar::BigInt),
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
    impl substreams_ethereum::Function for GetDexLimits {
        const NAME: &'static str = "getDexLimits";
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
            (substreams::scalar::BigInt, substreams::scalar::BigInt, substreams::scalar::BigInt),
            (substreams::scalar::BigInt, substreams::scalar::BigInt, substreams::scalar::BigInt),
            (substreams::scalar::BigInt, substreams::scalar::BigInt, substreams::scalar::BigInt),
            (substreams::scalar::BigInt, substreams::scalar::BigInt, substreams::scalar::BigInt),
        )> for GetDexLimits
    {
        fn output(
            data: &[u8],
        ) -> Result<
            (
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
            ),
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetDexPricesAndExchangePrices {
        pub dex: Vec<u8>,
    }
    impl GetDexPricesAndExchangePrices {
        const METHOD_ID: [u8; 4] = [1u8, 95u8, 108u8, 250u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                dex: values
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
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.dex))]);
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
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
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
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
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
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[4usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[5usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[6usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[7usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[8usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
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
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
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
    impl substreams_ethereum::Function for GetDexPricesAndExchangePrices {
        const NAME: &'static str = "getDexPricesAndExchangePrices";
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
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
        )> for GetDexPricesAndExchangePrices
    {
        fn output(
            data: &[u8],
        ) -> Result<
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetPool {
        pub pool_id: substreams::scalar::BigInt,
    }
    impl GetPool {
        const METHOD_ID: [u8; 4] = [6u8, 139u8, 205u8, 141u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values =
                ethabi::decode(&[ethabi::ParamType::Uint(256usize)], maybe_data.unwrap())
                    .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                pool_id: {
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
            let data = ethabi::encode(&[ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                match self.pool_id.clone().to_bytes_be() {
                    (num_bigint::Sign::Plus, bytes) => bytes,
                    (num_bigint::Sign::NoSign, bytes) => bytes,
                    (num_bigint::Sign::Minus, _) => {
                        panic!("negative numbers are not supported")
                    }
                }
                .as_slice(),
            ))]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>, substreams::scalar::BigInt), String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>, substreams::scalar::BigInt), String> {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Uint(256usize),
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
                        .into_address()
                        .expect(INTERNAL_ERR)
                        .as_bytes()
                        .to_vec(),
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[3usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
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
        ) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>, substreams::scalar::BigInt)> {
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
    impl substreams_ethereum::Function for GetPool {
        const NAME: &'static str = "getPool";
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
            Vec<u8>,
            Vec<u8>,
            Vec<u8>,
            substreams::scalar::BigInt,
        )> for GetPool
    {
        fn output(
            data: &[u8],
        ) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>, substreams::scalar::BigInt), String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetPoolAddress {
        pub pool_id: substreams::scalar::BigInt,
    }
    impl GetPoolAddress {
        const METHOD_ID: [u8; 4] = [0u8, 165u8, 174u8, 33u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values =
                ethabi::decode(&[ethabi::ParamType::Uint(256usize)], maybe_data.unwrap())
                    .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                pool_id: {
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
            let data = ethabi::encode(&[ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                match self.pool_id.clone().to_bytes_be() {
                    (num_bigint::Sign::Plus, bytes) => bytes,
                    (num_bigint::Sign::NoSign, bytes) => bytes,
                    (num_bigint::Sign::Minus, _) => {
                        panic!("negative numbers are not supported")
                    }
                }
                .as_slice(),
            ))]);
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
    impl substreams_ethereum::Function for GetPoolAddress {
        const NAME: &'static str = "getPoolAddress";
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
    impl substreams_ethereum::rpc::RPCDecodable<Vec<u8>> for GetPoolAddress {
        fn output(data: &[u8]) -> Result<Vec<u8>, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetPoolConstantsView {
        pub pool: Vec<u8>,
    }
    impl GetPoolConstantsView {
        const METHOD_ID: [u8; 4] = [57u8, 115u8, 161u8, 27u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                pool: values
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
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.pool))]);
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
                Vec<u8>,
                Vec<u8>,
                (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>),
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                [u8; 32usize],
                [u8; 32usize],
                [u8; 32usize],
                [u8; 32usize],
                [u8; 32usize],
                [u8; 32usize],
                substreams::scalar::BigInt,
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
                Vec<u8>,
                Vec<u8>,
                (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>),
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                [u8; 32usize],
                [u8; 32usize],
                [u8; 32usize],
                [u8; 32usize],
                [u8; 32usize],
                [u8; 32usize],
                substreams::scalar::BigInt,
            ),
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Address,
                    ]),
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::FixedBytes(32usize),
                    ethabi::ParamType::FixedBytes(32usize),
                    ethabi::ParamType::FixedBytes(32usize),
                    ethabi::ParamType::FixedBytes(32usize),
                    ethabi::ParamType::FixedBytes(32usize),
                    ethabi::ParamType::FixedBytes(32usize),
                    ethabi::ParamType::Uint(256usize),
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
                    tuple_elements[1usize]
                        .clone()
                        .into_address()
                        .expect(INTERNAL_ERR)
                        .as_bytes()
                        .to_vec(),
                    tuple_elements[2usize]
                        .clone()
                        .into_address()
                        .expect(INTERNAL_ERR)
                        .as_bytes()
                        .to_vec(),
                    {
                        let tuple_elements = tuple_elements[3usize]
                            .clone()
                            .into_tuple()
                            .expect(INTERNAL_ERR);
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
                                .into_address()
                                .expect(INTERNAL_ERR)
                                .as_bytes()
                                .to_vec(),
                            tuple_elements[3usize]
                                .clone()
                                .into_address()
                                .expect(INTERNAL_ERR)
                                .as_bytes()
                                .to_vec(),
                            tuple_elements[4usize]
                                .clone()
                                .into_address()
                                .expect(INTERNAL_ERR)
                                .as_bytes()
                                .to_vec(),
                        )
                    },
                    tuple_elements[4usize]
                        .clone()
                        .into_address()
                        .expect(INTERNAL_ERR)
                        .as_bytes()
                        .to_vec(),
                    tuple_elements[5usize]
                        .clone()
                        .into_address()
                        .expect(INTERNAL_ERR)
                        .as_bytes()
                        .to_vec(),
                    tuple_elements[6usize]
                        .clone()
                        .into_address()
                        .expect(INTERNAL_ERR)
                        .as_bytes()
                        .to_vec(),
                    {
                        let mut result = [0u8; 32];
                        let v = tuple_elements[7usize]
                            .clone()
                            .into_fixed_bytes()
                            .expect(INTERNAL_ERR);
                        result.copy_from_slice(&v);
                        result
                    },
                    {
                        let mut result = [0u8; 32];
                        let v = tuple_elements[8usize]
                            .clone()
                            .into_fixed_bytes()
                            .expect(INTERNAL_ERR);
                        result.copy_from_slice(&v);
                        result
                    },
                    {
                        let mut result = [0u8; 32];
                        let v = tuple_elements[9usize]
                            .clone()
                            .into_fixed_bytes()
                            .expect(INTERNAL_ERR);
                        result.copy_from_slice(&v);
                        result
                    },
                    {
                        let mut result = [0u8; 32];
                        let v = tuple_elements[10usize]
                            .clone()
                            .into_fixed_bytes()
                            .expect(INTERNAL_ERR);
                        result.copy_from_slice(&v);
                        result
                    },
                    {
                        let mut result = [0u8; 32];
                        let v = tuple_elements[11usize]
                            .clone()
                            .into_fixed_bytes()
                            .expect(INTERNAL_ERR);
                        result.copy_from_slice(&v);
                        result
                    },
                    {
                        let mut result = [0u8; 32];
                        let v = tuple_elements[12usize]
                            .clone()
                            .into_fixed_bytes()
                            .expect(INTERNAL_ERR);
                        result.copy_from_slice(&v);
                        result
                    },
                    {
                        let mut v = [0 as u8; 32];
                        tuple_elements[13usize]
                            .clone()
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    },
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
            Vec<u8>,
            Vec<u8>,
            (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>),
            Vec<u8>,
            Vec<u8>,
            Vec<u8>,
            [u8; 32usize],
            [u8; 32usize],
            [u8; 32usize],
            [u8; 32usize],
            [u8; 32usize],
            [u8; 32usize],
            substreams::scalar::BigInt,
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
    impl substreams_ethereum::Function for GetPoolConstantsView {
        const NAME: &'static str = "getPoolConstantsView";
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
            Vec<u8>,
            Vec<u8>,
            (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>),
            Vec<u8>,
            Vec<u8>,
            Vec<u8>,
            [u8; 32usize],
            [u8; 32usize],
            [u8; 32usize],
            [u8; 32usize],
            [u8; 32usize],
            [u8; 32usize],
            substreams::scalar::BigInt,
        )> for GetPoolConstantsView
    {
        fn output(
            data: &[u8],
        ) -> Result<
            (
                substreams::scalar::BigInt,
                Vec<u8>,
                Vec<u8>,
                (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>),
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                [u8; 32usize],
                [u8; 32usize],
                [u8; 32usize],
                [u8; 32usize],
                [u8; 32usize],
                [u8; 32usize],
                substreams::scalar::BigInt,
            ),
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetPoolConstantsView2 {
        pub pool: Vec<u8>,
    }
    impl GetPoolConstantsView2 {
        const METHOD_ID: [u8; 4] = [62u8, 200u8, 65u8, 228u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                pool: values
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
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.pool))]);
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
            ),
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
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
    impl substreams_ethereum::Function for GetPoolConstantsView2 {
        const NAME: &'static str = "getPoolConstantsView2";
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
        )> for GetPoolConstantsView2
    {
        fn output(
            data: &[u8],
        ) -> Result<
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetPoolFee {
        pub pool: Vec<u8>,
    }
    impl GetPoolFee {
        const METHOD_ID: [u8; 4] = [66u8, 252u8, 198u8, 251u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                pool: values
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
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.pool))]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<substreams::scalar::BigInt, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            let mut values = ethabi::decode(&[ethabi::ParamType::Uint(256usize)], data.as_ref())
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok({
                let mut v = [0 as u8; 32];
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_uint()
                    .expect(INTERNAL_ERR)
                    .to_big_endian(v.as_mut_slice());
                substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
            })
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<substreams::scalar::BigInt> {
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
    impl substreams_ethereum::Function for GetPoolFee {
        const NAME: &'static str = "getPoolFee";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for GetPoolFee {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetPoolReserves {
        pub pool: Vec<u8>,
    }
    impl GetPoolReserves {
        const METHOD_ID: [u8; 4] = [75u8, 238u8, 147u8, 149u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                pool: values
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
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.pool))]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<
            (
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            ),
            String,
        > {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<
            (
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            ),
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                    ]),
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
                        .into_address()
                        .expect(INTERNAL_ERR)
                        .as_bytes()
                        .to_vec(),
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
                    {
                        let tuple_elements = tuple_elements[5usize]
                            .clone()
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
                        )
                    },
                    {
                        let tuple_elements = tuple_elements[6usize]
                            .clone()
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
                            {
                                let mut v = [0 as u8; 32];
                                tuple_elements[4usize]
                                    .clone()
                                    .into_uint()
                                    .expect(INTERNAL_ERR)
                                    .to_big_endian(v.as_mut_slice());
                                substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                            },
                            {
                                let mut v = [0 as u8; 32];
                                tuple_elements[5usize]
                                    .clone()
                                    .into_uint()
                                    .expect(INTERNAL_ERR)
                                    .to_big_endian(v.as_mut_slice());
                                substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                            },
                        )
                    },
                    {
                        let tuple_elements = tuple_elements[7usize]
                            .clone()
                            .into_tuple()
                            .expect(INTERNAL_ERR);
                        (
                            {
                                let tuple_elements = tuple_elements[0usize]
                                    .clone()
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
                                )
                            },
                            {
                                let tuple_elements = tuple_elements[1usize]
                                    .clone()
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
                                )
                            },
                            {
                                let tuple_elements = tuple_elements[2usize]
                                    .clone()
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
                                )
                            },
                            {
                                let tuple_elements = tuple_elements[3usize]
                                    .clone()
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
                                )
                            },
                        )
                    },
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
            Vec<u8>,
            Vec<u8>,
            Vec<u8>,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            (
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
            ),
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
    impl substreams_ethereum::Function for GetPoolReserves {
        const NAME: &'static str = "getPoolReserves";
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
            Vec<u8>,
            Vec<u8>,
            Vec<u8>,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            (
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
            ),
        )> for GetPoolReserves
    {
        fn output(
            data: &[u8],
        ) -> Result<
            (
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            ),
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetPoolReservesAdjusted {
        pub pool: Vec<u8>,
    }
    impl GetPoolReservesAdjusted {
        const METHOD_ID: [u8; 4] = [189u8, 150u8, 77u8, 56u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                pool: values
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
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.pool))]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<
            (
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            ),
            String,
        > {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<
            (
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            ),
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                    ]),
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
                        .into_address()
                        .expect(INTERNAL_ERR)
                        .as_bytes()
                        .to_vec(),
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
                    {
                        let tuple_elements = tuple_elements[5usize]
                            .clone()
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
                        )
                    },
                    {
                        let tuple_elements = tuple_elements[6usize]
                            .clone()
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
                            {
                                let mut v = [0 as u8; 32];
                                tuple_elements[4usize]
                                    .clone()
                                    .into_uint()
                                    .expect(INTERNAL_ERR)
                                    .to_big_endian(v.as_mut_slice());
                                substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                            },
                            {
                                let mut v = [0 as u8; 32];
                                tuple_elements[5usize]
                                    .clone()
                                    .into_uint()
                                    .expect(INTERNAL_ERR)
                                    .to_big_endian(v.as_mut_slice());
                                substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                            },
                        )
                    },
                    {
                        let tuple_elements = tuple_elements[7usize]
                            .clone()
                            .into_tuple()
                            .expect(INTERNAL_ERR);
                        (
                            {
                                let tuple_elements = tuple_elements[0usize]
                                    .clone()
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
                                )
                            },
                            {
                                let tuple_elements = tuple_elements[1usize]
                                    .clone()
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
                                )
                            },
                            {
                                let tuple_elements = tuple_elements[2usize]
                                    .clone()
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
                                )
                            },
                            {
                                let tuple_elements = tuple_elements[3usize]
                                    .clone()
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
                                )
                            },
                        )
                    },
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
            Vec<u8>,
            Vec<u8>,
            Vec<u8>,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            (
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
            ),
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
    impl substreams_ethereum::Function for GetPoolReservesAdjusted {
        const NAME: &'static str = "getPoolReservesAdjusted";
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
            Vec<u8>,
            Vec<u8>,
            Vec<u8>,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            (
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            ),
            (
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
            ),
        )> for GetPoolReservesAdjusted
    {
        fn output(
            data: &[u8],
        ) -> Result<
            (
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            ),
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetPoolTokens {
        pub pool: Vec<u8>,
    }
    impl GetPoolTokens {
        const METHOD_ID: [u8; 4] = [202u8, 79u8, 40u8, 3u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(&[ethabi::ParamType::Address], maybe_data.unwrap())
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                pool: values
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
                ethabi::encode(&[ethabi::Token::Address(ethabi::Address::from_slice(&self.pool))]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<(Vec<u8>, Vec<u8>), String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Address, ethabi::ParamType::Address],
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
                values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            ))
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<(Vec<u8>, Vec<u8>)> {
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
    impl substreams_ethereum::Function for GetPoolTokens {
        const NAME: &'static str = "getPoolTokens";
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
    impl substreams_ethereum::rpc::RPCDecodable<(Vec<u8>, Vec<u8>)> for GetPoolTokens {
        fn output(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetPoolsReserves {
        pub pools: Vec<Vec<u8>>,
    }
    impl GetPoolsReserves {
        const METHOD_ID: [u8; 4] = [165u8, 151u8, 55u8, 99u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Array(Box::new(ethabi::ParamType::Address))],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                pools: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_array()
                    .expect(INTERNAL_ERR)
                    .into_iter()
                    .map(|inner| {
                        inner
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec()
                    })
                    .collect(),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[{
                let v = self
                    .pools
                    .iter()
                    .map(|inner| ethabi::Token::Address(ethabi::Address::from_slice(&inner)))
                    .collect();
                ethabi::Token::Array(v)
            }]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
            String,
        > {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                    ]),
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
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec(),
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
                        {
                            let tuple_elements = tuple_elements[5usize]
                                .clone()
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
                            )
                        },
                        {
                            let tuple_elements = tuple_elements[6usize]
                                .clone()
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
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[4usize]
                                        .clone()
                                        .into_uint()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[5usize]
                                        .clone()
                                        .into_uint()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                                },
                            )
                        },
                        {
                            let tuple_elements = tuple_elements[7usize]
                                .clone()
                                .into_tuple()
                                .expect(INTERNAL_ERR);
                            (
                                {
                                    let tuple_elements = tuple_elements[0usize]
                                        .clone()
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
                                    )
                                },
                                {
                                    let tuple_elements = tuple_elements[1usize]
                                        .clone()
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
                                    )
                                },
                                {
                                    let tuple_elements = tuple_elements[2usize]
                                        .clone()
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
                                    )
                                },
                                {
                                    let tuple_elements = tuple_elements[3usize]
                                        .clone()
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
                                    )
                                },
                            )
                        },
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
        pub fn call(
            &self,
            address: Vec<u8>,
        ) -> Option<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
        > {
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
    impl substreams_ethereum::Function for GetPoolsReserves {
        const NAME: &'static str = "getPoolsReserves";
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
        substreams_ethereum::rpc::RPCDecodable<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
        > for GetPoolsReserves
    {
        fn output(
            data: &[u8],
        ) -> Result<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetPoolsReservesAdjusted {
        pub pools: Vec<Vec<u8>>,
    }
    impl GetPoolsReservesAdjusted {
        const METHOD_ID: [u8; 4] = [252u8, 204u8, 160u8, 60u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Array(Box::new(ethabi::ParamType::Address))],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                pools: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_array()
                    .expect(INTERNAL_ERR)
                    .into_iter()
                    .map(|inner| {
                        inner
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec()
                    })
                    .collect(),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[{
                let v = self
                    .pools
                    .iter()
                    .map(|inner| ethabi::Token::Address(ethabi::Address::from_slice(&inner)))
                    .collect();
                ethabi::Token::Array(v)
            }]);
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
            String,
        > {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(
            data: &[u8],
        ) -> Result<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
            String,
        > {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]),
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                        ethabi::ParamType::Tuple(vec![
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                            ethabi::ParamType::Uint(256usize),
                        ]),
                    ]),
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
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec(),
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
                        {
                            let tuple_elements = tuple_elements[5usize]
                                .clone()
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
                            )
                        },
                        {
                            let tuple_elements = tuple_elements[6usize]
                                .clone()
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
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[4usize]
                                        .clone()
                                        .into_uint()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                                },
                                {
                                    let mut v = [0 as u8; 32];
                                    tuple_elements[5usize]
                                        .clone()
                                        .into_uint()
                                        .expect(INTERNAL_ERR)
                                        .to_big_endian(v.as_mut_slice());
                                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                                },
                            )
                        },
                        {
                            let tuple_elements = tuple_elements[7usize]
                                .clone()
                                .into_tuple()
                                .expect(INTERNAL_ERR);
                            (
                                {
                                    let tuple_elements = tuple_elements[0usize]
                                        .clone()
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
                                    )
                                },
                                {
                                    let tuple_elements = tuple_elements[1usize]
                                        .clone()
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
                                    )
                                },
                                {
                                    let tuple_elements = tuple_elements[2usize]
                                        .clone()
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
                                    )
                                },
                                {
                                    let tuple_elements = tuple_elements[3usize]
                                        .clone()
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
                                    )
                                },
                            )
                        },
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
        pub fn call(
            &self,
            address: Vec<u8>,
        ) -> Option<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
        > {
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
    impl substreams_ethereum::Function for GetPoolsReservesAdjusted {
        const NAME: &'static str = "getPoolsReservesAdjusted";
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
        substreams_ethereum::rpc::RPCDecodable<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
        > for GetPoolsReservesAdjusted
    {
        fn output(
            data: &[u8],
        ) -> Result<
            Vec<(
                Vec<u8>,
                Vec<u8>,
                Vec<u8>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                ),
                (
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                    (
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                        substreams::scalar::BigInt,
                    ),
                ),
            )>,
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetTotalPools {}
    impl GetTotalPools {
        const METHOD_ID: [u8; 4] = [211u8, 255u8, 230u8, 122u8];
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
        ) -> Result<substreams::scalar::BigInt, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            let mut values = ethabi::decode(&[ethabi::ParamType::Uint(256usize)], data.as_ref())
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok({
                let mut v = [0 as u8; 32];
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_uint()
                    .expect(INTERNAL_ERR)
                    .to_big_endian(v.as_mut_slice());
                substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
            })
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<substreams::scalar::BigInt> {
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
    impl substreams_ethereum::Function for GetTotalPools {
        const NAME: &'static str = "getTotalPools";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for GetTotalPools {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Liquidity {}
    impl Liquidity {
        const METHOD_ID: [u8; 4] = [40u8, 97u8, 199u8, 209u8];
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
    impl substreams_ethereum::Function for Liquidity {
        const NAME: &'static str = "LIQUIDITY";
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
    impl substreams_ethereum::rpc::RPCDecodable<Vec<u8>> for Liquidity {
        fn output(data: &[u8]) -> Result<Vec<u8>, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LiquidityResolver {}
    impl LiquidityResolver {
        const METHOD_ID: [u8; 4] = [105u8, 2u8, 247u8, 159u8];
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
    impl substreams_ethereum::Function for LiquidityResolver {
        const NAME: &'static str = "LIQUIDITY_RESOLVER";
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
    impl substreams_ethereum::rpc::RPCDecodable<Vec<u8>> for LiquidityResolver {
        fn output(data: &[u8]) -> Result<Vec<u8>, String> {
            Self::output(data)
        }
    }
}
/// Contract's events.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod events {
    use super::INTERNAL_ERR;
}
