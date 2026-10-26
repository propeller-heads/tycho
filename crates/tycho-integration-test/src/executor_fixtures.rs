use std::{collections::HashMap, path::PathBuf, str::FromStr};

use alloy::primitives::Address;
use miette::{miette, IntoDiagnostic, WrapErr};
use tycho_common::{models::Chain, Bytes};
use tycho_execution::encoding::evm::swap_encoder::swap_encoder_registry::SwapEncoderRegistry;
use tycho_test::execution::{encoding::EXECUTOR_ADDRESS, models::RouterOverwritesData};

#[derive(Clone, Debug)]
pub struct ExecutorFixture {
    protocol: String,
    path: PathBuf,
}

impl FromStr for ExecutorFixture {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (protocol, path) = value
            .split_once('=')
            .filter(|(protocol, path)| !protocol.is_empty() && !path.is_empty())
            .ok_or_else(|| "expected PROTOCOL=PATH".to_string())?;
        Ok(Self { protocol: protocol.to_string(), path: PathBuf::from(path) })
    }
}

/// Uses the same simulated address as the runtime fixture generator. Only RPC state overrides
/// receive this bytecode; deployed contracts and the default executor configuration stay intact.
pub fn load(
    chain: Chain,
    fixtures: &[ExecutorFixture],
) -> miette::Result<(Option<String>, RouterOverwritesData)> {
    if fixtures.is_empty() {
        return Ok((None, RouterOverwritesData::default()));
    }
    let registry = SwapEncoderRegistry::new(chain)
        .add_default_encoders(None)
        .into_diagnostic()?;
    let mut addresses: HashMap<_, _> = registry
        .executor_addresses()
        .into_iter()
        .map(|(protocol, address)| (protocol, address.to_string()))
        .collect();
    let executor = Address::from_str(EXECUTOR_ADDRESS).into_diagnostic()?;
    let mut bytecode = None;
    for fixture in fixtures {
        let contents = std::fs::read_to_string(&fixture.path)
            .into_diagnostic()
            .wrap_err_with(|| format!("Cannot read fixture {}", fixture.path.display()))?;
        let runtime = decode_runtime(&contents)?;
        if bytecode
            .as_ref()
            .is_some_and(|previous| previous != &runtime)
        {
            return Err(miette!(
                "Conflicting executor fixtures: the runtime generator uses one simulated address"
            ));
        }
        bytecode = Some(runtime);
        addresses.insert(fixture.protocol.clone(), EXECUTOR_ADDRESS.to_string());
    }
    let config = serde_json::to_string(&HashMap::from([(chain, addresses)])).into_diagnostic()?;
    // Reject an unknown encoder before opening any feed.
    SwapEncoderRegistry::new(chain)
        .add_default_encoders(Some(config.clone()))
        .into_diagnostic()?;
    Ok((
        Some(config),
        RouterOverwritesData {
            executors: HashMap::from([(executor, bytecode)]),
            ..Default::default()
        },
    ))
}

fn decode_runtime(contents: &str) -> miette::Result<Vec<u8>> {
    let fixture: serde_json::Value = serde_json::from_str(contents).into_diagnostic()?;
    let runtime = fixture["runtimeBytecode"]
        .as_str()
        .ok_or_else(|| miette!("Fixture has no runtimeBytecode string"))?;
    let runtime = format!(
        "0x{}",
        runtime
            .strip_prefix("0x")
            .unwrap_or(runtime)
    );
    let bytecode = Bytes::from_str(&runtime)
        .into_diagnostic()
        .wrap_err("Invalid fixture runtime bytecode")?;
    if bytecode.is_empty() {
        return Err(miette!("Fixture runtime bytecode is empty"));
    }
    Ok(bytecode.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_argument_keeps_protocol_and_path() {
        let fixture = ExecutorFixture::from_str("propamm:elfomofi=/tmp/runtime.json").unwrap();
        assert_eq!(fixture.protocol, "propamm:elfomofi");
        assert_eq!(fixture.path, PathBuf::from("/tmp/runtime.json"));
        for invalid in ["", "elfomofi", "=runtime.json", "elfomofi="] {
            assert!(ExecutorFixture::from_str(invalid).is_err());
        }
    }

    #[test]
    fn runtime_fixture_requires_nonempty_hex_bytecode() {
        assert_eq!(decode_runtime(r#"{"runtimeBytecode":"0x6000"}"#).unwrap(), [0x60, 0]);
        assert_eq!(decode_runtime(r#"{"runtimeBytecode":"6000"}"#).unwrap(), [0x60, 0]);
        for invalid in [r#"{}"#, r#"{"runtimeBytecode":"0x"}"#, r#"{"runtimeBytecode":"bad"}"#] {
            assert!(decode_runtime(invalid).is_err());
        }
    }

    #[test]
    fn no_fixtures_preserves_deployed_execution() {
        let (config, overrides) = load(Chain::Base, &[]).unwrap();
        assert!(config.is_none());
        assert!(overrides.is_empty());
    }

    #[test]
    fn runtime_fixture_overrides_the_encoded_executor_and_activates_it() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../protocols/testing/fixtures/UniswapV2.runtime.json");
        let fixture = ExecutorFixture { protocol: "uniswap_v2".to_string(), path };
        let (config, overrides) = load(Chain::Base, &[fixture]).unwrap();
        let registry = SwapEncoderRegistry::new(Chain::Base)
            .add_default_encoders(config)
            .unwrap();
        assert_eq!(
            registry.executor_addresses()["uniswap_v2"],
            Bytes::from_str(EXECUTOR_ADDRESS).unwrap()
        );
        let executor = Address::from_str(EXECUTOR_ADDRESS).unwrap();
        let expected = decode_runtime(include_str!(
            "../../../protocols/testing/fixtures/UniswapV2.runtime.json"
        ))
        .unwrap();
        assert_eq!(overrides.executors[&executor], Some(expected));
        assert!(overrides.router_bytecode.is_none());
        assert!(overrides
            .fee_calculator_bytecode
            .is_none());
    }

    #[test]
    fn different_runtimes_cannot_occupy_the_same_simulated_address() {
        let directory =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../protocols/testing/fixtures");
        let fixtures = [
            ExecutorFixture {
                protocol: "uniswap_v2".to_string(),
                path: directory.join("UniswapV2.runtime.json"),
            },
            ExecutorFixture {
                protocol: "uniswap_v3".to_string(),
                path: directory.join("UniswapV3.runtime.json"),
            },
        ];
        assert!(load(Chain::Base, &fixtures)
            .err()
            .unwrap()
            .to_string()
            .contains("Conflicting executor fixtures"));
    }
}
