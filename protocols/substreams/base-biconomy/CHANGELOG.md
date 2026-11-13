# Changelog

## v0.1.0

- Index the Biconomy PropAMM venue on Base as one component from storage changes only: the
  executor's board, anchor and pause storage for makers on the venue's list and tracked tokens,
  the venue's fee and maker list, each configured provider's signer and approved executor, the
  vaults' router, and the token balances and allowances behind each provider's inventory. No RPC
  calls, so the package runs on Flashblocks.
- Emit the providers' inventory per token as the component's balances.
