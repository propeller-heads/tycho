# Licensing

This file explains which license governs which parts of this repository. The license texts themselves control.

## Fynd License 1.1

Unless a file or directory listed below says otherwise, the material in this repository is licensed under the [Fynd License 1.1](LICENSE.md). This applies to:

- Tycho releases `0.435.0` and later, including every crate, binary, container image, and Python package built from them.
- `tycho-substreams` releases after `0.8.1` and `substreams-helper` releases after `0.0.2`.
- `tycho-indexer-client` (Python) releases after `0.158.0`.
- Any other copy of this repository, or of material from it, that is distributed with the Fynd License 1.1.

Solidity files that carry `SPDX-License-Identifier: LicenseRef-Fynd-License-1.1` are covered by this License. So are files written by PropellerHeads AG that carry `SPDX-License-Identifier: UNLICENSED` or no SPDX header.

For a custom license, contact legal@propellerheads.xyz.

## Earlier releases

Tycho releases before `0.435.0` were released under the MIT License, and they remain under that license. Router and executor contract source published before this change was released under the Business Source License 1.1 in [propeller-heads/tycho-execution](https://github.com/propeller-heads/tycho-execution), and it remains under that license.

Parts of this repository were contributed under the MIT License before `0.435.0`. As the MIT License requires, its notice is reproduced here and applies to those portions:

```
MIT License

Copyright (c) 2024 PropellerHeads

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Exceptions

The following material stays under its own license. The Fynd License 1.1 does not cover it (see Sections 1.2 and 10 of the License):

- **Files with a different SPDX header.** Any file whose `SPDX-License-Identifier` names a license other than `LicenseRef-Fynd-License-1.1` or `UNLICENSED` is governed by the license it names. This includes the `AGPL-3.0-or-later`, `GPL-3.0-or-later`, and `MIT` files under `protocols/adapter-integration/`, `crates/tycho-simulation/token-proxy-contracts/`, and `crates/tycho-ethereum/src/services/token_analyzer/contracts/`.
- **Git submodules and vendored libraries.** These include everything under `crates/tycho-execution/contracts/lib/` except the PropellerHeads files that carry the `LicenseRef-Fynd-License-1.1` header, plus `crates/tycho-simulation/token-proxy-contracts/lib/` and `protocols/adapter-integration/evm/lib/`.
- **Curve math.** `crates/tycho-simulation/src/evm/protocol/curve/math/` and `adapter/` are vendored under the MIT License. See [`LICENSE-curve-math`](crates/tycho-simulation/src/evm/protocol/curve/LICENSE-curve-math).
- **Attributed snippets.** Code that a source comment attributes to a third-party project, together with its license (for example the alloy-derived retry logic in `crates/tycho-ethereum/src/rpc/retry.rs`), stays under that license.
- **Third-party ABIs and bytecode.** ABI JSON files and compiled bytecode of third-party protocol contracts belong to their respective owners.

## Contributions

Before we can merge your first pull request, you need to sign the [PropellerHeads Contributor License Agreement](CLA.md). The CLA bot asks for this on the pull request.
