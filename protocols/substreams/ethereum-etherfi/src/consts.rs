use substreams::hex;

// Native ETH as Tycho addresses it, not the router's 0xEeee..EEeE sentinel. Reporting the
// sentinel leaves the token unpriced (so the component gets no TVL row and is filtered out by
// min_tvl) and makes the swap encoder reject every ETH-side swap.
pub const ETH_ADDRESS: [u8; 20] = hex!("0000000000000000000000000000000000000000");

pub const EETH_ADDRESS: [u8; 20] = hex!("35fA164735182de50811E8e2E824cFb9B6118ac2");

pub const LIQUIDITY_POOL_ADDRESS: [u8; 20] = hex!("308861a430be4cce5502d0a12724771fc6daf216");

pub const WEETH_ADDRESS: [u8; 20] = hex!("Cd5fE23C85820F7B72D0926FC9b05b43E359b7ee");

pub const REDEMPTION_MANAGER_ADDRESS: [u8; 20] = hex!("DadEf1fFBFeaAB4f68A9fD181395F68b4e4E7Ae0");
