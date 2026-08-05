const INTERNAL_ERR: &'static str = "`ethabi_derive` internal error";
/// Contract's functions.
#[allow(dead_code, unused_imports, unused_variables)]
pub mod functions {
    use super::INTERNAL_ERR;
    #[derive(Debug, Clone, PartialEq)]
    pub struct GetDexTypeAndLiquidityAddr {}
    impl GetDexTypeAndLiquidityAddr {
        const METHOD_ID: [u8; 4] = [90u8, 175u8, 215u8, 176u8];
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
        ) -> Result<(substreams::scalar::BigInt, Vec<u8>), String> {
            Self::output(call.return_data.as_ref())
        }
        pub fn output(data: &[u8]) -> Result<(substreams::scalar::BigInt, Vec<u8>), String> {
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Uint(256usize), ethabi::ParamType::Address],
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
        pub fn call(&self, address: Vec<u8>) -> Option<(substreams::scalar::BigInt, Vec<u8>)> {
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
    impl substreams_ethereum::Function for GetDexTypeAndLiquidityAddr {
        const NAME: &'static str = "getDexTypeAndLiquidityAddr";
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
    impl substreams_ethereum::rpc::RPCDecodable<(substreams::scalar::BigInt, Vec<u8>)>
        for GetDexTypeAndLiquidityAddr
    {
        fn output(data: &[u8]) -> Result<(substreams::scalar::BigInt, Vec<u8>), String> {
            Self::output(data)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct StopPerPoolAccounting {
        pub dex_key:
            (Vec<u8>, Vec<u8>, substreams::scalar::BigInt, substreams::scalar::BigInt, Vec<u8>),
    }
    impl StopPerPoolAccounting {
        const METHOD_ID: [u8; 4] = [77u8, 116u8, 169u8, 56u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Tuple(vec![
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Address,
                    ethabi::ParamType::Uint(24usize),
                    ethabi::ParamType::Uint(24usize),
                    ethabi::ParamType::Address,
                ])],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                dex_key: {
                    let tuple_elements = values
                        .pop()
                        .expect(INTERNAL_ERR)
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
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec(),
                    )
                },
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[ethabi::Token::Tuple(vec![
                ethabi::Token::Address(ethabi::Address::from_slice(&self.dex_key.0)),
                ethabi::Token::Address(ethabi::Address::from_slice(&self.dex_key.1)),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.dex_key.2.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.dex_key.3.clone().to_bytes_be() {
                        (num_bigint::Sign::Plus, bytes) => bytes,
                        (num_bigint::Sign::NoSign, bytes) => bytes,
                        (num_bigint::Sign::Minus, _) => {
                            panic!("negative numbers are not supported")
                        }
                    }
                    .as_slice(),
                )),
                ethabi::Token::Address(ethabi::Address::from_slice(&self.dex_key.4)),
            ])]);
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
    impl substreams_ethereum::Function for StopPerPoolAccounting {
        const NAME: &'static str = "stopPerPoolAccounting";
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
    pub struct UpdateProtocolCutFee {
        pub dex_key:
            (Vec<u8>, Vec<u8>, substreams::scalar::BigInt, substreams::scalar::BigInt, Vec<u8>),
        pub protocol_cut_fee: substreams::scalar::BigInt,
    }
    impl UpdateProtocolCutFee {
        const METHOD_ID: [u8; 4] = [168u8, 134u8, 84u8, 125u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Uint(24usize),
                        ethabi::ParamType::Uint(24usize),
                        ethabi::ParamType::Address,
                    ]),
                    ethabi::ParamType::Uint(256usize),
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                dex_key: {
                    let tuple_elements = values
                        .pop()
                        .expect(INTERNAL_ERR)
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
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec(),
                    )
                },
                protocol_cut_fee: {
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
                ethabi::Token::Tuple(vec![
                    ethabi::Token::Address(ethabi::Address::from_slice(&self.dex_key.0)),
                    ethabi::Token::Address(ethabi::Address::from_slice(&self.dex_key.1)),
                    ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                        match self.dex_key.2.clone().to_bytes_be() {
                            (num_bigint::Sign::Plus, bytes) => bytes,
                            (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                                panic!("negative numbers are not supported")
                            }
                        }
                        .as_slice(),
                    )),
                    ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                        match self.dex_key.3.clone().to_bytes_be() {
                            (num_bigint::Sign::Plus, bytes) => bytes,
                            (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                                panic!("negative numbers are not supported")
                            }
                        }
                        .as_slice(),
                    )),
                    ethabi::Token::Address(ethabi::Address::from_slice(&self.dex_key.4)),
                ]),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self
                        .protocol_cut_fee
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
        pub fn match_call(call: &substreams_ethereum::pb::eth::v2::Call) -> bool {
            match call.input.get(0..4) {
                Some(signature) => Self::METHOD_ID == signature,
                None => false,
            }
        }
    }
    impl substreams_ethereum::Function for UpdateProtocolCutFee {
        const NAME: &'static str = "updateProtocolCutFee";
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
    pub struct UpdateProtocolFee {
        pub dex_key:
            (Vec<u8>, Vec<u8>, substreams::scalar::BigInt, substreams::scalar::BigInt, Vec<u8>),
        pub swap0_to1: bool,
        pub protocol_fee: substreams::scalar::BigInt,
    }
    impl UpdateProtocolFee {
        const METHOD_ID: [u8; 4] = [200u8, 115u8, 231u8, 76u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[
                    ethabi::ParamType::Tuple(vec![
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Address,
                        ethabi::ParamType::Uint(24usize),
                        ethabi::ParamType::Uint(24usize),
                        ethabi::ParamType::Address,
                    ]),
                    ethabi::ParamType::Bool,
                    ethabi::ParamType::Uint(256usize),
                ],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                dex_key: {
                    let tuple_elements = values
                        .pop()
                        .expect(INTERNAL_ERR)
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
                            .into_address()
                            .expect(INTERNAL_ERR)
                            .as_bytes()
                            .to_vec(),
                    )
                },
                swap0_to1: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
                protocol_fee: {
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
                ethabi::Token::Tuple(vec![
                    ethabi::Token::Address(ethabi::Address::from_slice(&self.dex_key.0)),
                    ethabi::Token::Address(ethabi::Address::from_slice(&self.dex_key.1)),
                    ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                        match self.dex_key.2.clone().to_bytes_be() {
                            (num_bigint::Sign::Plus, bytes) => bytes,
                            (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                                panic!("negative numbers are not supported")
                            }
                        }
                        .as_slice(),
                    )),
                    ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                        match self.dex_key.3.clone().to_bytes_be() {
                            (num_bigint::Sign::Plus, bytes) => bytes,
                            (num_bigint::Sign::NoSign, bytes) => bytes,
                            (num_bigint::Sign::Minus, _) => {
                                panic!("negative numbers are not supported")
                            }
                        }
                        .as_slice(),
                    )),
                    ethabi::Token::Address(ethabi::Address::from_slice(&self.dex_key.4)),
                ]),
                ethabi::Token::Bool(self.swap0_to1.clone()),
                ethabi::Token::Uint(ethabi::Uint::from_big_endian(
                    match self.protocol_fee.clone().to_bytes_be() {
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
    impl substreams_ethereum::Function for UpdateProtocolFee {
        const NAME: &'static str = "updateProtocolFee";
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
    pub struct UpdateUserWhitelist {
        pub user: Vec<u8>,
        pub is_whitelisted: bool,
    }
    impl UpdateUserWhitelist {
        const METHOD_ID: [u8; 4] = [55u8, 158u8, 89u8, 195u8];
        pub fn decode(call: &substreams_ethereum::pb::eth::v2::Call) -> Result<Self, String> {
            let maybe_data = call.input.get(4..);
            if maybe_data.is_none() {
                return Err("no data to decode".to_string());
            }
            let mut values = ethabi::decode(
                &[ethabi::ParamType::Address, ethabi::ParamType::Bool],
                maybe_data.unwrap(),
            )
            .map_err(|e| format!("unable to decode call.input: {:?}", e))?;
            values.reverse();
            Ok(Self {
                user: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                is_whitelisted: values
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_bool()
                    .expect(INTERNAL_ERR),
            })
        }
        pub fn encode(&self) -> Vec<u8> {
            let data = ethabi::encode(&[
                ethabi::Token::Address(ethabi::Address::from_slice(&self.user)),
                ethabi::Token::Bool(self.is_whitelisted.clone()),
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
    impl substreams_ethereum::Function for UpdateUserWhitelist {
        const NAME: &'static str = "updateUserWhitelist";
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
    pub struct LogStopPerPoolAccounting {
        pub dex_type: substreams::scalar::BigInt,
        pub dex_id: [u8; 32usize],
    }
    impl LogStopPerPoolAccounting {
        const TOPIC_ID: [u8; 32] = [
            32u8, 230u8, 210u8, 104u8, 90u8, 203u8, 41u8, 101u8, 55u8, 69u8, 139u8, 228u8, 57u8,
            189u8, 71u8, 222u8, 206u8, 184u8, 126u8, 51u8, 110u8, 150u8, 226u8, 200u8, 201u8, 2u8,
            217u8, 242u8, 88u8, 159u8, 125u8, 134u8,
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
                dex_type: {
                    let mut v = [0 as u8; 32];
                    ethabi::decode(
                        &[ethabi::ParamType::Uint(256usize)],
                        log.topics[1usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'dex_type' from topic of type 'uint256': {:?}",
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
                dex_id: {
                    let mut result = [0u8; 32];
                    let v = ethabi::decode(
                        &[ethabi::ParamType::FixedBytes(32usize)],
                        log.topics[2usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'dex_id' from topic of type 'bytes32': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_fixed_bytes()
                    .expect(INTERNAL_ERR);
                    result.copy_from_slice(&v);
                    result
                },
            })
        }
    }
    impl substreams_ethereum::Event for LogStopPerPoolAccounting {
        const NAME: &'static str = "LogStopPerPoolAccounting";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogUpdateProtocolCutFee {
        pub dex_type: substreams::scalar::BigInt,
        pub dex_id: [u8; 32usize],
        pub protocol_cut_fee: substreams::scalar::BigInt,
    }
    impl LogUpdateProtocolCutFee {
        const TOPIC_ID: [u8; 32] = [
            58u8, 248u8, 179u8, 218u8, 64u8, 75u8, 125u8, 109u8, 64u8, 202u8, 65u8, 130u8, 254u8,
            111u8, 70u8, 45u8, 127u8, 192u8, 168u8, 117u8, 208u8, 232u8, 115u8, 37u8, 229u8, 165u8,
            194u8, 158u8, 248u8, 75u8, 13u8, 180u8,
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
            let mut values =
                ethabi::decode(&[ethabi::ParamType::Uint(256usize)], log.data.as_ref())
                    .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                dex_type: {
                    let mut v = [0 as u8; 32];
                    ethabi::decode(
                        &[ethabi::ParamType::Uint(256usize)],
                        log.topics[1usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'dex_type' from topic of type 'uint256': {:?}",
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
                dex_id: {
                    let mut result = [0u8; 32];
                    let v = ethabi::decode(
                        &[ethabi::ParamType::FixedBytes(32usize)],
                        log.topics[2usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'dex_id' from topic of type 'bytes32': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_fixed_bytes()
                    .expect(INTERNAL_ERR);
                    result.copy_from_slice(&v);
                    result
                },
                protocol_cut_fee: {
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
    impl substreams_ethereum::Event for LogUpdateProtocolCutFee {
        const NAME: &'static str = "LogUpdateProtocolCutFee";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogUpdateProtocolFee {
        pub dex_type: substreams::scalar::BigInt,
        pub dex_id: [u8; 32usize],
        pub protocol_fee: substreams::scalar::BigInt,
    }
    impl LogUpdateProtocolFee {
        const TOPIC_ID: [u8; 32] = [
            177u8, 140u8, 219u8, 132u8, 138u8, 6u8, 253u8, 250u8, 41u8, 125u8, 187u8, 213u8, 102u8,
            110u8, 5u8, 193u8, 142u8, 148u8, 155u8, 131u8, 100u8, 99u8, 174u8, 207u8, 170u8, 46u8,
            28u8, 119u8, 45u8, 70u8, 97u8, 198u8,
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
            let mut values =
                ethabi::decode(&[ethabi::ParamType::Uint(256usize)], log.data.as_ref())
                    .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
            values.reverse();
            Ok(Self {
                dex_type: {
                    let mut v = [0 as u8; 32];
                    ethabi::decode(
                        &[ethabi::ParamType::Uint(256usize)],
                        log.topics[1usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'dex_type' from topic of type 'uint256': {:?}",
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
                dex_id: {
                    let mut result = [0u8; 32];
                    let v = ethabi::decode(
                        &[ethabi::ParamType::FixedBytes(32usize)],
                        log.topics[2usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'dex_id' from topic of type 'bytes32': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_fixed_bytes()
                    .expect(INTERNAL_ERR);
                    result.copy_from_slice(&v);
                    result
                },
                protocol_fee: {
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
    impl substreams_ethereum::Event for LogUpdateProtocolFee {
        const NAME: &'static str = "LogUpdateProtocolFee";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    pub struct LogUpdateUserWhitelist {
        pub dex_type: substreams::scalar::BigInt,
        pub user: Vec<u8>,
        pub is_whitelisted: bool,
    }
    impl LogUpdateUserWhitelist {
        const TOPIC_ID: [u8; 32] = [
            6u8, 48u8, 205u8, 246u8, 199u8, 114u8, 189u8, 182u8, 84u8, 92u8, 35u8, 168u8, 38u8,
            254u8, 104u8, 136u8, 149u8, 83u8, 82u8, 236u8, 69u8, 135u8, 60u8, 96u8, 154u8, 152u8,
            165u8, 160u8, 229u8, 28u8, 0u8, 248u8,
        ];
        pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            if log.topics.len() != 4usize {
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
                dex_type: {
                    let mut v = [0 as u8; 32];
                    ethabi::decode(
                        &[ethabi::ParamType::Uint(256usize)],
                        log.topics[1usize].as_ref(),
                    )
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'dex_type' from topic of type 'uint256': {:?}",
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
                user: ethabi::decode(&[ethabi::ParamType::Address], log.topics[2usize].as_ref())
                    .map_err(|e| {
                        format!(
                            "unable to decode param 'user' from topic of type 'address': {:?}",
                            e
                        )
                    })?
                    .pop()
                    .expect(INTERNAL_ERR)
                    .into_address()
                    .expect(INTERNAL_ERR)
                    .as_bytes()
                    .to_vec(),
                is_whitelisted: ethabi::decode(
                    &[ethabi::ParamType::Bool],
                    log.topics[3usize].as_ref(),
                )
                .map_err(|e| {
                    format!(
                        "unable to decode param 'is_whitelisted' from topic of type 'bool': {:?}",
                        e
                    )
                })?
                .pop()
                .expect(INTERNAL_ERR)
                .into_bool()
                .expect(INTERNAL_ERR),
            })
        }
    }
    impl substreams_ethereum::Event for LogUpdateUserWhitelist {
        const NAME: &'static str = "LogUpdateUserWhitelist";
        fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
            Self::match_log(log)
        }
        fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
            Self::decode(log)
        }
    }
}
