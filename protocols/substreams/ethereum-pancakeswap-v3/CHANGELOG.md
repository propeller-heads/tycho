# Changelog

## v0.1.3

- `map_pools_created` takes a query-string parameter:
  `factory_address=<hex>&protocol_type_name=<name>`. The protocol type name used to be hardcoded
  to `pancakeswap_v3_pool`, so a fork indexed by this package can now emit its own. The
  Ethereum, Base, BSC and Arbitrum manifests pass `protocol_type_name=pancakeswap_v3_pool`, so
  their components are unchanged.
- `map_protocol_changes` takes a query-string parameter. `default_protocol_fee=<n>` sets the
  `protocol_fees/zero2one` and `protocol_fees/one2zero` attributes written on `Initialize` to `n`
  for every fee tier. Without it the module keeps the PancakeSwap V3 defaults, which depend on the
  fee tier (100, 500, 2500 and 10000 only). A pool with any other fee used to panic the module with
  `Unexpected fee value`. It still panics, but the message now names the fee and the parameter. The
  existing manifests pass no value, so their output is unchanged.
- Add the Robinhood Chain gigaDex V3 manifest, `robinhood-gigadex-v3.yaml` (CLFactory
  `0xece6ecd61177336ea6fb9b17937ac439d85ee20b`, first pool at block `10439675`). gigaDex V3 is
  PancakeSwap V3 with the contracts renamed (`CLFactory`, `CLPoolDeployer`, `CLPool`). The factory
  and pool event layouts are identical, including `Swap` with `protocolFeesToken0/1` and
  `SetFeeProtocol` with `uint32` fields. Pools are deployed by `CLPoolDeployer`
  (`0x5952f5d501a130da00fa8fe2257d8b35ddc0a57b`), which emits no `PoolCreated`, so the manifest
  indexes the factory. `CLPool.initialize` sets a 10% protocol fee (`1000`) whatever the fee tier,
  so the manifest passes `default_protocol_fee=1000`. Components are emitted as `gigadex_v3_pool`.
  The protocol fee is taken out of the LP fee. It does not change the trader's amount out or the
  event-derived balances. The factory enables `(fee, tickSpacing)` pairs that canonical Uniswap V3
  does not, such as fee `50` with spacing `10` and fee `200` with spacing `4`. Both values are
  emitted as the `fee` and `tick_spacing` static attributes, read from `PoolCreated`.
