# Python Client

A python package is available to ease integration into python-based projects. To install locally:

## Setup Guide

#### Prerequisites

* Git
* Rust 1.84.0 or later
* Python 3.9 or above

#### Install the package

```bash
pip install git+https://github.com/propeller-heads/tycho-indexer.git#subdirectory=crates/tycho-client-py
```

## Understanding and using  the Python Client

The Python client is a Python wrapper around our [rust-client.md](rust-client.md "mention") that enables interaction with the Tycho Indexer. It provides two main functionalities:

* **Streaming Client**: Python wrapper around [rust-client.md](rust-client.md "mention") for real-time data streaming&#x20;
* **RPC Client**: Pure Python implementation for querying [tycho-rpc.md](../tycho-rpc.md "mention") data&#x20;

### Streaming Implementation

The `TychoStream` class:

1. Locates the Rust binary (`tycho-client-cli`)
2. Spawns the binary as a subprocess
3. Configures it with parameters like URL, authentication, exchanges, and filters
4. Implements an async iterator pattern that:
   * Reads JSON output from the binary's stdout
   * Parses messages into Pydantic models
   * Handles errors and process termination

Here's one example on how to use it:

```python
import asyncio
from tycho_indexer_client import Chain, TychoStream
from decimal import Decimal

async def main():
    stream = TychoStream(
        tycho_url="localhost:8888",
        auth_token="secret_token",
        exchanges=["uniswap_v2"],
        min_tvl=Decimal(100),
        blockchain=Chain.ethereum,
    )

    await stream.start()

    async for message in stream:
        print(message)

asyncio.run(main())
```

### RPC Client Implementation

The `TychoRPCClient` class:

* Makes HTTP requests to the Tycho RPC server
* Serializes Python objects to JSON
* Deserializes JSON responses to typed Pydantic models
* Handles blockchain-specific data types like `HexBytes`

Here's one example on how to use it to fetch tokens information (available at [#v1-tokens](../tycho-rpc.md#v1-tokens "mention") endpoint):

```python
from tycho_indexer_client import (
    TychoRPCClient,
    TokensParams,
    Chain,
    PaginationParams
)

client = TychoRPCClient("http://0.0.0.0:4242", chain=Chain.ethereum)

all_tokens = []
page = 0

while True:
    tokens = client.get_tokens(
        TokensParams(
            min_quality=51,
            traded_n_days_ago=30,
            pagination=PaginationParams(page=page, page_size=1000),
        )
    )
    
    if not tokens:
        break
    
    all_tokens.extend(tokens)
    page += 1
```



