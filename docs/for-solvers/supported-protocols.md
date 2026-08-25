# Supported Protocols

Currently, Tycho supports the following protocols:

<table data-full-width="true">
<thead><tr><th width="204.7578125">Protocol</th><th width="251.88671875">Integration Type</th><th width="156.2734375">Simulation Time</th><th width="148.26953125">Chains</th><th width="261.26953125">Partial Support Notes</th></tr></thead>
<tbody>
<tr><td><code>uniswap_v2</code></td><td>Native (<code>UniswapV2State</code>)</td><td>1 μs (0.001 ms)</td><td>Ethereum, Base, Unichain</td><td></td></tr>
<tr><td><code>uniswap_v3</code></td><td>Native (<code>UniswapV3State</code>)</td><td>20 μs (0.02 ms)</td><td>Ethereum, Base, Unichain</td><td></td></tr>
<tr><td><code>uniswap_v4</code></td><td>Native (<code>UniswapV4State</code>)</td><td>3 μs (0.003 ms)</td><td>Ethereum, Base, Unichain</td><td>Only core uniswap V4 pools are supported on this native implementation.</td></tr>
<tr><td><code>uniswap_v4_hooks</code></td><td>Hybrid (<code>UniswapV4State</code>)<br>[DCI indexed]</td><td>1 ms</td><td>Ethereum, Unichain</td><td>All composable hooks are supported.<br><strong>Angstrom</strong>: see more details <a href="supported-protocols.md#angstrom-uniswap-v4-hook">below</a>.<br><em>recommended</em>: set a high startup timeout on the stream builder: <code>.startup_timeout(Duration::from_secs(120))</code></td></tr>
<tr><td><code>vm:balancer_v2</code></td><td>VM (<code>EVMPoolState</code>)<br>[DCI indexed]</td><td>0.5 ms</td><td>Ethereum</td><td>A few pools are currently unsupported. Use <code>balancer_v2_pool_filter</code></td></tr>
<tr><td><code>vm:curve</code></td><td>Hybrid (<code>CurveState</code>)<br>[DCI indexed]</td><td>30 μs (0.03 ms)</td><td>Ethereum</td><td>Pools with rate-bearing or rebasing coins are unsupported. Use <code>curve_filter</code>.<br>NOTE: curve requires a node RPC to fetch some code at startup. Please set the <code>RPC_URL</code> env var.</td></tr>
<tr><td><code>sushiswap_v2</code></td><td>Native (<code>UniswapV2State</code>)</td><td>1 μs (0.001 ms)</td><td>Ethereum</td><td></td></tr>
<tr><td><code>pancakeswap_v2</code></td><td>Native (<code>PancakeswapV2State</code>)</td><td>1 μs (0.001 ms)</td><td>Ethereum</td><td></td></tr>
<tr><td><code>pancakeswap_v3</code></td><td>Native (<code>UniswapV3State</code>)</td><td>20 μs (0.02 ms)</td><td>Ethereum, Base</td><td></td></tr>
<tr><td><code>quickswap_v2</code></td><td>Native (<code>UniswapV2State</code>)</td><td>3 μs (0.003 ms)</td><td>Polygon</td><td></td></tr>
<tr><td><code>ekubo_v2</code></td><td>Native (<code>EkuboState</code>)</td><td>1.5 μs (0.0015 ms)</td><td>Ethereum</td><td></td></tr>
<tr><td><code>ekubo_v3</code></td><td>Native (<code>EkuboV3State</code>)</td><td>9μs</td><td>Ethereum</td><td>Some extensions are unsupported. Use <code>ekubo_v3_extension_filter</code>. It also drops SignedExclusiveSwap pools, which need a per-swap signature passed to the encoder as <code>user_data</code>. If you can supply that signature, use <code>ekubo_v3_extension_filter_with_signed_exclusive_swap</code> instead to keep those pools.</td></tr>
<tr><td><code>vm:maverick_v2</code></td><td>VM (<code>EVMPoolState</code>)</td><td>-</td><td>Ethereum</td><td></td></tr>
<tr><td><code>vm:bopamm</code></td><td>VM (<code>EVMPoolState</code>)</td><td>-</td><td>Ethereum</td><td></td></tr>
<tr><td><code>vm:fermiswap</code></td><td>VM (<code>EVMPoolState</code>)</td><td>4 ms</td><td>Ethereum</td><td></td></tr>
<tr><td><code>aerodrome_v1</code></td><td>Native (<code>AerodromeV1State</code>)</td><td>3 μs (0.003 ms)</td><td>Base</td><td></td></tr>
<tr><td><code>aerodrome_slipstreams</code></td><td><p>Native</p><p>(<code>AerodromeSlipstreamsState</code>)</p></td><td>-</td><td>Base</td><td></td></tr>
<tr><td><code>lunarbase</code></td><td>Native (<code>LunarBaseState</code>)</td><td>7 μs (0.007 ms)</td><td>Base</td><td></td></tr>
<tr><td><code>pendle</code></td><td>Native (<code>PendleState</code>)</td><td>10 μs (0.01 ms)</td><td>Ethereum</td><td>Two component types under one system: <code>pendle_market</code> (SY↔PT↔YT) and <code>pendle_sy</code> (the ERC-5115 wrapper). A quote is only exact at the block the SY exchange rate was read at — a state the chain has moved past refuses to quote rather than extrapolating. Expired markets report zero depth and error on every edge. SY tokens the indexer could not classify carry no wrap edge; use <code>pendle_filter</code> to drop an SY that has none in either direction. Native ETH legs are unsupported — wrap first.</td></tr>
<tr><td><code>rocketpool</code></td><td>Native (<code>RocketpoolState</code>)</td><td>-</td><td>Ethereum</td><td>Note: the DepositPool was recently updated to v1.4. This new version is supported by tycho_simulation <a href="https://github.com/propeller-heads/tycho-simulation/releases/tag/0.248.0" target="_blank" rel="noopener noreferrer">> v0.248.0</a> and above.</td></tr>
<tr><td><code>fluid_v1</code></td><td>Native (<code>FluidV1</code>)</td><td>-</td><td>Ethereum</td><td>Note: paused pools are still indexed. To filter them out use <code>fluid_v1_paused_pools_filter</code>.</td></tr>
<tr><td><code>cowamm</code></td><td>Native (<code>CowAMMState</code>)</td><td>-</td><td>Ethereum</td><td><p>CoWAMM doesn't have a Tycho Execution component. This is because of CoWAMM's unique design where only cowswap solvers can unlock the liquidity pools after sharing a quote.</p><p>You will have to integrate execution yourself (see <a href="https://docs.cow.fi/cow-amm/tutorials/cow-amm-for-solvers#creating-cow-amm-orders-with-the-helper-contract" target="_blank" rel="noopener noreferrer">cowamm docs</a> and <a href="https://github.com/adpthegreat/cowamm-execution/blob/main/examples/example.rs" target="_blank" rel="noopener noreferrer">example</a>).</p></td></tr>
</tbody>
</table>

For RFQ protocols that fetch prices from market makers via WebSocket or API, see [Request for Quote Protocols](request-for-quote-protocols.md).

{% hint style="info" %}
**Live tracker & Upcoming protocols**

* Currently supported protocols and Tycho status: <a href="http://tycho.live/" target="_blank" rel="noopener noreferrer">http://tycho.live/</a>
* <a href="https://docs.google.com/spreadsheets/d/1vDl57BthpeJ9WDqmCVXFdLLvmsm-_SwhSjVzobXJZ04/edit?usp=sharing" target="_blank" rel="noopener noreferrer">List of upcoming protocols</a>
{% endhint %}

{% hint style="info" %}
**Register code snippet**

{% code expandable="true" %}
```rust
fn register_exchanges(
    mut builder: ProtocolStreamBuilder,
    chain: &Chain,
    tvl_filter: ComponentFilter,
) -> ProtocolStreamBuilder {
    match chain {
        Chain::Ethereum => {
            builder = builder
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4_hooks", tvl_filter.clone(), None)
                .exchange::<UniswapV2State>("sushiswap_v2", tvl_filter.clone(), None)
                .exchange::<PancakeswapV2State>("pancakeswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("pancakeswap_v3", tvl_filter.clone(), None)
                .exchange::<EVMPoolState<PreCachedDB>>("vm:balancer_v2", tvl_filter.clone(), Some(balancer_v2_pool_filter))
                .exchange::<EkuboState>("ekubo_v2", tvl_filter.clone(), None)
                .exchange::<EkuboV3State>("ekubo_v3", tvl_filter.clone(), Some(ekubo_v3_extension_filter))
                .exchange::<CurveState>("vm:curve", tvl_filter.clone(), Some(curve_filter))
                .exchange::<EVMPoolState<PreCachedDB>>("vm:maverick_v2", tvl_filter.clone(), None)
                .exchange::<EVMPoolState<PreCachedDB>>("vm:bopamm", tvl_filter.clone(), None)
                .exchange::<EVMPoolState<PreCachedDB>>("vm:fermiswap", tvl_filter.clone(), None)
        }
        Chain::Base => {
            builder = builder
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("pancakeswap_v3", tvl_filter.clone(), None)
                .exchange::<AerodromeV1State>("aerodrome_v1", tvl_filter.clone(), None)
                .exchange::<AerodromeSlipstreamsState>("aerodrome_slipstreams", tvl_filter.clone(), None)
                .exchange::<LunarBaseState>("lunarbase", tvl_filter.clone(), None)
        }
        Chain::Unichain => {
            builder = builder
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None)
        }
        Chain::Polygon => {
            builder = builder
                .exchange::<UniswapV2State>("quickswap_v2", tvl_filter.clone(), None)
        }
        _ => {}
    }
    builder
}
```
{% endcode %}
{% endhint %}

### Integration Types

There are three types of protocol integrations:

* **Native** protocols have been implemented using an analytical approach and are ported to Rust - faster simulation.
* **VM** protocols execute the VM bytecode locally - this is easier to integrate the more complex protocols, however has slower simulation times than a native implementation.
  * Some VM protocols are **DCI indexed**. DCI is our Dynamic Contract Indexer and provides more flexibility on indexing restraints. Note - these protocols tend to serve a lot of data and experience occasional streaming delays.
* **Hybrid** uses a combination of the two - native for general protocol logic portable to Rust, and VM for the more complex or pool-specific logic.

Interested in adding a protocol? Refer to the [Tycho Simulation for DEXs](../for-dexs/protocol-integration/) documentation for implementation guidelines.

### Protocol-Specific Details

While most protocols work out of the box, some require additional configuration or have specific considerations you should be aware of.

#### Angstrom (Uniswap V4 Hook) <a href="#angstrom-uniswap-v4-hook" id="angstrom-uniswap-v4-hook"></a>

Angstrom locks its pools at the start of every block. A swap that trades against one in the same block must carry a pool unlock <a href="https://docs.angstrom.xyz/l1/core-mechanisms/pool-unlock#2-user-initiated-off-chain-signature-unlock" target="_blank" rel="noopener noreferrer">attestation</a>, which Tycho fetches from Angstrom's API, as its hook data. If the transaction lands after the attested blocks have passed, the attestation no longer unlocks the pool.

**Required configuration**:

* Set the `ANGSTROM_API_KEY` environment variable (request one from the Angstrom team directly)
* Set `ANGSTROM_BLOCKS_IN_FUTURE` environment variable (if you want to override the <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/src/encoding/evm/constants.rs" target="_blank" rel="noopener noreferrer">default value</a> of 5 blocks). **Important trade-off**: The more blocks you fetch, the more calldata will be sent to the Tycho Router, making execution more gas expensive.

If `ANGSTROM_API_KEY` is not set, `ProtocolStreamBuilder` excludes Angstrom pools from `uniswap_v4_hooks` by default (unless you pass your own filter function), since routes over these pools would fail at encoding without attestations.

**Attestations are prefetched**, so encoding an Angstrom swap makes no API call. A background thread refreshes the attestation window twice per block and encoding reads the result from a process-wide cache. The thread starts when you build a `SwapEncoderRegistry` for Ethereum with `ANGSTROM_API_KEY` set, and it reads all three environment variables once, at that point.

Two consequences for your setup:

* **Build the encoder once at startup and reuse it.** Encoding still works if you build a new encoder per quote, but the first Angstrom swap it encodes waits for the first fetch to finish.
* **Watch your logs for `Angstrom attestation cache is cold or stale`.** When the cached window is more than one block old, encoding fetches a fresh one inline and logs that warning. Encoding then pays the API round trip, so a steady stream of these means the background refresh is failing — the refresh logs its own error alongside them.
