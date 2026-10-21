use hex_literal::hex;

#[derive(Debug, Clone, PartialEq)]
pub struct PoolConfigured {
    pub asset0: Vec<u8>,
    pub asset1: Vec<u8>,
    pub tick_spacing: Vec<u8>,
    pub bundle_fee: Vec<u8>,
    pub unlocked_fee: Vec<u8>,
    pub protocol_unlocked_fee: Vec<u8>,
}
impl PoolConfigured {
    const TOPIC_ID: [u8; 32] =
        hex!("f325a037d71efc98bc41dc5257edefd43a1d1162e206373e53af271a7a3224e9");
    pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
        if log.topics.len() != 3usize {
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
                ethabi::ParamType::Uint(16usize), // tickSpacing
                ethabi::ParamType::Uint(24usize), // bundleFee
                ethabi::ParamType::Uint(24usize), // unlockedFee
                ethabi::ParamType::Uint(24usize), // protocolUnlockedFee
            ],
            log.data.as_ref(),
        )
        .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
        values.reverse();

        // Extract indexed parameters from topics (last 20 bytes for addresses)
        let asset0 = log
            .topics
            .get(1)
            .expect("asset0 topic missing")
            .iter()
            .skip(12)
            .copied()
            .collect::<Vec<u8>>();
        let asset1 = log
            .topics
            .get(2)
            .expect("asset1 topic missing")
            .iter()
            .skip(12)
            .copied()
            .collect::<Vec<u8>>();

        Ok(Self {
            asset0,
            asset1,
            tick_spacing: {
                let mut v = [0 as u8; 32];
                values
                    .pop()
                    .expect("Missing tickSpacing")
                    .into_uint()
                    .expect("Invalid tickSpacing")
                    .to_big_endian(v.as_mut_slice());
                v.to_vec()
            },
            bundle_fee: {
                let uint_val = values
                    .pop()
                    .expect("Missing bundleFee")
                    .into_uint()
                    .expect("Invalid bundleFee");
                let mut v = [0 as u8; 32];
                uint_val.to_big_endian(&mut v);
                v[29..32].to_vec()
            },
            unlocked_fee: {
                let uint_val = values
                    .pop()
                    .expect("Missing unlockedFee")
                    .into_uint()
                    .expect("Invalid unlockedFee");
                let mut v = [0 as u8; 32];
                uint_val.to_big_endian(&mut v);
                v[29..32].to_vec()
            },
            protocol_unlocked_fee: {
                let uint_val = values
                    .pop()
                    .expect("Missing protocolUnlockedFee")
                    .into_uint()
                    .expect("Invalid protocolUnlockedFee");
                let mut v = [0 as u8; 32];
                uint_val.to_big_endian(&mut v);
                v[29..32].to_vec()
            },
        })
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct PoolUpdate {
    pub asset_a: Vec<u8>,
    pub asset_b: Vec<u8>,
    pub bundle_fee: Vec<u8>,
    pub unlocked_fee: Vec<u8>,
    pub protocol_unlocked_fee: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BatchUpdatePools {
    pub updates: Vec<PoolUpdate>,
}

impl BatchUpdatePools {
    const SELECTOR: [u8; 4] = hex!("4d2bf47c");

    pub fn match_call(call_input: &[u8]) -> bool {
        call_input.len() >= 4 && call_input[0..4] == Self::SELECTOR
    }

    pub fn decode_call(call_input: &[u8]) -> Result<Self, String> {
        if !Self::match_call(call_input) {
            return Err("Invalid selector for batchUpdatePools".to_string());
        }

        let values = ethabi::decode(
            &[ethabi::ParamType::Array(Box::new(ethabi::ParamType::Tuple(vec![
                ethabi::ParamType::Address,  // assetA
                ethabi::ParamType::Address,  // assetB
                ethabi::ParamType::Uint(24), // bundleFee
                ethabi::ParamType::Uint(24), // unlockedFee
                ethabi::ParamType::Uint(24), // protocolUnlockedFee
            ])))],
            &call_input[4..],
        )
        .map_err(|e| format!("unable to decode call input: {:?}", e))?;

        if let Some(ethabi::Token::Array(updates_tokens)) = values.first() {
            let mut updates = Vec::new();

            for update_token in updates_tokens {
                if let ethabi::Token::Tuple(tuple_tokens) = update_token {
                    if tuple_tokens.len() == 5 {
                        if let (
                            ethabi::Token::Address(asset_a),
                            ethabi::Token::Address(asset_b),
                            ethabi::Token::Uint(bundle_fee),
                            ethabi::Token::Uint(unlocked_fee),
                            ethabi::Token::Uint(protocol_unlocked_fee),
                        ) = (
                            &tuple_tokens[0],
                            &tuple_tokens[1],
                            &tuple_tokens[2],
                            &tuple_tokens[3],
                            &tuple_tokens[4],
                        ) {
                            // Convert addresses to 20 bytes (remove padding)
                            let asset_a_bytes = asset_a.as_bytes()[12..].to_vec();
                            let asset_b_bytes = asset_b.as_bytes()[12..].to_vec();

                            // Convert fees to 3-byte arrays (U24)
                            let mut bundle_fee_temp = [0u8; 32];
                            bundle_fee.to_big_endian(&mut bundle_fee_temp);
                            let bundle_fee_bytes = bundle_fee_temp[29..32].to_vec();

                            let mut unlocked_fee_temp = [0u8; 32];
                            unlocked_fee.to_big_endian(&mut unlocked_fee_temp);
                            let unlocked_fee_bytes = unlocked_fee_temp[29..32].to_vec();

                            let mut protocol_unlocked_fee_temp = [0u8; 32];
                            protocol_unlocked_fee.to_big_endian(&mut protocol_unlocked_fee_temp);
                            let protocol_unlocked_fee_bytes =
                                protocol_unlocked_fee_temp[29..32].to_vec();

                            updates.push(PoolUpdate {
                                asset_a: asset_a_bytes,
                                asset_b: asset_b_bytes,
                                bundle_fee: bundle_fee_bytes,
                                unlocked_fee: unlocked_fee_bytes,
                                protocol_unlocked_fee: protocol_unlocked_fee_bytes,
                            });
                        }
                    }
                }
            }

            Ok(BatchUpdatePools { updates })
        } else {
            Err("Failed to decode updates array".to_string())
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoolRemoved {
    pub asset0: Vec<u8>,
    pub asset1: Vec<u8>,
    pub tick_spacing: Vec<u8>,
    pub fee_in_e6: Vec<u8>,
}

impl PoolRemoved {
    const TOPIC_ID: [u8; 32] =
        hex!("79f612570217c00df128c1b828dd6b321b3a70ae4c61b7a97fe4a71fc19df9ba");

    pub fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
        if log.topics.len() != 3usize {
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
                ethabi::ParamType::Int(24),  // tickSpacing
                ethabi::ParamType::Uint(24), // feeInE6
            ],
            log.data.as_ref(),
        )
        .map_err(|e| format!("unable to decode log.data: {:?}", e))?;
        values.reverse();

        // Extract indexed parameters from topics (last 20 bytes for addresses)
        let asset0 = log
            .topics
            .get(1)
            .expect("asset0 topic missing")
            .iter()
            .skip(12)
            .copied()
            .collect::<Vec<u8>>();
        let asset1 = log
            .topics
            .get(2)
            .expect("asset1 topic missing")
            .iter()
            .skip(12)
            .copied()
            .collect::<Vec<u8>>();

        Ok(Self {
            asset0,
            asset1,
            tick_spacing: {
                let uint_val = values
                    .pop()
                    .expect("Missing tickSpacing")
                    .into_int()
                    .expect("Invalid tickSpacing");
                let mut v = [0 as u8; 32];
                uint_val.to_big_endian(&mut v);
                v.to_vec()
            },
            fee_in_e6: {
                let uint_val = values
                    .pop()
                    .expect("Missing feeInE6")
                    .into_uint()
                    .expect("Invalid feeInE6");
                let mut v = [0 as u8; 32];
                uint_val.to_big_endian(&mut v);
                v[29..32].to_vec()
            },
        })
    }
}

impl substreams_ethereum::Event for PoolConfigured {
    const NAME: &'static str = "PoolConfigured";
    fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
        Self::match_log(log)
    }
    fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
        Self::decode(log)
    }
}

impl substreams_ethereum::Event for PoolRemoved {
    const NAME: &'static str = "PoolRemoved";
    fn match_log(log: &substreams_ethereum::pb::eth::v2::Log) -> bool {
        Self::match_log(log)
    }
    fn decode(log: &substreams_ethereum::pb::eth::v2::Log) -> Result<Self, String> {
        Self::decode(log)
    }
}
