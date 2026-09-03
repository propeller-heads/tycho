use serde::Deserialize;
use tiny_keccak::{Hasher, Keccak};

pub const OVERRIDE_BLOCK_TIMESTAMP_ATTRIBUTE: &str = "override_block_timestamp";
pub const BALANCE_OWNER_ATTRIBUTE: &str = "balance_owner";

/// Router-state store key holding the current `TempestVault` address, from `VaultUpdated`.
pub const VAULT_KEY: &str = "vault";
/// Router-state store key holding the router's `Pausable` flag: `"1"` while paused.
pub const PAUSED_KEY: &str = "paused";

/// Component-index store key holding every component id the package has created.
///
/// Deliberately not a valid `token:` key, so the balance modules can tell the two apart.
pub const ALL_COMPONENTS_KEY: &str = "all";

#[derive(Debug, Deserialize)]
pub struct Config {
    /// The Tempest router (`TempestEth` proxy) — emits `PairRegistered` and settles swaps.
    #[serde(with = "hex::serde")]
    pub router_address: Vec<u8>,
    /// The `TempestVault` holding all pair inventory, as of the package's `initialBlock`.
    ///
    /// Only a starting point: the live address is event-sourced from `VaultUpdated`, and this is
    /// the fallback for runs that start after that event. See `modules::vault_address`.
    #[serde(with = "hex::serde")]
    pub vault_address: Vec<u8>,
    /// The shared `PrioUpdateRegistry` the router reads quote lanes from.
    #[serde(with = "hex::serde")]
    pub registry_address: Vec<u8>,
}

/// Computes the Tycho component id for a token pair on a given Tempest router.
///
/// `keccak256(abi.encodePacked(router, token0, token1))` over the ascending-sorted pair. The
/// router address is mixed in because component ids are chain-global while the pair itself is
/// not: two pAMMs quoting the same pair would otherwise produce the same id and one of the two
/// components would be silently discarded.
///
/// Deliberately *not* the on-chain `laneFor` value, which is only `keccak256(token0, token1)` and
/// therefore not deployment-specific. Resolving a registry lane index back to a component goes
/// through the [`lane_key`] entry the package writes at registration. The id stays 32 bytes
/// because the VM adapter receives it as a `bytes32` pool id.
///
/// Returns the `0x`-prefixed 32-byte hex string.
pub fn component_id(router: &[u8], token_a: &[u8], token_b: &[u8]) -> String {
    let (token0, token1) = sort_tokens(token_a, token_b);

    let mut out = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(router);
    hasher.update(token0);
    hasher.update(token1);
    hasher.finalize(&mut out);

    format!("0x{}", hex::encode(out))
}

/// Computes the on-chain `Tempest.laneFor` value for a token pair.
///
/// `keccak256(abi.encodePacked(token0, token1))` over the ascending-sorted pair. This is the key
/// the router uses for both the registry lane and its `pairRegistered` mapping, so it is what a
/// registry `updateState` call carries — but it is not the component id; see [`component_id`].
pub fn lane_for(token_a: &[u8], token_b: &[u8]) -> [u8; 32] {
    let (token0, token1) = sort_tokens(token_a, token_b);

    let mut out = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(token0);
    hasher.update(token1);
    hasher.finalize(&mut out);
    out
}

/// Prefix distinguishing lane-mapping keys from component ids in the pair-registered store.
pub const LANE_KEY_PREFIX: &str = "lane:";

/// Store key under which the component id for a registry lane is recorded.
pub fn lane_key(lane: &[u8]) -> String {
    format!("{LANE_KEY_PREFIX}0x{}", hex::encode(lane))
}

/// Orders a token pair ascending by address, matching the router's canonical `token0 < token1`.
pub fn sort_tokens<'a>(token_a: &'a [u8], token_b: &'a [u8]) -> (&'a [u8], &'a [u8]) {
    if token_a < token_b {
        (token_a, token_b)
    } else {
        (token_b, token_a)
    }
}

/// Converts a decoded registry `laneIndex` uint256 into the [`lane_key`] it is recorded under.
///
/// The ABI decoder yields uint256 values as big-endian bytes without leading zeros; lane indices
/// are the canonical zero-padded 32-byte form. Returns `None` for values that cannot be a
/// `keccak256` output (negative, or wider than 32 bytes).
pub fn lane_index_to_lane_key(value: &substreams::scalar::BigInt) -> Option<String> {
    let (sign, bytes) = value.to_bytes_be();
    if sign == num_bigint::Sign::Minus || bytes.len() > 32 {
        return None;
    }

    let mut padded = [0u8; 32];
    padded[32 - bytes.len()..].copy_from_slice(&bytes);
    Some(lane_key(&padded))
}

#[cfg(test)]
mod tests {
    use substreams::scalar::BigInt;

    use super::*;

    fn addr(hex_str: &str) -> Vec<u8> {
        hex::decode(hex_str).unwrap()
    }

    fn weth() -> Vec<u8> {
        addr("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
    }

    fn usdc() -> Vec<u8> {
        addr("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")
    }

    fn usdt() -> Vec<u8> {
        addr("dac17f958d2ee523a2206206994597c13d831ec7")
    }

    fn router() -> Vec<u8> {
        addr("00000003f1ec2379e79f58e12ec6c4f51ee92149")
    }

    /// `lane_for` must equal the deployed router's `Tempest.laneFor`, otherwise a registry
    /// `updateState` would never resolve to its component and quotes would go unpinned. Expected
    /// values were read from the deployed router at `0x00000003f1ec…2149`.
    #[test]
    fn test_lane_for_matches_onchain() {
        assert_eq!(
            format!("0x{}", hex::encode(lane_for(&weth(), &usdt()))),
            "0x2b2f5776e38002e0c013d0d89828fdb06fee595ea2d5ed4b194e3883e823e350"
        );
        assert_eq!(
            format!("0x{}", hex::encode(lane_for(&usdc(), &weth()))),
            "0x85053f65cd1ece2bb37b70c13d66eadebf2779df5ddd68cf12f3ccfdc6bfe760"
        );
        assert_eq!(
            format!("0x{}", hex::encode(lane_for(&usdc(), &usdt()))),
            "0x4aafb64a36177dc82e7ace74cf60cc655659bc049da9533b5f7a6881bea995c6"
        );
    }

    /// The router must be mixed in, otherwise a second pAMM quoting the same pair would collide
    /// on a chain-global component id and one of the two would be silently dropped.
    #[test]
    fn test_component_id_is_router_scoped() {
        let other_router = addr("00000000000000000000000000000000deadbeef");
        assert_ne!(
            component_id(&router(), &usdc(), &weth()),
            component_id(&other_router, &usdc(), &weth())
        );
        assert_ne!(
            component_id(&router(), &usdc(), &weth()),
            format!("0x{}", hex::encode(lane_for(&usdc(), &weth())))
        );
    }

    #[test]
    fn test_component_id_is_direction_independent() {
        assert_eq!(
            component_id(&router(), &usdc(), &weth()),
            component_id(&router(), &weth(), &usdc())
        );
    }

    #[test]
    fn test_sort_tokens_orders_ascending() {
        let (weth, usdc) = (weth(), usdc());
        let (token0, token1) = sort_tokens(&weth, &usdc);
        assert_eq!(token0, usdc.as_slice());
        assert_eq!(token1, weth.as_slice());
    }

    /// A lane index decoded from `updateState` calldata must round-trip to the store key the
    /// package records the component under at registration.
    #[test]
    fn test_lane_index_to_lane_key_round_trips() {
        let lane = lane_for(&usdc(), &weth());
        let decoded = BigInt::from_unsigned_bytes_be(&lane);
        assert_eq!(lane_index_to_lane_key(&decoded), Some(lane_key(&lane)));
    }

    /// Lane indices with leading zero bytes must still pad back to the full 32-byte key.
    #[test]
    fn test_lane_index_to_lane_key_pads_leading_zeros() {
        assert_eq!(
            lane_index_to_lane_key(&BigInt::from(1)),
            Some(
                "lane:0x0000000000000000000000000000000000000000000000000000000000000001"
                    .to_string()
            )
        );
    }

    #[test]
    fn test_lane_index_to_lane_key_rejects_negative() {
        assert_eq!(lane_index_to_lane_key(&BigInt::from(-1)), None);
    }

    #[test]
    fn test_config_parses_substreams_params() {
        let config: Config = serde_qs::from_str(
            "router_address=00000003f1ec2379e79f58e12ec6c4f51ee92149\
             &vault_address=c9d748e601d9984a43da0b80e5b91dc28d31d9fb\
             &registry_address=DA7AFeEd01fe625cF15D187A19F94B45F00b8C5f",
        )
        .unwrap();

        assert_eq!(config.router_address, addr("00000003f1ec2379e79f58e12ec6c4f51ee92149"));
        assert_eq!(config.vault_address, addr("c9d748e601d9984a43da0b80e5b91dc28d31d9fb"));
        assert_eq!(config.registry_address, addr("da7afeed01fe625cf15d187a19f94b45f00b8c5f"));
    }
}
