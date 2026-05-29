# Base LunarBase Substreams

Indexes LunarBase Prop AMM state on Base into Tycho-compatible `BlockChanges`.

The same package is intended to run on finalized blocks and Base Flashblocks. State changes emitted by the package are absolute values at transaction level so Tycho can apply partial-block updates and Substreams-level rollback messages consistently.

## Pool configuration

The current Base deployment can keep using the singleton parameters:

```text
pool=0x...&token_x=0x...&token_y=0x...&bootstrap_block=45125288
```

For future LunarBase deployments with multiple known pools, use `pools`:

```text
pools=0xpool:0xtokenX:0xtokenY:bootstrapBlock,0xpool2:0xtokenX2:0xtokenY2:bootstrapBlock2
```

Each pool becomes its own Tycho component keyed by the pool address. The same Substreams package indexes full blocks and Flashblocks for all configured pools.
