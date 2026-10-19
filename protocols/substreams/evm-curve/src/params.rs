use crate::consts::ZERO_ADDRESS;
use anyhow::{Context, Result};
use hex::FromHex;
use serde::{
    de::{self, Deserializer},
    Deserialize,
};
use std::{collections::HashMap, iter::zip};
use substreams_ethereum::pb::eth::v2::TransactionTrace;
use tycho_substreams::prelude::*;

pub type Address = [u8; 20];

pub fn deserialize_address<'de, D>(deserializer: D) -> Result<Address, D::Error>
where
    D: Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    let trimmed = s.trim();

    if trimmed.is_empty() || trimmed == "0x" {
        return Ok(ZERO_ADDRESS);
    }

    let hex_str = trimmed
        .strip_prefix("0x")
        .unwrap_or(trimmed);

    let bytes = <[u8; 20]>::from_hex(hex_str)
        .map_err(|e| de::Error::custom(format!("invalid address hex: {}", e)))?;

    Ok(bytes)
}

#[derive(Debug, Deserialize)]
pub struct CurveParams {
    pub protocol_params: ProtocolParams,
    #[serde(default)]
    pool_params: Option<Vec<PoolQueryParams>>,
}

impl CurveParams {
    pub fn contracts_to_index(&self) -> Vec<Address> {
        [
            self.protocol_params.crypto_pool_factory,
            self.protocol_params
                .crypto_swap_ng_factory,
            self.protocol_params.tricrypto_factory,
            self.protocol_params.tricrypto_2_lp,
            self.protocol_params.tricrypto_2_math,
            self.protocol_params.tricrypto_factory,
            self.protocol_params
                .core_stableswap_factory,
        ]
        .into_iter()
        .filter(|addr| *addr != ZERO_ADDRESS)
        .collect()
    }

    fn pools_by_tx_hash(&self) -> HashMap<String, &PoolQueryParams> {
        self.pool_params
            .as_ref()
            .into_iter()
            .flatten()
            .map(|pool| (pool.tx_hash.clone(), pool))
            .collect()
    }
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct ProtocolParams {
    #[serde(deserialize_with = "deserialize_address")]
    pub meta_registry: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub crypto_pool_factory: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub meta_pool_factory: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub crypto_swap_ng_factory: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub tricrypto_factory: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub twocrypto_factory: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub stableswap_factory: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub core_stableswap_factory: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub weth: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub steth: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub rusdy: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub rusdy_blocklist: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub old_susd: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub new_susd: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub tricrypto_2_lp: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub tricrypto_2_math: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub twocrypto_custom_view: Address,

    #[serde(deserialize_with = "deserialize_address")]
    pub twocrypto_custom_math: Address,

    #[serde(default)]
    pub twocrypto_custom_implementation_ids: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq)]
struct PoolQueryParams {
    address: String,
    contracts: Option<Vec<String>>,
    tx_hash: String,
    tokens: Vec<String>,
    static_attribute_keys: Option<Vec<String>>,
    static_attribute_vals: Option<Vec<String>>,
    attribute_keys: Option<Vec<String>>,
    attribute_vals: Option<Vec<String>>,
}

/// This function parses the `params` string and extracts the pool query parameters. `params` are
///  comma-separated, URL-encoded (defined by `serde-qs`) strings, with each component defining the
///  pool query parameters defined in the struct above. We then iterate through the transactions in
///  a block, and then if the transaction hash matches our parameter, we emit a `ProtocolComponent`
///  defined by the metadata from above alongside some basic defaults that we know for Curve.
///
/// Static attributes are defined as a vector of tuples with the name and value of the attribute.
///  These contain things like the pool type, specific pool fees, etc. You can see
///  `pool_factories.rs` for an example of the modern curve pool attributes and also the ones chosen
///  for 3pool, etc.
///
/// This function can error based on some basic parsing errors and deeper down hex decoding errors
///  if various addresses are not formatted properly.
pub fn emit_specific_pools(
    params: &str,
    tx: &TransactionTrace,
) -> Result<Option<(ProtocolComponent, Vec<EntityChanges>)>> {
    let curve_params = parse_curve_params(params)?;
    create_component(tx, curve_params.pools_by_tx_hash())
}

fn create_component(
    tx: &TransactionTrace,
    pools: HashMap<String, &PoolQueryParams>,
) -> Result<Option<(ProtocolComponent, Vec<EntityChanges>)>> {
    let encoded_hash = hex::encode(tx.hash.clone());
    if let Some(pool) = pools.get(&encoded_hash) {
        Ok(Some((
            ProtocolComponent {
                id: format!("0x{}", pool.address.clone()),
                tokens: pool
                    .tokens
                    .clone()
                    .into_iter()
                    .map(|token| Result::Ok(hex::decode(token)?))
                    .collect::<Result<Vec<_>>>()
                    .with_context(|| "Token addresses were not formatted properly")?,
                static_att: zip(
                    pool.static_attribute_keys
                        .clone()
                        .unwrap_or(vec![]),
                    pool.static_attribute_vals
                        .clone()
                        .unwrap_or(vec![]),
                )
                .clone()
                .map(|(key, value)| Attribute {
                    name: key,
                    value: value.into(),
                    change: ChangeType::Creation.into(),
                })
                .collect::<Vec<_>>(),
                contracts: pool
                    .contracts
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|contract| {
                        hex::decode(contract)
                            .with_context(|| "Pool contracts was not formatted properly")
                    })
                    .chain(std::iter::once(
                        hex::decode(&pool.address)
                            .with_context(|| "Pool address was not formatted properly"),
                    ))
                    .collect::<Result<Vec<Vec<u8>>>>()?,
                change: ChangeType::Creation.into(),
                protocol_type: Some(ProtocolType {
                    name: "curve_pool".into(),
                    financial_type: FinancialType::Swap.into(),
                    attribute_schema: Vec::new(),
                    implementation_type: ImplementationType::Vm.into(),
                }),
            },
            vec![EntityChanges {
                component_id: format!("0x{}", pool.address.clone()),
                attributes: zip(
                    pool.attribute_keys
                        .clone()
                        .unwrap_or(vec![]),
                    pool.attribute_vals
                        .clone()
                        .unwrap_or(vec![]),
                )
                .clone()
                .map(|(key, value)| Attribute {
                    name: key,
                    value: value.into(),
                    change: ChangeType::Creation.into(),
                })
                .collect::<Vec<_>>(),
            }],
        )))
    } else {
        Ok(None)
    }
}

pub fn parse_curve_params(params: &str) -> Result<CurveParams, anyhow::Error> {
    let curve: CurveParams = serde_qs::from_str(params)
        .with_context(|| format!("Failed to parse curve params: {params}"))?;

    Ok(curve)
}

mod tests {
    #[test]
    fn test_deserialize_address_empty_and_zero() {
        use super::*;
        use serde::Deserialize;

        #[derive(Deserialize, Debug)]
        struct Test {
            #[serde(deserialize_with = "deserialize_address")]
            addr: Address,
        }

        let t: Test = serde_qs::from_str("addr=").unwrap();
        assert_eq!(t.addr, ZERO_ADDRESS);

        let t: Test = serde_qs::from_str("addr=0x").unwrap();
        assert_eq!(t.addr, ZERO_ADDRESS);

        let t: Test = serde_qs::from_str("addr=0000000000000000000000000000000000000000").unwrap();
        assert_eq!(t.addr, ZERO_ADDRESS);

        let t1: Test =
            serde_qs::from_str("addr=0x6b175474e89094c44da98b954eedeac495271d0f").unwrap();
        let t2: Test = serde_qs::from_str("addr=6b175474e89094c44da98b954eedeac495271d0f").unwrap();

        assert_eq!(t1.addr, t2.addr);

        let err = serde_qs::from_str::<Test>("addr=0x1234zz").unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("invalid address hex"), "unexpected error: {msg}");
    }

    #[test]
    fn test_contracts_to_index_filters_zero_address() {
        use super::*;

        let params = ProtocolParams {
            meta_registry: ZERO_ADDRESS,
            crypto_pool_factory: [1u8; 20],
            meta_pool_factory: ZERO_ADDRESS,
            crypto_swap_ng_factory: [2u8; 20],
            tricrypto_factory: ZERO_ADDRESS,
            twocrypto_factory: ZERO_ADDRESS,
            stableswap_factory: ZERO_ADDRESS,
            core_stableswap_factory: [3u8; 20],
            weth: ZERO_ADDRESS,
            steth: ZERO_ADDRESS,
            rusdy: ZERO_ADDRESS,
            rusdy_blocklist: ZERO_ADDRESS,
            old_susd: ZERO_ADDRESS,
            new_susd: ZERO_ADDRESS,
            tricrypto_2_lp: ZERO_ADDRESS,
            tricrypto_2_math: ZERO_ADDRESS,
            twocrypto_custom_view: ZERO_ADDRESS,
            twocrypto_custom_math: ZERO_ADDRESS,
            twocrypto_custom_implementation_ids: vec![],
        };

        let curve = CurveParams { protocol_params: params, pool_params: None };

        let contracts = curve.contracts_to_index();
        assert_eq!(contracts.len(), 3);
        assert!(contracts
            .iter()
            .all(|a| *a != ZERO_ADDRESS));
    }

    #[test]
    fn test_parse_curve_params() {
        use super::*;

        // Existing test case
        let params = r#"protocol_params[meta_registry]=f98b45fa17de75fb1ad0e7afd971b0ca00e379fc&protocol_params[crypto_pool_factory]=f18056bbd320e96a48e3fbf8bc061322531aac99&protocol_params[meta_pool_factory]=b9fc157394af804a3578134a6585c0dc9cc990d4&protocol_params[crypto_swap_ng_factory]=6a8cbed756804b16e05e741edabd5cb544ae21bf&protocol_params[tricrypto_factory]=0c0e5f2ff0ff18a3be9b835635039256dc4b4963&protocol_params[twocrypto_factory]=98ee851a00abee0d95d08cf4ca2bdce32aeaaf7f&protocol_params[stableswap_factory]=4f8846ae9380b90d2e71d5e3d042dff3e7ebb40d&protocol_params[core_stableswap_factory]=&protocol_params[weth]=c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2&protocol_params[steth]=ae7ab96520de3a18e5e111b5eaab095312d7fe84&protocol_params[rusdy]=af37c1167910ebc994e266949387d2c7c326b879&protocol_params[rusdy_blocklist]=d8c8174691d936e2c80114ec449037b13421b0a8&protocol_params[old_susd]=57ab1e02fee23774580c119740129eac7081e9d3&protocol_params[new_susd]=57ab1ec28d129707052df4df418d58a2d46d5f51&protocol_params[tricrypto_2_lp]=c4ad29ba4b3c580e6d59105fff484999997675ff&protocol_params[tricrypto_2_math]=40745803c2faa8e8402e2ae935933d07ca8f355c&protocol_params[twocrypto_custom_view]=35048188c02cbc9239e1e5ecb3761ef9dfdcd31f&protocol_params[twocrypto_custom_math]=79839c2d74531a8222c0f555865aac1834e82e51&protocol_params[twocrypto_custom_implementation_ids][]=110827960954786879070795645317684308345156454977361180728234664032152099907574&protocol_params[twocrypto_custom_implementation_ids][]=110205523814837221872401067839670671012439480455633721548677383351514213591649&pool_params[0][address]=bebc44782c7db0a1a60cb6fe97d0b483032ff1c7&pool_params[0][tx_hash]=20793bbf260912aae189d5d261ff003c9b9166da8191d8f9d63ff1c7722f3ac6&pool_params[0][tokens][]=6b175474e89094c44da98b954eedeac495271d0f&pool_params[0][tokens][]=a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48&pool_params[0][tokens][]=dac17f958d2ee523a2206206994597c13d831ec7&pool_params[0][static_attribute_keys][]=coins&pool_params[0][static_attribute_vals][]=["0x6b175474e89094c44da98b954eedeac495271d0f","0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48","0xdac17f958d2ee523a2206206994597c13d831ec7"]&pool_params[0][static_attribute_keys][]=name&pool_params[0][static_attribute_vals][]=3pool&pool_params[0][static_attribute_keys][]=factory_name&pool_params[0][static_attribute_vals][]=NA&pool_params[0][static_attribute_keys][]=factory&pool_params[0][static_attribute_vals][]=0x0000000000000000000000000000000000000000&pool_params[1][address]=dc24316b9ae028f1497c275eb9192a3ea0f67022&pool_params[1][tx_hash]=fac67ecbd423a5b915deff06045ec9343568edaec34ae95c43d35f2c018afdaa&pool_params[1][tokens][]=eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee&pool_params[1][tokens][]=ae7ab96520de3a18e5e111b5eaab095312d7fe84&pool_params[1][static_attribute_keys][]=coins&pool_params[1][static_attribute_vals][]=["0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","0xae7ab96520de3a18e5e111b5eaab095312d7fe84"]&pool_params[1][static_attribute_keys][]=rebase_tokens&pool_params[1][static_attribute_vals][]=["ae7ab96520de3a18e5e111b5eaab095312d7fe84"]&pool_params[1][static_attribute_keys][]=name&pool_params[1][static_attribute_vals][]=steth&pool_params[1][static_attribute_keys][]=factory_name&pool_params[1][static_attribute_vals][]=NA&pool_params[1][static_attribute_keys][]=factory&pool_params[1][static_attribute_vals][]=0x0000000000000000000000000000000000000000&pool_params[2][address]=d51a44d3fae010294c616388b506acda1bfaae46&pool_params[2][tx_hash]=dafb6385ed988ce8aacecfe1d97b38ea5e60b1ebce74d2423f71ddd621680138&pool_params[2][contracts][]=c4ad29ba4b3c580e6d59105fff484999997675ff&pool_params[2][contracts][]=40745803c2faa8e8402e2ae935933d07ca8f355c&pool_params[2][tokens][]=dac17f958d2ee523a2206206994597c13d831ec7&pool_params[2][tokens][]=2260fac5e5542a773aa44fbcfedf7c193bc2c599&pool_params[2][tokens][]=c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2&pool_params[2][static_attribute_keys][]=coins&pool_params[2][static_attribute_vals][]=["0xdac17f958d2ee523a2206206994597c13d831ec7","0x2260fac5e5542a773aa44fbcfedf7c193bc2c599","0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"]&pool_params[2][static_attribute_keys][]=name&pool_params[2][static_attribute_vals][]=tricrypto2&pool_params[2][static_attribute_keys][]=factory_name&pool_params[2][static_attribute_vals][]=NA&pool_params[2][static_attribute_keys][]=factory&pool_params[2][static_attribute_vals][]=0x0000000000000000000000000000000000000000&pool_params[2][attribute_keys][]=stateless_contract_addr_0&pool_params[2][attribute_vals][]=0x8F68f4810CcE3194B6cB6F3d50fa58c2c9bDD1d5&pool_params[3][address]=a5407eae9ba41422680e2e00537571bcc53efbfd&pool_params[3][tx_hash]=51aca4a03a395de8855fa2ca59b7febe520c2a223e69c502066162f7c1a95ec2&pool_params[3][tokens][]=6b175474e89094c44da98b954eedeac495271d0f&pool_params[3][tokens][]=a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48&pool_params[3][tokens][]=dac17f958d2ee523a2206206994597c13d831ec7&pool_params[3][tokens][]=57ab1ec28d129707052df4df418d58a2d46d5f51&pool_params[3][static_attribute_keys][]=coins&pool_params[3][static_attribute_vals][]=["0x6b175474e89094c44da98b954eedeac495271d0f","0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48","0xdac17f958d2ee523a2206206994597c13d831ec7","0x57ab1ec28d129707052df4df418d58a2d46d5f51"]&pool_params[3][static_attribute_keys][]=name&pool_params[3][static_attribute_vals][]=susd&pool_params[3][static_attribute_keys][]=factory_name&pool_params[3][static_attribute_vals][]=NA&pool_params[3][static_attribute_keys][]=factory&pool_params[3][static_attribute_vals][]=0x0000000000000000000000000000000000000000&pool_params[4][address]=dcef968d416a41cdac0ed8702fac8128a64241a2&pool_params[4][tx_hash]=1f4254004ce9e19d4eb742ee5a69d30f29085902d976f73e97c44150225ef775&pool_params[4][tokens][]=853d955acef822db058eb8505911ed77f175b99e&pool_params[4][tokens][]=a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48&pool_params[4][static_attribute_keys][]=coins&pool_params[4][static_attribute_vals][]=["0x853d955acef822db058eb8505911ed77f175b99e","0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"]&pool_params[4][static_attribute_keys][]=name&pool_params[4][static_attribute_vals][]=fraxusdc&pool_params[4][static_attribute_keys][]=factory_name&pool_params[4][static_attribute_vals][]=NA&pool_params[4][static_attribute_keys][]=factory&pool_params[4][static_attribute_vals][]=0x0000000000000000000000000000000000000000"#;

        let parsed = parse_curve_params(params).unwrap();

        let pools = parsed
            .pool_params
            .as_ref()
            .expect("pool_params should not be None");

        // --- pool assertions ---
        assert_eq!(pools.len(), 5);

        let p0 = &pools[0];

        assert_eq!(p0.address, "bebc44782c7db0a1a60cb6fe97d0b483032ff1c7");
        assert_eq!(p0.tx_hash, "20793bbf260912aae189d5d261ff003c9b9166da8191d8f9d63ff1c7722f3ac6");

        assert_eq!(
            p0.tokens,
            vec![
                "6b175474e89094c44da98b954eedeac495271d0f",
                "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "dac17f958d2ee523a2206206994597c13d831ec7",
            ]
        );

        // --- twocrypto_custom_implementation_ids assertions ---
        assert_eq!(
            parsed
                .protocol_params
                .twocrypto_custom_implementation_ids,
            vec![
                "110827960954786879070795645317684308345156454977361180728234664032152099907574",
                "110205523814837221872401067839670671012439480455633721548677383351514213591649",
            ]
        );
    }

    #[test]
    fn test_parse_curve_params_without_implementation_ids() {
        use super::*;

        let params = "protocol_params[meta_registry]=f98b45fa17de75fb1ad0e7afd971b0ca00e379fc\
            &protocol_params[crypto_pool_factory]=f18056bbd320e96a48e3fbf8bc061322531aac99\
            &protocol_params[meta_pool_factory]=b9fc157394af804a3578134a6585c0dc9cc990d4\
            &protocol_params[crypto_swap_ng_factory]=6a8cbed756804b16e05e741edabd5cb544ae21bf\
            &protocol_params[tricrypto_factory]=0c0e5f2ff0ff18a3be9b835635039256dc4b4963\
            &protocol_params[twocrypto_factory]=98ee851a00abee0d95d08cf4ca2bdce32aeaaf7f\
            &protocol_params[stableswap_factory]=4f8846ae9380b90d2e71d5e3d042dff3e7ebb40d\
            &protocol_params[core_stableswap_factory]=\
            &protocol_params[weth]=c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2\
            &protocol_params[steth]=ae7ab96520de3a18e5e111b5eaab095312d7fe84\
            &protocol_params[rusdy]=\
            &protocol_params[rusdy_blocklist]=\
            &protocol_params[old_susd]=\
            &protocol_params[new_susd]=\
            &protocol_params[tricrypto_2_lp]=\
            &protocol_params[tricrypto_2_math]=\
            &protocol_params[twocrypto_custom_view]=\
            &protocol_params[twocrypto_custom_math]=";

        let parsed = parse_curve_params(params).unwrap();
        assert!(parsed
            .protocol_params
            .twocrypto_custom_implementation_ids
            .is_empty());
    }
}
