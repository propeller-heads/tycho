# Base LunarBase Substreams

Indexes LunarBase Prop AMM state on Base into Tycho-compatible `BlockChanges`.

The same package is intended to run on finalized blocks and Base Flashblocks. State changes emitted by the package are absolute values at transaction level so Tycho can apply partial-block updates and Substreams-level rollback messages consistently.
