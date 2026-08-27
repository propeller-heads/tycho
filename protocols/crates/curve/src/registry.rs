//! The pools the processor knows about, and which addresses affect each of them.
//!
//! Built entirely from confirmed blocks. A pool created inside a pending block stays invisible
//! until `apply_block` registers it, matching what the indexer serves downstream.

use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
};

use alloy::primitives::Address as AlloyAddress;
use revm::DatabaseRef;
use tycho_common::{models::protocol::ProtocolComponent, Bytes};
use tycho_simulation::evm::{
    engine_db::engine_db_interface::EngineDatabaseInterface,
    protocol::curve::{resolve_variant, CurveVariant},
    simulation::SimulationEngine,
};

/// Curve's substreams encodes native ETH as `0xEee…EeE` in the `coins` attribute, while the
/// component's token list uses the zero address.
const ETH_SENTINEL: [u8; 20] = [0xEE; 20];

/// A Curve pool the processor tracks.
#[derive(Debug, Clone, PartialEq)]
pub struct CurvePool {
    /// Component id as the substreams emit it: `0x`-prefixed lower-case hex.
    pub id: String,
    pub address: AlloyAddress,
    /// Component tokens, as the substreams emit them (native ETH as the zero address).
    pub tokens: Vec<Bytes>,
    /// Number of coins the pool's indexed getters expose.
    pub n_coins: usize,
    pub variant: CurveVariant,
}

impl CurvePool {
    /// Whether the pool holds native ETH, whose balance is not tracked by transfer logs.
    pub fn holds_native_eth(&self) -> bool {
        self.tokens
            .iter()
            .any(|token| token.iter().all(|byte| *byte == 0))
    }
}

/// The registry of tracked pools, and the reverse index from an address to the pools it affects.
#[derive(Debug, Default)]
pub struct PoolRegistry {
    /// Keyed by normalized id (lower-case hex, no `0x`).
    pools: HashMap<String, CurvePool>,
    /// Address → normalized ids of the pools whose readings that address can change.
    affects: HashMap<AlloyAddress, HashSet<String>>,
}

impl PoolRegistry {
    /// Registers `component` if its static attributes describe a usable Curve pool.
    ///
    /// Returns `false` when the component is skipped: without a `coins` attribute or a resolvable
    /// variant its getters cannot be read, so tracking it would only produce failures later.
    /// `engine` is used for the variant's on-chain probing fallback and reads confirmed state.
    pub fn register<D: EngineDatabaseInterface + Clone + Debug>(
        &mut self,
        id: &str,
        component: &ProtocolComponent,
        engine: &SimulationEngine<D>,
    ) -> bool
    where
        <D as DatabaseRef>::Error: Debug,
        <D as EngineDatabaseInterface>::Error: Debug,
    {
        let key = normalize_id(id);
        let Ok(address_bytes) = hex::decode(&key) else { return false };
        if address_bytes.len() != 20 {
            return false;
        }
        let address = AlloyAddress::from_slice(&address_bytes);

        let Some(n_coins) = coin_count(&component.static_attributes) else { return false };
        let Ok(variant) = resolve_variant(&component.static_attributes, &address, n_coins, engine)
        else {
            return false;
        };

        self.affects
            .entry(address)
            .or_default()
            .insert(key.clone());
        for contract in &component.contract_addresses {
            if let Some(contract) = as_address(contract) {
                self.affects
                    .entry(contract)
                    .or_default()
                    .insert(key.clone());
            }
        }
        // A metapool's quote depends on its base pool's virtual price, so the base pool's storage
        // has to pull the metapool in too.
        if let Some(base_pool) = base_pool(&component.static_attributes) {
            self.affects
                .entry(base_pool)
                .or_default()
                .insert(key.clone());
        }

        self.pools.insert(
            key,
            CurvePool {
                id: emitted_id(id),
                address,
                tokens: component.tokens.clone(),
                n_coins,
                variant,
            },
        );
        true
    }

    pub fn remove(&mut self, id: &str) {
        let key = normalize_id(id);
        self.pools.remove(&key);
        self.affects.retain(|_, ids| {
            ids.remove(&key);
            !ids.is_empty()
        });
    }

    /// Every tracked pool a change to one of `addresses` can affect.
    pub fn affected_by<'a>(
        &self,
        addresses: impl IntoIterator<Item = &'a AlloyAddress>,
    ) -> Vec<&CurvePool> {
        let mut keys: HashSet<&String> = HashSet::new();
        for address in addresses {
            if let Some(ids) = self.affects.get(address) {
                keys.extend(ids);
            }
        }
        let mut affected: Vec<&CurvePool> = keys
            .into_iter()
            .filter_map(|key| self.pools.get(key))
            .collect();
        // Stable output regardless of hash order, so repeated calls agree.
        affected.sort_unstable_by_key(|pool| pool.address);
        affected
    }

    /// Whether any tracked pool is affected by changes at `address`.
    pub fn is_tracked(&self, address: &AlloyAddress) -> bool {
        self.affects.contains_key(address)
    }

    /// The pool whose component id normalizes to `id`, if tracked.
    pub fn get(&self, id: &str) -> Option<&CurvePool> {
        self.pools.get(&normalize_id(id))
    }

    pub fn len(&self) -> usize {
        self.pools.len()
    }
}

/// Canonical internal key for a component id: lower-case hex without the `0x` prefix.
pub fn normalize_id(id: &str) -> String {
    id.trim_start_matches("0x")
        .to_lowercase()
}

/// The id format the substreams emit: `0x`-prefixed lower-case hex.
pub fn emitted_id(id: &str) -> String {
    format!("0x{}", normalize_id(id))
}

/// Rewrites Curve's ETH sentinel to the zero address, leaving other addresses alone.
pub fn normalize_eth(address: Bytes) -> Bytes {
    if address.as_ref() == ETH_SENTINEL {
        Bytes::from(vec![0u8; 20])
    } else {
        address
    }
}

/// Coin count from the `coins` static attribute, the pool's on-chain coin order.
///
/// Taken from `coins` rather than `component.tokens`, which Tycho sorts by address and may
/// deduplicate, because the getters are indexed in on-chain order.
fn coin_count(static_attributes: &HashMap<String, Bytes>) -> Option<usize> {
    let raw = static_attributes.get("coins")?;
    let text = std::str::from_utf8(raw.as_ref()).ok()?;
    let coins: Vec<String> = serde_json::from_str(text).ok()?;
    (coins.len() >= 2).then_some(coins.len())
}

/// The `base_pool` static attribute, a `0x`-prefixed hex address stored as UTF-8 text.
fn base_pool(static_attributes: &HashMap<String, Bytes>) -> Option<AlloyAddress> {
    let raw = static_attributes.get("base_pool")?;
    let text = std::str::from_utf8(raw.as_ref()).ok()?;
    let bytes = hex::decode(text.trim_start_matches("0x")).ok()?;
    (bytes.len() == 20).then(|| AlloyAddress::from_slice(&bytes))
}

fn as_address(bytes: &Bytes) -> Option<AlloyAddress> {
    (bytes.len() == 20).then(|| AlloyAddress::from_slice(bytes.as_ref()))
}

#[cfg(test)]
mod tests {
    use tycho_simulation::evm::engine_db::tycho_db::PreCachedDB;

    use super::*;

    fn attrs(pairs: &[(&str, &str)]) -> HashMap<String, Bytes> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Bytes::from(v.as_bytes().to_vec())))
            .collect()
    }

    #[test]
    fn test_coin_count_reads_the_coins_attribute() {
        let coins = r#"["0xaa","0xbb","0xcc"]"#;
        assert_eq!(coin_count(&attrs(&[("coins", coins)])), Some(3));
    }

    #[test]
    fn test_coin_count_rejects_unusable_pools() {
        assert_eq!(coin_count(&HashMap::new()), None, "missing attribute");
        assert_eq!(coin_count(&attrs(&[("coins", r#"["0xaa"]"#)])), None, "single coin");
        assert_eq!(coin_count(&attrs(&[("coins", "not json")])), None, "malformed");
    }

    #[test]
    fn test_base_pool_parses_hex_text() {
        let address = "0xbebc44782c7db0a1a60cb6fe97d0b483032ff1c7";
        assert_eq!(
            base_pool(&attrs(&[("base_pool", address)])),
            Some(AlloyAddress::from_slice(
                &hex::decode("bebc44782c7db0a1a60cb6fe97d0b483032ff1c7").unwrap()
            ))
        );
        assert_eq!(base_pool(&attrs(&[("base_pool", "0xdeadbeef")])), None, "wrong length");
    }

    #[test]
    fn test_registry_starts_empty_and_rejects_unusable_components() {
        let mut registry = PoolRegistry::default();
        assert_eq!(registry.len(), 0);

        let engine: SimulationEngine<PreCachedDB> =
            SimulationEngine::new(PreCachedDB::new().unwrap(), false);
        let no_coins = ProtocolComponent::default();
        assert!(
            !registry.register("0xbebc44782c7db0a1a60cb6fe97d0b483032ff1c7", &no_coins, &engine),
            "a component without coins is unusable"
        );
        assert_eq!(registry.len(), 0, "a rejected component must not be tracked");
    }

    /// 3pool resolves its variant from the legacy address table, so registering it needs no VM.
    #[test]
    fn test_registering_a_legacy_pool_indexes_it_by_address() {
        let mut registry = PoolRegistry::default();
        let id = "0xbebc44782c7db0a1a60cb6fe97d0b483032ff1c7";
        let coins = format!(
            r#"["0x{}","0x{}","0x{}"]"#,
            "6b175474e89094c44da98b954eedeac495271d0f",
            "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            "dac17f958d2ee523a2206206994597c13d831ec7",
        );
        let lp_token = Bytes::from(vec![0x11u8; 20]);
        let component = ProtocolComponent {
            contract_addresses: vec![lp_token.clone()],
            static_attributes: attrs(&[("coins", &coins)]),
            ..Default::default()
        };
        let engine: SimulationEngine<PreCachedDB> =
            SimulationEngine::new(PreCachedDB::new().unwrap(), false);

        assert!(registry.register(id, &component, &engine));

        let pool = registry.get(id).expect("registered");
        assert_eq!(pool.id, id, "the emitted id keeps the substreams format");
        assert_eq!(pool.n_coins, 3);
        assert_eq!(pool.variant, CurveVariant::StableSwapV1);
        let pool_address = as_address(&Bytes::from(hex::decode(&id[2..]).unwrap())).unwrap();
        assert_eq!(
            registry
                .affected_by([&pool_address])
                .len(),
            1,
            "its own storage"
        );
        assert_eq!(
            registry
                .affected_by([&as_address(&lp_token).unwrap()])
                .len(),
            1,
            "a component contract must pull the pool in"
        );
        assert!(registry
            .affected_by([&AlloyAddress::ZERO])
            .is_empty());

        registry.remove(id);
        assert_eq!(registry.len(), 0);
        assert!(
            registry
                .affected_by([&pool_address])
                .is_empty(),
            "removal must clear the reverse index"
        );
    }

    #[test]
    fn test_ids_normalize_and_emit_consistently() {
        assert_eq!(normalize_id("0xAbCd"), "abcd");
        assert_eq!(normalize_id("abcd"), "abcd");
        assert_eq!(emitted_id("AbCd"), "0xabcd");
        assert_eq!(emitted_id("0xabcd"), "0xabcd");
    }

    #[test]
    fn test_normalize_eth_maps_only_the_sentinel() {
        assert_eq!(normalize_eth(Bytes::from(ETH_SENTINEL.to_vec())), Bytes::from(vec![0u8; 20]));
        let usdc = Bytes::from(vec![0xa0u8; 20]);
        assert_eq!(normalize_eth(usdc.clone()), usdc);
    }

    #[test]
    fn test_holds_native_eth_follows_the_token_list() {
        let pool = |tokens: Vec<Bytes>| CurvePool {
            id: "0x00".to_string(),
            address: AlloyAddress::ZERO,
            tokens,
            n_coins: 2,
            variant: CurveVariant::StableSwapV1,
        };
        assert!(
            pool(vec![Bytes::from(vec![0u8; 20]), Bytes::from(vec![1u8; 20])]).holds_native_eth()
        );
        assert!(
            !pool(vec![Bytes::from(vec![2u8; 20]), Bytes::from(vec![1u8; 20])]).holds_native_eth()
        );
    }
}
