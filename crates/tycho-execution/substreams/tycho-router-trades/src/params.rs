//! Module parameter parsing.
//!
//! Format (query-string style, all addresses lowercase hex with `0x` prefix):
//!
//! ```text
//! chain=ethereum&routers=<addr>:<version>,...&fee_calculators=<router>:<fee_calculator>,...
//! ```
//!
//! `version` is one of `v2`, `v3_0`, `v3_1`. `fee_calculators` lists the FeeCalculator each
//! router was constructed with (the constructor emits no event); later rotations are picked up
//! from router events.
use anyhow::{anyhow, bail, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterVersion {
    /// `singleSwap(amountIn, tokenIn, tokenOut, minAmountOut, wrapEth, unwrapEth, receiver, ...)`
    V2,
    /// ClientFeeParams with `uint16` bps, `minAmountOut` only.
    V3_0,
    /// ClientFeeParams with `uint32` fee units, `expectedAmountOut` + `minAmountOut`.
    V3_1,
}

impl RouterVersion {
    pub fn as_str(self) -> &'static str {
        match self {
            RouterVersion::V2 => "v2",
            RouterVersion::V3_0 => "v3_0",
            RouterVersion::V3_1 => "v3_1",
        }
    }

    /// Denominator of the fee bps values stored in the FeeCalculator of this generation.
    pub fn bps_scale(self) -> Option<u64> {
        match self {
            RouterVersion::V2 => None,
            RouterVersion::V3_0 => Some(10_000),
            RouterVersion::V3_1 => Some(100_000_000),
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "v2" => Ok(RouterVersion::V2),
            "v3_0" => Ok(RouterVersion::V3_0),
            "v3_1" => Ok(RouterVersion::V3_1),
            other => bail!("unknown router version '{other}', expected v2, v3_0 or v3_1"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RouterConfig {
    pub address: Vec<u8>,
    pub version: RouterVersion,
    /// FeeCalculator set in the router constructor, if any.
    pub fee_calculator: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct Params {
    pub chain: String,
    pub routers: Vec<RouterConfig>,
}

impl Params {
    pub fn parse(raw: &str) -> Result<Self> {
        let mut chain = None;
        let mut routers = Vec::new();
        let mut fee_calculators: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for pair in raw.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("parameter '{pair}' is not key=value"))?;
            match key {
                "chain" => chain = Some(value.to_string()),
                "routers" => {
                    for entry in value
                        .split(',')
                        .filter(|e| !e.is_empty())
                    {
                        let (addr, version) = entry
                            .split_once(':')
                            .ok_or_else(|| anyhow!("router entry '{entry}' is not addr:version"))?;
                        routers.push(RouterConfig {
                            address: parse_address(addr)?,
                            version: RouterVersion::parse(version)?,
                            fee_calculator: None,
                        });
                    }
                }
                "fee_calculators" => {
                    for entry in value
                        .split(',')
                        .filter(|e| !e.is_empty())
                    {
                        let (router, fc) = entry.split_once(':').ok_or_else(|| {
                            anyhow!("fee_calculators entry '{entry}' is not router:fee_calculator")
                        })?;
                        fee_calculators.push((parse_address(router)?, parse_address(fc)?));
                    }
                }
                other => bail!("unknown parameter '{other}'"),
            }
        }
        let chain = chain.ok_or_else(|| anyhow!("missing 'chain' parameter"))?;
        if routers.is_empty() {
            bail!("missing or empty 'routers' parameter");
        }
        for (router, fc) in fee_calculators {
            let config = routers
                .iter_mut()
                .find(|r| r.address == router)
                .ok_or_else(|| {
                    anyhow!("fee_calculators references unknown router 0x{}", hex::encode(&router))
                })?;
            config.fee_calculator = Some(fc);
        }
        Ok(Params { chain, routers })
    }

    pub fn router(&self, address: &[u8]) -> Option<&RouterConfig> {
        self.routers
            .iter()
            .find(|r| r.address == address)
    }
}

fn parse_address(s: &str) -> Result<Vec<u8>> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(stripped).with_context(|| format!("invalid hex address '{s}'"))?;
    if bytes.len() != 20 {
        bail!("address '{s}' is not 20 bytes");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_routers_and_fee_calculators() {
        let p = Params::parse(
            "chain=base&routers=0x1111111111111111111111111111111111111111:v2,\
             0x2222222222222222222222222222222222222222:v3_1\
             &fee_calculators=0x2222222222222222222222222222222222222222:0x3333333333333333333333333333333333333333",
        )
        .unwrap();
        assert_eq!(p.chain, "base");
        assert_eq!(p.routers.len(), 2);
        assert_eq!(p.routers[0].version, RouterVersion::V2);
        assert!(p.routers[0].fee_calculator.is_none());
        assert_eq!(p.routers[1].version, RouterVersion::V3_1);
        assert_eq!(p.routers[1].fee_calculator.as_deref(), Some(&[0x33u8; 20][..]));
        assert!(p.router(&[0x22u8; 20]).is_some());
        assert!(p.router(&[0x44u8; 20]).is_none());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Params::parse("routers=0x1111111111111111111111111111111111111111:v2").is_err());
        assert!(Params::parse("chain=x").is_err());
        assert!(Params::parse("chain=x&routers=0x11:v2").is_err());
        assert!(
            Params::parse("chain=x&routers=0x1111111111111111111111111111111111111111:v9").is_err()
        );
        assert!(Params::parse(
            "chain=x&routers=0x1111111111111111111111111111111111111111:v2\
             &fee_calculators=0x2222222222222222222222222222222222222222:0x3333333333333333333333333333333333333333"
        )
        .is_err());
    }
}
