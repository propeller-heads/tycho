# Complete Case Study: Euler Hooks (External Liquidity Example)

{% hint style="info" %}
⚠️ **Important Context**: Euler represents a hook with **EXTERNAL LIQUIDITY**. This case study demonstrates implementing custom metadata generators and parsers. **If your hook is Composable and uses internal PoolManager liquidity, your hook should be already indexed by Tycho.**
{% endhint %}

This section provides a comprehensive walkthrough of the Euler hook integration as a real-world example of handling external liquidity.

#### Euler Vault Architecture <a href="#euler-vault-architecture" id="euler-vault-architecture"></a>

**What is Euler?** Euler is a lending protocol that allows users to deposit tokens into vaults to earn yield. Each vault is an ERC-4626 compliant contract that manages deposits and withdrawals.

Euler is a standalone protocol, that designed an interface to be Hook-compliant, allowing it to be accessible by UniswapV4 Pools. This is a common pattern with current hooks, and are considered by Tycho Hooks with External Liquidity.

You can learn more about the protocol <a href="https://docs.euler.finance/" target="_blank" rel="noopener noreferrer">here</a>

**Euler Hook Pattern (External Liquidity)**:

```
┌──────────────────────────┐
│  Uniswap V4 Euler Hook   │
│  (Liquidity Coordinator) │
└────────────┬─────────────┘
             │
             │ Manages deposits/withdrawals
             ↓
┌──────────────────────────┐
│  Euler Vault Contract    │  ← EXTERNAL liquidity storage
│  - Token0 deposited      │
│  - Token1 deposited      │
│  - Earns lending yield   │
└──────────────────────────┘
```

**Contrast with Internal Liquidity**:

```
Internal Liquidity Hook (No Custom Code Needed):
┌──────────────────────────┐
│  Uniswap V4 Hook         │
└────────────┬─────────────┘
             │
             ↓
┌──────────────────────────┐
│  PoolManager (ERC6909)   │  ← INTERNAL liquidity storage
│  - Automatic extraction  │
└──────────────────────────┘
```

**Why Euler Requires Custom Implementation**:

1. **External Balances**: Tokens are in Euler vaults, not PoolManager → Need `MetadataRequestGenerator`
2. **Withdrawal Limits**: Vaults have maximum withdrawal amounts → Need limits fetching logic
3. **Yield Accrual**: Balances increase over time from lending yield → Need periodic balance updates
4. **Multiple Vaults**: Each token pair might use different vault addresses → Need parser logic

**What Euler Does NOT Need**:

* ❌ Custom Hook Orchestrator (default works fine)
* ❌ Special entrypoint encoding (standard Uniswap V4 swaps)
* ❌ Custom state transformations

#### Implementation Walkthrough <a href="#implementation-walkthrough" id="implementation-walkthrough"></a>

**1. Balance Collection**

**Objective**: Query the current token reserves in the Euler vaults.

**Approach**: Euler hooks implement a `getReserves()` function that returns the current balances of both tokens.

**Code**:

```rust
// From: euler/metadata_generator.rs

fn create_balance_request(
    &self,
    component: &ProtocolComponent,
    block: &Block,
    hook_address: &Address,
) -> Result<MetadataRequest, MetadataError> {
    Ok(MetadataRequest {
        request_type: MetadataRequestType::ComponentBalance {
            token_addresses: component.tokens.clone(),
        },
        routing_key: "rpc_default".to_string(),
        generator_name: "euler".to_string(),
        transport: RpcTransport::new(
            self.rpc_url.clone(),
            "eth_call".to_string(),
            vec![
                json!({
                    "to": hook_address,
                    "data": "0x0902f1ac" // getReserves() selector
                }),
                json!(format!("0x{:x}", block.number)),
            ],
        ),
    })
}
```

**Response Format**:

```
0x
  0000000000000000000000000000000000000000000000000de0b6b3a7640000  // reserve0 (1e18)
  0000000000000000000000000000000000000000000000000de0b6b3a7640000  // reserve1 (1e18)
```

**Parsing**:

```rust
// From: euler/metadata_generator.rs

fn parse_balance_response(&self, res_str: &str) -> Result<MetadataValue, MetadataError> {
    // Extract two 32-byte values
    let balance_0 = Bytes::from(&res_str[0..64]);
    let balance_1 = Bytes::from(&res_str[64..128]);

    // Map to sorted tokens
    let mut tokens = component.tokens.clone();
    tokens.sort();

    let mut balances = HashMap::new();
    balances.insert(tokens[0].clone(), balance_0);
    balances.insert(tokens[1].clone(), balance_1);

    Ok(MetadataValue::Balances(balances))
}
```

**2. Limits Collection Using Lens Contract**

**Objective**: Determine the maximum swap amounts for each direction (token0→token1, token1→token0).

**Challenge**: Euler vaults have withdrawal limits that depend on available liquidity, which requires complex calculations involving multiple contract calls.

**Solution**: Deploy a "lens" contract via state overrides that performs the calculation in a single `eth_call`.

**Lens Contract Pattern**:

```solidity
// Simplified EulerLensContract
contract EulerLensContract {
    address public hook; // Stored in slot 0
    
    function getLimits(address tokenIn, address tokenOut)
        external
        view
        returns (uint256 realInLimit, uint256 realOutLimit)
    {
        IEulerSwap pool = IEulerSwap(hookAddress);

        // Step 1: Get the protocol limits
        (uint256 inLimit, uint256 outLimit) = pool.getLimits(tokenIn, tokenOut);

        // If no limits returned (e.g., not authorized), return zeros
        if (inLimit == 0 && outLimit == 0) {
            return (0, 0);
        }

        // Step 2: Compute quotes in both directions
        uint256 quotedOutFromIn;
        uint256 requiredInFromOut;
        bool exactInSucceeded = false;
        bool exactOutSucceeded = false;

        // Try exactIn = inLimit
        try pool.computeQuote(tokenIn, tokenOut, inLimit * 99 / 100, true) returns (uint256 quotedOut) {
            quotedOutFromIn = quotedOut;
            exactInSucceeded = true;
        } catch {}

        // Try exactOut = outLimit
        try pool.computeQuote(tokenIn, tokenOut, outLimit * 99 / 100, false) returns (uint256 requiredIn) {
            requiredInFromOut = requiredIn;
            exactOutSucceeded = true;
        } catch {}

        // Step 3: If both failed, revert
        if (!exactInSucceeded && !exactOutSucceeded) {
            revert QuoteComputationFailed();
        }

        // Step 4: Keep the smallest valid limits
        if (exactInSucceeded && exactOutSucceeded) {
            // Both succeeded - take the minimum of both approaches
            uint256 outLimitFromIn = quotedOutFromIn < outLimit ? quotedOutFromIn : outLimit;
            uint256 inLimitFromOut = requiredInFromOut < inLimit ? requiredInFromOut : inLimit;

            // Choose the approach that gives the smallest limits
            realInLimit = inLimitFromOut < inLimit ? inLimitFromOut : inLimit;
            realOutLimit = outLimitFromIn < outLimit ? outLimitFromIn : outLimit;
        } else if (exactInSucceeded) {
            // Only exactIn succeeded
            realInLimit = inLimit;
            realOutLimit = quotedOutFromIn < outLimit ? quotedOutFromIn : outLimit;
        } else {
            // Only exactOut succeeded
            realInLimit = requiredInFromOut < inLimit ? requiredInFromOut : inLimit;
            realOutLimit = outLimit;
        }
    }
}
```

**Request Generation**:

```rust
// From: euler/metadata_generator.rs

fn create_limits_request(
    &self,
    component: &ProtocolComponent,
    block: &Block,
    hook_address: &Address,
    token_pair: &[Address],
) -> Result<MetadataRequest, MetadataError> {
    let lens_address = "0x0000000000000000000000000000000000001337";
    let lens_bytecode_hex = hex::encode(EULER_LENS_BYTECODE_BYTES);

    // Encode getLimits(address,address) call
    let token0_hex = &token_pair[0].to_string()[2..];  // Remove 0x prefix
    let token1_hex = &token_pair[1].to_string()[2..];
    let calldata = format!("0xaaed87a3{token0_hex}{token1_hex}");

    Ok(MetadataRequest {
        request_type: MetadataRequestType::Limits {
            token_pair: token_pair.to_vec(),
        },
        routing_key: "rpc_default".to_string(),
        generator_name: "euler".to_string(),
        transport: RpcTransport::new(
            self.rpc_url.clone(),
            "eth_call".to_string(),
            vec![
                json!({
                    "to": lens_address,
                    "data": calldata
                }),
                json!(format!("0x{:x}", block.number)),
                json!({
                    lens_address: {
                        // Deploy lens bytecode at deterministic address
                        "code": format!("0x{}", lens_bytecode_hex),
                        "state": {
                            // Store hook address in slot 0
                            "0x0000000000000000000000000000000000000000000000000000000000000000":
                                format!("0x{:0>64}", &hook_address.to_string()[2..])
                        }
                    }
                }),
            ],
        ),
    })
}
```

**Response Format**:

```
0x
  0000000000000000000000000000000000000000000000056bc75e2d63100000  // limit0 (100e18)
  0000000000000000000000000000000000000000000000056bc75e2d63100000  // limit1 (100e18)
```

**Parsing**:

```rust
// From: euler/metadata_generator.rs

fn parse_limits_response(
    &self,
    component: &ProtocolComponent,
    request: &MetadataRequest,
    res_str: &str,
    token_pair: &[Address],
) -> Result<MetadataValue, MetadataError> {
    // Extract limits
    let limit_0 = Bytes::from(&res_str[0..64]);
    let limit_1 = Bytes::from(&res_str[64..128]);

    // Create entrypoint for the limits call (for reference)
    let limits_entrypoint = create_euler_limits_entrypoint(
        component,
        hook_address,
        token_pair,
    )?;

    Ok(MetadataValue::Limits(vec![
        (token_pair[0].clone(), (limit_0, limit_1, Some(limits_entrypoint)))
    ]))
}
```

**3. Entrypoint Generation with Detected Slots**

**Objective**: Generate entrypoints that simulate swaps with correct balance overwrites for both PoolManager and external vault tokens.

**Process**:

1. **Estimate Swap Amounts** (using limits):

```rust
// From: entrypoint_generator.rs

let estimator = DefaultSwapAmountEstimator::with_limits();
let swap_amounts = estimator.estimate_swap_amounts(&metadata, &component.tokens)?;

// For Euler with limits = [100e18, 100e18]:
// swap_amounts = [
//     (token0, token1, 1e18),   // 1% of limit
//     (token0, token1, 10e18),  // 10% of limit
//     (token0, token1, 50e18),  // 50% of limit
//     (token0, token1, 95e18),  // 95% of limit
// ]
```

2. **Detect Balance Slots** (for wstETH, WETH, etc.):

```rust
// From: entrypoint_generator.rs

let detected_slots = balance_slot_detector
    .detect_balance_slots(
        &component.tokens,
        pool_manager,
        &block.hash,
    )
    .await?;

// Returns mapping: token_address → storage_slot
// Example: wstETH → 0x0000...0001 (slot 1 for standard ERC20)
```

3. **Build State Overrides**:

```rust
let mut state_overrides = HashMap::new();

// A. Deploy V4MiniRouter
state_overrides.insert(
    router_address,
    AccountOverrides {
        code: Some(V4_MINI_ROUTER_BYTECODE),
        balance: None,
        nonce: None,
        slots: None,
    },
);

// B. Set ERC6909 balances in PoolManager
let erc6909_slot = calculate_erc6909_balance_slot(&sender, &token_in);
state_overrides.insert(
    pool_manager,
    AccountOverrides {
        slots: Some(StorageOverride::Diff(
            vec![(erc6909_slot, amount_in * 2)].into_iter().collect()
        )),
        ..Default::default()
    },
);

// C. Set detected ERC20 balance slots
if let Some(token_in_slot) = detected_slots.get(&token_in) {
    state_overrides.insert(
        token_in.clone(),
        AccountOverrides {
            slots: Some(StorageOverride::Diff(
                vec![(token_in_slot.clone(), amount_in * 2)].into_iter().collect()
            )),
            ..Default::default()
        },
    );
}
```

4. **Create Entrypoint**:

```rust
// Build V4Router execute() call
let pool_key = build_pool_key_from_component(component)?;
let params = ExactInputSingleParams {
    pool_key,
    zero_for_one: true,
    amount_in,
    amount_out_minimum: Bytes::from([0u8]),
    hook_data: Bytes::from([0u8]),
};

let actions = vec![
    V4RouterAction::SWAP_EXACT_IN_SINGLE,
    V4RouterAction::SETTLE_ALL,
    V4RouterAction::TAKE_ALL,
];

let calldata = encode_execute_call(actions, vec![params])?;

let entrypoint = EntryPointWithTracingParams {
    entry_point: EntryPoint {
        external_id: format!("swap_{}_{}_{}_{}",
            component.id, token_in, token_out, amount_in),
        target: router_address,
        signature: "execute(bytes,bytes[])".to_string(),
    },
    params: TracingParams::RPCTracer(RPCTracerParams {
        caller: Some(sender),
        calldata,
        state_overrides: Some(state_overrides),
        prune_addresses: None,
    }),
};
```

**4. Full Processing Flow**

**Initialization** (one-time):

```
1. Load all uniswap_v4_hooks components from database
2. Filter for components with swap hook permissions
3. Check if entrypoints already exist
   - Has entrypoints → State = TracingComplete
   - No entrypoints → State = Unprocessed
4. Cache all components and states
```

**Block Processing** (per block):

```
1. Extract components with balance/state changes
2. Filter for swap hook permissions
3. Categorize:
   - Unprocessed → Full processing list
   - TracingComplete → Balance-only list
   - Failed (retryable) → Full processing list
   - Failed (paused) → Skip

4. Collect Metadata:
   - Full processing: getLimits() + getReserves()
   - Balance-only: getReserves()

5. Check for metadata errors:
   - Errors → Mark as Failed, increment retry_count
   - retry_count >= pause_after_retries → Set "paused" attribute

6. Process each component via orchestrator:
   - Generate entrypoints (if full processing)
   - Inject balances into component
   - Inject limits for optimization
   - Update block_changes

7. Delegate to inner DCI:
   - Trace entrypoints
   - Store results in database
   - Prune old data

8. Handle finality:
   - Prune cache layers below finalized height
```

#### Key Takeaways from Euler <a href="#key-takeaways-from-euler" id="key-takeaways-from-euler"></a>

1. **Balance Slot Detection**: Essential for hooks with external token holdings
2. **Lens Contract Pattern**: Powerful technique for complex multi-call queries using state overrides
3. **Limits-Based Estimation**: Provides more accurate swap amount samples than balance-based
4. **Default Orchestrator**: Often sufficient even for complex hooks like Euler
5. **State Override Composition**: Combine router deployment, ERC6909 overwrites, and ERC20 overwrites in a single call

The Euler implementation demonstrates that with proper metadata collection and entrypoint generation, the Hooks DCI can handle even complex external liquidity scenarios.

