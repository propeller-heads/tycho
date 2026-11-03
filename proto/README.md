# Protobuf
We use protobuf mainly to communicate with substreams. You can use the [language support extension](https://marketplace.visualstudio.com/items?itemName=pbkit.vscode-pbkit) and for formatting use the [buf extension for VS Code](https://marketplace.visualstudio.com/items?itemName=bufbuild.vscode-buf).

1. To communicate with substreams, we always want to use their latest protobuf images. Usually the sf module is already committed but in case there was an update we can pull the latest version into the repo with:
```bash
buf export buf.build/streamingfast/substreams -o ./proto
```
2. Now that we have all our protobuf modules together we can generate the respective Rust code for them:
```bash
buf generate 
```

## Protobuf Fomatting
1. To format the proto files:
```bash
buf format -w
```

## Protobuf Linting
1. `buf` is pinned in the repository's `mise.toml`; install it with:
```bash
mise install buf
```
2. Linting the files works like:
```bash
buf lint
```
It will return a list of errors that you need to fix. If not, then all the changes you made conform to the linter.