use substreams::scalar::BigInt;

const BIT_MASK: u128 = 0xc00000000000000000000000;
const NOT_BIT_MASK: u128 = 0x3fffffffffffffffffffffff;

pub fn float_sqrt_ratio_to_fixed(sqrt_ratio_float: BigInt) -> Vec<u8> {
    let sqrt_ratio_fixed = (sqrt_ratio_float.clone() & NOT_BIT_MASK) <<
        <BigInt as Into<u32>>::into(2_u64 + ((sqrt_ratio_float & BIT_MASK) >> 89_u8));

    sqrt_ratio_fixed.to_bytes_be().1
}
