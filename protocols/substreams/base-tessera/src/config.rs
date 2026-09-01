use serde::Deserialize;

/// Deployment-specific addresses and storage layout for a Tessera venue.
///
/// Supplied through substreams `params` (see `base-tessera.yaml`) so the module can be
/// re-pointed at another deployment (e.g. BSC) or a pair-implementation generation with a
/// different storage layout without code changes. Addresses are hex, no `0x` prefix.
#[derive(Clone, Deserialize)]
pub struct DeploymentConfig {
    /// `TesseraSwap` — the verified swap/quote entrypoint.
    #[serde(with = "hex::serde")]
    pub tesseraswap: Vec<u8>,
    /// Pricing engine (TesseraSwap `slot0`); owns the pair registry.
    #[serde(with = "hex::serde")]
    pub engine: Vec<u8>,
    /// Code-only satellites (pair implementations, pricing libs, the write-path contract),
    /// concatenated 20-byte hex addresses. They are deployed top-level by rotating EOAs, so
    /// they cannot be discovered at creation time — each new generation is added here and the
    /// spkg re-released (see HANDOVER §9.3).
    pub tracked: String,
    /// TesseraSwap storage slot holding the treasury (inventory custodian).
    pub treasury_slot: u64,
    /// Fallback treasury for runs whose initial block is patched past the constructor write
    /// (the protocol-testing harness does this). A production sync from the package's real
    /// initial block witnesses every treasury write, so this value is never read there.
    #[serde(with = "hex::serde")]
    pub treasury: Vec<u8>,
    /// Base slot of the engine's `pairKey => pair address` mapping. The pair key is
    /// `keccak256(abi.encode(tokenLo, tokenHi))` over the pair's two tokens sorted ascending,
    /// so the entry lives at `keccak256(abi.encode(pairKey, pair_map_slot))`.
    pub pair_map_slot: u64,
    /// Pair-contract slot holding the base token.
    pub pair_base_token_slot: u64,
    /// Pair-contract slot holding the packed `decimals ‖ quote token`.
    pub pair_quote_token_slot: u64,
    /// Pair-contract slot holding the pricing-lib address (written after creation; a write is
    /// surfaced as a monitoring attribute — a new lib generation needs a params update).
    pub pair_lib_slot: u64,
}

impl DeploymentConfig {
    /// The `tracked` param split into 20-byte addresses.
    pub fn tracked_addresses(&self) -> Vec<Vec<u8>> {
        self.tracked
            .as_bytes()
            .chunks(40)
            .filter_map(|c| hex::decode(c).ok())
            .filter(|a| a.len() == 20)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARAMS: &str = "tesseraswap=55555522005bcae1c2424d474bfd5ed477749e3e\
                          &engine=31e99e05fee3dce580af777c3fd63ee1b3b40c17\
                          &tracked=f3be571a3a73201033b43bec1d1a566d45f590956d9dd143e42b6338f4f6a7c0c26d124658f641cb\
                          &treasury_slot=1\
                          &treasury=3dbe077e7986657e95e1cc50089f17a5a4af0aae\
                          &pair_map_slot=8\
                          &pair_base_token_slot=48\
                          &pair_quote_token_slot=49\
                          &pair_lib_slot=51";

    #[test]
    fn parses_params() {
        let config: DeploymentConfig = serde_qs::from_str(PARAMS).unwrap();
        assert_eq!(config.tesseraswap.len(), 20);
        assert_eq!(config.engine.len(), 20);
        assert_eq!(hex::encode(&config.treasury), "3dbe077e7986657e95e1cc50089f17a5a4af0aae");
        assert_eq!(config.treasury_slot, 1);
        assert_eq!(config.pair_map_slot, 8);
        assert_eq!(config.pair_base_token_slot, 48);
        assert_eq!(config.pair_quote_token_slot, 49);
        assert_eq!(config.pair_lib_slot, 51);
        let tracked = config.tracked_addresses();
        assert_eq!(tracked.len(), 2);
        assert_eq!(hex::encode(&tracked[0]), "f3be571a3a73201033b43bec1d1a566d45f59095");
        assert_eq!(hex::encode(&tracked[1]), "6d9dd143e42b6338f4f6a7c0c26d124658f641cb");
    }

    #[test]
    fn tracked_addresses_of_empty_string_is_empty() {
        let config: DeploymentConfig =
            serde_qs::from_str(&PARAMS.replace(
                "tracked=f3be571a3a73201033b43bec1d1a566d45f590956d9dd143e42b6338f4f6a7c0c26d124658f641cb",
                "tracked=",
            ))
            .unwrap();
        assert!(config.tracked_addresses().is_empty());
    }
}
