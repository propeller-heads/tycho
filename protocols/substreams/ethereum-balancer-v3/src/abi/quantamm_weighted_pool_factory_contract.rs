const INTERNAL_ERR: &'static str = "`ethabi_derive` internal error";
/// Contract's functions.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod functions {
    use super::INTERNAL_ERR;
    #[derive(Clone)]
    pub struct Create {
        pub params: (
            String,
            String,
            Vec<(Vec<u8>, substreams::scalar::BigInt, Vec<u8>, bool)>,
            Vec<substreams::scalar::BigInt>,
            (Vec<u8>, Vec<u8>, Vec<u8>),
            substreams::scalar::BigInt,
            Vec<u8>,
            bool,
            bool,
            [u8; 32usize],
            Vec<substreams::scalar::BigInt>,
            (
                Vec<Vec<u8>>,
                Vec<u8>,
                Vec<Vec<Vec<u8>>>,
                substreams::scalar::BigInt,
                Vec<substreams::scalar::BigInt>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                Vec<Vec<substreams::scalar::BigInt>>,
                Vec<u8>,
            ),
            Vec<substreams::scalar::BigInt>,
            Vec<substreams::scalar::BigInt>,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            Vec<Vec<String>>,
        ),
    }
    impl Create {
        const METHOD_ID: [u8; 4] = [232u8, 62u8, 150u8, 218u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                    &[
                        ethabi::ParamType::Tuple(
                            vec![
                                ethabi::ParamType::String, ethabi::ParamType::String,
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![ethabi::ParamType::Address,
                                ethabi::ParamType::Uint(8usize), ethabi::ParamType::Address,
                                ethabi::ParamType::Bool]))),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Uint(256usize))),
                                ethabi::ParamType::Tuple(vec![ethabi::ParamType::Address,
                                ethabi::ParamType::Address, ethabi::ParamType::Address]),
                                ethabi::ParamType::Uint(256usize),
                                ethabi::ParamType::Address, ethabi::ParamType::Bool,
                                ethabi::ParamType::Bool,
                                ethabi::ParamType::FixedBytes(32usize),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Int(256usize))),
                                ethabi::ParamType::Tuple(vec![ethabi::ParamType::Array(Box::new(ethabi::ParamType::Address)),
                                ethabi::ParamType::Address,
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Array(Box::new(ethabi::ParamType::Address)))),
                                ethabi::ParamType::Uint(40usize),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Uint(64usize))),
                                ethabi::ParamType::Uint(64usize),
                                ethabi::ParamType::Uint(64usize),
                                ethabi::ParamType::Uint(64usize),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Array(Box::new(ethabi::ParamType::Int(256usize))))),
                                ethabi::ParamType::Address]),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Int(256usize))),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Int(256usize))),
                                ethabi::ParamType::Uint(256usize),
                                ethabi::ParamType::Uint(256usize),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Array(Box::new(ethabi::ParamType::String))))
                            ],
                        ),
                    ],
                    maybe_data.unwrap(),
                )
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                params: {
                    let tuple_elements = values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_tuple()
                        .expect(INTERNAL_ERR);
                    (
                        tuple_elements[0usize]
                            .clone()
                            .into_string()
                            .expect(INTERNAL_ERR),
                        tuple_elements[1usize]
                            .clone()
                            .into_string()
                            .expect(INTERNAL_ERR),
                        tuple_elements[2usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                let tuple_elements = inner
                                    .into_tuple()
                                    .expect(INTERNAL_ERR);
                                (
                                    tuple_elements[0usize]
                                        .clone()
                                        .into_address()
                                        .expect(INTERNAL_ERR)
                                        .as_bytes()
                                        .to_vec(),
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[1usize]
                                            .clone()
                                            .into_uint()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                                    },
                                    tuple_elements[2usize]
                                        .clone()
                                        .into_address()
                                        .expect(INTERNAL_ERR)
                                        .as_bytes()
                                        .to_vec(),
                                    tuple_elements[3usize]
                                        .clone()
                                        .into_bool()
                                        .expect(INTERNAL_ERR),
                                )
                            })
                            .collect(),
                        tuple_elements[3usize]
                            .clone()
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
                        {
                            let tuple_elements = tuple_elements[4usize]
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
                            )
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
                        tuple_elements[6usize]
                            .clone()
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec(),
                        tuple_elements[7usize].clone().into_bool().expect(INTERNAL_ERR),
                        tuple_elements[8usize].clone().into_bool().expect(INTERNAL_ERR),
                        {
                            let mut result = [0u8; 32];
                            let v = tuple_elements[9usize]
                                .clone()
                                .into_fixed_bytes()
                                .expect(INTERNAL_ERR);
                            result.copy_from_slice(&v);
                            result
                        },
                        tuple_elements[10usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                let mut v = [0 as u8; 32];
                                inner
                                    .into_int()
                                    .expect(INTERNAL_ERR)
                                    .to_big_endian(v.as_mut_slice());
                                substreams::scalar::BigInt::from_signed_bytes_be(&v)
                            })
                            .collect(),
                        {
                            let tuple_elements = tuple_elements[11usize]
                                .clone()
                                .into_tuple()
                                .expect(INTERNAL_ERR);
                            (
                                tuple_elements[0usize]
                                    .clone()
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
                                tuple_elements[1usize]
                                    .clone()
                                    .into_address()
                                    .expect(INTERNAL_ERR)
                                    .as_bytes()
                                    .to_vec(),
                                tuple_elements[2usize]
                                    .clone()
                                    .into_array()
                                    .expect(INTERNAL_ERR)
                                    .into_iter()
                                    .map(|inner| {
                                        inner
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
                                            .collect()
                                    })
                                    .collect(),
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
                                        let mut v = [0 as u8; 32];
                                        inner
                                            .into_uint()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                                    })
                                    .collect(),
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
                                tuple_elements[8usize]
                                    .clone()
                                    .into_array()
                                    .expect(INTERNAL_ERR)
                                    .into_iter()
                                    .map(|inner| {
                                        inner
                                            .into_array()
                                            .expect(INTERNAL_ERR)
                                            .into_iter()
                                            .map(|inner| {
                                                let mut v = [0 as u8; 32];
                                                inner
                                                    .into_int()
                                                    .expect(INTERNAL_ERR)
                                                    .to_big_endian(v.as_mut_slice());
                                                substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                            })
                                            .collect()
                                    })
                                    .collect(),
                                tuple_elements[9usize]
                                    .clone()
                                    .into_address()
                                    .expect(INTERNAL_ERR)
                                    .as_bytes()
                                    .to_vec(),
                            )
                        },
                        tuple_elements[12usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                let mut v = [0 as u8; 32];
                                inner
                                    .into_int()
                                    .expect(INTERNAL_ERR)
                                    .to_big_endian(v.as_mut_slice());
                                substreams::scalar::BigInt::from_signed_bytes_be(&v)
                            })
                            .collect(),
                        tuple_elements[13usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                let mut v = [0 as u8; 32];
                                inner
                                    .into_int()
                                    .expect(INTERNAL_ERR)
                                    .to_big_endian(v.as_mut_slice());
                                substreams::scalar::BigInt::from_signed_bytes_be(&v)
                            })
                            .collect(),
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[14usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[15usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                        tuple_elements[16usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                inner
                                    .into_array()
                                    .expect(INTERNAL_ERR)
                                    .into_iter()
                                    .map(|inner| inner.into_string().expect(INTERNAL_ERR))
                                    .collect()
                            })
                            .collect(),
                    )
                },
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(
                &[
                    ethabi::Token::Tuple(
                        vec![
                            ethabi::Token::String(self.params.0.clone()),
                            ethabi::Token::String(self.params.1.clone()), { let v = self
                            .params.2.iter().map(| inner |
                            ethabi::Token::Tuple(vec![ethabi::Token::Address(ethabi::Address::from_slice(&
                            inner.0)),
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match inner
                            .1.clone().to_bytes_be() { (num_bigint::Sign::Plus, bytes) =>
                            bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),),
                            ethabi::Token::Address(ethabi::Address::from_slice(& inner
                            .2)), ethabi::Token::Bool(inner.3.clone())])).collect();
                            ethabi::Token::Array(v) }, { let v = self.params.3.iter()
                            .map(| inner |
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match inner
                            .clone().to_bytes_be() { (num_bigint::Sign::Plus, bytes) =>
                            bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),)).collect(); ethabi::Token::Array(v) },
                            ethabi::Token::Tuple(vec![ethabi::Token::Address(ethabi::Address::from_slice(&
                            self.params.4.0)),
                            ethabi::Token::Address(ethabi::Address::from_slice(& self
                            .params.4.1)),
                            ethabi::Token::Address(ethabi::Address::from_slice(& self
                            .params.4.2))]),
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.5.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),),
                            ethabi::Token::Address(ethabi::Address::from_slice(& self
                            .params.6)), ethabi::Token::Bool(self.params.7.clone()),
                            ethabi::Token::Bool(self.params.8.clone()),
                            ethabi::Token::FixedBytes(self.params.9.as_ref().to_vec()), {
                            let v = self.params.10.iter().map(| inner | { let
                            non_full_signed_bytes = inner.to_signed_bytes_be(); let
                            full_signed_bytes_init = if non_full_signed_bytes[0] & 0x80
                            == 0x80 { 0xff } else { 0x00 }; let mut full_signed_bytes =
                            [full_signed_bytes_init as u8; 32]; non_full_signed_bytes
                            .into_iter().rev().enumerate().for_each(| (i, byte) |
                            full_signed_bytes[31 - i] = byte);
                            ethabi::Token::Int(ethabi::Int::from_big_endian(full_signed_bytes
                            .as_ref())) }).collect(); ethabi::Token::Array(v) },
                            ethabi::Token::Tuple(vec![{ let v = self.params.11.0.iter()
                            .map(| inner |
                            ethabi::Token::Address(ethabi::Address::from_slice(& inner)))
                            .collect(); ethabi::Token::Array(v) },
                            ethabi::Token::Address(ethabi::Address::from_slice(& self
                            .params.11.1)), { let v = self.params.11.2.iter().map(| inner
                            | { let v = inner.iter().map(| inner |
                            ethabi::Token::Address(ethabi::Address::from_slice(& inner)))
                            .collect(); ethabi::Token::Array(v) }).collect();
                            ethabi::Token::Array(v) },
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.11.3.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),), { let v = self.params.11.4.iter().map(|
                            inner |
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match inner
                            .clone().to_bytes_be() { (num_bigint::Sign::Plus, bytes) =>
                            bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),)).collect(); ethabi::Token::Array(v) },
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.11.5.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),),
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.11.6.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),),
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.11.7.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),), { let v = self.params.11.8.iter().map(|
                            inner | { let v = inner.iter().map(| inner | { let
                            non_full_signed_bytes = inner.to_signed_bytes_be(); let
                            full_signed_bytes_init = if non_full_signed_bytes[0] & 0x80
                            == 0x80 { 0xff } else { 0x00 }; let mut full_signed_bytes =
                            [full_signed_bytes_init as u8; 32]; non_full_signed_bytes
                            .into_iter().rev().enumerate().for_each(| (i, byte) |
                            full_signed_bytes[31 - i] = byte);
                            ethabi::Token::Int(ethabi::Int::from_big_endian(full_signed_bytes
                            .as_ref())) }).collect(); ethabi::Token::Array(v) })
                            .collect(); ethabi::Token::Array(v) },
                            ethabi::Token::Address(ethabi::Address::from_slice(& self
                            .params.11.9))]), { let v = self.params.12.iter().map(| inner
                            | { let non_full_signed_bytes = inner.to_signed_bytes_be();
                            let full_signed_bytes_init = if non_full_signed_bytes[0] &
                            0x80 == 0x80 { 0xff } else { 0x00 }; let mut
                            full_signed_bytes = [full_signed_bytes_init as u8; 32];
                            non_full_signed_bytes.into_iter().rev().enumerate()
                            .for_each(| (i, byte) | full_signed_bytes[31 - i] = byte);
                            ethabi::Token::Int(ethabi::Int::from_big_endian(full_signed_bytes
                            .as_ref())) }).collect(); ethabi::Token::Array(v) }, { let v
                            = self.params.13.iter().map(| inner | { let
                            non_full_signed_bytes = inner.to_signed_bytes_be(); let
                            full_signed_bytes_init = if non_full_signed_bytes[0] & 0x80
                            == 0x80 { 0xff } else { 0x00 }; let mut full_signed_bytes =
                            [full_signed_bytes_init as u8; 32]; non_full_signed_bytes
                            .into_iter().rev().enumerate().for_each(| (i, byte) |
                            full_signed_bytes[31 - i] = byte);
                            ethabi::Token::Int(ethabi::Int::from_big_endian(full_signed_bytes
                            .as_ref())) }).collect(); ethabi::Token::Array(v) },
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.14.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),),
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.15.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),), { let v = self.params.16.iter().map(| inner
                            | { let v = inner.iter().map(| inner |
                            ethabi::Token::String(inner.clone())).collect();
                            ethabi::Token::Array(v) }).collect(); ethabi::Token::Array(v)
                            }
                        ],
                    ),
                ],
            );
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
                    &[ethabi::ParamType::Address, ethabi::ParamType::Bytes],
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
                values.pop().expect(INTERNAL_ERR).into_bytes().expect(INTERNAL_ERR),
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
    impl substreams_ethereum::Function for Create {
        const NAME: &'static str = "create";
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
    impl substreams_ethereum::rpc::RPCDecodable<(Vec<u8>, Vec<u8>)> for Create {
        fn output(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct CreateWithoutArgs {
        pub params: (
            String,
            String,
            Vec<(Vec<u8>, substreams::scalar::BigInt, Vec<u8>, bool)>,
            Vec<substreams::scalar::BigInt>,
            (Vec<u8>, Vec<u8>, Vec<u8>),
            substreams::scalar::BigInt,
            Vec<u8>,
            bool,
            bool,
            [u8; 32usize],
            Vec<substreams::scalar::BigInt>,
            (
                Vec<Vec<u8>>,
                Vec<u8>,
                Vec<Vec<Vec<u8>>>,
                substreams::scalar::BigInt,
                Vec<substreams::scalar::BigInt>,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                substreams::scalar::BigInt,
                Vec<Vec<substreams::scalar::BigInt>>,
                Vec<u8>,
            ),
            Vec<substreams::scalar::BigInt>,
            Vec<substreams::scalar::BigInt>,
            substreams::scalar::BigInt,
            substreams::scalar::BigInt,
            Vec<Vec<String>>,
        ),
    }
    impl CreateWithoutArgs {
        const METHOD_ID: [u8; 4] = [236u8, 126u8, 111u8, 94u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                    &[
                        ethabi::ParamType::Tuple(
                            vec![
                                ethabi::ParamType::String, ethabi::ParamType::String,
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![ethabi::ParamType::Address,
                                ethabi::ParamType::Uint(8usize), ethabi::ParamType::Address,
                                ethabi::ParamType::Bool]))),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Uint(256usize))),
                                ethabi::ParamType::Tuple(vec![ethabi::ParamType::Address,
                                ethabi::ParamType::Address, ethabi::ParamType::Address]),
                                ethabi::ParamType::Uint(256usize),
                                ethabi::ParamType::Address, ethabi::ParamType::Bool,
                                ethabi::ParamType::Bool,
                                ethabi::ParamType::FixedBytes(32usize),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Int(256usize))),
                                ethabi::ParamType::Tuple(vec![ethabi::ParamType::Array(Box::new(ethabi::ParamType::Address)),
                                ethabi::ParamType::Address,
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Array(Box::new(ethabi::ParamType::Address)))),
                                ethabi::ParamType::Uint(40usize),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Uint(64usize))),
                                ethabi::ParamType::Uint(64usize),
                                ethabi::ParamType::Uint(64usize),
                                ethabi::ParamType::Uint(64usize),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Array(Box::new(ethabi::ParamType::Int(256usize))))),
                                ethabi::ParamType::Address]),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Int(256usize))),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Int(256usize))),
                                ethabi::ParamType::Uint(256usize),
                                ethabi::ParamType::Uint(256usize),
                                ethabi::ParamType::Array(Box::new(ethabi::ParamType::Array(Box::new(ethabi::ParamType::String))))
                            ],
                        ),
                    ],
                    maybe_data.unwrap(),
                )
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                params: {
                    let tuple_elements = values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_tuple()
                        .expect(INTERNAL_ERR);
                    (
                        tuple_elements[0usize]
                            .clone()
                            .into_string()
                            .expect(INTERNAL_ERR),
                        tuple_elements[1usize]
                            .clone()
                            .into_string()
                            .expect(INTERNAL_ERR),
                        tuple_elements[2usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                let tuple_elements = inner
                                    .into_tuple()
                                    .expect(INTERNAL_ERR);
                                (
                                    tuple_elements[0usize]
                                        .clone()
                                        .into_address()
                                        .expect(INTERNAL_ERR)
                                        .as_bytes()
                                        .to_vec(),
                                    {
                                        let mut v = [0 as u8; 32];
                                        tuple_elements[1usize]
                                            .clone()
                                            .into_uint()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                                    },
                                    tuple_elements[2usize]
                                        .clone()
                                        .into_address()
                                        .expect(INTERNAL_ERR)
                                        .as_bytes()
                                        .to_vec(),
                                    tuple_elements[3usize]
                                        .clone()
                                        .into_bool()
                                        .expect(INTERNAL_ERR),
                                )
                            })
                            .collect(),
                        tuple_elements[3usize]
                            .clone()
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
                        {
                            let tuple_elements = tuple_elements[4usize]
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
                            )
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
                        tuple_elements[6usize]
                            .clone()
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec(),
                        tuple_elements[7usize].clone().into_bool().expect(INTERNAL_ERR),
                        tuple_elements[8usize].clone().into_bool().expect(INTERNAL_ERR),
                        {
                            let mut result = [0u8; 32];
                            let v = tuple_elements[9usize]
                                .clone()
                                .into_fixed_bytes()
                                .expect(INTERNAL_ERR);
                            result.copy_from_slice(&v);
                            result
                        },
                        tuple_elements[10usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                let mut v = [0 as u8; 32];
                                inner
                                    .into_int()
                                    .expect(INTERNAL_ERR)
                                    .to_big_endian(v.as_mut_slice());
                                substreams::scalar::BigInt::from_signed_bytes_be(&v)
                            })
                            .collect(),
                        {
                            let tuple_elements = tuple_elements[11usize]
                                .clone()
                                .into_tuple()
                                .expect(INTERNAL_ERR);
                            (
                                tuple_elements[0usize]
                                    .clone()
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
                                tuple_elements[1usize]
                                    .clone()
                                    .into_address()
                                    .expect(INTERNAL_ERR)
                                    .as_bytes()
                                    .to_vec(),
                                tuple_elements[2usize]
                                    .clone()
                                    .into_array()
                                    .expect(INTERNAL_ERR)
                                    .into_iter()
                                    .map(|inner| {
                                        inner
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
                                            .collect()
                                    })
                                    .collect(),
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
                                        let mut v = [0 as u8; 32];
                                        inner
                                            .into_uint()
                                            .expect(INTERNAL_ERR)
                                            .to_big_endian(v.as_mut_slice());
                                        substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                                    })
                                    .collect(),
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
                                tuple_elements[8usize]
                                    .clone()
                                    .into_array()
                                    .expect(INTERNAL_ERR)
                                    .into_iter()
                                    .map(|inner| {
                                        inner
                                            .into_array()
                                            .expect(INTERNAL_ERR)
                                            .into_iter()
                                            .map(|inner| {
                                                let mut v = [0 as u8; 32];
                                                inner
                                                    .into_int()
                                                    .expect(INTERNAL_ERR)
                                                    .to_big_endian(v.as_mut_slice());
                                                substreams::scalar::BigInt::from_signed_bytes_be(&v)
                                            })
                                            .collect()
                                    })
                                    .collect(),
                                tuple_elements[9usize]
                                    .clone()
                                    .into_address()
                                    .expect(INTERNAL_ERR)
                                    .as_bytes()
                                    .to_vec(),
                            )
                        },
                        tuple_elements[12usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                let mut v = [0 as u8; 32];
                                inner
                                    .into_int()
                                    .expect(INTERNAL_ERR)
                                    .to_big_endian(v.as_mut_slice());
                                substreams::scalar::BigInt::from_signed_bytes_be(&v)
                            })
                            .collect(),
                        tuple_elements[13usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                let mut v = [0 as u8; 32];
                                inner
                                    .into_int()
                                    .expect(INTERNAL_ERR)
                                    .to_big_endian(v.as_mut_slice());
                                substreams::scalar::BigInt::from_signed_bytes_be(&v)
                            })
                            .collect(),
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[14usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                        {
                            let mut v = [0 as u8; 32];
                            tuple_elements[15usize]
                                .clone()
                                .into_uint()
                                .expect(INTERNAL_ERR)
                                .to_big_endian(v.as_mut_slice());
                            substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                        },
                        tuple_elements[16usize]
                            .clone()
                            .into_array()
                            .expect(INTERNAL_ERR)
                            .into_iter()
                            .map(|inner| {
                                inner
                                    .into_array()
                                    .expect(INTERNAL_ERR)
                                    .into_iter()
                                    .map(|inner| inner.into_string().expect(INTERNAL_ERR))
                                    .collect()
                            })
                            .collect(),
                    )
                },
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(
                &[
                    ethabi::Token::Tuple(
                        vec![
                            ethabi::Token::String(self.params.0.clone()),
                            ethabi::Token::String(self.params.1.clone()), { let v = self
                            .params.2.iter().map(| inner |
                            ethabi::Token::Tuple(vec![ethabi::Token::Address(ethabi::Address::from_slice(&
                            inner.0)),
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match inner
                            .1.clone().to_bytes_be() { (num_bigint::Sign::Plus, bytes) =>
                            bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),),
                            ethabi::Token::Address(ethabi::Address::from_slice(& inner
                            .2)), ethabi::Token::Bool(inner.3.clone())])).collect();
                            ethabi::Token::Array(v) }, { let v = self.params.3.iter()
                            .map(| inner |
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match inner
                            .clone().to_bytes_be() { (num_bigint::Sign::Plus, bytes) =>
                            bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),)).collect(); ethabi::Token::Array(v) },
                            ethabi::Token::Tuple(vec![ethabi::Token::Address(ethabi::Address::from_slice(&
                            self.params.4.0)),
                            ethabi::Token::Address(ethabi::Address::from_slice(& self
                            .params.4.1)),
                            ethabi::Token::Address(ethabi::Address::from_slice(& self
                            .params.4.2))]),
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.5.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),),
                            ethabi::Token::Address(ethabi::Address::from_slice(& self
                            .params.6)), ethabi::Token::Bool(self.params.7.clone()),
                            ethabi::Token::Bool(self.params.8.clone()),
                            ethabi::Token::FixedBytes(self.params.9.as_ref().to_vec()), {
                            let v = self.params.10.iter().map(| inner | { let
                            non_full_signed_bytes = inner.to_signed_bytes_be(); let
                            full_signed_bytes_init = if non_full_signed_bytes[0] & 0x80
                            == 0x80 { 0xff } else { 0x00 }; let mut full_signed_bytes =
                            [full_signed_bytes_init as u8; 32]; non_full_signed_bytes
                            .into_iter().rev().enumerate().for_each(| (i, byte) |
                            full_signed_bytes[31 - i] = byte);
                            ethabi::Token::Int(ethabi::Int::from_big_endian(full_signed_bytes
                            .as_ref())) }).collect(); ethabi::Token::Array(v) },
                            ethabi::Token::Tuple(vec![{ let v = self.params.11.0.iter()
                            .map(| inner |
                            ethabi::Token::Address(ethabi::Address::from_slice(& inner)))
                            .collect(); ethabi::Token::Array(v) },
                            ethabi::Token::Address(ethabi::Address::from_slice(& self
                            .params.11.1)), { let v = self.params.11.2.iter().map(| inner
                            | { let v = inner.iter().map(| inner |
                            ethabi::Token::Address(ethabi::Address::from_slice(& inner)))
                            .collect(); ethabi::Token::Array(v) }).collect();
                            ethabi::Token::Array(v) },
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.11.3.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),), { let v = self.params.11.4.iter().map(|
                            inner |
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match inner
                            .clone().to_bytes_be() { (num_bigint::Sign::Plus, bytes) =>
                            bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),)).collect(); ethabi::Token::Array(v) },
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.11.5.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),),
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.11.6.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),),
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.11.7.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),), { let v = self.params.11.8.iter().map(|
                            inner | { let v = inner.iter().map(| inner | { let
                            non_full_signed_bytes = inner.to_signed_bytes_be(); let
                            full_signed_bytes_init = if non_full_signed_bytes[0] & 0x80
                            == 0x80 { 0xff } else { 0x00 }; let mut full_signed_bytes =
                            [full_signed_bytes_init as u8; 32]; non_full_signed_bytes
                            .into_iter().rev().enumerate().for_each(| (i, byte) |
                            full_signed_bytes[31 - i] = byte);
                            ethabi::Token::Int(ethabi::Int::from_big_endian(full_signed_bytes
                            .as_ref())) }).collect(); ethabi::Token::Array(v) })
                            .collect(); ethabi::Token::Array(v) },
                            ethabi::Token::Address(ethabi::Address::from_slice(& self
                            .params.11.9))]), { let v = self.params.12.iter().map(| inner
                            | { let non_full_signed_bytes = inner.to_signed_bytes_be();
                            let full_signed_bytes_init = if non_full_signed_bytes[0] &
                            0x80 == 0x80 { 0xff } else { 0x00 }; let mut
                            full_signed_bytes = [full_signed_bytes_init as u8; 32];
                            non_full_signed_bytes.into_iter().rev().enumerate()
                            .for_each(| (i, byte) | full_signed_bytes[31 - i] = byte);
                            ethabi::Token::Int(ethabi::Int::from_big_endian(full_signed_bytes
                            .as_ref())) }).collect(); ethabi::Token::Array(v) }, { let v
                            = self.params.13.iter().map(| inner | { let
                            non_full_signed_bytes = inner.to_signed_bytes_be(); let
                            full_signed_bytes_init = if non_full_signed_bytes[0] & 0x80
                            == 0x80 { 0xff } else { 0x00 }; let mut full_signed_bytes =
                            [full_signed_bytes_init as u8; 32]; non_full_signed_bytes
                            .into_iter().rev().enumerate().for_each(| (i, byte) |
                            full_signed_bytes[31 - i] = byte);
                            ethabi::Token::Int(ethabi::Int::from_big_endian(full_signed_bytes
                            .as_ref())) }).collect(); ethabi::Token::Array(v) },
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.14.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),),
                            ethabi::Token::Uint(ethabi::Uint::from_big_endian(match self
                            .params.15.clone().to_bytes_be() { (num_bigint::Sign::Plus,
                            bytes) => bytes, (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported") }, }
                            .as_slice(),),), { let v = self.params.16.iter().map(| inner
                            | { let v = inner.iter().map(| inner |
                            ethabi::Token::String(inner.clone())).collect();
                            ethabi::Token::Array(v) }).collect(); ethabi::Token::Array(v)
                            }
                        ],
                    ),
                ],
            );
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
            Ok(
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            )
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
    impl substreams_ethereum::Function for CreateWithoutArgs {
        const NAME: &'static str = "createWithoutArgs";
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
    impl substreams_ethereum::rpc::RPCDecodable<Vec<u8>> for CreateWithoutArgs {
        fn output(data: &[u8]) -> Result<Vec<u8>, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct Disable {}
    impl Disable {
        const METHOD_ID: [u8; 4] = [47u8, 39u8, 112u8, 219u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
    impl substreams_ethereum::Function for Disable {
        const NAME: &'static str = "disable";
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
    #[derive(Clone)]
    pub struct GetActionId {
        pub selector: [u8; 4usize],
    }
    impl GetActionId {
        const METHOD_ID: [u8; 4] = [133u8, 28u8, 27u8, 179u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                    &[ethabi::ParamType::FixedBytes(4usize)],
                    maybe_data.unwrap(),
                )
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                selector: {
                    let mut result = [0u8; 4];
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
            let data = ethabi::encode(
                &[ethabi::Token::FixedBytes(self.selector.as_ref().to_vec())],
            );
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<[u8; 32usize], String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<[u8; 32usize], String> {
            let mut values = ethabi::decode(
                    &[ethabi::ParamType::FixedBytes(32usize)],
                    data.as_ref(),
                )
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok({
                let mut result = [0u8; 32];
                let v = values
                    .pop()
                    .expect("one output data should have existed")
                    .into_fixed_bytes()
                    .expect(INTERNAL_ERR);
                result.copy_from_slice(&v);
                result
            })
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<[u8; 32usize]> {
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
    impl substreams_ethereum::Function for GetActionId {
        const NAME: &'static str = "getActionId";
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
    impl substreams_ethereum::rpc::RPCDecodable<[u8; 32usize]> for GetActionId {
        fn output(data: &[u8]) -> Result<[u8; 32usize], String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetAuthorizer {}
    impl GetAuthorizer {
        const METHOD_ID: [u8; 4] = [170u8, 171u8, 173u8, 197u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
            Ok(
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            )
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
    impl substreams_ethereum::Function for GetAuthorizer {
        const NAME: &'static str = "getAuthorizer";
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
    impl substreams_ethereum::rpc::RPCDecodable<Vec<u8>> for GetAuthorizer {
        fn output(data: &[u8]) -> Result<Vec<u8>, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetCreationCode {}
    impl GetCreationCode {
        const METHOD_ID: [u8; 4] = [0u8, 193u8, 148u8, 219u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
            let mut values = ethabi::decode(&[ethabi::ParamType::Bytes], data.as_ref())
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok(
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_bytes()
                    .expect(INTERNAL_ERR),
            )
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
    impl substreams_ethereum::Function for GetCreationCode {
        const NAME: &'static str = "getCreationCode";
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
    impl substreams_ethereum::rpc::RPCDecodable<Vec<u8>> for GetCreationCode {
        fn output(data: &[u8]) -> Result<Vec<u8>, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetCreationCodeContracts {}
    impl GetCreationCodeContracts {
        const METHOD_ID: [u8; 4] = [23u8, 68u8, 129u8, 250u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
    impl substreams_ethereum::Function for GetCreationCodeContracts {
        const NAME: &'static str = "getCreationCodeContracts";
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
    impl substreams_ethereum::rpc::RPCDecodable<(Vec<u8>, Vec<u8>)>
    for GetCreationCodeContracts {
        fn output(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetDefaultLiquidityManagement {}
    impl GetDefaultLiquidityManagement {
        const METHOD_ID: [u8; 4] = [25u8, 58u8, 213u8, 15u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
        ) -> Result<(bool, bool, bool, bool), String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<(bool, bool, bool, bool), String> {
            let mut values = ethabi::decode(
                    &[
                        ethabi::ParamType::Tuple(
                            vec![
                                ethabi::ParamType::Bool, ethabi::ParamType::Bool,
                                ethabi::ParamType::Bool, ethabi::ParamType::Bool
                            ],
                        ),
                    ],
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
                    tuple_elements[0usize].clone().into_bool().expect(INTERNAL_ERR),
                    tuple_elements[1usize].clone().into_bool().expect(INTERNAL_ERR),
                    tuple_elements[2usize].clone().into_bool().expect(INTERNAL_ERR),
                    tuple_elements[3usize].clone().into_bool().expect(INTERNAL_ERR),
                )
            })
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<(bool, bool, bool, bool)> {
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
    impl substreams_ethereum::Function for GetDefaultLiquidityManagement {
        const NAME: &'static str = "getDefaultLiquidityManagement";
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
    impl substreams_ethereum::rpc::RPCDecodable<(bool, bool, bool, bool)>
    for GetDefaultLiquidityManagement {
        fn output(data: &[u8]) -> Result<(bool, bool, bool, bool), String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetDefaultPoolHooksContract {}
    impl GetDefaultPoolHooksContract {
        const METHOD_ID: [u8; 4] = [236u8, 136u8, 128u8, 97u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
            Ok(
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            )
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
    impl substreams_ethereum::Function for GetDefaultPoolHooksContract {
        const NAME: &'static str = "getDefaultPoolHooksContract";
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
    impl substreams_ethereum::rpc::RPCDecodable<Vec<u8>>
    for GetDefaultPoolHooksContract {
        fn output(data: &[u8]) -> Result<Vec<u8>, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetDeploymentAddress {
        pub constructor_args: Vec<u8>,
        pub salt: [u8; 32usize],
    }
    impl GetDeploymentAddress {
        const METHOD_ID: [u8; 4] = [68u8, 246u8, 254u8, 199u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                    &[ethabi::ParamType::Bytes, ethabi::ParamType::FixedBytes(32usize)],
                    maybe_data.unwrap(),
                )
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                constructor_args: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bytes()
                    .expect(INTERNAL_ERR),
                salt: {
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
            let data = ethabi::encode(
                &[
                    ethabi::Token::Bytes(self.constructor_args.clone()),
                    ethabi::Token::FixedBytes(self.salt.as_ref().to_vec()),
                ],
            );
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
            Ok(
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            )
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
    impl substreams_ethereum::Function for GetDeploymentAddress {
        const NAME: &'static str = "getDeploymentAddress";
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
    impl substreams_ethereum::rpc::RPCDecodable<Vec<u8>> for GetDeploymentAddress {
        fn output(data: &[u8]) -> Result<Vec<u8>, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetNewPoolPauseWindowEndTime {}
    impl GetNewPoolPauseWindowEndTime {
        const METHOD_ID: [u8; 4] = [219u8, 3u8, 94u8, 188u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
            let mut values = ethabi::decode(
                    &[ethabi::ParamType::Uint(32usize)],
                    data.as_ref(),
                )
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
    impl substreams_ethereum::Function for GetNewPoolPauseWindowEndTime {
        const NAME: &'static str = "getNewPoolPauseWindowEndTime";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt>
    for GetNewPoolPauseWindowEndTime {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetOriginalPauseWindowEndTime {}
    impl GetOriginalPauseWindowEndTime {
        const METHOD_ID: [u8; 4] = [233u8, 213u8, 110u8, 25u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
            let mut values = ethabi::decode(
                    &[ethabi::ParamType::Uint(32usize)],
                    data.as_ref(),
                )
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
    impl substreams_ethereum::Function for GetOriginalPauseWindowEndTime {
        const NAME: &'static str = "getOriginalPauseWindowEndTime";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt>
    for GetOriginalPauseWindowEndTime {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetPauseWindowDuration {}
    impl GetPauseWindowDuration {
        const METHOD_ID: [u8; 4] = [120u8, 218u8, 128u8, 203u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
            let mut values = ethabi::decode(
                    &[ethabi::ParamType::Uint(32usize)],
                    data.as_ref(),
                )
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
    impl substreams_ethereum::Function for GetPauseWindowDuration {
        const NAME: &'static str = "getPauseWindowDuration";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt>
    for GetPauseWindowDuration {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetPoolCount {}
    impl GetPoolCount {
        const METHOD_ID: [u8; 4] = [142u8, 236u8, 93u8, 112u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
            let mut values = ethabi::decode(
                    &[ethabi::ParamType::Uint(256usize)],
                    data.as_ref(),
                )
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
    impl substreams_ethereum::Function for GetPoolCount {
        const NAME: &'static str = "getPoolCount";
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
    impl substreams_ethereum::rpc::RPCDecodable<substreams::scalar::BigInt>
    for GetPoolCount {
        fn output(data: &[u8]) -> Result<substreams::scalar::BigInt, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetPoolVersion {}
    impl GetPoolVersion {
        const METHOD_ID: [u8; 4] = [63u8, 129u8, 155u8, 111u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
        ) -> Result<String, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<String, String> {
            let mut values = ethabi::decode(&[ethabi::ParamType::String], data.as_ref())
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok(
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_string()
                    .expect(INTERNAL_ERR),
            )
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<String> {
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
    impl substreams_ethereum::Function for GetPoolVersion {
        const NAME: &'static str = "getPoolVersion";
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
    impl substreams_ethereum::rpc::RPCDecodable<String> for GetPoolVersion {
        fn output(data: &[u8]) -> Result<String, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetPools {}
    impl GetPools {
        const METHOD_ID: [u8; 4] = [103u8, 58u8, 42u8, 31u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
            Ok(
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_array()
                    .expect(INTERNAL_ERR)
                    .into_iter()
                    .map(|inner| {
                        inner.into_address().expect(INTERNAL_ERR).as_bytes().to_vec()
                    })
                    .collect(),
            )
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
    impl substreams_ethereum::Function for GetPools {
        const NAME: &'static str = "getPools";
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
    impl substreams_ethereum::rpc::RPCDecodable<Vec<Vec<u8>>> for GetPools {
        fn output(data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetPoolsInRange {
        pub start: substreams::scalar::BigInt,
        pub count: substreams::scalar::BigInt,
    }
    impl GetPoolsInRange {
        const METHOD_ID: [u8; 4] = [83u8, 167u8, 47u8, 126u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                    &[
                        ethabi::ParamType::Uint(256usize),
                        ethabi::ParamType::Uint(256usize),
                    ],
                    maybe_data.unwrap(),
                )
                .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                start: {
                    let mut v = [0 as u8; 32];
                    values
                        .pop()
                        .expect(INTERNAL_ERR)
                        .into_uint()
                        .expect(INTERNAL_ERR)
                        .to_big_endian(v.as_mut_slice());
                    substreams::scalar::BigInt::from_unsigned_bytes_be(&v)
                },
                count: {
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
            let data = ethabi::encode(
                &[
                    ethabi::Token::Uint(
                        ethabi::Uint::from_big_endian(
                            match self.start.clone().to_bytes_be() {
                                (num_bigint::Sign::Plus, bytes) => bytes,
                                (num_bigint::Sign::NoSign, bytes) => bytes,
                                (num_bigint::Sign::Minus, _) => {
                                    panic!("negative numbers are not supported")
                                }
                            }
                                .as_slice(),
                        ),
                    ),
                    ethabi::Token::Uint(
                        ethabi::Uint::from_big_endian(
                            match self.count.clone().to_bytes_be() {
                                (num_bigint::Sign::Plus, bytes) => bytes,
                                (num_bigint::Sign::NoSign, bytes) => bytes,
                                (num_bigint::Sign::Minus, _) => {
                                    panic!("negative numbers are not supported")
                                }
                            }
                                .as_slice(),
                        ),
                    ),
                ],
            );
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
            Ok(
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_array()
                    .expect(INTERNAL_ERR)
                    .into_iter()
                    .map(|inner| {
                        inner.into_address().expect(INTERNAL_ERR).as_bytes().to_vec()
                    })
                    .collect(),
            )
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
    impl substreams_ethereum::Function for GetPoolsInRange {
        const NAME: &'static str = "getPoolsInRange";
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
    impl substreams_ethereum::rpc::RPCDecodable<Vec<Vec<u8>>> for GetPoolsInRange {
        fn output(data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct GetVault {}
    impl GetVault {
        const METHOD_ID: [u8; 4] = [141u8, 146u8, 138u8, 248u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
            Ok(
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
            )
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
    impl substreams_ethereum::Function for GetVault {
        const NAME: &'static str = "getVault";
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
    impl substreams_ethereum::rpc::RPCDecodable<Vec<u8>> for GetVault {
        fn output(data: &[u8]) -> Result<Vec<u8>, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct IsDisabled {}
    impl IsDisabled {
        const METHOD_ID: [u8; 4] = [108u8, 87u8, 245u8, 169u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
        ) -> Result<bool, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<bool, String> {
            let mut values = ethabi::decode(&[ethabi::ParamType::Bool], data.as_ref())
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok(
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_bool()
                    .expect(INTERNAL_ERR),
            )
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
    impl substreams_ethereum::Function for IsDisabled {
        const NAME: &'static str = "isDisabled";
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
    impl substreams_ethereum::rpc::RPCDecodable<bool> for IsDisabled {
        fn output(data: &[u8]) -> Result<bool, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct IsPoolFromFactory {
        pub pool: Vec<u8>,
    }
    impl IsPoolFromFactory {
        const METHOD_ID: [u8; 4] = [102u8, 52u8, 183u8, 83u8];
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
            let data = ethabi::encode(
                &[ethabi::Token::Address(ethabi::Address::from_slice(&self.pool))],
            );
            let mut encoded = Vec::with_capacity(4 + data.len());
            encoded.extend(Self::METHOD_ID);
            encoded.extend(data);
            encoded
        }
        pub fn output_call(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<bool, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<bool, String> {
            let mut values = ethabi::decode(&[ethabi::ParamType::Bool], data.as_ref())
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok(
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_bool()
                    .expect(INTERNAL_ERR),
            )
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
    impl substreams_ethereum::Function for IsPoolFromFactory {
        const NAME: &'static str = "isPoolFromFactory";
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
    impl substreams_ethereum::rpc::RPCDecodable<bool> for IsPoolFromFactory {
        fn output(data: &[u8]) -> Result<bool, String> {
            Self::output(data)
        }
    }
    #[derive(Clone)]
    pub struct Version {}
    impl Version {
        const METHOD_ID: [u8; 4] = [84u8, 253u8, 77u8, 80u8];
        pub fn decode(
            call: &substreams_ethereum::pb::eth::v2::Call,
        ) -> Result<Self, String> {
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
        ) -> Result<String, String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<String, String> {
            let mut values = ethabi::decode(&[ethabi::ParamType::String], data.as_ref())
                .map_err(|e| format!("unable to decode output data: {:?}", e))?;
            Ok(
                values
                    .pop()
                    .expect("one output data should have existed")
                    .into_string()
                    .expect(INTERNAL_ERR),
            )
        }
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
        pub fn call(&self, address: Vec<u8>) -> Option<String> {
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
    impl substreams_ethereum::Function for Version {
        const NAME: &'static str = "version";
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
    impl substreams_ethereum::rpc::RPCDecodable<String> for Version {
        fn output(data: &[u8]) -> Result<String, String> {
            Self::output(data)
        }
    }
}
/// Contract's events.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod events {
    use super::INTERNAL_ERR;
    #[derive(Clone)]
    pub struct FactoryDisabled {}
    impl FactoryDisabled {
        const TOPIC_ID: [u8; 32] = [
            67u8,
            42u8,
            203u8,
            253u8,
            102u8,
            45u8,
            187u8,
            93u8,
            139u8,
            55u8,
            131u8,
            132u8,
            166u8,
            113u8,
            89u8,
            180u8,
            124u8,
            169u8,
            208u8,
            241u8,
            183u8,
            159u8,
            151u8,
            207u8,
            100u8,
            207u8,
            133u8,
            133u8,
            250u8,
            54u8,
            45u8,
            80u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 1usize {
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
            Ok(Self {})
        }
    }
    impl substreams_ethereum::Event for FactoryDisabled {
        const NAME: &'static str = "FactoryDisabled";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Clone)]
    pub struct PoolCreated {
        pub pool: Vec<u8>,
    }
    impl PoolCreated {
        const TOPIC_ID: [u8; 32] = [
            131u8,
            164u8,
            143u8,
            188u8,
            252u8,
            153u8,
            19u8,
            53u8,
            49u8,
            78u8,
            116u8,
            208u8,
            73u8,
            106u8,
            171u8,
            106u8,
            25u8,
            135u8,
            233u8,
            146u8,
            221u8,
            200u8,
            93u8,
            221u8,
            188u8,
            196u8,
            214u8,
            221u8,
            110u8,
            242u8,
            233u8,
            252u8,
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