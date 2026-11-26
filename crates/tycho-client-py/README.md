# Tycho Python Client

A Python client library for interacting with the Tycho RPC server and streaming data from Tycho.

## Features

- RPC Client: Interact with the Tycho RPC server to query various blockchain data, including protocol components,
  states, contract states, and tokens.
- Streaming: Stream real-time data using the Tycho client binary, allowing for efficient monitoring and processing of
  live data.

## Installation

If you want to build and install the package locally, follow these steps (assumes you
have the Rust workspace already set up):

1. Build the package:
```bash
pip install maturin
python -m maturin build --release
```

2. Install the package:
```bash
pip install <path/to/your_package.whl>
```

## Usage

### RPC Client

The TychoRPCClient class allows you to interact with the Tycho RPC server. You can query for protocol components,
protocol states, contract states, and tokens.

#### Example

```python
from tycho_indexer_client import (
    TychoRPCClient,
    ProtocolComponentsParams,
    ProtocolStateParams,
    ContractStateParams,
    TokensParams,
    Chain
)

client = TychoRPCClient("http://0.0.0.0:4242", chain=Chain.ethereum)

# Query protocol components
protocol_components = client.get_protocol_components(
    ProtocolComponentsParams(protocol_system="test_protocol")
)

# Query protocol state
protocol_state = client.get_protocol_state(
    ProtocolStateParams(protocol_system="test_protocol")
)

# Query contract state
contract_state = client.get_contract_state(
    ContractStateParams(
        contract_ids=["contract_id_0"], protocol_system="test_protocol"
    )
)

# Query tokens
tokens = client.get_tokens(TokensParams(min_quality=10, traded_n_days_ago=30))

```

### Streaming

The TychoStream class allows you to start the Tycho client binary and stream data asynchronously.

#### Example

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
