use serde::Deserialize;

/// Which AERO basic factory a pool belongs to. Stable and volatile pools come from separate
/// factories, each with its own custom-fee module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolKind {
    Volatile,
    Stable,
}

impl PoolKind {
    pub fn key(self) -> &'static str {
        match self {
            PoolKind::Volatile => "volatile",
            PoolKind::Stable => "stable",
        }
    }

    pub fn is_stable_attribute(self) -> Vec<u8> {
        match self {
            PoolKind::Volatile => vec![0],
            PoolKind::Stable => vec![1],
        }
    }

    pub fn from_is_stable_attribute(value: &[u8]) -> Option<Self> {
        match value {
            [0] => Some(PoolKind::Volatile),
            [1] => Some(PoolKind::Stable),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawParams {
    volatile_factory: String,
    stable_factory: String,
    volatile_fee_module: String,
    stable_fee_module: String,
}

/// Contract addresses of one chain's AERO basic deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Params {
    pub volatile_factory: Vec<u8>,
    pub stable_factory: Vec<u8>,
    pub volatile_fee_module: Vec<u8>,
    pub stable_fee_module: Vec<u8>,
}

impl Params {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw: RawParams =
            serde_qs::from_str(raw).map_err(|err| format!("invalid params `{raw}`: {err}"))?;
        Ok(Self {
            volatile_factory: parse_address("volatile_factory", &raw.volatile_factory)?,
            stable_factory: parse_address("stable_factory", &raw.stable_factory)?,
            volatile_fee_module: parse_address("volatile_fee_module", &raw.volatile_fee_module)?,
            stable_fee_module: parse_address("stable_fee_module", &raw.stable_fee_module)?,
        })
    }

    pub fn factory_kind(&self, address: &[u8]) -> Option<PoolKind> {
        if address == self.volatile_factory.as_slice() {
            Some(PoolKind::Volatile)
        } else if address == self.stable_factory.as_slice() {
            Some(PoolKind::Stable)
        } else {
            None
        }
    }

    pub fn fee_module_kind(&self, address: &[u8]) -> Option<PoolKind> {
        if address == self.volatile_fee_module.as_slice() {
            Some(PoolKind::Volatile)
        } else if address == self.stable_fee_module.as_slice() {
            Some(PoolKind::Stable)
        } else {
            None
        }
    }
}

fn parse_address(name: &str, value: &str) -> Result<Vec<u8>, String> {
    let bytes = hex::decode(value.trim_start_matches("0x"))
        .map_err(|err| format!("param `{name}` is not hex: {err}"))?;
    if bytes.len() != 20 {
        return Err(format!("param `{name}` must be a 20-byte address, got {} bytes", bytes.len()));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARAMS: &str = "volatile_factory=0x8E6a9FCd1d638CC760Ffba397605Fe3355dE76D2\
        &stable_factory=78589E876edcEbdce72ee3436ab3c9AD3a7cEF2D\
        &volatile_fee_module=0x7949DE8d67E4F6b8509Bdbb43FF0503aF1247a7A\
        &stable_fee_module=0x0195fC76Da986D66B2A468133a1be41E90893Da6";

    #[test]
    fn parses_addresses_with_or_without_prefix() {
        let params = Params::parse(PARAMS).unwrap();
        assert_eq!(params.factory_kind(&params.stable_factory.clone()), Some(PoolKind::Stable));
        assert_eq!(
            params.fee_module_kind(&params.volatile_fee_module.clone()),
            Some(PoolKind::Volatile)
        );
        assert_eq!(params.factory_kind(&[0u8; 20]), None);
    }

    #[test]
    fn rejects_short_address() {
        let err =
            Params::parse(&PARAMS.replace("78589E876edcEbdce72ee3436ab3c9AD3a7cEF2D", "1234"))
                .unwrap_err();
        assert!(err.contains("stable_factory"), "{err}");
    }

    #[test]
    fn rejects_missing_param() {
        assert!(Params::parse("volatile_factory=0x00").is_err());
    }

    #[test]
    fn is_stable_attribute_round_trips() {
        for kind in [PoolKind::Volatile, PoolKind::Stable] {
            assert_eq!(PoolKind::from_is_stable_attribute(&kind.is_stable_attribute()), Some(kind));
        }
        assert_eq!(PoolKind::from_is_stable_attribute(&[2]), None);
    }
}
