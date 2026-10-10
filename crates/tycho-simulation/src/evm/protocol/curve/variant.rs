//! Resolves a Curve pool's [`CurveVariant`] from its static attributes, falling back to on-chain
//! probing when the factory/address are unknown.
//!
//! Static attributes carry the deploying `factory` address and a coarse `pool_type`. The factory
//! address determines the math variant for every factory-deployed pool; `pool_type` alone is
//! insufficient because the same value (`plain_pool`) maps to different variants across factories.
//! `twocrypto` factory pools split into NG vs Stable by their MATH contract.
use std::{collections::HashMap, fmt::Debug};

use alloy::primitives::{address, Address as AlloyAddress};
use revm::DatabaseRef;
use tycho_common::{simulation::errors::SimulationError, Bytes};

use crate::evm::{
    engine_db::engine_db_interface::EngineDatabaseInterface,
    protocol::curve::{
        adapter::{detect_variant, CurveVariant},
        vm,
    },
    simulation::SimulationEngine,
};

// Ethereum Curve factories (from `protocols/substreams/ethereum-curve/ethereum-params.json`).
const CRYPTO_POOL_FACTORY: AlloyAddress = address!("f18056bbd320e96a48e3fbf8bc061322531aac99");
const META_POOL_FACTORY: AlloyAddress = address!("b9fc157394af804a3578134a6585c0dc9cc990d4");
const CRYPTO_SWAP_NG_FACTORY: AlloyAddress = address!("6a8cbed756804b16e05e741edabd5cb544ae21bf");
const TRICRYPTO_FACTORY: AlloyAddress = address!("0c0e5f2ff0ff18a3be9b835635039256dc4b4963");
const TWOCRYPTO_FACTORY: AlloyAddress = address!("98ee851a00abee0d95d08cf4ca2bdce32aeaaf7f");
const STABLESWAP_FACTORY: AlloyAddress = address!("4f8846ae9380b90d2e71d5e3d042dff3e7ebb40d");

/// MATH contract of the legacy `v0.1.0` TwoCrypto deployment (StableSwap math, gamma ignored).
const TWOCRYPTO_CUSTOM_MATH: AlloyAddress = address!("79839c2d74531a8222c0f555865aac1834e82e51");

// Manually admitted legacy pools (no factory) that need an explicit variant.
const POOL_3POOL: AlloyAddress = address!("bebc44782c7db0a1a60cb6fe97d0b483032ff1c7");
const POOL_STETH: AlloyAddress = address!("dc24316b9ae028f1497c275eb9192a3ea0f67022");
const POOL_TRICRYPTO2: AlloyAddress = address!("d51a44d3fae010294c616388b506acda1bfaae46");
const POOL_FRAXUSDC: AlloyAddress = address!("dcef968d416a41cdac0ed8702fac8128a64241a2");

/// Factories that deploy a single, unambiguous math variant. The TwoCrypto factory is intentionally
/// absent (NG vs Stable needs the MATH contract — see [`resolve_twocrypto`]), as are unknown
/// factories (resolved by on-chain probing).
const FACTORY_VARIANTS: &[(AlloyAddress, CurveVariant)] = &[
    (CRYPTO_POOL_FACTORY, CurveVariant::TwoCryptoV1),
    (CRYPTO_SWAP_NG_FACTORY, CurveVariant::StableSwapNG),
    (STABLESWAP_FACTORY, CurveVariant::StableSwapNG),
    // This factory deploys plain pools and metapools, but only its plain-pool branch is live in
    // the substreams, and plain pools are the base template. Should the metapool branch be
    // enabled, its pools need `StableSwapMeta` and this entry becomes wrong for them: the factory
    // table is consulted before the probing that is the only source of `StableSwapMeta`.
    (META_POOL_FACTORY, CurveVariant::StableSwapV2),
    (TRICRYPTO_FACTORY, CurveVariant::TriCryptoNG),
];

/// Legacy pools deployed without a recognised factory, mapped to their known math variant.
const LEGACY_POOL_VARIANTS: &[(AlloyAddress, CurveVariant)] = &[
    (POOL_3POOL, CurveVariant::StableSwapV1),
    (POOL_STETH, CurveVariant::StableSwapSTETH),
    (POOL_TRICRYPTO2, CurveVariant::TriCryptoV1),
    (POOL_FRAXUSDC, CurveVariant::StableSwapV2),
];

/// Resolve the variant for `pool` from `static_attributes`, probing on-chain only when needed.
///
/// Returns the matched [`CurveVariant`], or a [`SimulationError`] if probing fails to classify the
/// pool.
pub fn resolve_variant<D: EngineDatabaseInterface + Clone + Debug>(
    static_attributes: &HashMap<String, Bytes>,
    pool: &AlloyAddress,
    n_coins: usize,
    engine: &SimulationEngine<D>,
) -> Result<CurveVariant, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let factory = attr_address(static_attributes, "factory");

    if let Some(f) = factory {
        if let Some(variant) = factory_variant(f) {
            return Ok(variant);
        }
        if f == TWOCRYPTO_FACTORY {
            return Ok(resolve_twocrypto(engine, pool));
        }
    }
    resolve_legacy_or_probe(engine, pool, n_coins)
}

/// Deterministic factory → variant mapping for factories that emit a single math variant.
///
/// Returns `None` for the TwoCrypto factory (NG vs Stable needs the MATH contract) and unknown
/// factories (require on-chain probing).
fn factory_variant(factory: AlloyAddress) -> Option<CurveVariant> {
    FACTORY_VARIANTS
        .iter()
        .find(|(addr, _)| *addr == factory)
        .map(|(_, variant)| *variant)
}

/// TwoCrypto factory pools split by their MATH contract: `v0.x` MATH is StableSwap math
/// (TwoCryptoStable, gamma ignored), `v2.x` is CryptoSwap math (TwoCryptoNG). Classification reads
/// `MATH().version()`; if the MATH contract exposes no `version()`, fall back to the known v0.1.0
/// address. Defaults to TwoCryptoNG when the MATH contract is unavailable.
fn resolve_twocrypto<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
) -> CurveVariant
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    let Some(math) = vm::read_math_address(engine, pool) else {
        return CurveVariant::TwoCryptoNG;
    };
    if let Some(version) = vm::read_version(engine, &math) {
        if version.starts_with("v0.") {
            return CurveVariant::TwoCryptoStable;
        }
        return CurveVariant::TwoCryptoNG;
    }
    if math == TWOCRYPTO_CUSTOM_MATH {
        CurveVariant::TwoCryptoStable
    } else {
        CurveVariant::TwoCryptoNG
    }
}

/// Resolve pools without a recognised factory: known legacy addresses first, then on-chain probing.
fn resolve_legacy_or_probe<D: EngineDatabaseInterface + Clone + Debug>(
    engine: &SimulationEngine<D>,
    pool: &AlloyAddress,
    n_coins: usize,
) -> Result<CurveVariant, SimulationError>
where
    <D as DatabaseRef>::Error: Debug,
    <D as EngineDatabaseInterface>::Error: Debug,
{
    if let Some((_, variant)) = LEGACY_POOL_VARIANTS
        .iter()
        .find(|(addr, _)| addr == pool)
    {
        return Ok(*variant);
    }
    let probing = vm::probe(engine, pool, n_coins);
    detect_variant(&probing)
        .map_err(|e| SimulationError::FatalError(format!("curve variant detection failed: {e}")))
}

/// Read an address-typed static attribute. The substreams stores addresses as the ASCII bytes of
/// their `"0x…"` string representation.
fn attr_address(attrs: &HashMap<String, Bytes>, key: &str) -> Option<AlloyAddress> {
    let raw = attrs.get(key)?;
    let text = String::from_utf8(raw.to_vec()).ok()?;
    text.trim().parse::<AlloyAddress>().ok()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn attrs(factory: &str) -> HashMap<String, Bytes> {
        let mut m = HashMap::new();
        // Stored as the ASCII bytes of the "0x…" string, matching the substreams encoding.
        m.insert("factory".to_string(), Bytes::from(factory.as_bytes().to_vec()));
        m
    }

    #[test]
    fn attr_address_decodes_ascii_hex() {
        let m = attrs("0xf18056bbd320e96a48e3fbf8bc061322531aac99");
        assert_eq!(attr_address(&m, "factory"), Some(CRYPTO_POOL_FACTORY));
    }

    #[test]
    fn attr_address_missing_is_none() {
        let m: HashMap<String, Bytes> = HashMap::new();
        assert_eq!(attr_address(&m, "factory"), None);
    }

    #[test]
    fn legacy_addresses_map_to_known_variants() {
        let three_pool =
            AlloyAddress::from_str("0xbebc44782c7db0a1a60cb6fe97d0b483032ff1c7").unwrap();
        assert_eq!(three_pool, POOL_3POOL);
    }

    #[test]
    fn factory_variant_maps_unambiguous_factories() {
        assert_eq!(factory_variant(CRYPTO_POOL_FACTORY), Some(CurveVariant::TwoCryptoV1));
        assert_eq!(factory_variant(CRYPTO_SWAP_NG_FACTORY), Some(CurveVariant::StableSwapNG));
        assert_eq!(factory_variant(STABLESWAP_FACTORY), Some(CurveVariant::StableSwapNG));
        assert_eq!(factory_variant(META_POOL_FACTORY), Some(CurveVariant::StableSwapV2));
        assert_eq!(factory_variant(TRICRYPTO_FACTORY), Some(CurveVariant::TriCryptoNG));
    }

    #[test]
    fn factory_variant_none_for_twocrypto_and_unknown() {
        // TwoCrypto needs the MATH contract to disambiguate NG vs Stable.
        assert_eq!(factory_variant(TWOCRYPTO_FACTORY), None);
        assert_eq!(factory_variant(AlloyAddress::ZERO), None);
    }
}
