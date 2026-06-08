# Supported Protocols

Currently, Tycho supports the following protocols:

<table data-full-width="true"><thead><tr><th width="204.7578125">Protocol</th><th width="251.88671875">Integration Type</th><th width="156.2734375">Simulation Time</th><th width="148.26953125">Chains</th><th width="261.26953125">Partial Support Notes</th></tr></thead><tbody><tr><td><code>uniswap_v2</code></td><td>Native (<code>UniswapV2State</code>)</td><td>1 μs (0.001 ms)</td><td>Ethereum, Base, Unichain</td><td></td></tr><tr><td><code>uniswap_v3</code></td><td>Native (<code>UniswapV3State</code>)</td><td>20 μs (0.02 ms)</td><td>Ethereum, Base, Unichain</td><td></td></tr><tr><td><code>uniswap_v4</code></td><td>Native (<code>UniswapV4State</code>)</td><td>3 μs (0.003 ms)</td><td>Ethereum, Base, Unichain</td><td>Only core uniswap V4 pools are supported on this native implementation.</td></tr><tr><td><code>uniswap_v4_hooks</code></td><td>Hybrid (<code>UniswapV4State</code>)<br>[DCI indexed]</td><td>1 ms</td><td>Ethereum, Unichain</td><td>All composable hooks are supported.<br><strong>Angstrom</strong>: see more details <a href="supported-protocols.md#angstrom-uniswap-v4-hook">below</a>.<br><em>recommended</em>: set a high startup timeout on the stream builder: <code>.startup_timeout(Duration::from_secs(120))</code></td></tr><tr><td><code>vm:balancer_v2</code></td><td>VM (<code>EVMPoolState</code>)<br>[DCI indexed]</td><td>0.5 ms</td><td>Ethereum</td><td>A few pools are currently unsupported. Use <code>balancer_v2_pool_filter</code></td></tr><tr><td><code>vm:curve</code></td><td>VM (<code>EVMPoolState</code>)<br>[DCI indexed]</td><td>1 ms</td><td>Ethereum</td><td>NOTE: curve requires a node RPC to fetch some code at startup. Please set the <code>RPC_URL</code> env var.</td></tr><tr><td><code>sushiswap_v2</code></td><td>Native (<code>UniswapV2State</code>)</td><td>1 μs (0.001 ms)</td><td>Ethereum</td><td></td></tr><tr><td><code>pancakeswap_v2</code></td><td>Native (<code>PancakeswapV2State</code>)</td><td>1 μs (0.001 ms)</td><td>Ethereum</td><td></td></tr><tr><td><code>pancakeswap_v3</code></td><td>Native (<code>UniswapV3State</code>)</td><td>20 μs (0.02 ms)</td><td>Ethereum, Base</td><td></td></tr><tr><td><code>ekubo_v2</code></td><td>Native (<code>EkuboState</code>)</td><td>1.5 μs (0.0015 ms)</td><td>Ethereum</td><td></td></tr><tr><td><code>ekubo_v3</code></td><td>Native (<code>EkuboV3State</code>)</td><td>9μs</td><td>Ethereum</td><td>Some extensions are unsupported. Use the <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-simulation/src/evm/protocol/ekubo_v3/mod.rs#L16" target="_blank" rel="noopener noreferrer">>ekubo_v3 filter fn</a>.</td></tr><tr><td><code>vm:maverick_v2</code></td><td>VM (<code>EVMPoolState</code>)</td><td>-</td><td>Ethereum</td><td></td></tr><tr><td><code>aerodrome_slipstreams</code></td><td><p>Native</p><p>(<code>AerodromeSlipstreamsState</code>)</p></td><td>-</td><td>Base</td><td></td></tr><tr><td><code>rocketpool</code></td><td>Native (<code>RocketpoolState</code>)</td><td>-</td><td>Ethereum</td><td>Note: the DepositPool was recently updated to v1.4. This new version is supported by tycho_simulation <a href="https://github.com/propeller-heads/tycho-simulation/releases/tag/0.248.0" target="_blank" rel="noopener noreferrer">>v0.248.0</a> and above.</td></tr><tr><td><code>fluid_v1</code></td><td>Native (<code>FluidV1</code>)</td><td>-</td><td>Ethereum</td><td>Note: paused pools are still indexed. To filter them out use <code>fluid_v1_paused_pools_filter</code>.</td></tr><tr><td><code>cowamm</code></td><td>Native (<code>CowAMMState</code>)</td><td>-</td><td>Ethereum</td><td><p>CoWAMM doesn't have a Tycho Execution component. This is because of CoWAMM's unique design where only cowswap solvers can unlock the liquidity pools after sharing a quote.</p><p>You will have to integrate execution yourself (see <a href="https://docs.cow.fi/cow-amm/tutorials/cow-amm-for-solvers#creating-cow-amm-orders-with-the-helper-contract" target="_blank" rel="noopener noreferrer">>cowamm docs</a> and <a href="https://github.com/adpthegreat/cowamm-execution/blob/main/examples/example.rs" target="_blank" rel="noopener noreferrer">>example</a>).</p></td></tr></tbody></table>

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
                .exchange::<EkuboV3State>("ekubo_v3", tvl_filter.clone(), Some(ekubo_v3::filter_fn))
                .exchange::<EVMPoolState<PreCachedDB>>("vm:curve", tvl_filter.clone(), None)
                .exchange::<EVMPoolState<PreCachedDB>>("vm:maverick_v2", tvl_filter.clone(), None)
        }
        Chain::Base => {
            builder = builder
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("pancakeswap_v3", tvl_filter.clone(), None)
                .exchange::<AerodromeSlipstreamsState>("aerodrome_slipstreams", tvl_filter.clone(), None)
        }
        Chain::Unichain => {
            builder = builder
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None)
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

#### Angstrom (Uniswap V4 Hook)

Angstrom requires querying their <a href="https://docs.angstrom.xyz/l1/core-mechanisms/pool-unlock#2-user-initiated-off-chain-signature-unlock" target="_blank" rel="noopener noreferrer">API for attestations</a> per block to unlock their contract. If execution comes too late, the contract can no longer be unlocked for that block.

**Required configuration**:

* Set the `ANGSTROM_API_KEY` environment variable (request one from the Angstrom team directly)
* Set `ANGSTROM_BLOCKS_IN_FUTURE` environment variable (if you want to override the <a href="https://github.com/propeller-heads/tycho-indexer/blob/main/crates/tycho-execution/src/encoding/evm/constants.rs" target="_blank" rel="noopener noreferrer">default value</a> of 5 blocks). **Important trade-off**: The more blocks you fetch, the more calldata will be sent to the Tycho Router, making execution more gas expensive.
