use crate::pb::cowamm::{CowPool, CowPoolBind, CowPoolCreations, CowPools, Transaction};
use anyhow::{Ok, Result};
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
}

pub fn parse_binds(bind_str: &str) -> Option<Vec<CowPoolBind>> {
    let bind_strs: Vec<&str> = bind_str.split(';').collect();
    let mut binds = Vec::new();
    for bind in bind_strs {
        let bind = bind.trim();
        // Skip empty strings (which can happen if there are extra semicolons)
        if bind.is_empty() {
            continue;
        }
        // Wrap the bind in square brackets to create an array of JSON objects
        let formatted_str = format!("[{}]", bind.replace("};", "},"));

        let parsed: Vec<CowPoolBindJson> =
            serde_json::from_str(&formatted_str).expect("should successfully parse binds string");
        for bind_json in parsed {
            let cow_bind = CowPoolBind {
                address: hex::decode(&bind_json.address).expect("Invalid hex for address"),
                token: hex::decode(&bind_json.token).expect("Invalid hex for token"),
                weight: hex::decode(&bind_json.weight).expect("Invalid hex for weight"),
                amount: hex::decode(&bind_json.amount).expect("Invalid hex for amount"),
                tx: Some(Transaction {
                    from: hex::decode(&bind_json.from).expect("Invalid hex for tx from address"),
                    to: hex::decode(&bind_json.to).expect("Invalid hex for tx to address"),
                    hash: hex::decode(&bind_json.hash).expect("Invalid hex for tx to hash"),
                    index: u64::from_le_bytes(
                        //verify this
                        hex::decode(&bind_json.index)
                            .expect("Invalid hex for tx index")
                            .try_into()
                            .expect("tx index must be exactly 8 bytes"),
                    ),
                }),
                ordinal: u64::from_le_bytes(
                    //verify this
                    hex::decode(&bind_json.ordinal)
                        .expect("Invalid hex for tx ordinal")
                        .try_into()
                        .expect("ordinal must be exactly 8 bytes"),
                ),
            };
            binds.push(cow_bind);
        }
    }
    if binds.is_empty() {
        None
    } else {
        Some(binds)
    }
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
        let bind_first = match binds.get_first(&base_key) {
            Some(data) => data,
            None => continue, // skip if no bind found
        };

        let parsed_binds = match parse_binds(&bind_first) {
            Some(binds) if binds.len() == 2 => binds,
            _ => continue, // skip if parsing fails or not enough binds
        };
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
        let result = parse_binds(bind_str);

        assert!(result.is_some());
        let binds = result.unwrap();
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
        let result = parse_binds(bind_str);
        assert!(result.is_some());
        let binds = result.unwrap();
        assert_eq!(binds.len(), 2);
        assert_eq!(binds[0].address, hex!("9bd702e05b9c97e4a4a3e47df1e0fe7a0c26d2f1"));
        assert_eq!(binds[0].token, hex!("def1ca1fb7fbcdc777520aa7f396b4e015f497ab"));

        assert_eq!(binds[1].address, hex!("9bd702e05b9c97e4a4a3e47df1e0fe7a0c26d2f1"));
        assert_eq!(binds[1].token, hex!("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0"));
    }

    #[test]
    #[should_panic(expected = "should successfully parse binds string")]
    fn test_parse_binds_invalid_json() {
        let bind_str = r#"invalid_json"#;
        let _result = parse_binds(bind_str);
    }
}
