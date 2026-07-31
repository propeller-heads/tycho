/// All 11 Curve pool math variants.
///
/// Each variant uses a different on-chain smart contract implementation with
/// different invariant math, fee formulas, and state layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
pub enum CurveVariant {
    /// Earliest StableSwap (2020): sUSD, Compound, BUSD, y, USDT, Pax, Ren, sBTC.
    /// A_PRECISION=1, no `-1` offset on dy.
    StableSwapV0,
    /// 3pool era (2021): 3pool, hBTC.
    /// A_PRECISION=1, `-1` offset on dy.
    StableSwapV1,
    /// Base/plain template: factory plain pools, FRAX/USDC.
    /// A_PRECISION=100, `-1` offset, fee before denormalize.
    StableSwapV2,
    /// Lido stETH/ETH custom pool (0xDC24316b9AE028F1497c275EB9192a3Ea0f67022).
    /// Same shape as `StableSwapV2`, but `get_D` adds a `+1` divisor guard.
    StableSwapSTETH,
    /// Aave lending template: aave, sAAVE, IB pools.
    /// A_PRECISION=100, dynamic fee via `offpeg_fee_multiplier`.
    StableSwapALend,
    /// StableSwap-NG factory pools.
    /// A_PRECISION=100, dynamic fee, supports oracle rates (ERC4626).
    StableSwapNG,
    /// Meta pool template: pools paired with a base pool LP token (e.g. GUSD/3CRV).
    /// A_PRECISION=100, `rates[1]` = base pool `virtual_price`.
    StableSwapMeta,
    /// Legacy 2-coin CryptoSwap (CRV/ETH era).
    /// Newton solver for y, inline math (no external MATH contract).
    TwoCryptoV1,
    /// TwoCrypto-NG factory pools with CryptoSwap math.
    /// Cardano cubic solver. MATH contract version v2.0.0 or v2.1.0.
    TwoCryptoNG,
    /// TwoCrypto-NG factory pools with StableSwap math.
    /// Newton solver, gamma parameter ignored. MATH contract version v0.1.0.
    TwoCryptoStable,
    /// Legacy 3-coin CryptoSwap (tricrypto2: USDT/WBTC/WETH).
    /// Newton solver for y.
    TriCryptoV1,
    /// TriCrypto-NG factory pools.
    /// Hybrid cubic + Newton solver.
    TriCryptoNG,
}

impl CurveVariant {
    /// Returns the variant name as used in curve-math's `Pool` enum.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::StableSwapV0 => "StableSwapV0",
            Self::StableSwapV1 => "StableSwapV1",
            Self::StableSwapV2 => "StableSwapV2",
            Self::StableSwapSTETH => "StableSwapSTETH",
            Self::StableSwapALend => "StableSwapALend",
            Self::StableSwapNG => "StableSwapNG",
            Self::StableSwapMeta => "StableSwapMeta",
            Self::TwoCryptoV1 => "TwoCryptoV1",
            Self::TwoCryptoNG => "TwoCryptoNG",
            Self::TwoCryptoStable => "TwoCryptoStable",
            Self::TriCryptoV1 => "TriCryptoV1",
            Self::TriCryptoNG => "TriCryptoNG",
        }
    }
}

impl std::fmt::Display for CurveVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for CurveVariant {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "StableSwapV0" => Ok(Self::StableSwapV0),
            "StableSwapV1" => Ok(Self::StableSwapV1),
            "StableSwapV2" => Ok(Self::StableSwapV2),
            "StableSwapSTETH" => Ok(Self::StableSwapSTETH),
            "StableSwapALend" => Ok(Self::StableSwapALend),
            "StableSwapNG" => Ok(Self::StableSwapNG),
            "StableSwapMeta" => Ok(Self::StableSwapMeta),
            "TwoCryptoV1" => Ok(Self::TwoCryptoV1),
            "TwoCryptoNG" => Ok(Self::TwoCryptoNG),
            "TwoCryptoStable" => Ok(Self::TwoCryptoStable),
            "TriCryptoV1" => Ok(Self::TriCryptoV1),
            "TriCryptoNG" => Ok(Self::TriCryptoNG),
            _ => Err(format!("unknown CurveVariant: {s}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_str_roundtrip() {
        let variants = [
            CurveVariant::StableSwapV0,
            CurveVariant::StableSwapV1,
            CurveVariant::StableSwapV2,
            CurveVariant::StableSwapSTETH,
            CurveVariant::StableSwapALend,
            CurveVariant::StableSwapNG,
            CurveVariant::StableSwapMeta,
            CurveVariant::TwoCryptoV1,
            CurveVariant::TwoCryptoNG,
            CurveVariant::TwoCryptoStable,
            CurveVariant::TriCryptoV1,
            CurveVariant::TriCryptoNG,
        ];
        for v in variants {
            let s = v.as_str();
            let parsed: CurveVariant = s.parse().unwrap();
            assert_eq!(parsed, v);
        }
    }

    #[test]
    fn from_str_unknown() {
        let result: Result<CurveVariant, _> = "FooBar".parse();
        assert!(result.is_err());
    }
}
