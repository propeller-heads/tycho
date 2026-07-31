# Tycho Substreams SDK

Some shared functionality that is used to create tycho substream packages.

## Protobuf Models

To generate the rust structs run the following command from within the root
directory:

```bash
buf generate --template substreams/crates/tycho-substreams/buf.gen.yaml --output substreams/crates/tycho-substreams/
```

## Generate block test assets

To be able to write complete unit tests, we rely on full block assets. These assets can be generated using the firecore tool from Substreams. More info in [Substreams documentation](https://docs.substreams.dev/reference-material/log-and-debug#generating-the-input-of-the-test)
