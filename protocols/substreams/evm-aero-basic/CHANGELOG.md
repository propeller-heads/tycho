# Changelog

## v0.1.1

- Add the Elysium manifest (`elysium-aero-basic.yaml`), with the same factories and fee modules as
  the other chains, from the volatile factory's deployment block 1945551. The wasm is unchanged.

## v0.1.0

- Index AERO v1.1.0 basic pools (protocol system `aero_basic`, type `aero_basic_pool`) on Base,
  Arc, Ethereum, Arbitrum and Robinhood.
- Pools come from the separate volatile and stable factories; `is_stable` records which one.
- Reserves and balances come from `Sync`; `custom_fee` from the matching factory's
  `CustomFeeModule` `SetCustomFee`; `default_fee`, `fee_module` and `paused` from factory events,
  copied into each pool at creation and fanned out to all of the factory's pools on change.
