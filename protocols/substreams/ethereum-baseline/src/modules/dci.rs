pub(crate) const QUOTE_ENTRYPOINTS: [(&str, [u8; 4]); 4] = [
    ("quoteBuyExactIn(address,uint256)", [0x28, 0x7c, 0x89, 0xa7]),
    ("quoteBuyExactOut(address,uint256)", [0xf3, 0xa8, 0xd7, 0xf2]),
    ("quoteSellExactIn(address,uint256)", [0x0a, 0x64, 0x92, 0x74]),
    ("quoteSellExactOut(address,uint256)", [0xfe, 0x28, 0x97, 0x7c]),
];

pub(crate) const LENS_ENTRYPOINTS: [(&str, [u8; 4]); 3] = [
    ("reserve(address)", [0xe7, 0x51, 0x79, 0xa4]),
    ("totalBTokens(address)", [0x94, 0x69, 0x51, 0x2b]),
    ("totalReserves(address)", [0x6d, 0x08, 0x00, 0xbc]),
];

pub(crate) const STAKING_ENTRYPOINTS: [(&str, [u8; 4]); 1] =
    [("getCurrentRate(address)", [0xdc, 0xe7, 0x7d, 0x84])];

const SAMPLE_AMOUNT_IN: u128 = 1_000_000_000_000_000;

pub(crate) fn quote_calldata(selector: [u8; 4], b_token: &[u8]) -> Vec<u8> {
    let mut calldata = Vec::with_capacity(68);
    calldata.extend_from_slice(&selector);
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(b_token);
    calldata.extend_from_slice(&[0u8; 16]);
    calldata.extend_from_slice(&SAMPLE_AMOUNT_IN.to_be_bytes());
    calldata
}

pub(crate) fn btoken_calldata(selector: [u8; 4], b_token: &[u8]) -> Vec<u8> {
    let mut calldata = Vec::with_capacity(36);
    calldata.extend_from_slice(&selector);
    calldata.extend_from_slice(&[0u8; 12]);
    calldata.extend_from_slice(b_token);
    calldata
}
