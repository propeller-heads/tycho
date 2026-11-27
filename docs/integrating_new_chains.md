## Integrating new chains
To integrate new chains with Tycho you need to first understand how Tycho works under the hood.

The Tycho indexer uses [substreams](https://github.com/streamingfast/substreams) technology to get blocks, and to do this you'll need a substreams endpoint, currently, substreams provide hosted solutions for some popular EVM -based chains like Ethereum and also Layer 2 blockchains like base and alt EVM L1's like avalanche, Binance Smart Chain but not all of them, if the chain is small it is unlikely that Streamingfast provides a hosted solution for that so you'd have to run it yourself.

For this example we will be focusing on [hyperliquid](https://app.hyperliquid.xyz/) - an alt L1 EVM based chain

Since Hyperliquid is not available as a substreams hosted endpoint, to integrate it you'll have to create a firehose instrumented version of the hyperEVM archive node which is currently an in-progress [Reth fork](https://github.com/hl-archive-node/nanoreth) then host it and serve it over a substreams endpoint, there are currently no Geth clients for hyperliquid right now which would have saved the stress of creating an instrumented node but that'll still take effort to modify and run.

You can also take the route of making an RPC polled firehose integration instead of an instrumented one which will be relatively easier. Still you'll most likely not be able to get lower level data like gas changes, storage changes, state diffs, internal traces, gas accounting etc, the data you'll get will be limited by the rpc methods exposed, (you'll also need to take note of this when writing your substreams map handlers for transforming your data eventually), you'll have to create protobufs for the [block structure](https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/examples/evm_block_indexer.py#L100). Then Rpc modules for each of the [rpc methods](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/hyperevm/json-rpc) provided, this will not be a tutorial to do that, the details can be found in their guide [here]().

Lastly, another requirement is that you also need a `call_traceMany` endpoint available on the chain's node client; otherwise Tycho wouldn't be able to estimate token quality. However this isn't a blocking requirement, they we are working on removing that requirement.

You can find more details setting up a substreams endpoint here:
https://github.com/streamingfast/firehose-ethereum/blob/develop/SUPPORTED-NETWORKS.md#instrumented-nodes and here: https://firehose.streamingfast.io/

After setting it up, you can configure the Tycho Indexer to start consuming the block data from substreams endpoint, which can be set with the [CLI](https://github.com/propeller-heads/tycho-indexer/blob/b84c6de83ca3e74856d8cd204a5141bf0377ee77/tycho-indexer/src/cli.rs#L60)

## Additional Notes
If the chain uses Geth or OP-Geth for example, Worldchain which uses OP stack, there's already an instrumented Geth node available it shouldn't be too hard to run it https://docs.world.org/world-chain/developers/evm-equivalence.
 

You can learn more about Tychos architecture [here](https://github.com/propeller-heads/tycho-indexer/tree/main/tycho-indexer).


## FAQs
Q: Does it need to be an instrumented node or is Rpc-polled ok too?

A: Depends on the protocols you’d like to use, for popular dexes Uniswap v2 and v3, Rpc-polled should be alright

Q: For this we need firehose specifically, not substream right?
or is it both

A: Yes you need to set up both. More detailed help here since we mostly use the hosted endpoints but usually the Streamingfast Discord has pretty good and fast support from their team.







