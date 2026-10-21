# Instructions

The run command for our substream is a little different here due to the inclusion of the dynamic parameters for manually
admitted pools.

This command will add extra parameters to the `map_components` module via the `python params.py` script. This embeds
directly in the bash/zsh compatible command here. If `python` is not ideal, the script can be easily converted into
`bash` but it would require the `jq` executable (I've used AI to convert it just fine in testing).

```bash
$ substreams run -e mainnet.eth.streamingfast.io:443 substreams.yaml map_protocol_changes --start-block 11507454 --stop-block +100 -p map_components=`python params.py`
```

## `params.json`

This json file is a top-level array containing objects that describe a specific `ProtocolComponent`. Each object
contains the following fields:

- `name`: Just for documentation purposes
- `address`: The **lowercase** address of the component
- `tx_hash`: The hash of the transaction where the component was emitted
- `tokens`: A list of token addresses ordered in the exact same way as the Pool
- `static_attributes`: A nested object of key to value that represents the static attributes of the component.
- `attributes`: A nested object of key to value that represents attributes.

Please see the included 3 examples for `3pool`, `steth`, and `tricrypto2`.

## Open tasks

### Add underlying tokens in metapools

Currently, metapools are not working properly due to the way we override token balances.
The issue arises because when we modify token balances, we end up changing the token contract code and storage.
This issue will be resolved once we implement a flexible method to adjust token balances without affecting the contract’s functionality.
We will also need to index additional contract such as the base pool lp token.

### Handle rebasing, ERC4644 and others special kind of tokens

At the moment, we are unable to manage certain types of tokens, such as rebasing tokens or ERC4644 tokens, because they have unique behavior or specific logic that complicates simulations.
To handle these tokens properly, we will likely need to use the dynamic contract indexer (DCI), which can track and index the full state of the token contract, allowing us to deal with their complexities effectively.

## Static Attributes

| name         | type  | description                                                                                                 |
| ------------ | ----- | ----------------------------------------------------------------------------------------------------------- |
| pool_type    | str   | A unique identifier per pool type. Set depending on the factory.                                            |
| name         | str   | A string representing the name of the pool, set if there is one.                                            |
| factory_name | str   | A string representing the name of the factory that created the pool. "na" if the pool was manually created. |
| factory      | bytes | The address of the factory that created the pool. "0x000..." if the pool was manually created.              |
| lp_token     | bytes | The pool lp token, set if the lp token is not the pool itself                                               |
| base_pool    | bytes | The base pool related to this pool, set only for metapools.                                                 |
