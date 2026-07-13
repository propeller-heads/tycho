use crate::pb::cowamm::{
    BindingChangeType, CowPool, CowPoolBind, CowPoolCreations, CowPools, Transaction,
};
use anyhow::{anyhow, Context, Ok, Result};
use serde::{Deserialize, Serialize};
use substreams::store::{StoreGet, StoreGetString};

#[derive(Debug, Deserialize, Serialize)]
struct CowPoolBindJson {
    address: String,
    token: String,
    weight: String,
    amount: String,
    //fields for Bind Transaction
    from: String,
    to: String,
    hash: String,
    index: String,
    ordinal: String,
    #[serde(default)]
    change_type: i32,
}

fn decode_hex(value: &str, field: &str) -> Result<Vec<u8>> {
    hex::decode(value).with_context(|| format!("invalid hex in CowAMM binding field {field}"))
}

fn decode_u64_le(value: &str, field: &str) -> Result<u64> {
    let bytes: [u8; 8] = decode_hex(value, field)?
        .try_into()
        .map_err(|_| anyhow!("CowAMM binding field {field} must be exactly 8 bytes"))?;
    Ok(u64::from_le_bytes(bytes))
}

pub fn parse_binds(bind_str: &str) -> Result<Vec<CowPoolBind>> {
    let bind_strs: Vec<&str> = bind_str.split(';').collect();
    let mut active_binds: Vec<CowPoolBind> = Vec::new();
    for bind in bind_strs {
        let bind = bind.trim();
        // Skip empty strings (which can happen if there are extra semicolons)
        if bind.is_empty() {
            continue;
        }
        // Wrap the bind in square brackets to create an array of JSON objects
        let formatted_str = format!("[{}]", bind.replace("};", "},"));

        let parsed: Vec<CowPoolBindJson> = serde_json::from_str(&formatted_str)
            .context("failed to parse CowAMM binding history")?;
        for bind_json in parsed {
            let token = decode_hex(&bind_json.token, "token")?;
            active_binds.retain(|bind| bind.token != token);
            let change_type = BindingChangeType::from_i32(bind_json.change_type)
                .context("invalid CowAMM binding change type")?;
            // Histories written before v0.1.4 have no change_type and contain bind events only.
            match change_type {
                BindingChangeType::Unbind => continue,
                BindingChangeType::Unspecified | BindingChangeType::Bind => {}
            }
            let cow_bind = CowPoolBind {
                address: decode_hex(&bind_json.address, "address")?,
                token,
                weight: decode_hex(&bind_json.weight, "weight")?,
                amount: decode_hex(&bind_json.amount, "amount")?,
                tx: Some(Transaction {
                    from: decode_hex(&bind_json.from, "transaction from")?,
                    to: decode_hex(&bind_json.to, "transaction to")?,
                    hash: decode_hex(&bind_json.hash, "transaction hash")?,
                    index: decode_u64_le(&bind_json.index, "transaction index")?,
                }),
                ordinal: decode_u64_le(&bind_json.ordinal, "ordinal")?,
                change_type: BindingChangeType::Bind.into(),
            };
            active_binds.push(cow_bind);
        }
    }
    Ok(active_binds)
}

#[substreams::handlers::map]
pub fn map_cowpools(
    creations: CowPoolCreations,
    binds: StoreGetString,
) -> Result<CowPools, substreams::errors::Error> {
    let mut pools: Vec<CowPool> = Vec::new();

    let creations = &creations;
    let binds = &binds;

    for creation in creations.pools.iter() {
        let base_key = hex::encode(&creation.address);
        let bind_history = match binds.get_at(creation.ordinal, &base_key) {
            Some(data) => data,
            None => continue, // skip if no bind found
        };

        let parsed_binds = parse_binds(&bind_history)?;
        if parsed_binds.len() != 2 {
            continue;
        }
        let bind1 = &parsed_binds[0];
        let bind2 = &parsed_binds[1];

        let (token_a, weight_a, token_b, weight_b) = if bind1.token < bind2.token {
            (&bind1.token, &bind1.weight, &bind2.token, &bind2.weight)
        } else {
            (&bind2.token, &bind2.weight, &bind1.token, &bind1.weight)
        };
        pools.push(CowPool {
            address: creation.address.clone(),
            token_a: token_a.clone(),
            token_b: token_b.clone(),
            lp_token: creation.lp_token.clone(),
            weight_a: weight_a.to_vec(),
            weight_b: weight_b.to_vec(),
            fee: 0,
            created_tx_hash: creation.created_tx_hash.clone(),
        });
    }

    Ok(CowPools { pools })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_literal::hex;

    #[test]
    fn test_parse_binds_single_entry() {
        let bind_str = r#"{"address":"9bd702e05b9c97e4a4a3e47df1e0fe7a0c26d2f1","token":"def1ca1fb7fbcdc777520aa7f396b4e015f497ab","weight":"0000000000000000000000000000000000000000000000000de0b6b3a7640000","amount":"0000000000000000000000000000000000000000000000000000000000000001","from":"1234567890123456789012345678901234567890","to":"abcdefabcdefabcdefabcdefabcdefabcdefabcd","hash":"fedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcba","index":"0100000000000000","ordinal":"0200000000000000"}"#;
        let binds = parse_binds(bind_str).expect("binding history should parse");
        assert_eq!(binds.len(), 1);

        assert_eq!(binds[0].address, hex!("9bd702e05b9c97e4a4a3e47df1e0fe7a0c26d2f1"));
        assert_eq!(binds[0].token, hex!("def1ca1fb7fbcdc777520aa7f396b4e015f497ab"));
        assert_eq!(
            binds[0].weight,
            hex!("0000000000000000000000000000000000000000000000000de0b6b3a7640000")
        );
    }

    #[test]
    fn test_parse_binds_multiple_entries() {
        let bind_str = r#"{"address":"9bd702e05b9c97e4a4a3e47df1e0fe7a0c26d2f1","token":"def1ca1fb7fbcdc777520aa7f396b4e015f497ab","weight":"0000000000000000000000000000000000000000000000000de0b6b3a7640000","amount":"0000000000000000000000000000000000000000000000000000000000000001","from":"1234567890123456789012345678901234567890","to":"abcdefabcdefabcdefabcdefabcdefabcdefabcd","hash":"fedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcba","index":"0100000000000000","ordinal":"0200000000000000"};{"address":"9bd702e05b9c97e4a4a3e47df1e0fe7a0c26d2f1","token":"7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0","weight":"0000000000000000000000000000000000000000000000000de0b6b3a7640000","amount":"0000000000000000000000000000000000000000000000000000000000000002","from":"1234567890123456789012345678901234567890","to":"abcdefabcdefabcdefabcdefabcdefabcdefabcd","hash":"fedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcba","index":"0100000000000000","ordinal":"0200000000000000"}"#;
        let binds = parse_binds(bind_str).expect("binding history should parse");
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[0].address, hex!("9bd702e05b9c97e4a4a3e47df1e0fe7a0c26d2f1"));
        assert_eq!(binds[0].token, hex!("def1ca1fb7fbcdc777520aa7f396b4e015f497ab"));

        assert_eq!(binds[1].address, hex!("9bd702e05b9c97e4a4a3e47df1e0fe7a0c26d2f1"));
        assert_eq!(binds[1].token, hex!("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0"));
    }

    #[test]
    fn test_parse_binds_returns_only_active_bindings_after_rebind() {
        fn binding_change(
            token: Vec<u8>,
            weight: Vec<u8>,
            amount: Vec<u8>,
            ordinal: u64,
            change_type: BindingChangeType,
        ) -> CowPoolBind {
            CowPoolBind {
                address: hex!("9bd702e05b9c97e4a4a3e47df1e0fe7a0c26d2f1").to_vec(),
                token,
                weight,
                amount,
                tx: Some(Transaction {
                    from: hex!("1234567890123456789012345678901234567890").to_vec(),
                    to: hex!("abcdefabcdefabcdefabcdefabcdefabcdefabcd").to_vec(),
                    hash: hex!(
                        "fedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcbafedcba"
                    )
                    .to_vec(),
                    index: ordinal,
                }),
                ordinal,
                change_type: change_type.into(),
            }
        }

        let token_a = hex!("def1ca1fb7fbcdc777520aa7f396b4e015f497ab").to_vec();
        let token_b = hex!("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0").to_vec();
        let changes = [
            binding_change(
                token_a.clone(),
                hex!("01").to_vec(),
                hex!("0a").to_vec(),
                1,
                BindingChangeType::Bind,
            ),
            binding_change(token_a.clone(), vec![], vec![], 2, BindingChangeType::Unbind),
            binding_change(
                token_b,
                hex!("02").to_vec(),
                hex!("14").to_vec(),
                3,
                BindingChangeType::Bind,
            ),
            binding_change(
                token_a.clone(),
                hex!("03").to_vec(),
                hex!("1e").to_vec(),
                4,
                BindingChangeType::Bind,
            ),
        ];
        let bind_str = changes
            .iter()
            .map(crate::modules::store_cowpool_binds::serialize_binding_change)
            .collect::<Result<Vec<_>>>()
            .expect("binding changes should serialize")
            .join(";");

        let binds = parse_binds(&bind_str).expect("two bindings should remain active");

        assert_eq!(binds.len(), 2);
        let rebound = binds
            .iter()
            .find(|bind| bind.token == token_a)
            .expect("rebound token should be active");
        assert_eq!(rebound.weight, hex!("03"));
        assert_eq!(rebound.amount, hex!("1e"));
    }

    #[test]
    fn test_parse_binds_invalid_json() {
        let error = parse_binds("invalid_json").expect_err("invalid JSON should return an error");
        assert!(error
            .to_string()
            .contains("failed to parse CowAMM binding history"));
    }
}
