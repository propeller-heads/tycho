//! The Montra modules' query-string parameters, in the forms [`crate::address_list`] reads. A
//! key the module does not read is an error, so a misspelt key cannot leave a setting at a
//! default.

use std::collections::BTreeSet;

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::address_list::{parse_address, parse_address_list, Address};

/// Parameters of `map_montra_pools_created` and `map_montra_enriched_block_changes`: the
/// `PoolManager` whose `Initialize` and `ModifyLiquidity` events the modules read, and the Montra
/// hooks whose pools and storage writes are indexed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Params {
    pub(crate) pool_manager: Address,
    pub(crate) montra_hooks: BTreeSet<Address>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    pool_manager: String,
    montra_hooks: String,
}

impl Params {
    /// Parses `pool_manager=<address>&montra_hooks=<address>[,<address>...]`.
    ///
    /// Fails when `input` is not a query string, misses either key or holds another key, when
    /// `pool_manager` fails [`parse_address`], and when `montra_hooks` fails
    /// [`parse_address_list`].
    pub(crate) fn parse(input: &str) -> Result<Self> {
        let query: Query = serde_qs::from_str(input)
            .map_err(|e| anyhow!("failed to parse params `{input}`: {e}"))?;
        Ok(Self {
            pool_manager: parse_address("pool_manager", &query.pool_manager)?,
            montra_hooks: parse_address_list("montra_hooks", &query.montra_hooks)?
                .into_iter()
                .collect(),
        })
    }

    /// Whether `address` is one of the `montra_hooks`. An address that is not 20 bytes is none.
    pub(crate) fn is_montra_hook(&self, address: &[u8]) -> bool {
        Address::try_from(address).is_ok_and(|address| self.montra_hooks.contains(&address))
    }
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;
    use rstest::rstest;

    use super::*;
    use crate::montra::test_fixtures::{params, HOOK, POOL_MANAGER};

    const SECOND_HOOK: Address = hex!("0000000000000000000000000000000000000002");
    const PREFIX: &str = "pool_manager=e2e105d3f7209a9da11f83ccf3e7398a753823f1&montra_hooks=";

    #[test]
    fn test_params_read_both_keys() {
        let params = Params::parse(
            "pool_manager=e2e105d3f7209a9da11f83ccf3e7398a753823f1&montra_hooks=0x6B1216A40B65C83E9C9E18A562C446697B8CF8EC,0x0000000000000000000000000000000000000002",
        )
        .unwrap();

        assert_eq!(
            params,
            Params {
                pool_manager: POOL_MANAGER,
                montra_hooks: BTreeSet::from([HOOK, SECOND_HOOK])
            }
        );
    }

    #[test]
    fn test_params_reject_a_missing_key() {
        let error = Params::parse("montra_hooks=6b1216a40b65c83e9c9e18a562c446697b8cf8ec")
            .unwrap_err()
            .to_string();

        assert!(error.contains("missing field `pool_manager`"), "{error}");
    }

    #[test]
    fn test_params_reject_an_unknown_key() {
        let error =
            Params::parse(&format!("{PREFIX}6b1216a40b65c83e9c9e18a562c446697b8cf8ec&hooks=1"))
                .unwrap_err()
                .to_string();

        assert!(error.contains("unknown field `hooks`"), "{error}");
    }

    /// Each key goes through its `address_list` parser under its own name; that module's tests
    /// cover every malformed form.
    #[rstest]
    #[case::pool_manager(
        "pool_manager=0xzz&montra_hooks=6b1216a40b65c83e9c9e18a562c446697b8cf8ec",
        "`pool_manager` value `0xzz` is not hex"
    )]
    #[case::montra_hooks(
        "pool_manager=e2e105d3f7209a9da11f83ccf3e7398a753823f1&montra_hooks=6b1216a40b65c83e9c9e18a562c446697b8cf8ec,0x6B1216A40B65C83E9C9E18A562C446697B8CF8EC",
        "`montra_hooks` lists `0x6B1216A40B65C83E9C9E18A562C446697B8CF8EC` twice"
    )]
    fn test_params_reject_a_malformed_address(#[case] input: &str, #[case] expected: &str) {
        let error = Params::parse(input)
            .unwrap_err()
            .to_string();

        assert!(error.starts_with(expected), "{error}");
    }

    #[rstest]
    #[case::listed(&HOOK, true)]
    #[case::second_listed(&SECOND_HOOK, true)]
    #[case::not_listed(&POOL_MANAGER, false)]
    #[case::not_an_address(&HOOK[..19], false)]
    fn test_is_montra_hook_matches_only_listed_addresses(
        #[case] address: &[u8],
        #[case] expected: bool,
    ) {
        let params = Params::parse(&format!(
            "{PREFIX}6b1216a40b65c83e9c9e18a562c446697b8cf8ec,0000000000000000000000000000000000000002"
        ))
        .unwrap();

        assert_eq!(params.is_montra_hook(address), expected);
    }

    #[test]
    fn test_manifest_params_parse_to_the_elysium_deployment() {
        let manifest = include_str!("../../elysium-uniswap-v4-with-hooks.yaml");
        let param = |module: &str| {
            manifest
                .lines()
                .find_map(|line| {
                    line.trim()
                        .strip_prefix(&format!("{module}:"))
                })
                .map(|value| {
                    value
                        .trim()
                        .trim_matches('"')
                        .to_string()
                })
                .unwrap_or_else(|| panic!("the manifest sets params for {module}"))
        };
        assert_eq!(Params::parse(&param("map_montra_pools_created")).unwrap(), params());
        assert_eq!(Params::parse(&param("map_montra_enriched_block_changes")).unwrap(), params());
    }
}
