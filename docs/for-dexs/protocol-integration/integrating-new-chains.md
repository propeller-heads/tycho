# Integrating New Chains

Integrating a new chain with Tycho requires understanding how the indexer works under the hood.

## How Tycho Consumes Blockchain Data

The Tycho indexer uses [Substreams](https://github.com/streamingfast/substreams) technology to consume blockchain data, which requires access to a Substreams endpoint. StreamingFast provides hosted solutions for many popular chains, including:

- EVM mainnet chains (Ethereum)
- Layer 2 blockchains (Base, Arbitrum, Optimism)
- Alternative EVM L1s (Avalanche, Binance Smart Chain)

For smaller or newer chains without hosted solutions, you'll need to run your own infrastructure.

## Example: Integrating Hyperliquid

Let's walk through how you could go about integrating [Hyperliquid](https://app.hyperliquid.xyz/), an alternative L1 EVM-based chain that doesn't have a hosted Substreams endpoint.

### Choosing Your Integration Approach

There are two main approaches to integrate a chain without a hosted endpoint:

#### Instrumented Node Approach

This approach provides full access to blockchain data but requires more setup effort:

1. Start with the HyperEVM archive node, currently an in-progress [Reth fork called nanoreth](https://github.com/hl-archive-node/nanoreth)
2. Instrument it for Firehose compatibility
3. Host and serve it over a Substreams endpoint

While there are no Geth clients currently available for Hyperliquid (which would have simplified instrumentation), this approach gives you complete access to all blockchain data including gas changes, storage changes, state diffs, and internal traces.

#### RPC-Polled Firehose Approach

This approach is simpler but comes with tradeoffs. You'll need to:

1. Create Protocol Buffers for the [block structure](https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/examples/evm_block_indexer.py#L100)
2. Implement RPC modules for each [RPC method](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/json-rpc) Hyperliquid provides

The main limitation is that your data will be constrained by what's available through the exposed RPC methods. You won't have access to lower-level data like gas changes, storage changes, state diffs, internal traces, or detailed gas accounting. Keep these limitations in mind when writing your Substreams map handlers for data transformation.

## Token Quality Estimation Requirements

Tycho also needs a `debug_traceBlockByNumber` endpoint with the `callTracer` available on the chain's node client to estimate token quality. This isn't a blocking requirement though—work is underway to remove this dependency.

## Setting Up Your Infrastructure

Once you've chosen your approach and set up the necessary infrastructure, you'll find detailed guidance in these resources:

- [Firehose Ethereum Supported Networks](https://github.com/streamingfast/firehose-ethereum/blob/develop/SUPPORTED-NETWORKS.md#instrumented-nodes)
- [Firehose Documentation](https://firehose.streamingfast.io/)

After your Substreams endpoint is up and running, configure the Tycho Indexer to consume block data from it using the [CLI configuration](https://github.com/propeller-heads/tycho-indexer/blob/b84c6de83ca3e74856d8cd204a5141bf0377ee77/tycho-indexer/src/cli.rs#L60).

## Special Case: Geth and OP-Geth Based Chains

If your chain uses Geth or OP-Geth (like Worldchain, which uses the OP Stack), the integration process is much simpler. Instrumented Geth nodes are already available, significantly reducing setup complexity. For example, check out [Worldchain's EVM equivalence documentation](https://docs.world.org/world-chain/developers/evm-equivalence).

## Learning More

To dive deeper into Tycho's architecture and how all the pieces fit together, explore the [Tycho Indexer repository](https://github.com/propeller-heads/tycho-indexer/tree/main/tycho-indexer).

## Frequently Asked Questions

### Does it need to be an instrumented node or is RPC-polled ok too?

It depends on which protocols you want to index. For popular DEXes like Uniswap V2 and V3, an RPC-polled approach should work fine. If you need access to more detailed blockchain data or are working with protocols that require it, you'll want an instrumented node.

### Do we need Firehose specifically, or Substreams, or both?

You need to set up both Firehose and Substreams. While most Tycho users rely on hosted endpoints, if you're setting up your own infrastructure, the StreamingFast Discord is a great resource, their team provides quick and helpful support.
