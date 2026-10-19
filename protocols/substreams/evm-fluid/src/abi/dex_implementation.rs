const INTERNAL_ERR: &'static str = "`ethabi_derive` internal error";
/// Contract's functions.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod functions {
    use super::INTERNAL_ERR;
    #[derive(Debug, Clone, PartialEq)]
    pub struct Borrow {
        pub token0_amt: substreams::scalar::BigInt,
        pub token1_amt: substreams::scalar::BigInt,
        pub max_shares_amt: substreams::scalar::BigInt,
        pub to: Vec<u8>,
    }
    impl Borrow {
        const METHOD_ID: [u8; 4] = [36u8, 32u8, 17u8, 213u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Address,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                token0_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token1_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                max_shares_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                to: values
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.token0_amt.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.token1_amt.clone().to_bytes_be() {
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
                        .max_shares_amt
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
                ethabi::Token::Address(ethabi::Address::from_slice(&self.to)),
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
    impl substreams_ethereum::Function for Borrow {
        const NAME: &'static str = "borrow";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for Borrow {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct BorrowPerfect {
        pub shares: substreams::scalar::BigInt,
        pub min_token0_borrow: substreams::scalar::BigInt,
        pub min_token1_borrow: substreams::scalar::BigInt,
        pub to: Vec<u8>,
    }
    impl BorrowPerfect {
        const METHOD_ID: [u8; 4] = [226u8, 114u8, 3u8, 205u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Address,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                shares: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                min_token0_borrow: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                min_token1_borrow: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                to: values
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.shares.clone().to_bytes_be() {
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
                        .min_token0_borrow
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self
                        .min_token1_borrow
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
                ethabi::Token::Address(ethabi::Address::from_slice(&self.to)),
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
    impl substreams_ethereum::Function for BorrowPerfect {
        const NAME: &'static str = "borrowPerfect";
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
        )> for BorrowPerfect
    {
        fn output(
            data: &[u8],
        ) -> Result<(substreams::scalar::BigInt, substreams::scalar::BigInt), String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct ConstantsView {}
    impl ConstantsView {
        const METHOD_ID: [u8; 4] = [183u8, 121u8, 27u8, 242u8];
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
    impl substreams_ethereum::Function for ConstantsView {
        const NAME: &'static str = "constantsView";
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
        )> for ConstantsView
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
    pub struct ConstantsView2 {}
    impl ConstantsView2 {
        const METHOD_ID: [u8; 4] = [21u8, 149u8, 203u8, 211u8];
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
    impl substreams_ethereum::Function for ConstantsView2 {
        const NAME: &'static str = "constantsView2";
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
        )> for ConstantsView2
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
    pub struct Deposit {
        pub token0_amt: substreams::scalar::BigInt,
        pub token1_amt: substreams::scalar::BigInt,
        pub min_shares_amt: substreams::scalar::BigInt,
        pub estimate: bool,
    }
    impl Deposit {
        const METHOD_ID: [u8; 4] = [233u8, 128u8, 225u8, 235u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Bool,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                token0_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token1_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                min_shares_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                estimate: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.token0_amt.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.token1_amt.clone().to_bytes_be() {
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
                        .min_shares_amt
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
                ethabi::Token::Bool(self.estimate.clone()),
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
    impl substreams_ethereum::Function for Deposit {
        const NAME: &'static str = "deposit";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for Deposit {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct DepositPerfect {
        pub shares: substreams::scalar::BigInt,
        pub max_token0_deposit: substreams::scalar::BigInt,
        pub max_token1_deposit: substreams::scalar::BigInt,
        pub estimate: bool,
    }
    impl DepositPerfect {
        const METHOD_ID: [u8; 4] = [77u8, 144u8, 54u8, 222u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Bool,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                shares: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                max_token0_deposit: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                max_token1_deposit: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                estimate: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.shares.clone().to_bytes_be() {
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
                        .max_token0_deposit
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self
                        .max_token1_deposit
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
                ethabi::Token::Bool(self.estimate.clone()),
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
    impl substreams_ethereum::Function for DepositPerfect {
        const NAME: &'static str = "depositPerfect";
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
        )> for DepositPerfect
    {
        fn output(
            data: &[u8],
        ) -> Result<(substreams::scalar::BigInt, substreams::scalar::BigInt), String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct DexId {}
    impl DexId {
        const METHOD_ID: [u8; 4] = [244u8, 185u8, 163u8, 251u8];
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
    impl substreams_ethereum::Function for DexId {
        const NAME: &'static str = "DEX_ID";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for DexId {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetCollateralReserves {
        pub geometric_mean: substreams::scalar::BigInt,
        pub upper_range: substreams::scalar::BigInt,
        pub lower_range: substreams::scalar::BigInt,
        pub token0_supply_exchange_price: substreams::scalar::BigInt,
        pub token1_supply_exchange_price: substreams::scalar::BigInt,
    }
    impl GetCollateralReserves {
        const METHOD_ID: [u8; 4] = [101u8, 96u8, 171u8, 170u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                geometric_mean: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                upper_range: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                lower_range: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token0_supply_exchange_price: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token1_supply_exchange_price: {
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self
                        .geometric_mean
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.upper_range.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.lower_range.clone().to_bytes_be() {
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
                        .token0_supply_exchange_price
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self
                        .token1_supply_exchange_price
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
    impl substreams_ethereum::Function for GetCollateralReserves {
        const NAME: &'static str = "getCollateralReserves";
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
        )> for GetCollateralReserves
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
    pub struct GetDebtReserves {
        pub geometric_mean: substreams::scalar::BigInt,
        pub upper_range: substreams::scalar::BigInt,
        pub lower_range: substreams::scalar::BigInt,
        pub token0_borrow_exchange_price: substreams::scalar::BigInt,
        pub token1_borrow_exchange_price: substreams::scalar::BigInt,
    }
    impl GetDebtReserves {
        const METHOD_ID: [u8; 4] = [5u8, 212u8, 85u8, 169u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                geometric_mean: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                upper_range: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                lower_range: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token0_borrow_exchange_price: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token1_borrow_exchange_price: {
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self
                        .geometric_mean
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.upper_range.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.lower_range.clone().to_bytes_be() {
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
                        .token0_borrow_exchange_price
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self
                        .token1_borrow_exchange_price
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
    impl substreams_ethereum::Function for GetDebtReserves {
        const NAME: &'static str = "getDebtReserves";
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
        )> for GetDebtReserves
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
    pub struct GetPricesAndExchangePrices {}
    impl GetPricesAndExchangePrices {
        const METHOD_ID: [u8; 4] = [145u8, 108u8, 239u8, 78u8];
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
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
    }
    impl substreams_ethereum::Function for GetPricesAndExchangePrices {
        const NAME: &'static str = "getPricesAndExchangePrices";
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
    pub struct LiquidityCallback {
        pub token: Vec<u8>,
        pub amount: substreams::scalar::BigInt,
        pub data: Vec<u8>,
    }
    impl LiquidityCallback {
        const METHOD_ID: [u8; 4] = [173u8, 32u8, 117u8, 1u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Bytes,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                token: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                amount: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                data: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bytes()
                    .expect(INTERNAL_ERR),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Address(ethabi::Address::from_slice(&self.token)),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.amount.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Bytes(self.data.clone()),
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
    impl substreams_ethereum::Function for LiquidityCallback {
        const NAME: &'static str = "liquidityCallback";
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
    pub struct OraclePrice {
        pub seconds_agos: Vec<substreams::scalar::BigInt>,
    }
    impl OraclePrice {
        const METHOD_ID: [u8; 4] = [216u8, 17u8, 178u8, 206u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Array(Box::new(ethabi::ParamType::Uint(256usize)))],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                seconds_agos: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_array()
                    .expect(INTERNAL_ERR)
                    .into_iter()
                    .map(|inner| {
                        let mut v = [0 as u8; 32];
                        inner
                            .into_uint()
                            .expect(INTERNAL_ERR)
                            .to_big_endian(v.as_mut_slice());
                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                    })
                    .collect(),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[{
                let v = self
                    .seconds_agos
                    .iter()
                    .map(|inner| {
                        ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                            match inner.clone().to_bytes_be() {
                                (num_bigint::Sign::Plus, bytes) => bytes,
                                (num_bigint::Sign::NoSign, bytes) => bytes,
                                (num_bigint::Sign::Minus, _) => {
                                    panic!("negative numbers are not supported")
                                }
                            }
                            .as_slice(),
                        ))
                    })
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
            (
                Vec<(
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                )>,
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
                Vec<(
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                )>,
                substreams::scalar::BigInt,
            ),
            String,
        > {
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ]))),
                    ethabi::ParamType::Uint(256usize),
                ],
                data.as_ref(),
            )
            .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            values.reverse();
            Ok((
                values
                    .pop()
                    .expect(INTERNAL_ERR)
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
                    .collect(),
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
        ) -> Option<(
            Vec<(
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            )>,
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
    impl substreams_ethereum::Function for OraclePrice {
        const NAME: &'static str = "oraclePrice";
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
            Vec<(
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
            )>,
            substreams::scalar::BigInt,
        )> for OraclePrice
    {
        fn output(
            data: &[u8],
        ) -> Result<
            (
                Vec<(
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                    substreams::scalar::BigInt,
                )>,
                substreams::scalar::BigInt,
            ),
            String,
        > {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Payback {
        pub token0_amt: substreams::scalar::BigInt,
        pub token1_amt: substreams::scalar::BigInt,
        pub min_shares_amt: substreams::scalar::BigInt,
        pub estimate: bool,
    }
    impl Payback {
        const METHOD_ID: [u8; 4] = [104u8, 118u8, 105u8, 129u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Bool,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                token0_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token1_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                min_shares_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                estimate: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.token0_amt.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.token1_amt.clone().to_bytes_be() {
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
                        .min_shares_amt
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
                ethabi::Token::Bool(self.estimate.clone()),
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
    impl substreams_ethereum::Function for Payback {
        const NAME: &'static str = "payback";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for Payback {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct PaybackPerfect {
        pub shares: substreams::scalar::BigInt,
        pub max_token0_payback: substreams::scalar::BigInt,
        pub max_token1_payback: substreams::scalar::BigInt,
        pub estimate: bool,
    }
    impl PaybackPerfect {
        const METHOD_ID: [u8; 4] = [91u8, 61u8, 56u8, 215u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Bool,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                shares: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                max_token0_payback: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                max_token1_payback: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                estimate: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.shares.clone().to_bytes_be() {
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
                        .max_token0_payback
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self
                        .max_token1_payback
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
                ethabi::Token::Bool(self.estimate.clone()),
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
    impl substreams_ethereum::Function for PaybackPerfect {
        const NAME: &'static str = "paybackPerfect";
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
        )> for PaybackPerfect
    {
        fn output(
            data: &[u8],
        ) -> Result<(substreams::scalar::BigInt, substreams::scalar::BigInt), String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct PaybackPerfectInOneToken {
        pub shares: substreams::scalar::BigInt,
        pub max_token0: substreams::scalar::BigInt,
        pub max_token1: substreams::scalar::BigInt,
        pub estimate: bool,
    }
    impl PaybackPerfectInOneToken {
        const METHOD_ID: [u8; 4] = [48u8, 172u8, 214u8, 253u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Bool,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                shares: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                max_token0: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                max_token1: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                estimate: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.shares.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.max_token0.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.max_token1.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Bool(self.estimate.clone()),
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
    impl substreams_ethereum::Function for PaybackPerfectInOneToken {
        const NAME: &'static str = "paybackPerfectInOneToken";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt>
        for PaybackPerfectInOneToken
    {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct ReadFromStorage {
        pub slot: [u8; 32usize],
    }
    impl ReadFromStorage {
        const METHOD_ID: [u8; 4] = [181u8, 199u8, 54u8, 228u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values =
                ethabi::decode(&[ethabi::ParamType::FixedBytes(32usize)], maybe_data.unwrap())
                    .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                slot: {
                    let mut result = [0u8; 32];
                    let v = values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_fixed_bytes()
                        .expect(INTERNAL_ERR);
                    result.copy_from_slice(&v);
                    result
                },
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[ethabi::Token::FixedBytes(self.slot.as_ref().to_vec())]);
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
    impl substreams_ethereum::Function for ReadFromStorage {
        const NAME: &'static str = "readFromStorage";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for ReadFromStorage {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct SwapIn {
        pub swap0to1: bool,
        pub amount_in: substreams::scalar::BigInt,
        pub amount_out_min: substreams::scalar::BigInt,
        pub to: Vec<u8>,
    }
    impl SwapIn {
        const METHOD_ID: [u8; 4] = [38u8, 104u8, 223u8, 170u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Bool,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Address,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
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
                to: values
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
                ethabi::Token::Address(ethabi::Address::from_slice(&self.to)),
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
    impl substreams_ethereum::Function for SwapIn {
        const NAME: &'static str = "swapIn";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for SwapIn {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct SwapInWithCallback {
        pub swap0to1: bool,
        pub amount_in: substreams::scalar::BigInt,
        pub amount_out_min: substreams::scalar::BigInt,
        pub to: Vec<u8>,
    }
    impl SwapInWithCallback {
        const METHOD_ID: [u8; 4] = [190u8, 23u8, 199u8, 156u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Bool,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Address,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
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
                to: values
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
                ethabi::Token::Address(ethabi::Address::from_slice(&self.to)),
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
    impl substreams_ethereum::Function for SwapInWithCallback {
        const NAME: &'static str = "swapInWithCallback";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for SwapInWithCallback {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct SwapOut {
        pub swap0to1: bool,
        pub amount_out: substreams::scalar::BigInt,
        pub amount_in_max: substreams::scalar::BigInt,
        pub to: Vec<u8>,
    }
    impl SwapOut {
        const METHOD_ID: [u8; 4] = [40u8, 111u8, 14u8, 97u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Bool,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Address,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
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
                to: values
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
                ethabi::Token::Address(ethabi::Address::from_slice(&self.to)),
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
    impl substreams_ethereum::Function for SwapOut {
        const NAME: &'static str = "swapOut";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for SwapOut {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct SwapOutWithCallback {
        pub swap0to1: bool,
        pub amount_out: substreams::scalar::BigInt,
        pub amount_in_max: substreams::scalar::BigInt,
        pub to: Vec<u8>,
    }
    impl SwapOutWithCallback {
        const METHOD_ID: [u8; 4] = [101u8, 50u8, 149u8, 170u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Bool,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Address,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
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
                to: values
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
                ethabi::Token::Address(ethabi::Address::from_slice(&self.to)),
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
    impl substreams_ethereum::Function for SwapOutWithCallback {
        const NAME: &'static str = "swapOutWithCallback";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for SwapOutWithCallback {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Withdraw {
        pub token0_amt: substreams::scalar::BigInt,
        pub token1_amt: substreams::scalar::BigInt,
        pub max_shares_amt: substreams::scalar::BigInt,
        pub to: Vec<u8>,
    }
    impl Withdraw {
        const METHOD_ID: [u8; 4] = [211u8, 49u8, 190u8, 247u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Address,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                token0_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token1_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                max_shares_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                to: values
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.token0_amt.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.token1_amt.clone().to_bytes_be() {
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
                        .max_shares_amt
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
                ethabi::Token::Address(ethabi::Address::from_slice(&self.to)),
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
    impl substreams_ethereum::Function for Withdraw {
        const NAME: &'static str = "withdraw";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt> for Withdraw {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct WithdrawPerfect {
        pub shares: substreams::scalar::BigInt,
        pub min_token0_withdraw: substreams::scalar::BigInt,
        pub min_token1_withdraw: substreams::scalar::BigInt,
        pub to: Vec<u8>,
    }
    impl WithdrawPerfect {
        const METHOD_ID: [u8; 4] = [53u8, 240u8, 223u8, 152u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Address,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                shares: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                min_token0_withdraw: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                min_token1_withdraw: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                to: values
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.shares.clone().to_bytes_be() {
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
                        .min_token0_withdraw
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self
                        .min_token1_withdraw
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
                ethabi::Token::Address(ethabi::Address::from_slice(&self.to)),
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
    impl substreams_ethereum::Function for WithdrawPerfect {
        const NAME: &'static str = "withdrawPerfect";
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
        )> for WithdrawPerfect
    {
        fn output(
            data: &[u8],
        ) -> Result<(substreams::scalar::BigInt, substreams::scalar::BigInt), String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct WithdrawPerfectInOneToken {
        pub shares: substreams::scalar::BigInt,
        pub min_token0: substreams::scalar::BigInt,
        pub min_token1: substreams::scalar::BigInt,
        pub to: Vec<u8>,
    }
    impl WithdrawPerfectInOneToken {
        const METHOD_ID: [u8; 4] = [76u8, 137u8, 191u8, 212u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Address,
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                shares: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                min_token0: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                min_token1: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                to: values
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
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.shares.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.min_token0.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.min_token1.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Address(ethabi::Address::from_slice(&self.to)),
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
    impl substreams_ethereum::Function for WithdrawPerfectInOneToken {
        const NAME: &'static str = "withdrawPerfectInOneToken";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt>
        for WithdrawPerfectInOneToken
    {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
}
/// Contract's events.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod events {
    use super::INTERNAL_ERR;
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogArbitrage {
        pub routing: substreams::scalar::BigInt,
        pub amt_out: substreams::scalar::BigInt,
    }
    impl LogArbitrage {
        const TOPIC_ID: [u8; 32] = [
            6u8, 61u8, 239u8, 3u8, 212u8, 26u8, 41u8, 87u8, 212u8, 49u8, 86u8, 185u8, 124u8, 39u8,
            31u8, 62u8, 74u8, 222u8, 166u8, 0u8, 114u8, 45u8, 239u8, 178u8, 207u8, 110u8, 191u8,
            154u8, 39u8, 101u8, 0u8, 86u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
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
                &[ethabi::ParamType::Int(256usize), ethabi::ParamType::Uint(256usize)],
                log.data.as_ref(),
            )
            .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                routing: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_int()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_signed_bytes_be(&v)
                },
                amt_out: {
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
    impl substreams_ethereum::Event for LogArbitrage {
        const NAME: &'static str = "LogArbitrage";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogBorrowDebtLiquidity {
        pub amount0: substreams::scalar::BigInt,
        pub amount1: substreams::scalar::BigInt,
        pub shares: substreams::scalar::BigInt,
    }
    impl LogBorrowDebtLiquidity {
        const TOPIC_ID: [u8; 32] = [
            127u8, 129u8, 66u8, 123u8, 237u8, 105u8, 157u8, 199u8, 230u8, 135u8, 197u8, 221u8,
            174u8, 96u8, 97u8, 147u8, 41u8, 56u8, 129u8, 143u8, 121u8, 252u8, 14u8, 104u8, 144u8,
            61u8, 85u8, 239u8, 117u8, 202u8, 69u8, 97u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 96usize {
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
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                log.data.as_ref(),
            )
            .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                amount0: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                amount1: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                shares: {
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
    impl substreams_ethereum::Event for LogBorrowDebtLiquidity {
        const NAME: &'static str = "LogBorrowDebtLiquidity";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogBorrowPerfectDebtLiquidity {
        pub shares: substreams::scalar::BigInt,
        pub token0_amt: substreams::scalar::BigInt,
        pub token1_amt: substreams::scalar::BigInt,
    }
    impl LogBorrowPerfectDebtLiquidity {
        const TOPIC_ID: [u8; 32] = [
            72u8, 109u8, 153u8, 25u8, 71u8, 168u8, 133u8, 128u8, 19u8, 15u8, 245u8, 172u8, 217u8,
            236u8, 84u8, 220u8, 55u8, 251u8, 141u8, 164u8, 191u8, 106u8, 183u8, 136u8, 113u8,
            213u8, 205u8, 111u8, 165u8, 129u8, 109u8, 247u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 96usize {
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
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                log.data.as_ref(),
            )
            .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                shares: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token0_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token1_amt: {
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
    impl substreams_ethereum::Event for LogBorrowPerfectDebtLiquidity {
        const NAME: &'static str = "LogBorrowPerfectDebtLiquidity";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogDepositColLiquidity {
        pub amount0: substreams::scalar::BigInt,
        pub amount1: substreams::scalar::BigInt,
        pub shares: substreams::scalar::BigInt,
    }
    impl LogDepositColLiquidity {
        const TOPIC_ID: [u8; 32] = [
            191u8, 234u8, 146u8, 9u8, 122u8, 36u8, 135u8, 214u8, 165u8, 204u8, 247u8, 183u8, 173u8,
            195u8, 107u8, 96u8, 2u8, 35u8, 143u8, 49u8, 6u8, 86u8, 139u8, 164u8, 53u8, 151u8,
            112u8, 244u8, 182u8, 115u8, 101u8, 164u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 96usize {
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
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                log.data.as_ref(),
            )
            .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                amount0: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                amount1: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                shares: {
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
    impl substreams_ethereum::Event for LogDepositColLiquidity {
        const NAME: &'static str = "LogDepositColLiquidity";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogDepositPerfectColLiquidity {
        pub shares: substreams::scalar::BigInt,
        pub token0_amt: substreams::scalar::BigInt,
        pub token1_amt: substreams::scalar::BigInt,
    }
    impl LogDepositPerfectColLiquidity {
        const TOPIC_ID: [u8; 32] = [
            37u8, 86u8, 114u8, 239u8, 250u8, 61u8, 139u8, 164u8, 110u8, 64u8, 159u8, 201u8, 100u8,
            174u8, 51u8, 43u8, 132u8, 211u8, 16u8, 123u8, 163u8, 165u8, 9u8, 107u8, 34u8, 115u8,
            70u8, 6u8, 81u8, 149u8, 40u8, 163u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 96usize {
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
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                log.data.as_ref(),
            )
            .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                shares: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token0_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token1_amt: {
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
    impl substreams_ethereum::Event for LogDepositPerfectColLiquidity {
        const NAME: &'static str = "LogDepositPerfectColLiquidity";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogPaybackDebtInOneToken {
        pub shares: substreams::scalar::BigInt,
        pub token0_amt: substreams::scalar::BigInt,
        pub token1_amt: substreams::scalar::BigInt,
    }
    impl LogPaybackDebtInOneToken {
        const TOPIC_ID: [u8; 32] = [
            151u8, 223u8, 168u8, 76u8, 191u8, 252u8, 246u8, 91u8, 141u8, 3u8, 79u8, 5u8, 116u8,
            57u8, 71u8, 43u8, 201u8, 56u8, 104u8, 166u8, 108u8, 192u8, 231u8, 40u8, 194u8, 250u8,
            255u8, 176u8, 15u8, 139u8, 73u8, 35u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 96usize {
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
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                log.data.as_ref(),
            )
            .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                shares: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token0_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token1_amt: {
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
    impl substreams_ethereum::Event for LogPaybackDebtInOneToken {
        const NAME: &'static str = "LogPaybackDebtInOneToken";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogPaybackDebtLiquidity {
        pub amount0: substreams::scalar::BigInt,
        pub amount1: substreams::scalar::BigInt,
        pub shares: substreams::scalar::BigInt,
    }
    impl LogPaybackDebtLiquidity {
        const TOPIC_ID: [u8; 32] = [
            182u8, 159u8, 21u8, 42u8, 112u8, 82u8, 7u8, 3u8, 254u8, 122u8, 180u8, 135u8, 42u8,
            12u8, 179u8, 146u8, 131u8, 134u8, 182u8, 140u8, 243u8, 198u8, 200u8, 60u8, 93u8, 31u8,
            192u8, 141u8, 25u8, 105u8, 145u8, 232u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 96usize {
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
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                log.data.as_ref(),
            )
            .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                amount0: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                amount1: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                shares: {
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
    impl substreams_ethereum::Event for LogPaybackDebtLiquidity {
        const NAME: &'static str = "LogPaybackDebtLiquidity";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogPaybackPerfectDebtLiquidity {
        pub shares: substreams::scalar::BigInt,
        pub token0_amt: substreams::scalar::BigInt,
        pub token1_amt: substreams::scalar::BigInt,
    }
    impl LogPaybackPerfectDebtLiquidity {
        const TOPIC_ID: [u8; 32] = [
            3u8, 183u8, 123u8, 68u8, 194u8, 254u8, 136u8, 22u8, 213u8, 90u8, 46u8, 79u8, 144u8,
            168u8, 117u8, 56u8, 196u8, 141u8, 65u8, 72u8, 227u8, 82u8, 36u8, 192u8, 112u8, 73u8,
            253u8, 35u8, 4u8, 250u8, 58u8, 48u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 96usize {
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
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                log.data.as_ref(),
            )
            .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                shares: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token0_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token1_amt: {
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
    impl substreams_ethereum::Event for LogPaybackPerfectDebtLiquidity {
        const NAME: &'static str = "LogPaybackPerfectDebtLiquidity";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogWithdrawColInOneToken {
        pub shares: substreams::scalar::BigInt,
        pub token0_amt: substreams::scalar::BigInt,
        pub token1_amt: substreams::scalar::BigInt,
    }
    impl LogWithdrawColInOneToken {
        const TOPIC_ID: [u8; 32] = [
            201u8, 143u8, 55u8, 145u8, 78u8, 6u8, 219u8, 54u8, 193u8, 134u8, 84u8, 72u8, 77u8,
            184u8, 92u8, 75u8, 184u8, 100u8, 87u8, 90u8, 27u8, 159u8, 129u8, 129u8, 19u8, 63u8,
            243u8, 61u8, 234u8, 45u8, 52u8, 243u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 96usize {
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
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                log.data.as_ref(),
            )
            .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                shares: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token0_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token1_amt: {
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
    impl substreams_ethereum::Event for LogWithdrawColInOneToken {
        const NAME: &'static str = "LogWithdrawColInOneToken";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogWithdrawColLiquidity {
        pub amount0: substreams::scalar::BigInt,
        pub amount1: substreams::scalar::BigInt,
        pub shares: substreams::scalar::BigInt,
    }
    impl LogWithdrawColLiquidity {
        const TOPIC_ID: [u8; 32] = [
            182u8, 28u8, 127u8, 59u8, 35u8, 254u8, 147u8, 53u8, 204u8, 108u8, 106u8, 110u8, 112u8,
            54u8, 69u8, 119u8, 88u8, 71u8, 8u8, 119u8, 230u8, 26u8, 25u8, 165u8, 180u8, 146u8,
            78u8, 31u8, 248u8, 40u8, 150u8, 36u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 96usize {
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
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                log.data.as_ref(),
            )
            .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                amount0: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                amount1: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                shares: {
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
    impl substreams_ethereum::Event for LogWithdrawColLiquidity {
        const NAME: &'static str = "LogWithdrawColLiquidity";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogWithdrawPerfectColLiquidity {
        pub shares: substreams::scalar::BigInt,
        pub token0_amt: substreams::scalar::BigInt,
        pub token1_amt: substreams::scalar::BigInt,
    }
    impl LogWithdrawPerfectColLiquidity {
        const TOPIC_ID: [u8; 32] = [
            111u8, 131u8, 117u8, 114u8, 193u8, 239u8, 110u8, 1u8, 10u8, 132u8, 31u8, 249u8, 56u8,
            213u8, 147u8, 236u8, 5u8, 73u8, 132u8, 254u8, 254u8, 41u8, 223u8, 42u8, 6u8, 52u8,
            187u8, 240u8, 31u8, 77u8, 179u8, 91u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 96usize {
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
                &[
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                ],
                log.data.as_ref(),
            )
            .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                shares: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token0_amt: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                token1_amt: {
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
    impl substreams_ethereum::Event for LogWithdrawPerfectColLiquidity {
        const NAME: &'static str = "LogWithdrawPerfectColLiquidity";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct Swap {
        pub swap0to1: bool,
        pub amount_in: substreams::scalar::BigInt,
        pub amount_out: substreams::scalar::BigInt,
        pub to: Vec<u8>,
    }
    impl Swap {
        const TOPIC_ID: [u8; 32] = [
            220u8, 0u8, 77u8, 188u8, 164u8, 239u8, 156u8, 150u8, 98u8, 24u8, 67u8, 30u8, 229u8,
            217u8, 19u8, 61u8, 51u8, 122u8, 208u8, 24u8, 221u8, 91u8, 92u8, 84u8, 147u8, 114u8,
            40u8, 3u8, 247u8, 92u8, 100u8, 247u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
                return false;
            }
            if log.data.len() != 128usize {
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
                &[
                    ethabi::ParamType::Bool,
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Uint(256usize),
                    ethabi::ParamType::Address,
                ],
                log.data.as_ref(),
            )
            .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
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
                to: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            })
        }
    }
    impl substreams_ethereum::Event for Swap {
        const NAME: &'static str = "Swap";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
}
