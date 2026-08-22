use substreams::hex;

/// The original market factory. It emits a four-parameter `CreateNewMarket` and keys its fee
/// config by router rather than by market — see `abi/README.md`.
pub const MARKET_FACTORY_V1: [u8; 20] = hex!("27b1dacd74688af24a64bd3c9c1b143118740784");

/// Market factories V3 through V6, which share one ABI.
pub const MARKET_FACTORIES_V3_PLUS: [[u8; 20]; 4] = [
    hex!("1a6fcc85557bc4fb7b534ed835a03ef056552d52"),
    hex!("3d75bd20c983edb5fd218a1b7e0024f1056c7a2f"),
    hex!("6fcf753f2c67b83f7b09746bbc4fa0047b35d050"),
    hex!("6d247b1c044fa1e22e6b04fa9f71baf99eb29a9f"),
];

pub const PENDLE_MARKET: &str = "pendle_market";
pub const PENDLE_SY: &str = "pendle_sy";
