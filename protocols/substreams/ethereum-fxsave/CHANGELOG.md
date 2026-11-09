# Changelog

## v0.1.0

### Added

- fxSAVE deposit component (`0x7743e50f534a7f9f1791dde7dcd89f7783eefc39`): USDC or fxUSD into
  fxSP, then fxSP into fxSAVE. Its `swap_directions` static attribute lists USDC → fxSAVE and
  fxUSD → fxSAVE only.
- DCI entrypoints for the fxSP and fxSAVE previews the simulation reads.
- Component balances: fxSP's fxUSD (`totalYieldToken`) and USDC (`totalStableToken`) holdings.
