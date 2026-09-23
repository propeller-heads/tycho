use std::collections::HashMap;

use alloy::{
    primitives::{Address, U256},
    sol_types::SolValue,
};
use tokio::runtime::Handle;
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    Bytes,
};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{bytes_to_address, create_encoding_runtime, on_blocking_thread, SafeRuntime},
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

/// Settlement entrypoints the `LiquoriceExecutor` forwards to. Kept in sync with
/// `_SETTLE_SINGLE_SELECTOR` and `_SETTLE_SELECTOR` in `LiquoriceExecutor.sol`, which rejects
/// anything else. `Liquorice.t.sol` pins both against the deployed settlement's ABI.
const SETTLE_SINGLE_SELECTOR: [u8; 4] = [0x99, 0x35, 0xc8, 0x68];
const SETTLE_SELECTOR: [u8; 4] = [0x05, 0x3b, 0x41, 0x00];

/// Encodes a swap on Liquorice (RFQ) through the given executor address.
///
/// Liquorice uses a Request-for-Quote model where quotes are obtained
/// off-chain and settled on-chain. The executor receives pre-encoded
/// calldata from the API.
///
/// # Fields
/// * `executor_address` - The address of the executor contract.
/// * `settlement_address` - The Liquorice settlement the executor forwards calldata to.
/// * `balance_manager_address` - The spender the router approves for the input token.
#[derive(Clone)]
pub struct LiquoriceSwapEncoder {
    executor_address: Bytes,
    settlement_address: Address,
    balance_manager_address: Address,
    runtime_handle: Handle,
    #[allow(dead_code)]
    runtime: SafeRuntime,
}

impl SwapEncoder for LiquoriceSwapEncoder {
    fn new(
        executor_address: Bytes,
        _chain: Chain,
        config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        let config = config
            .ok_or_else(|| EncodingError::FatalError("Liquorice config is empty".to_string()))?;
        let settlement_address = parse_config_address(&config, "settlement_address")?;
        let balance_manager_address = parse_config_address(&config, "balance_manager_address")?;
        let (runtime_handle, runtime) = create_encoding_runtime()?;
        Ok(Self {
            executor_address,
            settlement_address,
            balance_manager_address,
            runtime_handle,
            runtime,
        })
    }

    fn encode_swap(
        &self,
        swap: &Swap,
        encoding_context: &EncodingContext,
    ) -> Result<Vec<u8>, EncodingError> {
        let token_in = bytes_to_address(&swap.token_in().address)?;
        let token_out = bytes_to_address(&swap.token_out().address)?;

        let protocol_state = swap
            .protocol_state()
            .as_ref()
            .ok_or_else(|| {
                EncodingError::FatalError("protocol_state is required for Liquorice".to_string())
            })?;

        let estimated_amount_in = swap
            .estimated_amount_in()
            .clone()
            .ok_or(EncodingError::FatalError(
                "Estimated amount in is mandatory for a Liquorice swap".to_string(),
            ))?;

        let router_address = encoding_context
            .router_address
            .clone()
            .ok_or(EncodingError::FatalError(
                "The router address is needed to perform a Liquorice swap".to_string(),
            ))?;

        let params = GetAmountOutParams {
            amount_in: estimated_amount_in,
            token_in: swap.token_in().address.clone(),
            token_out: swap.token_out().address.clone(),
            sender: router_address.clone(),
            receiver: router_address.clone(),
        };

        let signed_quote = on_blocking_thread(|| {
            self.runtime_handle.block_on(async {
                protocol_state
                    .as_indicatively_priced()
                    .map_err(|e| {
                        EncodingError::FatalError(format!("State is not indicatively priced {e}"))
                    })?
                    .request_signed_quote(params)
                    .await
                    .map_err(|e| EncodingError::FatalError(e.to_string()))
            })
        })??;

        let settlement = signed_quote
            .quote_attributes
            .get("settlement")
            .ok_or(EncodingError::FatalError(
                "Liquorice quote must have a settlement attribute".to_string(),
            ))
            .and_then(bytes_to_address)?;
        if settlement != self.settlement_address {
            return Err(EncodingError::InvalidInput(format!(
                "Liquorice quote settles at {settlement}, but the executor forwards to {}",
                self.settlement_address
            )));
        }

        let allowances = signed_quote
            .quote_attributes
            .get("allowances")
            .ok_or(EncodingError::FatalError(
                "Liquorice quote must have an allowances attribute".to_string(),
            ))
            .and_then(|allowances| {
                <Vec<(Address, Address, U256)> as SolValue>::abi_decode_validate(allowances)
                    .map_err(|e| {
                        EncodingError::InvalidInput(format!(
                            "Liquorice quote has malformed allowances: {e}"
                        ))
                    })
            })?;
        let (allowance_token, allowance_spender) = match allowances[..] {
            [(token, spender, _)] => (token, spender),
            _ => {
                return Err(EncodingError::InvalidInput(format!(
                    "Liquorice quote requires {} approvals, but the router grants one",
                    allowances.len()
                )))
            }
        };
        if allowance_spender != self.balance_manager_address {
            return Err(EncodingError::InvalidInput(format!(
                "Liquorice quote wants an approval for {allowance_spender}, but the executor approves {}",
                self.balance_manager_address
            )));
        }
        if allowance_token != token_in {
            return Err(EncodingError::InvalidInput(format!(
                "Liquorice quote wants an approval for token {allowance_token}, but the swap sells {token_in}"
            )));
        }

        let liquorice_calldata = signed_quote
            .quote_attributes
            .get("calldata")
            .ok_or(EncodingError::FatalError(
                "Liquorice quote must have a calldata attribute".to_string(),
            ))?;

        // LiquoriceExecutor.swap reverts with LiquoriceExecutor__InvalidSelector on anything else.
        let selector: [u8; 4] = liquorice_calldata
            .get(..4)
            .and_then(|s| s.try_into().ok())
            .ok_or(EncodingError::InvalidInput(
                "Liquorice quote calldata is shorter than a selector".to_string(),
            ))?;
        if selector != SETTLE_SINGLE_SELECTOR && selector != SETTLE_SELECTOR {
            return Err(EncodingError::InvalidInput(format!(
                "Liquorice quote calls unsupported settlement entrypoint 0x{}",
                alloy::hex::encode(selector)
            )));
        }

        let base_token_amount = signed_quote
            .quote_attributes
            .get("base_token_amount")
            .ok_or(EncodingError::FatalError(
                "Liquorice quote must have a base_token_amount attribute".to_string(),
            ))?;

        // Defaults to 0 if not present (partial fill not available)
        let partial_fill_offset: [u8; 4] = signed_quote
            .quote_attributes
            .get("partial_fill_offset")
            .map(|b| {
                let mut padded = [0u8; 4];
                if b.len() >= 4 {
                    padded.copy_from_slice(&b[b.len() - 4..]);
                } else {
                    let start = 4 - b.len();
                    padded[start..].copy_from_slice(b);
                }
                padded
            })
            .unwrap_or([0u8; 4]);

        // Defaults to original base token amount if partial fill not
        // available
        let min_base_token_amount = signed_quote
            .quote_attributes
            .get("min_base_token_amount")
            .unwrap_or(base_token_amount);

        let original_base_token_amount = pad_to_32_bytes(base_token_amount);
        let min_base_token_amount = pad_to_32_bytes(min_base_token_amount);

        // Encode packed data for the executor
        // Format: token_in | token_out | partial_fill_offset |
        //         original_base_token_amount | min_base_token_amount |
        //         liquorice_calldata
        let args = (
            token_in,
            token_out,
            partial_fill_offset,
            original_base_token_amount,
            min_base_token_amount,
            liquorice_calldata.as_ref(),
        );

        Ok(args.abi_encode_packed())
    }

    fn executor_address(&self) -> &Bytes {
        &self.executor_address
    }

    fn blocks_on_quote(&self) -> bool {
        true
    }

    fn clone_box(&self) -> Box<dyn SwapEncoder> {
        Box::new(self.clone())
    }
}

fn parse_config_address(
    config: &HashMap<String, String>,
    key: &str,
) -> Result<Address, EncodingError> {
    config
        .get(key)
        .ok_or_else(|| EncodingError::FatalError(format!("Missing {key} in Liquorice config")))?
        .parse::<Address>()
        .map_err(|e| EncodingError::FatalError(format!("Invalid {key} in Liquorice config: {e}")))
}

fn pad_to_32_bytes(data: &Bytes) -> [u8; 32] {
    let mut padded = [0u8; 32];
    if data.len() >= 32 {
        padded.copy_from_slice(&data[data.len() - 32..]);
    } else {
        let start = 32 - data.len();
        padded[start..].copy_from_slice(data);
    }
    padded
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{
        evm::{
            swap_encoder::liquorice::LiquoriceSwapEncoder, testing_utils::MockRFQState,
            utils::biguint_to_u256,
        },
        models::default_token,
    };

    fn liquorice_config() -> Option<HashMap<String, String>> {
        Some(HashMap::from([
            (
                "settlement_address".to_string(),
                "0x43Dcd6586e6209eE7235A21bDC4aa301E5Bc44e8".to_string(),
            ),
            (
                "balance_manager_address".to_string(),
                "0x56B4720e40dF52C560E520238E34e9C33d57E593".to_string(),
            ),
        ]))
    }

    #[test]
    fn test_encode_liquorice_single_with_protocol_state() {
        let quote_amount_out = BigUint::from_str("1000000000000000000").unwrap();
        let liquorice_calldata = Bytes::from_str("0x9935c8681234567890").unwrap();
        let base_token_amount = biguint_to_u256(&BigUint::from(3_000_000_000_u64))
            .to_be_bytes::<32>()
            .to_vec();

        let liquorice_component = ProtocolComponent {
            id: String::from("liquorice-rfq"),
            protocol_system: String::from("book:liquorice"),
            ..Default::default()
        };

        let min_base_token_amount = biguint_to_u256(&BigUint::from(2_500_000_000_u64))
            .to_be_bytes::<32>()
            .to_vec();

        let liquorice_state = MockRFQState {
            quote_amount_in: None,
            quote_amount_out,
            quote_data: HashMap::from([
                ("calldata".to_string(), liquorice_calldata.clone()),
                ("base_token_amount".to_string(), Bytes::from(base_token_amount)),
                ("min_base_token_amount".to_string(), Bytes::from(min_base_token_amount)),
                ("partial_fill_offset".to_string(), Bytes::from(vec![12u8])),
                (
                    "settlement".to_string(),
                    Bytes::from_str("0x43Dcd6586e6209eE7235A21bDC4aa301E5Bc44e8").unwrap(),
                ),
                (
                    // [(USDC, balance manager, 3000000000)]
                    "allowances".to_string(),
                    Bytes::from_str(
                        "0x00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000001000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000056b4720e40df52c560e520238e34e9c33d57e59300000000000000000000000000000000000000000000000000000000b2d05e00"
                    )
                    .unwrap(),
                ),
            ]),
            ..Default::default()
        };

        let token_in = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let token_out = Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

        let swap = Swap::new(
            liquorice_component,
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from_str("3000000000").unwrap())
        .with_protocol_state(Arc::new(liquorice_state));

        let encoding_context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in.clone(),
            group_token_out: token_out.clone(),
        };

        let encoder = LiquoriceSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            liquorice_config(),
        )
        .unwrap();

        let encoded_swap = encoder
            .encode_swap(&swap, &encoding_context)
            .unwrap();
        let hex_swap = encode(&encoded_swap);

        // Expected format:
        // token_in (20) | token_out (20) | partial_fill_offset (4) |
        // original_base_token_amount (32) | min_base_token_amount (32)
        // | calldata (variable)
        let expected_swap = String::from(concat!(
            // token_in (USDC)
            "a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            // token_out (WETH)
            "c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            // partial_fill_offset
            "0000000c",
            // original_base_token_amount (3000000000 as U256)
            "00000000000000000000000000000000",
            "000000000000000000000000b2d05e00",
            // min_base_token_amount (2500000000 as U256)
            "00000000000000000000000000000000",
            "0000000000000000000000009502f900",
        ));
        assert_eq!(hex_swap, expected_swap + &liquorice_calldata.to_string()[2..]);
    }

    /// Builds an otherwise-valid Liquorice swap whose quote attributes are overridden, so each
    /// test states only the attribute it is about.
    fn swap_with_quote_attributes(overrides: Vec<(&str, Bytes)>) -> (Swap, EncodingContext) {
        let token_in = Bytes::from("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
        let token_out = Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

        let mut quote_data = HashMap::from([
            ("calldata".to_string(), Bytes::from_str("0x9935c8681234567890").unwrap()),
            (
                "base_token_amount".to_string(),
                Bytes::from(
                    biguint_to_u256(&BigUint::from(3_000_000_000_u64))
                        .to_be_bytes::<32>()
                        .to_vec(),
                ),
            ),
            (
                "settlement".to_string(),
                Bytes::from_str("0x43Dcd6586e6209eE7235A21bDC4aa301E5Bc44e8").unwrap(),
            ),
            (
                // [(USDC, balance manager, 3000000000)]
                "allowances".to_string(),
                Bytes::from_str(
                    "0x00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000001000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000056b4720e40df52c560e520238e34e9c33d57e59300000000000000000000000000000000000000000000000000000000b2d05e00"
                )
                .unwrap(),
            ),
        ]);
        for (key, value) in overrides {
            quote_data.insert(key.to_string(), value);
        }

        let swap = Swap::new(
            ProtocolComponent {
                id: String::from("liquorice-rfq"),
                protocol_system: String::from("book:liquorice"),
                ..Default::default()
            },
            default_token(token_in.clone()),
            default_token(token_out.clone()),
            BigUint::ZERO,
        )
        .with_estimated_amount_in(BigUint::from_str("3000000000").unwrap())
        .with_protocol_state(Arc::new(MockRFQState {
            quote_amount_in: None,
            quote_amount_out: BigUint::from_str("1000000000000000000").unwrap(),
            quote_data,
            ..Default::default()
        }));

        let context = EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: token_in,
            group_token_out: token_out,
        };
        (swap, context)
    }

    fn encoder() -> LiquoriceSwapEncoder {
        LiquoriceSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Ethereum,
            liquorice_config(),
        )
        .unwrap()
    }

    #[test]
    fn test_rejects_quote_for_another_settlement() {
        let (swap, context) = swap_with_quote_attributes(vec![(
            "settlement",
            Bytes::from_str("0x0448633eb8B0A42EfED924C42069E0DcF08fb552").unwrap(),
        )]);

        let error = encoder()
            .encode_swap(&swap, &context)
            .unwrap_err();

        assert!(
            matches!(&error, EncodingError::InvalidInput(m) if m.contains("settles at")),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn test_rejects_quote_for_another_balance_manager() {
        // [(USDC, the retired balance manager, 3000000000)]
        let (swap, context) = swap_with_quote_attributes(vec![(
            "allowances",
            Bytes::from_str(
                "0x00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000001000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48000000000000000000000000b87bae43a665eb5943a5642f81b26666bc9e5c9500000000000000000000000000000000000000000000000000000000b2d05e00"
            )
            .unwrap(),
        )]);

        let error = encoder()
            .encode_swap(&swap, &context)
            .unwrap_err();

        assert!(
            matches!(&error, EncodingError::InvalidInput(m) if m.contains("wants an approval")),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn test_rejects_quote_wanting_an_approval_for_another_token() {
        // [(WETH, balance manager, 3000000000)], against a swap that sells USDC
        let (swap, context) = swap_with_quote_attributes(vec![(
            "allowances",
            Bytes::from_str(
                "0x00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000001000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc200000000000000000000000056b4720e40df52c560e520238e34e9c33d57e59300000000000000000000000000000000000000000000000000000000b2d05e00"
            )
            .unwrap(),
        )]);

        let error = encoder()
            .encode_swap(&swap, &context)
            .unwrap_err();

        assert!(
            matches!(&error, EncodingError::InvalidInput(m) if m.contains("approval for token")),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn test_rejects_quote_wanting_two_approvals() {
        // [(USDC, balance manager, 3000000000)], twice
        let (swap, context) = swap_with_quote_attributes(vec![(
            "allowances",
            Bytes::from_str(
                "0x00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000002000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000056b4720e40df52c560e520238e34e9c33d57e59300000000000000000000000000000000000000000000000000000000b2d05e00000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb4800000000000000000000000056b4720e40df52c560e520238e34e9c33d57e59300000000000000000000000000000000000000000000000000000000b2d05e00"
            )
            .unwrap(),
        )]);

        let error = encoder()
            .encode_swap(&swap, &context)
            .unwrap_err();

        assert!(
            matches!(&error, EncodingError::InvalidInput(m) if m.contains("requires 2 approvals")),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn test_rejects_quote_wanting_no_approval() {
        // []
        let (swap, context) = swap_with_quote_attributes(vec![(
            "allowances",
            Bytes::from_str(
                "0x00000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000"
            )
            .unwrap(),
        )]);

        let error = encoder()
            .encode_swap(&swap, &context)
            .unwrap_err();

        assert!(
            matches!(&error, EncodingError::InvalidInput(m) if m.contains("requires 0 approvals")),
            "unexpected error: {error:?}"
        );
    }

    /// The selector the settlement deployment before 0x43Dcd658 used for `settle`.
    #[test]
    fn test_rejects_retired_settle_selector() {
        let (swap, context) = swap_with_quote_attributes(vec![(
            "calldata",
            Bytes::from_str("0xcba673a71234567890").unwrap(),
        )]);

        let error = encoder()
            .encode_swap(&swap, &context)
            .unwrap_err();

        assert!(
            matches!(
                &error,
                EncodingError::InvalidInput(m)
                    if m.contains("unsupported settlement entrypoint 0xcba673a7")
            ),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn test_accepts_both_settlement_entrypoints() {
        for selector in ["0x9935c868", "0x053b4100"] {
            let (swap, context) = swap_with_quote_attributes(vec![(
                "calldata",
                Bytes::from_str(&format!("{selector}1234567890")).unwrap(),
            )]);
            assert!(
                encoder()
                    .encode_swap(&swap, &context)
                    .is_ok(),
                "{selector} should be accepted"
            );
        }
    }
}
