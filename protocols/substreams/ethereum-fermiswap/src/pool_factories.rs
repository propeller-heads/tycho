use serde::Deserialize;
use tiny_keccak::{Hasher, Keccak};
use tycho_substreams::models::{ImplementationType, ProtocolComponent};

pub const ACTIVE_ATTRIBUTE: &str = "active";
pub const MANUAL_UPDATES_ATTRIBUTE: &str = "manual_updates";
pub const PAIR_STORE_PREFIX: &str = "pair:";
pub const VAULT_COMPONENT_ID: &str = "vault";
pub const PROTOCOL_TYPE_NAME: &str = "fermiswap_pair";

#[derive(Debug, Deserialize)]
pub struct DeploymentConfig {
    #[serde(with = "hex::serde")]
    pub engine_address: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub trader_vault: Vec<u8>,
    pub start_block: u64,
    pub init_tx_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairState {
    pub base_asset: Vec<u8>,
    pub quote_asset: Vec<u8>,
    pub active: bool,
}

impl PairState {
    pub fn new(base_asset: Vec<u8>, quote_asset: Vec<u8>, active: bool) -> Self {
        Self { base_asset, quote_asset, active }
    }

    pub fn id(&self) -> String {
        pair_id(&self.base_asset, &self.quote_asset)
    }

    pub fn encode(&self) -> String {
        format!(
            "{}:{}:{}",
            hex::encode(&self.base_asset),
            hex::encode(&self.quote_asset),
            if self.active { "1" } else { "0" }
        )
    }

    pub fn decode(value: &str) -> Option<Self> {
        let mut parts = value.split(':');
        let base_asset = hex::decode(parts.next()?).ok()?;
        let quote_asset = hex::decode(parts.next()?).ok()?;
        let active = parts.next()? == "1";
        Some(Self { base_asset, quote_asset, active })
    }
}

pub fn pair_id(base_asset: &[u8], quote_asset: &[u8]) -> String {
    let mut input = Vec::with_capacity(40);
    input.extend_from_slice(base_asset);
    input.extend_from_slice(quote_asset);

    let mut out = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(&input);
    hasher.finalize(&mut out);

    format!("0x{}", hex::encode(out))
}

pub fn pair_store_key(pair_id: &str) -> String {
    format!("{PAIR_STORE_PREFIX}{pair_id}")
}

pub fn token_store_key(token: &[u8]) -> String {
    hex::encode(token)
}

pub fn vault_balance_key(token: &[u8]) -> String {
    format!("{VAULT_COMPONENT_ID}:{}", token_store_key(token))
}

pub fn create_component(pair: &PairState, config: &DeploymentConfig) -> ProtocolComponent {
    ProtocolComponent::new(&pair.id())
        .with_tokens(&[pair.base_asset.as_slice(), pair.quote_asset.as_slice()])
        .with_contracts(&[config.engine_address.as_slice(), config.trader_vault.as_slice()])
        .with_attributes(&[(MANUAL_UPDATES_ATTRIBUTE, vec![1u8])])
        .as_swap_type(PROTOCOL_TYPE_NAME, ImplementationType::Vm)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_decode_config() {
        let config: DeploymentConfig = serde_qs::from_str(
            "engine_address=0001&trader_vault=0002&start_block=42&init_tx_hash=0xabc",
        )
        .unwrap();

        assert_eq!(config.engine_address, [0u8, 1u8]);
        assert_eq!(config.trader_vault, [0u8, 2u8]);
        assert_eq!(config.start_block, 42);
        assert_eq!(config.init_tx_hash, Some("0xabc".to_string()));
    }

    #[test]
    fn test_pair_id_is_directional() {
        let token_a = hex::decode("0000000000000000000000000000000000000001").unwrap();
        let token_b = hex::decode("0000000000000000000000000000000000000002").unwrap();

        assert_ne!(pair_id(&token_a, &token_b), pair_id(&token_b, &token_a));
    }

    #[test]
    fn test_pair_state_roundtrip() {
        let pair = PairState::new(vec![1u8; 20], vec![2u8; 20], false);

        assert_eq!(PairState::decode(&pair.encode()), Some(pair));
    }
}
