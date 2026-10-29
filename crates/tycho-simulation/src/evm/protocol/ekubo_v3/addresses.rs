use revm::primitives::{address, Address};
use tycho_common::models::Chain;

pub const ORACLE_ADDRESS: Address = address!("0x517E506700271AEa091b02f42756F5E174Af5230");
pub const TWAMM_ADDRESS_V1: Address = address!("0xd4F1060cB9c1A13e1d2d20379b8aa2cF7541eD9b");
pub const TWAMM_ADDRESS_V2: Address = address!("0xd47f1B1eDCfEaBb08F6eBd8FC337c27E636C75BA");
pub const MEV_CAPTURE_ADDRESS: Address = address!("0x5555fF9Ff2757500BF4EE020DcfD0210CFfa41Be");
pub const BOOSTED_FEES_CONCENTRATED_ADDRESS: Address =
    address!("0xd4b54d0ca6979da05f25895e6e269e678ba00f9e");
// SignedExclusiveSwap v3.2.0, deployed on Ethereum.
pub const SIGNED_EXCLUSIVE_SWAP_V3_2_ADDRESS: Address =
    address!("0x55b703eED01b35641963da2FB2E14885993605A3");
// SignedExclusiveSwap v3.3.0, which adds per-pool owner fees.
pub const SIGNED_EXCLUSIVE_SWAP_V3_3_ADDRESS: Address =
    address!("0x550CFe1292d0D9c0d28f232b3F42Edb4439B1D09");
pub const SIGNED_EXCLUSIVE_SWAP_DEPLOYMENTS: [(Chain, Address); 4] = [
    (Chain::Ethereum, SIGNED_EXCLUSIVE_SWAP_V3_2_ADDRESS),
    (Chain::Robinhood, SIGNED_EXCLUSIVE_SWAP_V3_3_ADDRESS),
    (Chain::Bsc, SIGNED_EXCLUSIVE_SWAP_V3_3_ADDRESS),
    (Chain::Base, SIGNED_EXCLUSIVE_SWAP_V3_3_ADDRESS),
];
// The Ve33 extension is deployment-specific (see the v3.2.0 release notes of
// EkuboProtocol/evm-contracts); this is the Robinhood deployment.
pub const VE33_ROBINHOOD_ADDRESS: Address = address!("0xD18685a514E59b06d59824e16Db07e73345d9953");
