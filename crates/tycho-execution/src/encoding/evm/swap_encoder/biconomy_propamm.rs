use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::{
    core::sol,
    primitives::U256,
    sol_types::{SolCall, SolValue},
};
use serde::Deserialize;
use tokio::runtime::Handle;
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    Bytes,
};

use crate::encoding::{
    errors::EncodingError,
    evm::utils::{
        biguint_to_u256, bytes_to_address, create_encoding_runtime, on_blocking_thread, SafeRuntime,
    },
    models::{EncodingContext, Swap},
    swap_encoder::SwapEncoder,
};

sol! {
    /// Structs mirror PropAMM's adapter (and the vendored copies in
    /// PropAMMExecutor.sol) exactly; field order is load-bearing.
    struct Level {
        uint256 size;
        uint256 price;
    }

    struct PriceLadder {
        address mm;
        address tokenIn;
        address tokenOut;
        Level[] levels;
        uint256 nonce;
        uint256 expiresAt;
    }

    struct FillLeg {
        address provider;
        PriceLadder ladder;
        uint256 amountIn;
    }

    /// Per-leg fill call of the PropAMM anchor executor, as returned in the
    /// firm quote's raw call list. Only used for decoding.
    function fillFromAnchor(
        address provider,
        PriceLadder ladder,
        uint256 amountIn,
        address receiver
    );
}

/// One settlement call of a PropAMM firm quote, mirroring the JSON shape
/// (`{to, value, data}`) that tycho-simulation's PropAMM client packs into the
/// `calls` quote attribute.
#[derive(Debug, Deserialize)]
struct PropAmmCall {
    /// Call target. Informational only: execution goes through the typed
    /// adapter call in PropAMMExecutor instead of raw call forwarding.
    #[allow(dead_code)]
    to: Bytes,
    /// Native value as a decimal string; PropAMM settlement is ERC20-only.
    #[allow(dead_code)]
    value: String,
    data: Bytes,
}

fn is_fill_call(data: &[u8]) -> bool {
    data.len() >= 4 && data[..4] == fillFromAnchorCall::SELECTOR[..]
}

/// Encodes a swap on PropAMM (streaming-maker RFQ) through the given executor
/// address.
///
/// PropAMM firm quotes return a raw list of settlement calls (price commit,
/// input transfer, one `fillFromAnchor` per maker leg). Raw calldata is never
/// forwarded on-chain: this encoder re-derives a typed payload from the calls
/// and the executor performs a single typed adapter call. The first call's
/// data is passed through opaquely as `commitData`, each `fillFromAnchor`
/// call is decoded into a `FillLeg`, and the result is abi encoded (not
/// packed) as `(tokenIn, tokenOut, commitData, legs)` to match the executor's
/// typed `abi.decode`.
///
/// Firm quotes are single-use and expire hard at `valid_until` (enforced
/// on-chain), so encoding fails with a recoverable error if the quote is
/// already expired: re-encoding fetches a fresh quote.
///
/// # Fields
/// * `executor_address` - The address of the executor contract that will perform the swap.
#[derive(Clone)]
pub struct PropAMMSwapEncoder {
    executor_address: Bytes,
    runtime_handle: Handle,
    #[allow(dead_code)]
    runtime: SafeRuntime,
}

impl SwapEncoder for PropAMMSwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        // The PropAMM adapter also exists on Base Sepolia, but tycho-common
        // has no built-in testnet chain, so this integration is Base only
        // (mirroring the tycho-simulation PropAMM client).
        if chain != Chain::Base {
            return Err(EncodingError::FatalError(
                "PropAMM swaps are only supported on Base".to_string(),
            ));
        }
        let (runtime_handle, runtime) = create_encoding_runtime()?;
        Ok(Self { executor_address, runtime_handle, runtime })
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
                EncodingError::FatalError("protocol_state is required for PropAMM".to_string())
            })?;
        let indicatively_priced_state = protocol_state
            .as_indicatively_priced()
            .map_err(|e| {
                EncodingError::FatalError(format!("State is not indicatively priced {e}"))
            })?;
        let estimated_amount_in = swap
            .estimated_amount_in()
            .clone()
            .ok_or(EncodingError::FatalError(
                "Estimated amount in is mandatory for a PropAMM swap".to_string(),
            ))?;
        let router_address = encoding_context
            .router_address
            .clone()
            .ok_or(EncodingError::FatalError(
                "The router address is needed to perform a PropAMM swap".to_string(),
            ))?;

        let params = GetAmountOutParams {
            amount_in: estimated_amount_in,
            token_in: swap.token_in().address.clone(),
            token_out: swap.token_out().address.clone(),
            sender: router_address.clone(),
            receiver: router_address,
        };
        let signed_quote = on_blocking_thread(|| {
            self.runtime_handle.block_on(async {
                indicatively_priced_state
                    .request_signed_quote(params)
                    .await
            })
        })??;

        // PropAMM firm quotes expire hard at valid_until (enforced on-chain),
        // so refuse to encode a payload that can only revert.
        let valid_until_bytes = signed_quote
            .quote_attributes
            .get("valid_until")
            .ok_or(EncodingError::FatalError(
                "PropAMM quote must have a valid_until attribute".to_string(),
            ))?;
        let valid_until: [u8; 8] = valid_until_bytes
            .as_ref()
            .try_into()
            .map_err(|_| {
                EncodingError::FatalError(
                    "PropAMM valid_until attribute must be 8 big-endian bytes".to_string(),
                )
            })?;
        let valid_until = u64::from_be_bytes(valid_until);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| EncodingError::FatalError("System time before UNIX epoch".to_string()))?
            .as_secs();
        if valid_until <= now {
            return Err(EncodingError::RecoverableError(format!(
                "PropAMM firm quote expired at {valid_until} (now {now}): firm quotes are \
                 single-use and short-lived, re-encode to fetch a fresh quote"
            )));
        }

        let calls_json = signed_quote
            .quote_attributes
            .get("calls")
            .ok_or(EncodingError::FatalError(
                "PropAMM quote must have a calls attribute".to_string(),
            ))?;
        let calls: Vec<PropAmmCall> = serde_json::from_slice(calls_json).map_err(|e| {
            EncodingError::FatalError(format!("Failed to parse PropAMM quote calls: {e}"))
        })?;

        // The first call commits the maker-signed price ladders on the PropAMM
        // anchor executor; its data is passed through opaquely as commitData
        // (the adapter forwards it to the anchor executor itself).
        let commit = calls
            .first()
            .ok_or(EncodingError::FatalError(
                "PropAMM firm quote contains no calls".to_string(),
            ))?;
        if is_fill_call(&commit.data) {
            return Err(EncodingError::FatalError(
                "The first PropAMM firm quote call must be the price commit, not a fill"
                    .to_string(),
            ));
        }
        let commit_data = commit.data.to_vec();

        // Re-derive the typed fill legs from the fillFromAnchor calls. The
        // remaining calls (the ERC20 input transfer in particular) are covered
        // by the router's TransferManager and the executor's approval, so they
        // are skipped here.
        let mut legs: Vec<FillLeg> = Vec::new();
        let mut legs_total = U256::ZERO;
        for call in &calls[1..] {
            if !is_fill_call(&call.data) {
                continue;
            }
            let fill = fillFromAnchorCall::abi_decode(&call.data).map_err(|e| {
                EncodingError::FatalError(format!(
                    "Failed to decode PropAMM fillFromAnchor call: {e}"
                ))
            })?;
            legs_total = legs_total
                .checked_add(fill.amountIn)
                .ok_or(EncodingError::FatalError(
                    "PropAMM fill leg amounts overflow".to_string(),
                ))?;
            legs.push(FillLeg { provider: fill.provider, ladder: fill.ladder, amountIn: fill.amountIn });
        }
        if legs.is_empty() {
            return Err(EncodingError::FatalError(
                "PropAMM firm quote contains no fillFromAnchor calls".to_string(),
            ));
        }
        // The adapter pulls exactly amountIn and reverts unless the legs
        // consume it fully, so a mismatched quote can never settle.
        if legs_total != biguint_to_u256(&signed_quote.amount_in) {
            return Err(EncodingError::FatalError(format!(
                "PropAMM fill legs sum {legs_total} does not match the quoted amount in {}",
                signed_quote.amount_in
            )));
        }

        // Standard abi encoding (not packed): the executor decodes this with
        // abi.decode(data, (address, address, bytes, FillLeg[])).
        let args = (token_in, token_out, commit_data, legs);
        Ok(args.abi_encode_params())
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
    use std::{str::FromStr, sync::Arc};

    use alloy::hex::encode;
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::testing_utils::MockRFQState, models::default_token};

    // 0x6af189df, cross-checked against
    // `cast sig "fillFromAnchor(address,(address,address,address,(uint256,uint256)[],uint256,uint256),uint256,address)"`
    const FILL_SELECTOR: [u8; 4] = [0x6a, 0xf1, 0x89, 0xdf];

    // Commit call data (anchor executor updatePrices); opaque to the encoder.
    const COMMIT_DATA: &str =
        "0x1a2b3c4d000000000000000000000000000000000000000000000000000000000000002a";

    // fillFromAnchor(0x1111..., (0x2222..., WETH, USDC,
    // [(10e18, 1878000000), (20e18, 1877000000)], 7, 1751536030), 10e18,
    // 0xfd0b...), generated with `cast calldata`.
    const FILL_CALL_1: &str = "0x6af189df000000000000000000000000111111111111111111111111111111111111111100000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000008ac7230489e80000000000000000000000000000fd0b31d2e955fa55e3fa641fe90e08b677188d3500000000000000000000000022222222222222222222222222222222222222220000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda0291300000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000007000000000000000000000000000000000000000000000000000000006866519e00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000008ac7230489e80000000000000000000000000000000000000000000000000000000000006ff00180000000000000000000000000000000000000000000000001158e460913d00000000000000000000000000000000000000000000000000000000000006fe0bf40";

    // fillFromAnchor(0x3333..., (0x4444..., WETH, USDC,
    // [(5e18, 1876500000)], 9, 1751536030), 5e18, 0xfd0b...), generated with
    // `cast calldata`.
    const FILL_CALL_2: &str = "0x6af189df000000000000000000000000333333333333333333333333333333333333333300000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000004563918244f40000000000000000000000000000fd0b31d2e955fa55e3fa641fe90e08b677188d3500000000000000000000000044444444444444444444444444444444444444440000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda0291300000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000009000000000000000000000000000000000000000000000000000000006866519e00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000004563918244f40000000000000000000000000000000000000000000000000000000000006fd91e20";

    // abi.encode(WETH, USDC, COMMIT_DATA, [leg1, leg2]) generated with
    // `cast abi-encode
    // "f(address,address,bytes,(address,(address,address,address,(uint256,uint256)[],uint256,uint256),uint256)[])"`
    // to cross-check alloy's params encoding against Solidity's abi.encode.
    const EXPECTED_ENCODED: &str = "0000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda02913000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000e000000000000000000000000000000000000000000000000000000000000000241a2b3c4d000000000000000000000000000000000000000000000000000000000000002a00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000200000000000000000000000000111111111111111111111111111111111111111100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000008ac7230489e8000000000000000000000000000022222222222222222222222222222222222222220000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda0291300000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000007000000000000000000000000000000000000000000000000000000006866519e00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000008ac7230489e80000000000000000000000000000000000000000000000000000000000006ff00180000000000000000000000000000000000000000000000001158e460913d00000000000000000000000000000000000000000000000000000000000006fe0bf40000000000000000000000000333333333333333333333333333333333333333300000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000004563918244f4000000000000000000000000000044444444444444444444444444444444444444440000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda0291300000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000009000000000000000000000000000000000000000000000000000000006866519e00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000004563918244f40000000000000000000000000000000000000000000000000000000000006fd91e20";

    fn weth() -> Bytes {
        Bytes::from_str("0x4200000000000000000000000000000000000006").unwrap()
    }

    fn usdc() -> Bytes {
        Bytes::from_str("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap()
    }

    fn far_future() -> u64 {
        4102444800 // 2100-01-01
    }

    fn calls_json(calls: &[(&str, &str)]) -> Bytes {
        let calls: Vec<serde_json::Value> = calls
            .iter()
            .map(|(to, data)| {
                serde_json::json!({"to": to, "value": "0", "data": data})
            })
            .collect();
        serde_json::to_vec(&calls).unwrap().into()
    }

    fn default_calls() -> Bytes {
        calls_json(&[
            // Price commit on the PropAMM anchor executor
            ("0xaaaa00000000000000000000000000000000aaaa", COMMIT_DATA),
            // ERC20 input transfer, skipped by the encoder
            (
                "0x4200000000000000000000000000000000000006",
                "0xa9059cbb0000000000000000000000001123da3cf775ee932a83f3d3b9edbe2e151f79b70000000000000000000000000000000000000000000000d02ab486cedc0000",
            ),
            ("0x1123da3cf775ee932a83f3d3b9edbe2e151f79b7", FILL_CALL_1),
            ("0x1123da3cf775ee932a83f3d3b9edbe2e151f79b7", FILL_CALL_2),
        ])
    }

    fn quote_attributes(calls: Bytes, valid_until: u64) -> HashMap<String, Bytes> {
        HashMap::from([
            ("calls".to_string(), calls),
            ("valid_until".to_string(), Bytes::from(valid_until.to_be_bytes().to_vec())),
        ])
    }

    fn propamm_swap(quote_data: HashMap<String, Bytes>, estimated_amount_in: BigUint) -> Swap {
        let component = ProtocolComponent {
            id: String::from("propamm-rfq"),
            protocol_system: String::from("rfq:biconomy_propamm"),
            ..Default::default()
        };
        let state = MockRFQState {
            quote_amount_out: BigUint::from_str("28164999999").unwrap(),
            quote_data,
        };
        Swap::new(component, default_token(weth()), default_token(usdc()), BigUint::ZERO)
            .with_estimated_amount_in(estimated_amount_in)
            .with_protocol_state(Arc::new(state))
    }

    fn encoding_context() -> EncodingContext {
        EncodingContext {
            router_address: Some(Bytes::zero(20)),
            group_token_in: weth(),
            group_token_out: usdc(),
        }
    }

    fn encoder() -> PropAMMSwapEncoder {
        PropAMMSwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Base,
            None,
        )
        .unwrap()
    }

    #[test]
    fn test_encode_propamm_with_protocol_state() {
        // 15 WETH -> USDC over two maker legs (10 + 5 WETH), using a mocked
        // RFQ state to get the firm quote.
        let swap = propamm_swap(
            quote_attributes(default_calls(), far_future()),
            BigUint::from_str("15000000000000000000").unwrap(),
        );

        let encoded_swap = encoder()
            .encode_swap(&swap, &encoding_context())
            .unwrap();

        // The expected payload was generated with cast abi-encode, so this
        // also cross-checks the Rust encoding against Solidity's abi.encode.
        assert_eq!(encode(&encoded_swap), EXPECTED_ENCODED);
    }

    #[test]
    fn test_encode_propamm_expired_quote() {
        let swap = propamm_swap(
            quote_attributes(default_calls(), 1751536030), // in the past
            BigUint::from_str("15000000000000000000").unwrap(),
        );

        let error = encoder()
            .encode_swap(&swap, &encoding_context())
            .unwrap_err();

        assert!(matches!(error, EncodingError::RecoverableError(ref msg) if msg.contains("expired")));
    }

    #[test]
    fn test_encode_propamm_missing_calls_attribute() {
        let swap = propamm_swap(
            HashMap::from([(
                "valid_until".to_string(),
                Bytes::from(far_future().to_be_bytes().to_vec()),
            )]),
            BigUint::from_str("15000000000000000000").unwrap(),
        );

        let error = encoder()
            .encode_swap(&swap, &encoding_context())
            .unwrap_err();

        assert!(
            matches!(error, EncodingError::FatalError(ref msg) if msg.contains("calls attribute"))
        );
    }

    #[test]
    fn test_encode_propamm_no_fill_calls() {
        let calls =
            calls_json(&[("0xaaaa00000000000000000000000000000000aaaa", COMMIT_DATA)]);
        let swap = propamm_swap(
            quote_attributes(calls, far_future()),
            BigUint::from_str("15000000000000000000").unwrap(),
        );

        let error = encoder()
            .encode_swap(&swap, &encoding_context())
            .unwrap_err();

        assert!(
            matches!(error, EncodingError::FatalError(ref msg) if msg.contains("no fillFromAnchor"))
        );
    }

    #[test]
    fn test_encode_propamm_fill_call_first() {
        let calls = calls_json(&[
            ("0x1123da3cf775ee932a83f3d3b9edbe2e151f79b7", FILL_CALL_1),
            ("0xaaaa00000000000000000000000000000000aaaa", COMMIT_DATA),
        ]);
        let swap = propamm_swap(
            quote_attributes(calls, far_future()),
            BigUint::from_str("15000000000000000000").unwrap(),
        );

        let error = encoder()
            .encode_swap(&swap, &encoding_context())
            .unwrap_err();

        assert!(
            matches!(error, EncodingError::FatalError(ref msg) if msg.contains("price commit"))
        );
    }

    #[test]
    fn test_encode_propamm_leg_sum_mismatch() {
        // The mocked quote echoes the estimated amount in as the quoted
        // amount, so estimating 14 WETH conflicts with legs summing 15 WETH.
        let swap = propamm_swap(
            quote_attributes(default_calls(), far_future()),
            BigUint::from_str("14000000000000000000").unwrap(),
        );

        let error = encoder()
            .encode_swap(&swap, &encoding_context())
            .unwrap_err();

        assert!(
            matches!(error, EncodingError::FatalError(ref msg) if msg.contains("does not match"))
        );
    }

    #[test]
    fn test_fill_selector_matches_cast() {
        assert_eq!(fillFromAnchorCall::SELECTOR, FILL_SELECTOR);
    }

    #[test]
    fn test_encoder_rejects_non_base_chain() {
        let result = PropAMMSwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None);
        assert!(result.is_err());
    }
}
