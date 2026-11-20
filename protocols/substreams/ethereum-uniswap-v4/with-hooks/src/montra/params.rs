//! The Montra and Ascend modules' query-string parameters, in the forms [`crate::address_list`]
//! reads. A key the module does not read is an error, so a misspelt key cannot leave a setting at a
//! default.

use std::collections::BTreeSet;

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::address_list::{parse_address, parse_address_list, Address};

/// Parameters of `map_montra_pools_created` and `map_montra_enriched_block_changes`: the
/// `PoolManager` whose `Initialize` and `ModifyLiquidity` events the modules read, the Montra hooks
/// and the Ascend hooks whose pools and storage writes are indexed.
///
/// The fields are private so that every value went through [`Params::new`], which refuses a hook
/// listed for both layouts.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Params {
    pool_manager: Address,
    montra_hooks: BTreeSet<Address>,
    ascend_hooks: BTreeSet<Address>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    pool_manager: String,
    montra_hooks: String,
    ascend_hooks: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AscendQuery {
    ascend_hooks: String,
}

/// Parses `ascend_hooks=<address>[,<address>...]`, the one parameter of `store_montra_slots`,
/// which only tells an Ascend hook's pools from a Montra hook's.
///
/// Fails when `input` is not a query string, misses the key or holds another key, and when the
/// list fails [`parse_address_list`].
pub(crate) fn parse_ascend_hooks(input: &str) -> Result<BTreeSet<Address>> {
    let query: AscendQuery =
        serde_qs::from_str(input).map_err(|e| anyhow!("failed to parse params `{input}`: {e}"))?;
    Ok(parse_address_list("ascend_hooks", &query.ascend_hooks)?
        .into_iter()
        .collect())
}

impl Params {
    /// Parses `pool_manager=<address>&montra_hooks=<address>[,<address>...]&ascend_hooks=
    /// <address>[,<address>...]`.
    ///
    /// Fails when `input` is not a query string, misses a key or holds another key, when
    /// `pool_manager` fails [`parse_address`], when `montra_hooks` or `ascend_hooks` fails
    /// [`parse_address_list`], and when a hook is in both lists: the two layouts differ, so a hook
    /// in both would be decoded as one of them by accident.
    pub(crate) fn parse(input: &str) -> Result<Self> {
        let query: Query = serde_qs::from_str(input)
            .map_err(|e| anyhow!("failed to parse params `{input}`: {e}"))?;
        let montra_hooks: BTreeSet<Address> =
            parse_address_list("montra_hooks", &query.montra_hooks)?
                .into_iter()
                .collect();
        let ascend_hooks: BTreeSet<Address> =
            parse_address_list("ascend_hooks", &query.ascend_hooks)?
                .into_iter()
                .collect();
        Self::new(parse_address("pool_manager", &query.pool_manager)?, montra_hooks, ascend_hooks)
    }

    /// The parameters of `pool_manager` and the two hook lists. Fails when a hook is in both
    /// lists: the two layouts differ, so a hook in both would be decoded as one of them by
    /// accident.
    pub(crate) fn new(
        pool_manager: Address,
        montra_hooks: BTreeSet<Address>,
        ascend_hooks: BTreeSet<Address>,
    ) -> Result<Self> {
        if let Some(shared) = montra_hooks
            .intersection(&ascend_hooks)
            .next()
        {
            return Err(anyhow!(
                "`montra_hooks` and `ascend_hooks` both list `0x{}`",
                hex::encode(shared)
            ));
        }
        Ok(Self { pool_manager, montra_hooks, ascend_hooks })
    }

    /// The `PoolManager` whose events the modules read.
    pub(crate) fn pool_manager(&self) -> &Address {
        &self.pool_manager
    }

    /// The hooks that use the Ascend layout.
    #[cfg(test)]
    pub(crate) fn ascend_hooks(&self) -> &BTreeSet<Address> {
        &self.ascend_hooks
    }

    /// Whether `address` is one of the `montra_hooks`. An address that is not 20 bytes is none.
    pub(crate) fn is_montra_hook(&self, address: &[u8]) -> bool {
        Address::try_from(address).is_ok_and(|address| self.montra_hooks.contains(&address))
    }

    /// Whether `address` is one of the `ascend_hooks`. An address that is not 20 bytes is none.
    pub(crate) fn is_ascend_hook(&self, address: &[u8]) -> bool {
        Address::try_from(address).is_ok_and(|address| self.ascend_hooks.contains(&address))
    }

    /// Every hook whose pools this package indexes.
    pub(crate) fn all_hooks(&self) -> BTreeSet<Address> {
        self.montra_hooks
            .union(&self.ascend_hooks)
            .copied()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;
    use rstest::rstest;

    use super::*;
    use crate::montra::test_fixtures::{params, ASCEND_HOOK, HOOK, POOL_MANAGER};

    const SECOND_HOOK: Address = hex!("0000000000000000000000000000000000000002");
    const PREFIX: &str = "pool_manager=e2e105d3f7209a9da11f83ccf3e7398a753823f1&montra_hooks=";
    const ASCEND: &str = "&ascend_hooks=e0def4221b9840a610d4e7d55812a79a6431a8ec";

    #[test]
    fn test_params_read_all_keys() {
        let params = Params::parse(
            "pool_manager=e2e105d3f7209a9da11f83ccf3e7398a753823f1&montra_hooks=0x6B1216A40B65C83E9C9E18A562C446697B8CF8EC,0x0000000000000000000000000000000000000002&ascend_hooks=0xE0DEF4221B9840A610D4E7D55812A79A6431A8EC",
        )
        .unwrap();

        assert_eq!(
            params,
            Params::new(
                POOL_MANAGER,
                BTreeSet::from([HOOK, SECOND_HOOK]),
                BTreeSet::from([ASCEND_HOOK])
            )
            .unwrap()
        );
    }

    #[rstest]
    #[case::pool_manager(
        "montra_hooks=6b1216a40b65c83e9c9e18a562c446697b8cf8ec&ascend_hooks=e0def4221b9840a610d4e7d55812a79a6431a8ec",
        "missing field `pool_manager`"
    )]
    #[case::ascend_hooks(
        "pool_manager=e2e105d3f7209a9da11f83ccf3e7398a753823f1&montra_hooks=6b1216a40b65c83e9c9e18a562c446697b8cf8ec",
        "missing field `ascend_hooks`"
    )]
    fn test_params_reject_a_missing_key(#[case] input: &str, #[case] expected: &str) {
        let error = Params::parse(input)
            .unwrap_err()
            .to_string();

        assert!(error.contains(expected), "{error}");
    }

    #[test]
    fn test_params_reject_an_unknown_key() {
        let error = Params::parse(&format!(
            "{PREFIX}6b1216a40b65c83e9c9e18a562c446697b8cf8ec{ASCEND}&hooks=1"
        ))
        .unwrap_err()
        .to_string();

        assert!(error.contains("unknown field `hooks`"), "{error}");
    }

    /// The two hook layouts differ, so a hook in both lists is refused.
    #[test]
    fn test_params_reject_a_hook_in_both_lists() {
        let error =
            Params::parse(&format!("{PREFIX}e0def4221b9840a610d4e7d55812a79a6431a8ec{ASCEND}"))
                .unwrap_err()
                .to_string();

        assert_eq!(
            error,
            "`montra_hooks` and `ascend_hooks` both list `0xe0def4221b9840a610d4e7d55812a79a6431a8ec`"
        );
    }

    /// Each key goes through its `address_list` parser under its own name; that module's tests
    /// cover every malformed form.
    #[rstest]
    #[case::pool_manager(
        "pool_manager=0xzz&montra_hooks=6b1216a40b65c83e9c9e18a562c446697b8cf8ec&ascend_hooks=e0def4221b9840a610d4e7d55812a79a6431a8ec",
        "`pool_manager` value `0xzz` is not hex"
    )]
    #[case::montra_hooks(
        "pool_manager=e2e105d3f7209a9da11f83ccf3e7398a753823f1&montra_hooks=6b1216a40b65c83e9c9e18a562c446697b8cf8ec,0x6B1216A40B65C83E9C9E18A562C446697B8CF8EC&ascend_hooks=e0def4221b9840a610d4e7d55812a79a6431a8ec",
        "`montra_hooks` lists `0x6B1216A40B65C83E9C9E18A562C446697B8CF8EC` twice"
    )]
    #[case::ascend_hooks_zero_address(
        "pool_manager=e2e105d3f7209a9da11f83ccf3e7398a753823f1&montra_hooks=6b1216a40b65c83e9c9e18a562c446697b8cf8ec&ascend_hooks=0000000000000000000000000000000000000000",
        "`ascend_hooks` lists the zero address"
    )]
    fn test_params_reject_a_malformed_address(#[case] input: &str, #[case] expected: &str) {
        let error = Params::parse(input)
            .unwrap_err()
            .to_string();

        assert!(error.starts_with(expected), "{error}");
    }

    #[rstest]
    #[case::montra(&HOOK, true, false)]
    #[case::second_montra(&SECOND_HOOK, true, false)]
    #[case::ascend(&ASCEND_HOOK, false, true)]
    #[case::not_listed(&POOL_MANAGER, false, false)]
    #[case::not_an_address(&HOOK[..19], false, false)]
    fn test_hook_lists_match_only_listed_addresses(
        #[case] address: &[u8],
        #[case] montra: bool,
        #[case] ascend: bool,
    ) {
        let params = Params::parse(&format!(
            "{PREFIX}6b1216a40b65c83e9c9e18a562c446697b8cf8ec,0000000000000000000000000000000000000002{ASCEND}"
        ))
        .unwrap();

        assert_eq!(params.is_montra_hook(address), montra);
        assert_eq!(params.is_ascend_hook(address), ascend);
    }

    #[test]
    fn test_all_hooks_is_both_lists() {
        assert_eq!(params().all_hooks(), BTreeSet::from([HOOK, ASCEND_HOOK]));
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
        for module in ["map_montra_pools_created", "map_montra_enriched_block_changes"] {
            assert_eq!(Params::parse(&param(module)).unwrap(), params(), "{module}");
        }
        assert_eq!(
            parse_ascend_hooks(&param("store_montra_slots")).unwrap(),
            params().ascend_hooks
        );
    }

    #[test]
    fn test_ascend_hooks_param_reads_the_list_and_refuses_other_keys() {
        assert_eq!(
            parse_ascend_hooks("ascend_hooks=0xE0DEF4221B9840A610D4E7D55812A79A6431A8EC").unwrap(),
            BTreeSet::from([ASCEND_HOOK])
        );
        let other_key = parse_ascend_hooks(&format!("pool_manager=e2e1{ASCEND}"))
            .unwrap_err()
            .to_string();
        assert!(other_key.contains("unknown field `pool_manager`"), "{other_key}");
        let missing = parse_ascend_hooks("")
            .unwrap_err()
            .to_string();
        assert!(missing.contains("missing field `ascend_hooks`"), "{missing}");
    }
}
