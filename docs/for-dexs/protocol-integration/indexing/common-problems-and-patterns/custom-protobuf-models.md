# Custom protobuf models

In some cases, you may need to create custom intermediate protobuf messages, especially when facilitating communication between Substreams handler modules or storing additional data in stores.

Place these protobuf files within your Substreams package, such as `./substreams/ethereum-template/proto/custom-messages.proto`. Be sure to link them in the `substreams.yaml` file. For more details, refer to the substreams <a href="https://docs.substreams.dev/reference-material/substreams-components/manifests" target="_blank" rel="noopener noreferrer">manifest documentation</a> or review the official Substreams <a href="https://github.com/messari/substreams/blob/master/uniswap-v2/substreams.yaml#L20-L22" target="_blank" rel="noopener noreferrer">UniswapV2</a> example integration.
