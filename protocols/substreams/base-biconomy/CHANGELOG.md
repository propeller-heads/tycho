# Changelog

## v0.1.0

- Index the Biconomy PropAMM venue on Base as one component from storage changes only: the
  executor's board, anchor and pause storage, the venue's fee and maker list, and the token
  balances and allowances behind each configured provider's inventory. No RPC calls, so the
  package runs on Flashblocks.
