"""Building blocks for step 6: diff a reference model against eth_call, stdlib only.

Import it from the reference model script. Python run with -I does not put the script's own
directory on sys.path, and ruff only lets imports follow plain `sys.path.insert` calls (no
assignment in between), so put these two statements, unchanged, right after the stdlib imports.
They find the repository by its `.claude/skills` directory, wherever the script lives, and
ruff's import sorting cannot break them:

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    sys.path.insert(
        0,
        str(
            next(p for p in Path(__file__).resolve().parents if (p / ".claude/skills").is_dir())
            / ".claude/skills/reverse-engineering-pamm-contracts/scripts"
        ),
    )
    from difftest import CallCase, Chain, keccak, mapping_slot, run_calls

    import model

- `Chain(block)` reads storage at one block, batched and cached, and lets a case override
  slots; the model reads through it so model and node see the same overridden state.
- `CallCase` is one eth_call with storage (`overrides`), code (`code`) and block (`number`,
  `time`) overrides and an optional `gas_price` (for `tx.gasprice` rules; the sender then gets
  a large ETH balance so the node can charge for gas).
- `run_calls(chain, cases)` sends them in batches and returns, per case, either the return
  words as a tuple of ints or `"REVERT <reason>"`, with `Error(string)`, `Panic(uint256)` (as
  `PANIC_0x11`) and custom errors decoded to their selector. With `raw=True` a revert is
  `"REVERT 0x<all revert data>"` instead, which keeps a custom error's arguments (contracts
  that return a quote inside a revert, such as `QuoteResult(int256,int256)`). Compare the
  model's output in the same shape and count mismatches.
- `storage_writes(chain, cases, address)` runs the cases with debug_traceCall's
  prestateTracer in diffMode and returns, per case, `{slot: new value}` for every slot of
  `address` the call changed; a slot cleared to 0 is in the result with value 0 (diffMode
  leaves it out of `post`), and a write of the value already there does not show. Diff it
  against the model's writes minus those that leave a word unchanged.
- `coverage_cases(chain, cases)` returns the cases as `coverage.py trace` input (json.dump it),
  to measure which branches of the contract they executed.
- `find_balance_slot(chain, token, holder)` finds the `balanceOf` mapping slot of a plain ERC20,
  so cases can override vault balances.
- `funding_overrides(chain, token, owner, spender, amount)` gives `owner` a balance and an
  allowance to `spender` (both slots found by probing, so a zero balance works), to run a
  router's swap that pulls the input with `transferFrom`.

ETH_RPC_URL selects the node. eth_call takes block overrides as its 4th parameter; nodes that
reject it (some providers) cannot run block-number or timestamp variants.
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field

BATCH = 50

# --- keccak256 (the pre-NIST padding Ethereum uses; hashlib.sha3_256 is different) -----------

_RC = [
    0x0000000000000001,
    0x0000000000008082,
    0x800000000000808A,
    0x8000000080008000,
    0x000000000000808B,
    0x0000000080000001,
    0x8000000080008081,
    0x8000000000008009,
    0x000000000000008A,
    0x0000000000000088,
    0x0000000080008009,
    0x000000008000000A,
    0x000000008000808B,
    0x800000000000008B,
    0x8000000000008089,
    0x8000000000008003,
    0x8000000000008002,
    0x8000000000000080,
    0x000000000000800A,
    0x800000008000000A,
    0x8000000080008081,
    0x8000000000008080,
    0x0000000080000001,
    0x8000000080008008,
]
_ROT = [
    [0, 36, 3, 41, 18],
    [1, 44, 10, 45, 2],
    [62, 6, 43, 15, 61],
    [28, 55, 25, 21, 56],
    [27, 20, 39, 8, 14],
]
_M64 = (1 << 64) - 1


def _rol(x, n):
    return ((x << n) | (x >> (64 - n))) & _M64 if n else x


def _keccak_f(a):
    for rc in _RC:
        c = [a[x][0] ^ a[x][1] ^ a[x][2] ^ a[x][3] ^ a[x][4] for x in range(5)]
        d = [c[(x - 1) % 5] ^ _rol(c[(x + 1) % 5], 1) for x in range(5)]
        a = [[a[x][y] ^ d[x] for y in range(5)] for x in range(5)]
        b = [[0] * 5 for _ in range(5)]
        for x in range(5):
            for y in range(5):
                b[y][(2 * x + 3 * y) % 5] = _rol(a[x][y], _ROT[x][y])
        a = [
            [b[x][y] ^ (~b[(x + 1) % 5][y] & b[(x + 2) % 5][y]) for y in range(5)] for x in range(5)
        ]
        a[0][0] ^= rc
    return a


def keccak(data: bytes) -> bytes:
    """keccak256 of `data`."""
    rate = 136
    pad = rate - len(data) % rate
    tail = b"\x81" if pad == 1 else b"\x01" + b"\x00" * (pad - 2) + b"\x80"
    padded = bytes(data) + tail
    state = [[0] * 5 for _ in range(5)]
    for start in range(0, len(padded), rate):
        block = padded[start : start + rate]
        for i in range(rate // 8):
            x, y = i % 5, i // 5
            state[x][y] ^= int.from_bytes(block[8 * i : 8 * i + 8], "little")
        state = _keccak_f(state)
    return b"".join(state[i % 5][i // 5].to_bytes(8, "little") for i in range(4))


def word(value: int) -> bytes:
    return (value % (1 << 256)).to_bytes(32, "big")


def mapping_slot(key: int, slot: int) -> int:
    """Storage slot of `mapping(slot)[key]` for value-type keys (addresses, uints, bytes32)."""
    return int.from_bytes(keccak(word(key) + word(slot)), "big")


def array_data_slot(slot: int) -> int:
    """First element slot of a dynamic array whose length is stored at `slot`."""
    return int.from_bytes(keccak(word(slot)), "big")


# --- JSON-RPC ---------------------------------------------------------------------------------


def rpc_url():
    url = os.environ.get("ETH_RPC_URL") or os.environ.get("MAINNET_RPC_URL")
    if not url:
        sys.exit("set ETH_RPC_URL to the chain under test (foundry convention)")
    return url


def rpc_batch(calls):
    """Send [(method, params)] in batches with retries; returns results or {'error': ...}."""
    out = []
    for start in range(0, len(calls), BATCH):
        part = calls[start : start + BATCH]
        body = json.dumps(
            [{"jsonrpc": "2.0", "id": i, "method": m, "params": p} for i, (m, p) in enumerate(part)]
        ).encode()
        req = urllib.request.Request(rpc_url(), body, {"Content-Type": "application/json"})
        for attempt in range(8):
            try:
                res = json.load(urllib.request.urlopen(req, timeout=300))
                break
            except urllib.error.HTTPError as err:
                if err.code not in (429, 503) or attempt == 7:
                    raise
                time.sleep(0.5 * 2**attempt)
        for r in sorted(res, key=lambda r: r["id"]):
            out.append({"error": r["error"]} if "error" in r else r["result"])
    return out


# --- state at one block ------------------------------------------------------------------------


class Chain:
    """Storage of any contract at `block`, fetched in batches and cached."""

    def __init__(self, block: int):
        self.block = block
        self.cache = {}

    def prefetch(self, pairs):
        need = [(a, s) for a, s in pairs if (a, s) not in self.cache]
        results = rpc_batch(
            [("eth_getStorageAt", [f"0x{a:040x}", hex(s), hex(self.block)]) for a, s in need]
        )
        for (a, s), value in zip(need, results):
            if isinstance(value, dict):
                raise RuntimeError(  # noqa: TRY004 - a JSON-RPC error object, not a type mistake
                    f"eth_getStorageAt {a:#x} {s:#x}: {value['error']}"
                )
            self.cache[(a, s)] = int(value, 16)

    def sload(self, address: int, slot: int, overrides=None) -> int:
        """Word at `slot`, or the case's override for it when it has one."""
        if overrides and (address, slot) in overrides:
            return overrides[(address, slot)] % (1 << 256)
        if (address, slot) not in self.cache:
            self.prefetch([(address, slot)])
        return self.cache[(address, slot)]

    def call(self, to: int, data: str) -> str:
        (result,) = rpc_batch(
            [("eth_call", [{"to": f"0x{to:040x}", "data": data}, hex(self.block)])]
        )
        if isinstance(result, dict):
            raise RuntimeError(  # noqa: TRY004 - a JSON-RPC error object, not a type mistake
                f"eth_call {to:#x}: {result['error']}"
            )
        return result


def find_balance_slot(chain: Chain, token: int, holder: int, max_slot: int = 64) -> int:
    """Mapping slot `s` with `storage[keccak(holder . s)] == balanceOf(holder)`, for overrides.

    Fails when the balance is zero (ambiguous) or the token keeps balances elsewhere (proxies
    with namespaced storage, rebasing tokens): find those with storage_access.py instead.
    """
    balance = int(chain.call(token, "0x70a08231" + word(holder).hex()), 16)
    if balance == 0:
        raise ValueError(f"{token:#x}: holder has no balance, the slot cannot be identified")
    slots = [mapping_slot(holder, s) for s in range(max_slot)]
    chain.prefetch([(token, s) for s in slots])
    for index, slot in enumerate(slots):
        # some tokens pack flags into the top bit next to the balance
        if chain.sload(token, slot) & ((1 << 255) - 1) == balance:
            return index
    raise ValueError(f"{token:#x}: no balanceOf mapping in slots 0..{max_slot - 1}")


def _probe_slot(chain: Chain, token: int, data: str, candidates: dict) -> int:
    """Index whose slot, set to a sentinel by state override, makes `data` return the sentinel."""
    sentinel = 0x1234567890ABCDEF1234
    cases = [CallCase(token, data, overrides={(token, s): sentinel}) for s in candidates.values()]
    for index, result in zip(candidates, run_calls(chain, cases)):
        if isinstance(result, tuple) and result and result[0] == sentinel:
            return index
    raise ValueError(f"{token:#x}: no candidate slot answers {data[:10]}")


def find_allowance_slot(chain: Chain, token: int, owner: int, spender: int, max_slot=64) -> int:
    """Mapping slot `s` with `allowance(owner, spender)` at `keccak(spender . keccak(owner . s))`.

    Found by overriding each candidate, so it works when the allowance is 0. Tokens that keep
    allowances elsewhere (namespaced proxies, Permit2-only flows) fail: use storage_access.py.
    """
    candidates = {s: mapping_slot(spender, mapping_slot(owner, s)) for s in range(max_slot)}
    return _probe_slot(chain, token, calldata("0xdd62ed3e", owner, spender), candidates)


def funding_overrides(chain: Chain, token: int, owner: int, spender: int, amount: int) -> dict:
    """Overrides that give `owner` a balance of `amount` and approve `spender` for it."""
    balance = {s: mapping_slot(owner, s) for s in range(64)}
    balance_slot = _probe_slot(chain, token, calldata("0x70a08231", owner), balance)
    allowance_slot = find_allowance_slot(chain, token, owner, spender)
    return {
        (token, mapping_slot(owner, balance_slot)): amount,
        (token, mapping_slot(spender, mapping_slot(owner, allowance_slot))): amount,
    }


# --- eth_call cases ----------------------------------------------------------------------------


@dataclass
class CallCase:
    """One eth_call. `overrides` maps (address, slot) to a value, `code` maps an address to
    runtime bytecode hex; `number`/`time` override the block the call executes in, `gas_price`
    sets tx.gasprice."""

    to: int
    data: str
    sender: int = 1
    overrides: dict = field(default_factory=dict)
    number: int | None = None
    time: int | None = None
    code: dict = field(default_factory=dict)
    gas_price: int | None = None


SENDER_BALANCE_FOR_GAS = 1 << 200


def _state_overrides(case):
    """The eth_call state override object of `case`: storage, code and, with a gas price, the
    sender's balance."""
    state = {}
    for (address, slot), value in case.overrides.items():
        account = state.setdefault(f"0x{address:040x}", {})
        diff = account.setdefault("stateDiff", {})
        diff[f"0x{slot:064x}"] = f"0x{value % (1 << 256):064x}"
    for address, code in case.code.items():
        state.setdefault(f"0x{address:040x}", {})["code"] = "0x" + code.removeprefix("0x")
    if case.gas_price is not None:
        state.setdefault(f"0x{case.sender:040x}", {})["balance"] = hex(SENDER_BALANCE_FOR_GAS)
    return state


def _call_object(case):
    call = {"from": f"0x{case.sender:040x}", "to": f"0x{case.to:040x}", "data": case.data}
    if case.gas_price is not None:
        call["gasPrice"] = hex(case.gas_price)
    return call


def _block_overrides(case):
    block = {}
    if case.number is not None:
        block["number"] = hex(case.number)
    if case.time is not None:
        block["time"] = hex(case.time)
    return block


def _revert_data(error):
    data = error.get("data") or ""
    if isinstance(data, dict):
        data = data.get("data", "")
    return data if isinstance(data, str) else ""


def decode_revert(error, raw=False) -> str:
    data = _revert_data(error)
    if raw and data:
        return "REVERT " + data
    if data.startswith("0x08c379a0"):
        body = bytes.fromhex(data[10:])
        length = int.from_bytes(body[32:64], "big")
        return "REVERT " + body[64 : 64 + length].decode(errors="replace")
    if data.startswith("0x4e487b71"):
        return f"REVERT PANIC_{int(data[10:], 16):#x}"
    if len(data) >= 10:
        return "REVERT " + data[:10]
    return "REVERT " + (data or error.get("message", ""))


def run_calls(chain: Chain, cases, raw=False):
    """Results of `cases` at `chain.block`: a tuple of return words, or 'REVERT <reason>'
    ('REVERT 0x<all revert data>' with `raw`)."""
    requests = []
    for case in cases:
        params = [_call_object(case), hex(chain.block), _state_overrides(case)]
        if _block_overrides(case):
            params.append(_block_overrides(case))
        requests.append(("eth_call", params))
    results = []
    for reply in rpc_batch(requests):
        if isinstance(reply, dict):
            results.append(decode_revert(reply["error"], raw))
        else:
            body = reply[2:]
            results.append(tuple(int(body[i : i + 64], 16) for i in range(0, len(body), 64)))
    return results


def _changed_slots(diff, address):
    """{slot: new value} of `address` from one prestateTracer diffMode result. A slot set to 0
    is only in `pre`; a slot first written by the call is only in `post`."""
    key = f"0x{address:040x}"
    before = {k.lower(): v for k, v in diff["pre"].items()}.get(key, {}).get("storage", {})
    after = {k.lower(): v for k, v in diff["post"].items()}.get(key, {}).get("storage", {})
    changed = {int(slot, 16): 0 for slot in before}
    changed.update({int(slot, 16): int(value, 16) for slot, value in after.items()})
    return changed


def storage_writes(chain: Chain, cases, address: int):
    """Per case, {slot: new value} for every storage slot of `address` the call changed, from
    debug_traceCall with prestateTracer in diffMode. A call that reverts changes nothing, so
    its result is {}; run_calls says whether it reverted."""
    requests = []
    for case in cases:
        config = {"tracer": "prestateTracer", "tracerConfig": {"diffMode": True}}
        if _state_overrides(case):
            config["stateOverrides"] = _state_overrides(case)
        if _block_overrides(case):
            config["blockOverrides"] = _block_overrides(case)
        requests.append(("debug_traceCall", [_call_object(case), hex(chain.block), config]))
    results = []
    for case, reply in zip(cases, rpc_batch(requests)):
        if "pre" not in reply:
            raise RuntimeError(
                f"debug_traceCall to {case.to:#x} data {case.data[:10]}: {reply.get('error')}"
            )
        results.append(_changed_slots(reply, address))
    return results


def coverage_cases(chain: Chain, cases):
    """`cases` as the CASES.json entries of `coverage.py trace`, to measure their branches."""
    entries = []
    for case in cases:
        state = _state_overrides(case)
        entry = {
            "to": f"0x{case.to:040x}",
            "from": f"0x{case.sender:040x}",
            "data": case.data,
            "block": hex(chain.block),
            "number": case.number,
            "time": case.time,
            "overrides": {a: d["stateDiff"] for a, d in state.items() if "stateDiff" in d},
        }
        if case.code:
            entry["code"] = {a: d["code"] for a, d in state.items() if "code" in d}
        if case.gas_price is not None:
            entry["gas_price"] = hex(case.gas_price)
        entries.append(entry)
    return entries


def calldata(selector: str, *args: int) -> str:
    """ABI-encode static arguments (addresses, uints, ints; negatives wrap) after `selector`."""
    return selector + "".join(word(a).hex() for a in args)
