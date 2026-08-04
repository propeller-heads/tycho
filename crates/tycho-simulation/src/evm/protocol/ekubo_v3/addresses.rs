use revm::primitives::{address, Address};

pub const ORACLE_ADDRESS: Address = address!("0x517E506700271AEa091b02f42756F5E174Af5230");
pub const TWAMM_ADDRESS: Address = address!("0xd4F1060cB9c1A13e1d2d20379b8aa2cF7541eD9b");
pub const MEV_CAPTURE_ADDRESS: Address = address!("0x5555fF9Ff2757500BF4EE020DcfD0210CFfa41Be");
pub const BOOSTED_FEES_CONCENTRATED_ADDRESS: Address =
    address!("0xd4b54d0ca6979da05f25895e6e269e678ba00f9e");
// TODO: replace with the deployed SignedExclusiveSwap extension address once
// EkuboProtocol/evm-contracts#319 lands.
pub const SIGNED_EXCLUSIVE_SWAP_ADDRESS: Address =
    address!("0x5519eD5e5e5E5E5e5E5E5e5e5e5E5e5e5e5E5E5E");
// The Ve33 extension is deployment-specific (see the v3.2.0 release notes of
// EkuboProtocol/evm-contracts); this is the Robinhood deployment.
pub const VE33_ROBINHOOD_ADDRESS: Address = address!("0xD18685a514E59b06d59824e16Db07e73345d9953");
