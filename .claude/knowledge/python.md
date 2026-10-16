# Python Client (`tycho-client-py`)

Python client for the Tycho RPC server and streaming API. Lives in `tycho-client-py/`.

## Build System

**Maturin + PyO3 hybrid.** The package bundles the `tycho-client-cli` Rust binary (for streaming)
alongside pure Python modules (for RPC and DTOs).

```bash
# Build locally
pip install maturin
maturin build --release

# Install the resulting wheel
pip install target/wheels/tycho_indexer_client-*.whl
```

`build_wheel.sh` exists but uses `python -m build` (setuptools path). Prefer `maturin build` for
local development since the package declares `maturin` as its build backend in `pyproject.toml`.

## Dependencies — pinned legacy versions

| Package | Version | Notes |
|---------|---------|-------|
| pydantic | `1.8.*` | **v1 only.** Do not use v2 APIs (`model_validate`, `model_dump`, `ConfigDict`). Use `BaseModel.dict()`, `@root_validator`, `__get_validators__`, `class Config`. Import `Literal` from `pydantic.typing`: pydantic 1.8 only recognises `typing_extensions.Literal`, which on Python 3.9 is not `typing.Literal`. |
| eth-abi | `2.2.0` | Pinned exact |
| eth-typing | `2.3.0` | Pinned exact |
| eth-utils | `1.9.5` | Pinned exact |
| hexbytes | `0.3.1` | Pinned exact |
| requests | `2.*` | Synchronous HTTP only |

**Do not upgrade these without testing the full client.** The eth-* stack and pydantic v1 are
tightly coupled — upgrading one usually breaks the others.

## Module Structure

```
tycho-client-py/
  Cargo.toml                 # Builds tycho-client-cli binary (Rust)
  pyproject.toml             # Package metadata, maturin config
  python/
    tycho_indexer_client/
      __init__.py            # Public API re-exports
      dto.py                 # Pydantic v1 models (request params + response types)
      rpc_client.py          # TychoRPCClient — sync HTTP client using requests
      stream.py              # TychoStream — async wrapper around tycho-client-cli binary
      exception.py           # TychoClientException, TychoStreamException
    tests/
      conftest.py            # Fixtures (asset_dir)
      test_decode.py         # FeedMessage deserialization tests
      test_tycho_rpc_client.py  # RPC client tests (mocked requests)
      assets/                # JSON fixtures for tests
```

## Key Classes

### `TychoRPCClient` (`rpc_client.py`)
Synchronous HTTP client. All RPC methods follow the same pattern:
1. Convert pydantic params to dict with `params.dict(exclude_none=True)`
2. Inject `chain` from the client's default chain
3. POST to `/v1/{endpoint}` with JSON body
4. Parse response into a pydantic response model

Endpoints: `protocol_components`, `protocol_state`, `contract_state`, `tokens`,
`protocol_systems`, `component_tvl`, `traced_entry_points`, `delta_catch_up`, `health` (GET).

`get_delta_catch_up` mirrors the Rust client's three outcomes as three pydantic models nested in
`DeltaCatchUp` (their union is `DeltaCatchUpResult`), each with a `status`: `Available`
(`messages: List[BlockChanges]`), `Unavailable` (`reason: DeltaCatchUpUnavailable`, where a reason
the client does not know parses as `unknown`) and `Unsupported`, returned on a 404 (a server
without the endpoint). Other HTTP errors raise `TychoRPCError`, e.g. 400 with the plain-text
`Unknown extractor: <chain>:<name>`.

### `TychoStream` (`stream.py`)
Async wrapper that spawns the `tycho-client-cli` Rust binary as a subprocess. Reads JSON lines
from stdout, deserializes each into a `FeedMessage`. Implements `__aiter__`/`__anext__` for
`async for` usage.

### DTOs (`dto.py`)
Pydantic v1 models mirroring the Rust types in `tycho-common/src/dto.rs`. Two categories:
- **Request params**: `ProtocolComponentsParams`, `ProtocolStateParams`, `ContractStateParams`,
  `TokensParams`, `ProtocolSystemsParams`, `ComponentTvlParams`, `TracedEntryPointParams`,
  `DeltaCatchUpParams`
- **Response types**: `ProtocolComponentsResponse`, `ProtocolStateResponse`,
  `ContractStateResponse`, `TokensResponse`, `DeltaCatchUp`, etc.
- **Streaming types**: `FeedMessage`, `BlockChanges`, `StateSyncMessage`, `SynchronizerState`.
  `BlockChanges` mirrors the Rust `BlockAggregatedChanges` DTO field for field (`dci_update` and
  `partial_block_index` included); the catch-up endpoint returns its messages in the same model.

Custom `HexBytes` subclass adds pydantic v1 validators (`__get_validators__`, `__modify_schema__`).

## DTO Sync with Rust

Python DTOs must stay in sync with `tycho-common/src/dto.rs`. When adding/removing fields or
endpoints on the Rust side, update the corresponding Python models manually.

**Common drift points:**
- `Chain` enum — Rust has: `Ethereum`, `Starknet`, `ZkSync`, `Arbitrum`, `Base`, `Bsc`,
  `Unichain`. Python must match (check for missing variants).
- Response field additions — new fields on Rust DTOs need corresponding pydantic fields.
- New RPC endpoints — add a method to `TychoRPCClient` and matching param/response models.

## Versioning

Version is in lockstep with the main tycho workspace (`pyproject.toml` mirrors `workspace.version` in the root `Cargo.toml`).
Update it as part of the release process (usually automated via release CI).

## Testing

```bash
cd tycho-client-py
pip install -e ".[testing]"
pytest python/tests/
```

The tests need only the pure Python modules, so they also run without building the wheel: install
the pinned dependencies from `pyproject.toml` plus `pytest` in a virtualenv, then run
`PYTHONPATH=python pytest python/tests/`. No CI workflow runs them.

Tests mock `requests.post` — no running server needed. JSON fixtures in `tests/assets/` provide
response data.

## Common Tasks

### Adding a new RPC endpoint
1. Add Rust endpoint in `tycho-indexer/src/services/rpc.rs`
2. Add request/response DTOs in `tycho-common/src/dto.rs`
3. Add matching pydantic models in `dto.py` (request params + response)
4. Add method to `TychoRPCClient` following the existing pattern
5. Export new types from `__init__.py`
6. Add test with mocked response fixture

### Adding a new chain
1. Add variant to `Chain` enum in `dto.py`
2. Verify it matches the Rust `Chain` enum in `tycho-common/src/dto.rs`

### Updating streaming fields
When `BlockChanges` or `FeedMessage` fields change in the Rust client output:
1. Update the corresponding pydantic models in `dto.py`
2. Update test fixtures in `tests/assets/` to match new JSON shape
3. Verify `TychoStream._process_message` still deserializes correctly
