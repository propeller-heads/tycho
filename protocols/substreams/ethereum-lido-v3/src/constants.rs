use substreams::hex;

pub const STETH_COMPONENT_ID: &str = "0xae7ab96520de3a18e5e111b5eaab095312d7fe84";
pub const WSTETH_COMPONENT_ID: &str = "0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0";

pub const STETH_ADDRESS: [u8; 20] = hex!("ae7ab96520de3a18e5e111b5eaab095312d7fe84");
pub const WSTETH_ADDRESS: [u8; 20] = hex!("7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0");
pub const ETH_ADDRESS: [u8; 20] = hex!("0000000000000000000000000000000000000000");

pub const TOTAL_AND_EXTERNAL_SHARES_POSITION: [u8; 32] =
    hex!("6038150aecaa250d524370a0fdcdec13f2690e0723eaf277f41d7cae26b359e6");
pub const BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_POSITION: [u8; 32] =
    hex!("a84c096ee27e195f25d7b6c7c2a03229e49f1a2a5087e57ce7d7127707942fe3");
pub const CL_BALANCE_AND_CL_VALIDATORS_POSITION: [u8; 32] =
    hex!("c36804a03ec742b57b141e4e5d8d3bd1ddb08451fd0f9983af8aaab357a78e2f");
pub const STAKING_STATE_POSITION: [u8; 32] =
    hex!("a3678de4a579be090bed1177e0a24f77cc29d181ac22fd7688aca344d8938015");
/// `shares[wstETH]` in stETH's share mapping (mapping slot 0), i.e. `sharesOf(wstETH)`. The stETH
/// locked in the wrapper is the wstETH component's tradable liquidity.
pub const WSTETH_SHARES_POSITION: [u8; 32] =
    hex!("f37caed32e4e49c83636e0f1684f3f4a9a23c463a49eb17cd63abd50680b378b");

pub const TOTAL_AND_EXTERNAL_SHARES_ATTR: &str = "total_and_external_shares";
pub const BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_ATTR: &str =
    "buffered_ether_and_deposited_validators";
pub const CL_BALANCE_AND_CL_VALIDATORS_ATTR: &str = "cl_balance_and_cl_validators";
pub const STAKING_STATE_ATTR: &str = "staking_state";
pub const WSTETH_SHARES_ATTR: &str = "wsteth_shares";
pub const TOKEN_TO_TRACK_TOTAL_POOLED_ETH_ATTR: &str = "token_to_track_total_pooled_eth";

/// Stake per beacon-chain validator. ETH sent to the deposit contract but not yet reflected in
/// the consensus-layer balance is still pooled, so it is counted at this size.
pub const DEPOSIT_SIZE_WEI: u128 = 32_000_000_000_000_000_000;

/// Store keys holding the last seen raw value of each packed slot needed to compute
/// `totalPooledEther`. Only two of the tracked slots feed the balance, and a block that touches
/// one of them usually leaves the other untouched, so the latest value has to be carried across
/// blocks.
pub const BUFFERED_ETHER_AND_DEPOSITED_VALIDATORS_KEY: &str =
    "buffered_ether_and_deposited_validators";
pub const CL_BALANCE_AND_CL_VALIDATORS_KEY: &str = "cl_balance_and_cl_validators";
pub const TOTAL_AND_EXTERNAL_SHARES_KEY: &str = "total_and_external_shares";
pub const WSTETH_SHARES_KEY: &str = "wsteth_shares";
