# Changelog

## v0.1.0

### Added

- fxSAVE deposit component (`0x7743e50f534a7f9f1791dde7dcd89f7783eefc39`): USDC or fxUSD into
  fxSP, then fxSP into fxSAVE. Its `swap_directions` static attribute lists USDC → fxSAVE and
  fxUSD → fxSAVE only.
- DCI entrypoints `fxSP.previewDeposit(USDC)`, `fxSP.previewRedeem`, `fxSP.stableDepegPrice`,
  `fxSAVE.previewDeposit` and `fxSAVE.getThreshold`, traced so DCI indexes the state the
  simulation reads.
- Component balances: fxSP's fxUSD (`totalYieldToken`) and USDC (`totalStableToken`) holdings,
  read by `eth_call` in the creation block and from each later transaction's storage changes.
