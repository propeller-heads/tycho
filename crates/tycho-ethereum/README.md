# Tycho Ethereum

The Tycho Ethereum module integrates Ethereum-specific blockchain functionalities into Tycho.
This module provides key tools to interact with Ethereum accounts, analyze tokens, and extract critical blockchain data using RPC calls.

### Account Extractor

The Account Extractor is a component that allows Tycho to retrieve detailed information about Ethereum accounts through RPC calls.
By providing an account address, it will gather all crucial data, including the account’s full storage, any associated code, and its ETH balance.

### Token Analyzer

The Token Analyzer assesses the behavior and quality of an ERC20 token. It evaluates whether the token follows standard ERC20 implementations or has unusual behavior. Additionally, it provides insights into:

- Gas usage per token transfer
- Potential transfer taxes

The analysis is done by simulating transfers for the token using `eth_call` with bytecode state overrides.

### Token Pre-Processor

The Token Pre-Processor gathers essential token information, such as the number of decimals and the token symbol, by querying the blockchain's RPC. It also runs the Token Analyzer to ensure a comprehensive analysis. Simply provide a token’s contract address to retrieve this data.

## Dependencies

To run Tycho on Ethereum, you will need an RPC connection that supports all the following Ethereum endpoints:

- `eth_call` (with [state override](https://www.quicknode.com/docs/ethereum/eth_call) support, used for token analysis), `eth_getCode` and `eth_getBalance`
- [debug_storageRangeAt](https://www.quicknode.com/docs/ethereum/debug_storageRangeAt) to retrieve full accounts storage
