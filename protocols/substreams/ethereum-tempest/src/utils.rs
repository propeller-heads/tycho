use serde::Deserialize;
use tiny_keccak::{Hasher, Keccak};

pub const OVERRIDE_BLOCK_TIMESTAMP_ATTRIBUTE: &str = "override_block_timestamp";
pub const BALANCE_OWNER_ATTRIBUTE: &str = "balance_owner";

/// Component-index store key holding every component id the package has created.
///
/// Deliberately not a valid `token:` key, so the balance modules can tell the two apart.
pub const ALL_COMPONENTS_KEY: &str = "all";

#[derive(Debug, Deserialize)]
pub struct Config {
    /// The Tempest router (`TempestEth` proxy) — emits `PairRegistered` and settles swaps.
    #[serde(with = "hex::serde")]
    pub router_address: Vec<u8>,
    /// The `TempestVault` holding all pair inventory.
    #[serde(with = "hex::serde")]
    pub vault_address: Vec<u8>,
    /// The shared `PrioUpdateRegistry` the router reads quote lanes from.
    #[serde(with = "hex::serde")]
    pub registry_address: Vec<u8>,
}

/// Computes the Tempest lane index for a token pair.
///
/// Mirrors the on-chain `Tempest.laneFor`: `keccak256(abi.encodePacked(token0, token1))` over the
/// ascending-sorted pair. Because the router keys both the registry lane and the `pairRegistered`
/// mapping off this value, it doubles as the Tycho component id — a registry `updateState` lane
/// index resolves to a component without any extra lookup table.
///
/// Returns the `0x`-prefixed 32-byte hex string.
pub fn component_id(token_a: &[u8], token_b: &[u8]) -> String {
    let (token0, token1) = sort_tokens(token_a, token_b);

    let mut out = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(token0);
    hasher.update(token1);
    hasher.finalize(&mut out);

    format!("0x{}", hex::encode(out))
}

/// Orders a token pair ascending by address, matching the router's canonical `token0 < token1`.
pub fn sort_tokens<'a>(token_a: &'a [u8], token_b: &'a [u8]) -> (&'a [u8], &'a [u8]) {
    if token_a < token_b {
        (token_a, token_b)
    } else {
        (token_b, token_a)
    }
}

/// Converts a decoded registry `laneIndex` uint256 into the component id it refers to.
///
/// The ABI decoder yields uint256 values as big-endian bytes without leading zeros; component ids
/// are the canonical zero-padded 32-byte form. Returns `None` for values that cannot be a
/// `keccak256` output (negative, or wider than 32 bytes).
pub fn lane_index_to_component_id(value: &substreams::scalar::BigInt) -> Option<String> {
    let (sign, bytes) = value.to_bytes_be();
    if sign == num_bigint::Sign::Minus || bytes.len() > 32 {
        return None;
    }

    let mut padded = [0u8; 32];
    padded[32 - bytes.len()..].copy_from_slice(&bytes);
    Some(format!("0x{}", hex::encode(padded)))
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

    /// Component ids must equal `Tempest.laneFor` for the live mainnet pairs, otherwise a registry
    /// `updateState` would never resolve to its component and quotes would go unpinned. Expected
    /// values were read from the deployed router at `0x00000003f1ec…2149`.
    #[test]
    fn test_component_id_matches_onchain_lane_for() {
        assert_eq!(
            component_id(&weth(), &usdt()),
            "0x2b2f5776e38002e0c013d0d89828fdb06fee595ea2d5ed4b194e3883e823e350"
        );
        assert_eq!(
            component_id(&usdc(), &weth()),
            "0x85053f65cd1ece2bb37b70c13d66eadebf2779df5ddd68cf12f3ccfdc6bfe760"
        );
        assert_eq!(
            component_id(&usdc(), &usdt()),
            "0x4aafb64a36177dc82e7ace74cf60cc655659bc049da9533b5f7a6881bea995c6"
        );
    }

    #[test]
    fn test_component_id_is_direction_independent() {
        assert_eq!(component_id(&usdc(), &weth()), component_id(&weth(), &usdc()));
    }

    #[test]
    fn test_sort_tokens_orders_ascending() {
        let (weth, usdc) = (weth(), usdc());
        let (token0, token1) = sort_tokens(&weth, &usdc);
        assert_eq!(token0, usdc.as_slice());
        assert_eq!(token1, weth.as_slice());
    }

    /// A lane index decoded from `updateState` calldata must round-trip to the component id
    /// emitted at pair creation.
    #[test]
    fn test_lane_index_to_component_id_round_trips() {
        let id = component_id(&usdc(), &weth());
        let lane = BigInt::from_unsigned_bytes_be(&hex::decode(&id[2..]).unwrap());
        assert_eq!(lane_index_to_component_id(&lane), Some(id));
    }

    /// Lane indices with leading zero bytes must still pad back to the full 32-byte id.
    #[test]
    fn test_lane_index_to_component_id_pads_leading_zeros() {
        assert_eq!(
            lane_index_to_component_id(&BigInt::from(1)),
            Some("0x0000000000000000000000000000000000000000000000000000000000000001".to_string())
        );
    }

    #[test]
    fn test_lane_index_to_component_id_rejects_negative() {
        assert_eq!(lane_index_to_component_id(&BigInt::from(-1)), None);
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
