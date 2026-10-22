use substreams::hex;

/// The original market factory. It emits a four-parameter `CreateNewMarket` and keys its fee
/// config by router rather than by market — see `crate::fees` for both generations' rules.
pub const MARKET_FACTORY_V1: [u8; 20] = hex!("27b1dacd74688af24a64bd3c9c1b143118740784");

/// Market factories V3 through V6, which share one ABI.
pub const MARKET_FACTORIES_V3_PLUS: [[u8; 20]; 4] = [
    hex!("1a6fcc85557bc4fb7b534ed835a03ef056552d52"),
    hex!("3d75bd20c983edb5fd218a1b7e0024f1056c7a2f"),
    hex!("6fcf753f2c67b83f7b09746bbc4fa0047b35d050"),
    hex!("6d247b1c044fa1e22e6b04fa9f71baf99eb29a9f"),
];

/// WETH9. It wraps and unwraps with `Deposit` and `Withdrawal` and emits no `Transfer` for
/// either, so a component's WETH balance has to follow those two events as well.
pub const WETH: [u8; 20] = hex!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

pub const PENDLE_MARKET: &str = "pendle_market";
pub const PENDLE_SY: &str = "pendle_sy";

/// Pendle Router V4, the execution entry point this integration quotes and swaps through.
///
/// The fee override is keyed by the *calling* router, so a market's effective fee is only
/// well-defined relative to one. Every override for any other router is irrelevant here and is
/// ignored rather than applied to a quote that will not use it.
pub const ROUTER_V4: [u8; 20] = hex!("888888888889758f76e7103c6cbf23abbf58f946");

/// The sUSDat vault, which charges an entry fee on deposits outside US equity market hours and
/// announces it in events: `ENTRY_FEE_SET` sets the fee in basis points, and each
/// `ENTRY_FEE_NEXT_CLOSE` sets the timestamp from which it applies again, until the next one. Its
/// source is not verified; the events' meaning is inferred from 123 sampled blocks, at every one of
/// which they predicted the fee the vault's `previewDeposit` charged.
pub const ENTRY_FEE_VAULT: [u8; 20] = hex!("d166337499e176bbc38a1fbd113ab144e5bd2df7");
/// Yield tokens of the SYs whose deposits pay that fee: sUSDat itself, and srUSDat, which reads
/// the fee from it.
pub const ENTRY_FEE_PAYER_YIELD_TOKENS: [[u8; 20]; 2] = [
    hex!("d166337499e176bbc38a1fbd113ab144e5bd2df7"),
    hex!("faa9a0e1db9e22ae3a20b2b58a68dc24d053d066"),
];
/// The vault's fee-setting event: one word, the fee in basis points.
pub const ENTRY_FEE_SET: [u8; 32] =
    hex!("206dd15151fcc83315419fbe53a71fcf9c09da803767d3dd05f7874e1b9bf645");
/// The vault's market-open event: one word, the timestamp of the next close, from which the fee
/// applies again.
pub const ENTRY_FEE_NEXT_CLOSE: [u8; 32] =
    hex!("542dda9ba5ea2e8acb6236921bf765f58edb87c0151e07bd8cf153d268488f82");
