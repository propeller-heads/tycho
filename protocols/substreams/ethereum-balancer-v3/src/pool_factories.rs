use crate::{abi, params::DeploymentConfig, utils::address_id};
use abi::{
    quantamm_weighted_pool_factory_contract::{
        events::PoolCreated as QuantAmmPoolCreated,
        functions::{
            Create as QuantAmmPoolCreate, CreateWithoutArgs as QuantAmmPoolCreateWithoutArgs,
        },
    },
    reclamm_pool_factory_contract::{
        events::PoolCreated as ReClammPoolCreated, functions::Create as ReClammPoolCreate,
    },
    stable_pool_factory_contract::{
        events::PoolCreated as StablePoolCreated, functions::Create as StablePoolCreate,
    },
    weighted_pool_factory_contract::{
        events::PoolCreated as WeightedPoolCreated, functions::Create as WeightedPoolCreate,
    },
};
use substreams::scalar::BigInt;
use substreams_ethereum::{
    pb::eth::v2::{Call, Log},
    Event, Function,
};
use tycho_substreams::{
    attributes::{json_serialize_address_list, json_serialize_bigint_list},
    prelude::*,
};

// Token config: (token_address, rate, rate_provider_address, is_exempt_from_yield_fees)
type TokenConfig = Vec<(Vec<u8>, substreams::scalar::BigInt, Vec<u8>, bool)>;

fn collect_rate_providers(tokens: &TokenConfig) -> Vec<Vec<u8>> {
    tokens
        .iter()
        .filter(|token| token.1 == BigInt::from(1)) // WITH_RATE == 1
        .map(|token| token.2.clone())
        .collect::<Vec<_>>()
}

fn should_skip_rate_provider_pool(config: &DeploymentConfig, rate_providers: &[Vec<u8>]) -> bool {
    config.skip_rate_provider_pools && !rate_providers.is_empty()
}

pub fn address_map(
    pool_factory_address: &[u8],
    log: &Log,
    call: &Call,
    config: &DeploymentConfig,
) -> Option<ProtocolComponent> {
    if pool_factory_address == config.weighted_factory.as_slice() {
        let WeightedPoolCreate {
            tokens: token_config,
            normalized_weights,
            swap_fee_percentage,
            ..
        } = WeightedPoolCreate::match_and_decode(call)?;
        let WeightedPoolCreated { pool } = WeightedPoolCreated::match_and_decode(log)?;
        let rate_providers = collect_rate_providers(&token_config);
        if should_skip_rate_provider_pool(config, &rate_providers) {
            return None;
        }

        // TODO: to add "buffers" support for boosted pools, we need to add the unwrapped
        // version of all ERC4626 tokens to the pool tokens list. Skipped for now - we need
        // to test that the adapter supports it correctly and ERC4626 overwrites are handled
        // correctly in simulation.
        let tokens = token_config
            .into_iter()
            .map(|t| t.0)
            .collect::<Vec<_>>();

        let normalized_weights_bytes = json_serialize_bigint_list(normalized_weights.as_slice());
        let fee_bytes = swap_fee_percentage.to_signed_bytes_be();
        let rate_providers_bytes = json_serialize_address_list(rate_providers.as_slice());

        let mut attributes = vec![
            ("pool_type", "WeightedPoolFactory".as_bytes()),
            ("normalized_weights", &normalized_weights_bytes),
            ("fee", &fee_bytes),
        ];

        if !rate_providers.is_empty() {
            attributes.push(("rate_providers", &rate_providers_bytes));
        }

        return Some(create_pool_component(&pool, tokens.as_slice(), &attributes, config));
    }

    if pool_factory_address == config.stable_factory.as_slice() {
        let StablePoolCreate { tokens: token_config, swap_fee_percentage, .. } =
            StablePoolCreate::match_and_decode(call)?;
        let StablePoolCreated { pool } = StablePoolCreated::match_and_decode(log)?;
        let rate_providers = collect_rate_providers(&token_config);
        if should_skip_rate_provider_pool(config, &rate_providers) {
            return None;
        }

        // TODO: to add "buffers" support for boosted pools, we need to add the unwrapped
        // version of all ERC4626 tokens to the pool tokens list. Skipped for now - we need
        // to test that the adapter supports it correctly and ERC4626 overwrites are handled
        // correctly in simulation.
        let tokens = token_config
            .into_iter()
            .map(|t| t.0)
            .collect::<Vec<_>>();

        let fee_bytes = swap_fee_percentage.to_signed_bytes_be();
        let rate_providers_bytes = json_serialize_address_list(rate_providers.as_slice());

        let mut attributes = vec![
            ("pool_type", "StablePoolFactory".as_bytes()),
            ("bpt", &pool),
            ("fee", &fee_bytes),
        ];

        if !rate_providers.is_empty() {
            attributes.push(("rate_providers", &rate_providers_bytes));
        }

        return Some(create_pool_component(&pool, tokens.as_slice(), &attributes, config));
    }

    if pool_factory_address == config.reclamm_factory.as_slice() {
        let ReClammPoolCreate {
            tokens: token_config,
            swap_fee_percentage,
            price_params,
            daily_price_shift_exponent,
            centeredness_margin,
            ..
        } = ReClammPoolCreate::match_and_decode(call)?;
        let ReClammPoolCreated { pool } = ReClammPoolCreated::match_and_decode(log)?;
        let rate_providers = collect_rate_providers(&token_config);
        if should_skip_rate_provider_pool(config, &rate_providers) {
            return None;
        }

        let tokens = token_config
            .iter()
            .map(|t| t.0.clone())
            .collect::<Vec<_>>();

        let fee_bytes = swap_fee_percentage.to_signed_bytes_be();
        let initial_min_price_bytes = price_params.0.to_signed_bytes_be();
        let initial_max_price_bytes = price_params.1.to_signed_bytes_be();
        let initial_target_price_bytes = price_params.2.to_signed_bytes_be();
        let daily_price_shift_exponent_bytes = daily_price_shift_exponent.to_signed_bytes_be();
        let centeredness_margin_bytes = centeredness_margin.to_signed_bytes_be();
        let token_a_price_includes_rate_bytes = [price_params.3 as u8];
        let token_b_price_includes_rate_bytes = [price_params.4 as u8];
        let rate_providers_bytes = json_serialize_address_list(rate_providers.as_slice());

        let mut attributes = vec![
            ("pool_type", "ReClammPoolFactory".as_bytes()),
            ("fee", &fee_bytes),
            ("initial_min_price", &initial_min_price_bytes),
            ("initial_max_price", &initial_max_price_bytes),
            ("initial_target_price", &initial_target_price_bytes),
            ("daily_price_shift_exponent", &daily_price_shift_exponent_bytes),
            ("centeredness_margin", &centeredness_margin_bytes),
            ("token_a_price_includes_rate", &token_a_price_includes_rate_bytes),
            ("token_b_price_includes_rate", &token_b_price_includes_rate_bytes),
        ];

        if !rate_providers.is_empty() {
            attributes.push(("rate_providers", &rate_providers_bytes));
        }

        return Some(create_pool_component(&pool, tokens.as_slice(), &attributes, config));
    }

    if !config.quantamm_factory.is_empty() &&
        pool_factory_address == config.quantamm_factory.as_slice()
    {
        // CreationNewPoolParams tuple: tokens at 2, swapFeePercentage at 5.
        // Omit normalized_weights — QuantAMM weights change via storage/time interpolation.
        let params = QuantAmmPoolCreate::match_and_decode(call)
            .map(|c| c.params)
            .or_else(|| QuantAmmPoolCreateWithoutArgs::match_and_decode(call).map(|c| c.params))?;
        let token_config = params.2;
        let swap_fee_percentage = params.5;
        let QuantAmmPoolCreated { pool } = QuantAmmPoolCreated::match_and_decode(log)?;
        let rate_providers = collect_rate_providers(&token_config);
        if should_skip_rate_provider_pool(config, &rate_providers) {
            return None;
        }

        let tokens = token_config
            .into_iter()
            .map(|t| t.0)
            .collect::<Vec<_>>();

        let fee_bytes = swap_fee_percentage.to_signed_bytes_be();
        let rate_providers_bytes = json_serialize_address_list(rate_providers.as_slice());

        let mut attributes =
            vec![("pool_type", "QuantAMMWeightedPoolFactory".as_bytes()), ("fee", &fee_bytes)];

        if !rate_providers.is_empty() {
            attributes.push(("rate_providers", &rate_providers_bytes));
        }

        return Some(create_pool_component(&pool, tokens.as_slice(), &attributes, config));
    }

    None
}

fn create_pool_component(
    pool: &[u8],
    tokens: &[Vec<u8>],
    attributes: &[(&str, &[u8])],
    config: &DeploymentConfig,
) -> ProtocolComponent {
    ProtocolComponent::new(&address_id(pool))
        .with_contracts(&[pool.to_vec(), config.vault.clone()])
        .with_tokens(tokens)
        .with_attributes(attributes)
        .as_swap_type("balancer_v3_pool", ImplementationType::Vm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::DeploymentConfig;

    fn sample_config(skip_rate_provider_pools: bool) -> DeploymentConfig {
        DeploymentConfig::parse(&format!(
            "vault=ba1333333333a1ba1108e8412f11850a5c319ba9\
             &vault_extension=0e8b07657d719b86e06bf0806d6729e3d528c9a9\
             &batch_router=136f1efcc3f8f88516b9e94110d56fdbfb1778d1\
             &permit2=000000000022d473030f116ddee9f6b43ac78ba3\
             &weighted_factory=201efd508c8dfe9de1a13c2452863a78cb2a86cc\
             &stable_factory=b9d01ca61b9c181da1051bfdd28e1097e920ab14\
             &reclamm_factory=3ccd78683effffddc1a16f5553c896ac6d3ab7ff\
             &skip_rate_provider_pools={skip_rate_provider_pools}"
        ))
        .unwrap()
    }

    #[test]
    fn should_skip_when_flag_set_and_rate_providers_present() {
        let config = sample_config(true);
        let rate_providers = vec![vec![0u8; 20]];
        assert!(should_skip_rate_provider_pool(&config, &rate_providers));
    }

    #[test]
    fn should_not_skip_when_flag_unset() {
        let config = sample_config(false);
        let rate_providers = vec![vec![0u8; 20]];
        assert!(!should_skip_rate_provider_pool(&config, &rate_providers));
    }

    #[test]
    fn should_not_skip_when_rate_providers_empty() {
        let config = sample_config(true);
        assert!(!should_skip_rate_provider_pool(&config, &[]));
    }
}
