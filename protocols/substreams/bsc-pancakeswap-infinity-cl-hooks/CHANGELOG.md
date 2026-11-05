# Changelog

## v0.1.1

Follows `base-pancakeswap-infinity-cl` v0.1.2: the hook bitmap and address decoding come from the
`pancakeswap-infinity-substreams` crate. The output is unchanged; the wasm hash is not.

## v0.1.0

Initial package: PancakeSwap Infinity CL pools on BSC whose hook is a `CLAlphaHook`, as protocol
system `pancakeswap_infinity_cl_hooks`. Reuses the `base-pancakeswap-infinity-cl` modules, emits
the real hook address in `hooks` and the hook's per-pool start time as `alpha_start_timestamp`.
