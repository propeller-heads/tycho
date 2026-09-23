# Changelog

## v0.8.0

- Add the Robinhood Chain Uniswap V4 with-hooks manifest. Pons V2 MemeHook pools get static
  `hook_identifier`, `pons_hook_fee_bps` and `pons_creator_tax_bps` attributes decoded from the
  hook's storage writes in the creation transaction.
- Move the Ethereum and Unichain manifests to `v0.8.0` so all three manifests declare the version
  of the single binary they are built from.
