use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::{
    core::sol,
    primitives::{Address, U256},
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
    /// Structs mirror the deployed PropAMM contracts exactly (SwapTypes.sol in
    /// propamm-protocol, and the vendored copies in BiconomyExecutor.sol);
    /// field order is load-bearing for abi decoding.
    struct Level {
        uint256 size;
        uint256 price;
    }

    struct PriceLadder {
        address mm;
        address provider;
        address tokenIn;
        address tokenOut;
        Level[] levels;
        uint256 nonce;
        uint256 expiresAt;
    }

    struct FillLeg {
        PriceLadder ladder;
        uint256 amountIn;
    }

    struct SwapParams {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        uint256 minAmountOut;
        address receiver;
    }

    /// One step of the settlement call's execution sequence.
    struct Step {
        address to;
        uint256 value;
        bytes data;
        bool isDelegatecall;
    }

    /// PropAMM settlement entrypoint - the single call a firm quote returns
    /// (selector 0x1eada922). Only used for decoding.
    function swap(SwapParams params, Step[] steps) returns (uint256 delivered);

    /// Anchor-executor price commit step (selector 0x86e97b02). Recognized by
    /// selector and passed through opaquely as the adapter's commitData.
    function updatePrices(PriceLadder[] ladders, bytes[] sigs);

    /// Anchor-executor fill step (selector 0x3290f81e). Decoded into the
    /// adapter's typed FillLeg.
    function fillFromAnchor(PriceLadder ladder, uint256 amountIn, address receiver);

    /// ERC20 input-funding step (selector 0xa9059cbb). Skipped: on the Tycho
    /// path the TransferManager approval plus the adapter's own pull replace it.
    function transfer(address to, uint256 amount) returns (bool);

    /// Settlement residue sweep step (selector 0x66cf5b60). Skipped: the
    /// adapter reverts on leg-sum mismatch, so there is no residue to sweep.
    function sweepBalance(address token, address receiver, uint256 minAmount);
}

/// One settlement call of a PropAMM firm quote, mirroring the JSON shape
/// (`{to, value, data}`) that tycho-simulation's PropAMM client packs into the
/// `calls` quote attribute.
#[derive(Debug, Deserialize)]
struct BiconomyCall {
    /// Call target: the PropAMM settlement contract. Raw calldata is never
    /// forwarded on-chain - execution goes through the typed adapter call in
    /// BiconomyExecutor - but step targets are validated against it.
    to: Bytes,
    /// Native value as a decimal string; PropAMM settlement is ERC20-only.
    #[allow(dead_code)]
    value: String,
    data: Bytes,
}

/// Encodes a swap on PropAMM (streaming-maker RFQ) through the given executor
/// address.
///
/// A PropAMM firm quote returns exactly one settlement `swap()` call whose
/// step list carries the price commit, the input transfer and one
/// `fillFromAnchor` per maker leg. Raw calldata is never forwarded on-chain:
/// this encoder decodes the settlement call, re-derives a typed payload from
/// its steps (commit data passed through opaquely, each fill decoded into a
/// `FillLeg`, funding/sweep steps validated and skipped - the router's
/// TransferManager and the adapter's own pull replace them), and the executor
/// performs a single typed adapter call. The result is abi encoded (not
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
pub struct BiconomySwapEncoder {
    executor_address: Bytes,
    runtime_handle: Handle,
    #[allow(dead_code)]
    runtime: SafeRuntime,
}

impl SwapEncoder for BiconomySwapEncoder {
    fn new(
        executor_address: Bytes,
        chain: Chain,
        _config: Option<HashMap<String, String>>,
    ) -> Result<Self, EncodingError> {
        // Base and BSC mainnet, mirroring the tycho-simulation client. The
        // PropAMM adapter also exists on Base Sepolia, but tycho-common has no
        // built-in testnet chain, so testnets are not wired up.
        if chain != Chain::Base && chain != Chain::Bsc {
            return Err(EncodingError::FatalError(
                "PropAMM swaps are only supported on Base and BSC".to_string(),
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
        let calls: Vec<BiconomyCall> = serde_json::from_slice(calls_json).map_err(|e| {
            EncodingError::FatalError(format!("Failed to parse PropAMM quote calls: {e}"))
        })?;

        // A firm quote is exactly one settlement `swap()` call. Anything else
        // means the API changed shape - refuse to encode rather than guess.
        if calls.len() != 1 {
            return Err(EncodingError::FatalError(format!(
                "PropAMM firm quote must contain exactly one settlement call, got {}",
                calls.len()
            )));
        }
        let settlement_call = &calls[0];
        let call_data: &[u8] = settlement_call.data.as_ref();
        if call_data.len() < 4 || call_data[..4] != swapCall::SELECTOR[..] {
            return Err(EncodingError::FatalError(
                "PropAMM firm quote call is not settlement swap(); the API may have changed, \
                 refusing to encode"
                    .to_string(),
            ));
        }
        let settlement = swapCall::abi_decode(call_data).map_err(|e| {
            EncodingError::FatalError(format!("Failed to decode PropAMM settlement swap call: {e}"))
        })?;
        if settlement.params.tokenIn != token_in || settlement.params.tokenOut != token_out {
            return Err(EncodingError::FatalError(format!(
                "PropAMM settlement pair {}/{} does not match the swap {token_in}/{token_out}",
                settlement.params.tokenIn, settlement.params.tokenOut
            )));
        }

        // Walk the settlement steps STRICTLY: every step must be one of the
        // four known shapes below or encoding fails - a lenient skip here
        // would silently drop behavior the settlement call relies on.
        let mut commit: Option<(Address, Vec<u8>)> = None;
        let mut legs: Vec<FillLeg> = Vec::new();
        let mut legs_total = U256::ZERO;
        for step in &settlement.steps {
            let step_data: &[u8] = step.data.as_ref();
            if step_data.len() < 4 {
                return Err(EncodingError::FatalError(
                    "PropAMM settlement step with no selector".to_string(),
                ));
            }
            let selector: [u8; 4] = step_data[..4]
                .try_into()
                .expect("length checked above");
            match selector {
                s if s == updatePricesCall::SELECTOR => {
                    if commit.is_some() {
                        return Err(EncodingError::FatalError(
                            "PropAMM settlement has more than one price commit step".to_string(),
                        ));
                    }
                    commit = Some((step.to, step_data.to_vec()));
                }
                s if s == fillFromAnchorCall::SELECTOR => {
                    // The commit binds every fill: its target is the anchor
                    // executor, and each fill must run on that same contract.
                    let Some((commit_to, _)) = &commit else {
                        return Err(EncodingError::FatalError(
                            "PropAMM fill step before the price commit".to_string(),
                        ));
                    };
                    if step.to != *commit_to {
                        return Err(EncodingError::FatalError(format!(
                            "PropAMM fill step targets {} instead of the anchor executor {}",
                            step.to, commit_to
                        )));
                    }
                    let fill = fillFromAnchorCall::abi_decode(step_data).map_err(|e| {
                        EncodingError::FatalError(format!(
                            "Failed to decode PropAMM fillFromAnchor step: {e}"
                        ))
                    })?;
                    // The adapter executes direct fills only; a route through a
                    // pivot token cannot be re-derived into adapter legs.
                    // Recoverable: routes are re-resolved per quote, so a fresh
                    // fetch may pick a direct route.
                    if fill.ladder.tokenIn != token_in || fill.ladder.tokenOut != token_out {
                        return Err(EncodingError::RecoverableError(format!(
                            "PropAMM quote routed through an intermediate pair ({}/{}); the \
                             adapter executes direct fills only - refetch for a direct route",
                            fill.ladder.tokenIn, fill.ladder.tokenOut
                        )));
                    }
                    legs_total = legs_total
                        .checked_add(fill.amountIn)
                        .ok_or(EncodingError::FatalError(
                            "PropAMM fill leg amounts overflow".to_string(),
                        ))?;
                    legs.push(FillLeg { ladder: fill.ladder, amountIn: fill.amountIn });
                }
                s if s == transferCall::SELECTOR => {
                    // Input funding move (tokenIn -> anchor executor). The
                    // Tycho path replaces it with the TransferManager approval
                    // plus the adapter's own transferFrom, so validate + skip.
                    if step.to != token_in {
                        return Err(EncodingError::FatalError(format!(
                            "PropAMM transfer step moves {} instead of the input token",
                            step.to
                        )));
                    }
                }
                s if s == sweepBalanceCall::SELECTOR => {
                    // Residue sweep on the settlement itself. The adapter
                    // reverts on leg-sum mismatch instead, so validate + skip.
                    let expected: Address = bytes_to_address(&settlement_call.to)?;
                    if step.to != expected {
                        return Err(EncodingError::FatalError(format!(
                            "PropAMM sweep step targets {} instead of the settlement",
                            step.to
                        )));
                    }
                }
                other => {
                    return Err(EncodingError::FatalError(format!(
                        "Unrecognized PropAMM settlement step selector 0x{}; the API may have \
                         changed, refusing to encode",
                        alloy::hex::encode(other)
                    )));
                }
            }
        }

        let Some((_, commit_data)) = commit else {
            return Err(EncodingError::FatalError(
                "PropAMM settlement has no price commit step".to_string(),
            ));
        };
        if legs.is_empty() {
            return Err(EncodingError::FatalError(
                "PropAMM firm quote contains no fillFromAnchor steps".to_string(),
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

    use alloy::{hex::encode, primitives::address};
    use num_bigint::BigUint;
    use tycho_common::models::protocol::ProtocolComponent;

    use super::*;
    use crate::encoding::{evm::testing_utils::MockRFQState, models::default_token};

    // Selectors cross-checked against `cast sig` of the deployed contracts:
    //   swap((address,address,uint256,uint256,address),(address,uint256,bytes,bool)[])
    //   updatePrices((address,address,address,address,(uint256,uint256)[],uint256,uint256)[],bytes[])
    //   fillFromAnchor((address,address,address,address,(uint256,uint256)[],uint256,uint256),uint256,address)
    //   sweepBalance(address,address,uint256)
    const SWAP_SELECTOR: [u8; 4] = [0x1e, 0xad, 0xa9, 0x22];
    const UPDATE_PRICES_SELECTOR: [u8; 4] = [0x86, 0xe9, 0x7b, 0x02];
    const FILL_SELECTOR: [u8; 4] = [0x32, 0x90, 0xf8, 0x1e];
    const SWEEP_SELECTOR: [u8; 4] = [0x66, 0xcf, 0x5b, 0x60];

    // Commit step data: real updatePrices selector, opaque body.
    const COMMIT_DATA: &str =
        "0x86e97b02000000000000000000000000000000000000000000000000000000000000002a";

    // abi.encode(WETH, USDC, COMMIT_DATA, [leg1, leg2]) generated with
    // `cast abi-encode
    // "f(address,address,bytes,((address,address,address,address,(uint256,uint256)[],uint256,uint256),uint256)[])"`
    // to cross-check alloy's params encoding against Solidity's abi.encode.
    const EXPECTED_ENCODED: &str = "0000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda02913000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000e0000000000000000000000000000000000000000000000000000000000000002486e97b02000000000000000000000000000000000000000000000000000000000000002a0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000008ac7230489e80000000000000000000000000000222222222222222222222222222222222222222200000000000000000000000011111111111111111111111111111111111111110000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda0291300000000000000000000000000000000000000000000000000000000000000e00000000000000000000000000000000000000000000000000000000000000007000000000000000000000000000000000000000000000000000000006866519e00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000008ac7230489e80000000000000000000000000000000000000000000000000000000000006ff00180000000000000000000000000000000000000000000000001158e460913d00000000000000000000000000000000000000000000000000000000000006fe0bf4000000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000004563918244f40000000000000000000000000000444444444444444444444444444444444444444400000000000000000000000033333333333333333333333333333333333333330000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda0291300000000000000000000000000000000000000000000000000000000000000e00000000000000000000000000000000000000000000000000000000000000009000000000000000000000000000000000000000000000000000000006866519e00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000004563918244f40000000000000000000000000000000000000000000000000000000000006fd91e20";

    const SETTLEMENT: Address = address!("aaaa00000000000000000000000000000000aaaa");
    const ANCHOR_EXECUTOR: Address = address!("eeee00000000000000000000000000000000eeee");

    fn weth() -> Bytes {
        Bytes::from_str("0x4200000000000000000000000000000000000006").unwrap()
    }

    fn usdc() -> Bytes {
        Bytes::from_str("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap()
    }

    fn far_future() -> u64 {
        4102444800 // 2100-01-01
    }

    fn ladder(mm: Address, provider: Address, levels: Vec<(u128, u64)>, nonce: u64) -> PriceLadder {
        PriceLadder {
            mm,
            provider,
            tokenIn: bytes_to_address(&weth()).unwrap(),
            tokenOut: bytes_to_address(&usdc()).unwrap(),
            levels: levels
                .into_iter()
                .map(|(size, price)| Level { size: U256::from(size), price: U256::from(price) })
                .collect(),
            nonce: U256::from(nonce),
            expiresAt: U256::from(1751536030u64),
        }
    }

    fn fill_step_data(l: PriceLadder, amount_in: u128) -> Vec<u8> {
        fillFromAnchorCall {
            ladder: l,
            amountIn: U256::from(amount_in),
            receiver: address!("fd0b31d2e955fa55e3fa641fe90e08b677188d35"),
        }
        .abi_encode()
    }

    fn step(to: Address, data: Vec<u8>) -> Step {
        Step { to, value: U256::ZERO, data: data.into(), isDelegatecall: false }
    }

    fn fill_step_1() -> Step {
        let l = ladder(
            address!("2222222222222222222222222222222222222222"),
            address!("1111111111111111111111111111111111111111"),
            vec![
                (10_000_000_000_000_000_000, 1_878_000_000),
                (20_000_000_000_000_000_000, 1_877_000_000),
            ],
            7,
        );
        step(ANCHOR_EXECUTOR, fill_step_data(l, 10_000_000_000_000_000_000))
    }

    fn fill_step_2() -> Step {
        let l = ladder(
            address!("4444444444444444444444444444444444444444"),
            address!("3333333333333333333333333333333333333333"),
            vec![(5_000_000_000_000_000_000, 1_876_500_000)],
            9,
        );
        step(ANCHOR_EXECUTOR, fill_step_data(l, 5_000_000_000_000_000_000))
    }

    fn commit_step() -> Step {
        step(ANCHOR_EXECUTOR, alloy::hex::decode(COMMIT_DATA).unwrap())
    }

    fn transfer_step() -> Step {
        let data = transferCall {
            to: ANCHOR_EXECUTOR,
            amount: U256::from(15_000_000_000_000_000_000u128),
        }
        .abi_encode();
        step(bytes_to_address(&weth()).unwrap(), data)
    }

    fn sweep_step() -> Step {
        let data = sweepBalanceCall {
            token: bytes_to_address(&weth()).unwrap(),
            receiver: address!("fd0b31d2e955fa55e3fa641fe90e08b677188d35"),
            minAmount: U256::ZERO,
        }
        .abi_encode();
        step(SETTLEMENT, data)
    }

    fn settlement_call_json(steps: Vec<Step>, amount_in: u128) -> Bytes {
        let data = swapCall {
            params: SwapParams {
                tokenIn: bytes_to_address(&weth()).unwrap(),
                tokenOut: bytes_to_address(&usdc()).unwrap(),
                amountIn: U256::from(amount_in),
                minAmountOut: U256::ZERO,
                receiver: address!("fd0b31d2e955fa55e3fa641fe90e08b677188d35"),
            },
            steps,
        }
        .abi_encode();
        calls_json(&[(&format!("{SETTLEMENT}"), &format!("0x{}", encode(&data)))])
    }

    fn calls_json(calls: &[(&str, &str)]) -> Bytes {
        let calls: Vec<serde_json::Value> = calls
            .iter()
            .map(|(to, data)| serde_json::json!({"to": to, "value": "0", "data": data}))
            .collect();
        serde_json::to_vec(&calls)
            .unwrap()
            .into()
    }

    fn default_calls() -> Bytes {
        settlement_call_json(
            vec![commit_step(), transfer_step(), fill_step_1(), fill_step_2(), sweep_step()],
            15_000_000_000_000_000_000,
        )
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

    fn encoder() -> BiconomySwapEncoder {
        BiconomySwapEncoder::new(
            Bytes::from("0x543778987b293C7E8Cf0722BB2e935ba6f4068D4"),
            Chain::Base,
            None,
        )
        .unwrap()
    }

    #[test]
    fn test_encode_with_protocol_state() {
        // 15 WETH -> USDC over two maker legs (10 + 5 WETH) inside one
        // settlement call, using a mocked RFQ state to get the firm quote.
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
    fn test_encode_expired_quote() {
        let swap = propamm_swap(
            quote_attributes(default_calls(), 1751536030), // in the past
            BigUint::from_str("15000000000000000000").unwrap(),
        );

        let error = encoder()
            .encode_swap(&swap, &encoding_context())
            .unwrap_err();

        assert!(
            matches!(error, EncodingError::RecoverableError(ref msg) if msg.contains("expired"))
        );
    }

    #[test]
    fn test_encode_missing_calls_attribute() {
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
    fn test_encode_rejects_multiple_top_level_calls() {
        let single =
            settlement_call_json(vec![commit_step(), fill_step_1()], 10_000_000_000_000_000_000);
        let calls: Vec<serde_json::Value> = [&single, &single]
            .iter()
            .map(|c| serde_json::from_slice::<serde_json::Value>(c.as_ref()).unwrap()[0].clone())
            .collect();
        let doubled: Bytes = serde_json::to_vec(&calls)
            .unwrap()
            .into();
        let swap = propamm_swap(
            quote_attributes(doubled, far_future()),
            BigUint::from_str("10000000000000000000").unwrap(),
        );

        let error = encoder()
            .encode_swap(&swap, &encoding_context())
            .unwrap_err();

        assert!(
            matches!(error, EncodingError::FatalError(ref msg) if msg.contains("exactly one settlement call"))
        );
    }

    #[test]
    fn test_encode_rejects_non_swap_call() {
        let calls = calls_json(&[(
            "0xaaaa00000000000000000000000000000000aaaa",
            COMMIT_DATA, // updatePrices selector at the top level, not swap()
        )]);
        let swap = propamm_swap(
            quote_attributes(calls, far_future()),
            BigUint::from_str("15000000000000000000").unwrap(),
        );

        let error = encoder()
            .encode_swap(&swap, &encoding_context())
            .unwrap_err();

        assert!(
            matches!(error, EncodingError::FatalError(ref msg) if msg.contains("not settlement swap"))
        );
    }

    #[test]
    fn test_encode_no_fill_steps() {
        let calls =
            settlement_call_json(vec![commit_step(), transfer_step()], 15_000_000_000_000_000_000);
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
    fn test_encode_fill_step_before_commit() {
        let calls =
            settlement_call_json(vec![fill_step_1(), commit_step()], 10_000_000_000_000_000_000);
        let swap = propamm_swap(
            quote_attributes(calls, far_future()),
            BigUint::from_str("10000000000000000000").unwrap(),
        );

        let error = encoder()
            .encode_swap(&swap, &encoding_context())
            .unwrap_err();

        assert!(
            matches!(error, EncodingError::FatalError(ref msg) if msg.contains("before the price commit"))
        );
    }

    #[test]
    fn test_encode_fill_step_target_mismatch() {
        let mut fill = fill_step_1();
        fill.to = address!("dddd00000000000000000000000000000000dddd");
        let calls = settlement_call_json(vec![commit_step(), fill], 10_000_000_000_000_000_000);
        let swap = propamm_swap(
            quote_attributes(calls, far_future()),
            BigUint::from_str("10000000000000000000").unwrap(),
        );

        let error = encoder()
            .encode_swap(&swap, &encoding_context())
            .unwrap_err();

        assert!(
            matches!(error, EncodingError::FatalError(ref msg) if msg.contains("anchor executor"))
        );
    }

    #[test]
    fn test_encode_rejects_unknown_step() {
        let calls = settlement_call_json(
            vec![commit_step(), fill_step_1(), step(ANCHOR_EXECUTOR, vec![0xde, 0xad, 0xbe, 0xef])],
            10_000_000_000_000_000_000,
        );
        let swap = propamm_swap(
            quote_attributes(calls, far_future()),
            BigUint::from_str("10000000000000000000").unwrap(),
        );

        let error = encoder()
            .encode_swap(&swap, &encoding_context())
            .unwrap_err();

        assert!(
            matches!(error, EncodingError::FatalError(ref msg) if msg.contains("Unrecognized"))
        );
    }

    #[test]
    fn test_encode_rejects_multihop_fill() {
        let mut l = ladder(
            address!("2222222222222222222222222222222222222222"),
            address!("1111111111111111111111111111111111111111"),
            vec![(10_000_000_000_000_000_000, 1_878_000_000)],
            7,
        );
        // A leg on an intermediate pair: tokenOut is not the swap's tokenOut.
        l.tokenOut = address!("cbb7c0000ab88b473b1f5afd9ef808440eed33bf");
        let fill = step(ANCHOR_EXECUTOR, fill_step_data(l, 10_000_000_000_000_000_000));
        let calls = settlement_call_json(vec![commit_step(), fill], 10_000_000_000_000_000_000);
        let swap = propamm_swap(
            quote_attributes(calls, far_future()),
            BigUint::from_str("10000000000000000000").unwrap(),
        );

        let error = encoder()
            .encode_swap(&swap, &encoding_context())
            .unwrap_err();

        assert!(
            matches!(error, EncodingError::RecoverableError(ref msg) if msg.contains("intermediate pair"))
        );
    }

    #[test]
    fn test_encode_leg_sum_mismatch() {
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
    fn test_selectors_match_cast() {
        assert_eq!(swapCall::SELECTOR, SWAP_SELECTOR);
        assert_eq!(updatePricesCall::SELECTOR, UPDATE_PRICES_SELECTOR);
        assert_eq!(fillFromAnchorCall::SELECTOR, FILL_SELECTOR);
        assert_eq!(sweepBalanceCall::SELECTOR, SWEEP_SELECTOR);
    }

    #[test]
    fn test_encoder_chain_gate() {
        assert!(BiconomySwapEncoder::new(Bytes::zero(20), Chain::Ethereum, None).is_err());
        assert!(BiconomySwapEncoder::new(Bytes::zero(20), Chain::Bsc, None).is_ok());
    }
}
