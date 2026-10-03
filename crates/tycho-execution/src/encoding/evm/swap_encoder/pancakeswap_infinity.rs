use std::collections::HashMap;

use alloy::{primitives::Address, sol_types::SolValue};
use tycho_common::{models::Chain, Bytes};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{
        bytes_to_address, convert_to_router_token, get_static_attribute, pad_or_truncate_to_size,
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Encodes a swap on a PancakeSwap Infinity CL or Bin pool for `PancakeswapInfinityExecutor`.
///
/// Calldata the executor decodes, 97 bytes packed
/// (`contracts/src/executors/PancakeswapInfinityExecutor.sol`):
///
/// ```text
/// [tokenIn 20][tokenOut 20][zeroForOne 1][poolType 1][fee 3][parameters 32][hooks 20]
/// ```
///
/// No tick_spacing field: Infinity packs it with the hook bitmap into `parameters`, passed
/// through opaquely. No multi-hop either, the executor has no path for it.
///
/// Two rules whose breach misorders the pool key, so the swap reverts `PoolNotInitialized`
/// instead of swapping something else. Both covered by tests:
/// - Read `hook_address`, never `hooks`. The substreams package avoids that name because the v4
///   state decoder attaches a VM hook handler to it.
/// - `zeroForOne` compares PROTOCOL-NATIVE addresses (native ETH is `Address::ZERO`) BEFORE
///   `convert_to_router_token`. CANONICAL definition lives in the executor contract.
#[derive(Clone)]
pub struct PancakeswapInfinitySwapEncoder {
    executor_address: Bytes,
}

impl SwapEncoder for PancakeswapInfinitySwapEncoder {
    fn new(
        executor_address: Bytes,
        _chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        Ok(Self { executor_address })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        _encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let fee = get_static_attribute(swap, "key_lp_fee")?;
        let pool_fee_u24 = pad_or_truncate_to_size::<3>(&fee)
            .map_err(|e| EncodingError::FatalError(format!("key_lp_fee is not a u24: {e}")))?;
        let parameters = get_static_attribute(swap, "parameters")?;
        let parameters = pad_or_truncate_to_size::<32>(&parameters)
            .map_err(|e| EncodingError::FatalError(format!("parameters is not a bytes32: {e}")))?;
        let hook_address = match get_static_attribute(swap, "hook_address") {
            Ok(hook) => bytes_to_address(&Bytes::from(hook))?,
            Err(_) => Address::ZERO,
        };
        // protocol_system, not protocol_type_name: the test mock blanks the latter, so it would
        // work live and pick CL in every test.
        let pool_type = match swap
            .component()
            .protocol_system
            .as_str()
        {
            "pancakeswap_infinity_cl" => 0u8,
            "pancakeswap_infinity_bin" => 1u8,
            other => {
                return Err(EncodingError::FatalError(format!(
                    "{other} is not a PancakeSwap Infinity protocol system"
                )))
            }
        };
        let token_in_address = bytes_to_address(&swap.token_in().address)?;
        let token_out_address = bytes_to_address(&swap.token_out().address)?;
        let zero_for_one = token_in_address < token_out_address;
        let token_in = convert_to_router_token(token_in_address);
        let token_out = convert_to_router_token(token_out_address);
        // to_be_bytes, not the bare u8: alloy has SolValue for [u8; N] but not u8.
        Ok((
            token_in,
            token_out,
            zero_for_one,
            pool_type.to_be_bytes(),
            pool_fee_u24,
            parameters,
            hook_address,
        )
            .abi_encode_packed())
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::hex::encode;
    use num_bigint::{BigInt, BigUint};
    use rstest::rstest;
    use tycho_common::{
        models::{protocol::ProtocolComponent, Chain},
        Bytes,
    };

    use super::*;
    use crate::encoding::{
        evm::utils::write_calldata_to_file,
        models::{default_token, Swap},
    };

    // USDC/USDT on Base, the pool the integration test indexes. Real values, so the expected
    // bytes are checkable against chain.
    const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
    const USDT: &str = "0xfde4c96c8593536e31f229ea8f37b2ada2699bb2";
    /// Native ETH as Tycho and Infinity represent it.
    const NATIVE: &str = "0x0000000000000000000000000000000000000000";
    /// Native ETH as the router represents it, `ETH_ADDRESS` in `NativeETH.sol`.
    const ROUTER_ETH: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

    /// The live pool's static attributes (LP fee 5, tick spacing 2, no hook bitmap) plus `extra`.
    /// Tick spacing is bits [16,40) of `parameters`, so byte 29 is its low byte.
    //
    // protocol_system is required, not defaulted: the pool-type byte comes from it and
    // ..Default::default() leaves it empty, hitting the error arm.
    fn swap_fixture(
        protocol_system: &str,
        token_in: &str,
        token_out: &str,
        extra: &[(&str, Bytes)],
    ) -> Swap {
        let mut parameters = vec![0u8; 32];
        parameters[29] = 2;
        let mut static_attributes: HashMap<String, Bytes> = HashMap::from([
            ("key_lp_fee".into(), Bytes::from(BigInt::from(5).to_signed_bytes_be())),
            ("parameters".into(), Bytes::from(parameters)),
        ]);
        static_attributes.extend(
            extra
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone())),
        );
        let pool = ProtocolComponent {
            id: String::from("0x9ed2b133457a9debb64997f932b15a0a81f61718e8a267a4b36fab7b960d788a"),
            protocol_system: protocol_system.to_string(),
            static_attributes,
            ..Default::default()
        };
        Swap::new(
            pool,
            default_token(Bytes::from(token_in)),
            default_token(Bytes::from(token_out)),
            BigUint::ZERO,
        )
    }

    fn encode_swap(swap: &Swap) -> Result<Vec<u8>, EncodingError> {
        // Deterministic test address, config/test_executor_addresses.json ("base").
        let encoder = PancakeswapInfinitySwapEncoder::new(
            Bytes::from("0xe54a55121A47451c5727ADBAF9b9FC1643477e25"),
            Chain::Base,
            None,
        )
        .unwrap();
        let context = EncodingContext {
            router_address: Some(Bytes::from("0x5615deb798bb3e4dfa0139dfa1b3d433cc23b72f")),
            group_token_in: swap.token_in().address.clone(),
            group_token_out: swap.token_out().address.clone(),
        };
        encoder.encode_swap(swap, &context)
    }

    /// The two pool types differ in the pool-type byte alone. The Forge tests replay the written
    /// calldata.
    #[rstest]
    #[case::cl("pancakeswap_infinity_cl", "00")]
    #[case::bin("pancakeswap_infinity_bin", "01")]
    fn test_encode_pancakeswap_infinity_swap(
        #[case] protocol_system: &str,
        #[case] pool_type: &str,
    ) {
        let swap = swap_fixture(protocol_system, USDC, USDT, &[]);
        let hex_swap = encode(encode_swap(&swap).unwrap());

        assert_eq!(
            hex_swap,
            [
                // token in (USDC)
                "833589fcd6edb6e08f4c7c32d4f71b54bda02913",
                // token out (USDT)
                "fde4c96c8593536e31f229ea8f37b2ada2699bb2",
                // zero for one (USDC sorts below USDT)
                "01",
                pool_type,
                // fee (5)
                "000005",
                // parameters (tick spacing 2 in bits [16,40), hook bitmap zero)
                "0000000000000000000000000000000000000000000000000000000000020000",
                // hook address (not set, so zero)
                "0000000000000000000000000000000000000000",
            ]
            .concat()
        );
        write_calldata_to_file(&format!("test_encode_{protocol_system}_swap"), &hex_swap);
    }

    /// Direction follows the pool's currency order, in which native ETH is `address(0)` and so
    /// always currency0. The router's `ETH_ADDRESS` sorts above every token here, so comparing
    /// after the conversion would flip both native cases.
    #[rstest]
    #[case::usdc_to_usdt(USDC, USDT, 1)]
    #[case::usdt_to_usdc(USDT, USDC, 0)]
    #[case::native_to_usdc(NATIVE, USDC, 1)]
    #[case::usdc_to_native(USDC, NATIVE, 0)]
    fn test_encode_pancakeswap_infinity_direction(
        #[case] token_in: &str,
        #[case] token_out: &str,
        #[case] zero_for_one: u8,
    ) {
        let swap = swap_fixture("pancakeswap_infinity_cl", token_in, token_out, &[]);
        let encoded = encode_swap(&swap).unwrap();

        assert_eq!(encoded[40], zero_for_one);
        let router_token = |token: &str| match token {
            NATIVE => ROUTER_ETH.to_string(),
            token => token
                .trim_start_matches("0x")
                .to_string(),
        };
        assert_eq!(encode(&encoded[..20]), router_token(token_in));
        assert_eq!(encode(&encoded[20..40]), router_token(token_out));
    }

    /// The hook is read from `hook_address`. A `hooks` attribute is ignored: the substreams
    /// package never emits one.
    #[rstest]
    #[case::hook_address("hook_address", "7777777777777777777777777777777777777777")]
    #[case::hooks("hooks", "0000000000000000000000000000000000000000")]
    fn test_encode_pancakeswap_infinity_hook(#[case] attribute: &str, #[case] expected: &str) {
        let hook = Bytes::from("0x7777777777777777777777777777777777777777");
        let swap = swap_fixture("pancakeswap_infinity_cl", USDC, USDT, &[(attribute, hook)]);
        let encoded = encode_swap(&swap).unwrap();

        assert_eq!(encode(&encoded[77..]), expected);
    }

    #[test]
    fn test_encode_pancakeswap_infinity_rejects_unknown_protocol() {
        let swap = swap_fixture("uniswap_v4", USDC, USDT, &[]);
        let result = encode_swap(&swap);

        assert!(
            matches!(result, Err(EncodingError::FatalError(_))),
            "a protocol system with no pool-type byte must not encode, got {result:?}"
        );
    }
}
