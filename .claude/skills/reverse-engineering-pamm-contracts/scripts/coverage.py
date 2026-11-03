"""Branch coverage of a contract's bytecode: which JUMPI edges a set of calls executed.

usage:
  coverage.py classify CODE [--list]
  coverage.py trace CODE_ADDR[,CODE_ADDR...] OUT (--cases CASES.json | --txs TXS.txt)
                    [--workers N]
  coverage.py report CODE SEEN.json [SEEN.json ...] [--function SEL [SEL ...]]
                     [--tac RESULTS_DIR] [--trace T.json --args a,b,c]

CODE is a runtime-bytecode hex file or an address (eth_getCode at latest). Run `classify` and
`report` with `uv run -q --with evmole==0.9.4 python -I`; `trace` is stdlib.

classify: every JUMPI, by what its two edges lead to. An edge is doomed when every path from it
ends in REVERT/INVALID; the reason (Panic code, Error string, custom selector, bare, a bubbled
callee revert) comes from replaying the path with constants and memory. A JUMPI with no doomed
edge is a fork, one doomed edge makes it a guard. Dispatcher compares and ABI plumbing are
marked so they are not counted as behaviors: CALLVALUE, CALLDATASIZE and RETURNDATASIZE checks,
the ABI decoder's bounds checks, `x == cleanup(x)` validators (and via-IR's `x - cleanup(x)`),
guards that only bubble a callee's revert, the second `if success` solc emits after
`if iszero(success) { revert }`, and the data-less `extcodesize(target) == 0` check before a
call. Conditions are read across the blocks a JUMPI falls through from, so a value checked in
an earlier block is recognised. Each JUMPI is assigned to the external functions (evmole entry
points) that reach it, following return addresses on the stack.

trace: runs every case through debug_traceCall (or every tx through debug_traceTransaction) with
a small JS tracer that keeps only (code address, entry selector, JUMPI pc, taken) for frames
executing the code at one of the CODE_ADDRs, including calls that reach it through a router or
a proxy's DELEGATECALL (pass the implementation). One pass covers several contracts: with one
CODE_ADDR, OUT is the SEEN file; with several, OUT is a directory that gets one
OUT/<address>.json per address. CASES.json is a list of {"to", "data", "from"?, "block",
"overrides"?: {addr: {slot: value}}, "code"?: {addr: hex}, "gas_price"?, "number"?, "time"?},
what difftest.coverage_cases writes. With "gas_price" the sender gets a large balance. Needs a
node that accepts custom JS tracers.

report: per function, the fork and non-panic guard edges no call took, with the other functions
whose calls took them; panic guards are only counted; then every unseen edge grouped by its
revert reason (or "fork edge"), so one explanation can cover a group. --function takes one or
more selectors, with or without 0x; an unknown one is an error. With --tac (a gigahorse_run.py
results dir) each unseen JUMPI shows gigahorse's condition for it. With --trace (a struct-log
trace whose depth-1 frame runs CODE) the condition of each unseen JUMPI that trace executed is
printed from symreplay. Exits 1 when an edge is unseen.
"""

import argparse
import contextlib
import io
import json
import os
import sys
import time
import urllib.error
import urllib.request
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

M = (1 << 256) - 1
FAKE_FREE_POINTER = 0x80
PANIC, ERROR_STRING = 0x4E487B71, 0x08C379A0
DOOM_BUDGET = 4000
REACH_BUDGET = 400_000
WIDEN_AFTER = 16

# name, pops, pushes; PUSH/DUP/SWAP/LOG are generated below.
OPCODES = {
    0x00: ("STOP", 0, 0), 0x01: ("ADD", 2, 1), 0x02: ("MUL", 2, 1), 0x03: ("SUB", 2, 1),
    0x04: ("DIV", 2, 1), 0x05: ("SDIV", 2, 1), 0x06: ("MOD", 2, 1), 0x07: ("SMOD", 2, 1),
    0x08: ("ADDMOD", 3, 1), 0x09: ("MULMOD", 3, 1), 0x0A: ("EXP", 2, 1),
    0x0B: ("SIGNEXTEND", 2, 1), 0x10: ("LT", 2, 1), 0x11: ("GT", 2, 1), 0x12: ("SLT", 2, 1),
    0x13: ("SGT", 2, 1), 0x14: ("EQ", 2, 1), 0x15: ("ISZERO", 1, 1), 0x16: ("AND", 2, 1),
    0x17: ("OR", 2, 1), 0x18: ("XOR", 2, 1), 0x19: ("NOT", 1, 1), 0x1A: ("BYTE", 2, 1),
    0x1B: ("SHL", 2, 1), 0x1C: ("SHR", 2, 1), 0x1D: ("SAR", 2, 1), 0x20: ("KECCAK256", 2, 1),
    0x30: ("ADDRESS", 0, 1), 0x31: ("BALANCE", 1, 1), 0x32: ("ORIGIN", 0, 1),
    0x33: ("CALLER", 0, 1), 0x34: ("CALLVALUE", 0, 1), 0x35: ("CALLDATALOAD", 1, 1),
    0x36: ("CALLDATASIZE", 0, 1), 0x37: ("CALLDATACOPY", 3, 0), 0x38: ("CODESIZE", 0, 1),
    0x39: ("CODECOPY", 3, 0), 0x3A: ("GASPRICE", 0, 1), 0x3B: ("EXTCODESIZE", 1, 1),
    0x3C: ("EXTCODECOPY", 4, 0), 0x3D: ("RETURNDATASIZE", 0, 1),
    0x3E: ("RETURNDATACOPY", 3, 0), 0x3F: ("EXTCODEHASH", 1, 1), 0x40: ("BLOCKHASH", 1, 1),
    0x41: ("COINBASE", 0, 1), 0x42: ("TIMESTAMP", 0, 1), 0x43: ("NUMBER", 0, 1),
    0x44: ("PREVRANDAO", 0, 1), 0x45: ("GASLIMIT", 0, 1), 0x46: ("CHAINID", 0, 1),
    0x47: ("SELFBALANCE", 0, 1), 0x48: ("BASEFEE", 0, 1), 0x49: ("BLOBHASH", 1, 1),
    0x4A: ("BLOBBASEFEE", 0, 1), 0x50: ("POP", 1, 0), 0x51: ("MLOAD", 1, 1),
    0x52: ("MSTORE", 2, 0), 0x53: ("MSTORE8", 2, 0), 0x54: ("SLOAD", 1, 1),
    0x55: ("SSTORE", 2, 0), 0x56: ("JUMP", 1, 0), 0x57: ("JUMPI", 2, 0), 0x58: ("PC", 0, 1),
    0x59: ("MSIZE", 0, 1), 0x5A: ("GAS", 0, 1), 0x5B: ("JUMPDEST", 0, 0),
    0x5C: ("TLOAD", 1, 1), 0x5D: ("TSTORE", 2, 0), 0x5E: ("MCOPY", 3, 0),
    0x5F: ("PUSH0", 0, 1), 0xF0: ("CREATE", 3, 1), 0xF1: ("CALL", 7, 1),
    0xF2: ("CALLCODE", 7, 1), 0xF3: ("RETURN", 2, 0), 0xF4: ("DELEGATECALL", 6, 1),
    0xF5: ("CREATE2", 4, 1), 0xFA: ("STATICCALL", 6, 1), 0xFD: ("REVERT", 2, 0),
    0xFE: ("INVALID", 0, 0), 0xFF: ("SELFDESTRUCT", 1, 0),
}  # fmt: skip
for _n in range(32):
    OPCODES[0x60 + _n] = (f"PUSH{_n + 1}", 0, 1)
for _n in range(16):
    OPCODES[0x80 + _n] = (f"DUP{_n + 1}", _n + 1, _n + 2)
    OPCODES[0x90 + _n] = (f"SWAP{_n + 1}", _n + 2, _n + 2)
for _n in range(5):
    OPCODES[0xA0 + _n] = (f"LOG{_n}", _n + 2, 0)
SUCCESS_ENDS = {"STOP", "RETURN", "SELFDESTRUCT"}


def _signed(x):
    return x - (1 << 256) if x >> 255 else x


def _signextend(b, x):
    if b >= 31:
        return x
    bit = 8 * b + 7
    low = x & ((1 << (bit + 1)) - 1)
    return (low | (M ^ ((1 << (bit + 1)) - 1))) if (x >> bit) & 1 else low


def _sdiv(a, b):
    if b == 0:
        return 0
    q = abs(_signed(a)) // abs(_signed(b))
    return (-q if (_signed(a) < 0) != (_signed(b) < 0) else q) & M


def _smod(a, b):
    if b == 0:
        return 0
    r = abs(_signed(a)) % abs(_signed(b))
    return (-r if _signed(a) < 0 else r) & M


# Arguments in pop order: the first is the top of the stack.
ARITH = {
    "ADD": lambda a, b: (a + b) & M,
    "MUL": lambda a, b: (a * b) & M,
    "SUB": lambda a, b: (a - b) & M,
    "DIV": lambda a, b: a // b if b else 0,
    "SDIV": _sdiv,
    "MOD": lambda a, b: a % b if b else 0,
    "SMOD": _smod,
    "ADDMOD": lambda a, b, n: (a + b) % n if n else 0,
    "MULMOD": lambda a, b, n: (a * b) % n if n else 0,
    "EXP": lambda a, b: pow(a, b, 1 << 256),
    "SIGNEXTEND": _signextend,
    "LT": lambda a, b: int(a < b),
    "GT": lambda a, b: int(a > b),
    "SLT": lambda a, b: int(_signed(a) < _signed(b)),
    "SGT": lambda a, b: int(_signed(a) > _signed(b)),
    "EQ": lambda a, b: int(a == b),
    "ISZERO": lambda a: int(a == 0),
    "AND": lambda a, b: a & b,
    "OR": lambda a, b: a | b,
    "XOR": lambda a, b: a ^ b,
    "NOT": lambda a: a ^ M,
    "BYTE": lambda i, x: (x >> (8 * (31 - i))) & 0xFF if i < 32 else 0,
    "SHL": lambda s, x: (x << s) & M if s < 256 else 0,
    "SHR": lambda s, x: x >> s if s < 256 else 0,
    "SAR": lambda s, x: (_signed(x) >> min(s, 255)) & M,
}


# --- bytecode ----------------------------------------------------------------------------------


@dataclass
class Code:
    raw: bytes
    ops: dict = field(default_factory=dict)  # pc -> (opcode name, immediate or None)
    following: dict = field(default_factory=dict)  # pc -> pc of the next instruction
    preceding: dict = field(default_factory=dict)  # pc -> pc of the previous instruction
    jumpdests: frozenset = frozenset()

    @classmethod
    def from_bytes(cls, raw):
        code = cls(raw)
        pc, previous = 0, None
        while pc < len(raw):
            opcode = raw[pc]
            width = opcode - 0x5F if 0x60 <= opcode <= 0x7F else 0
            name = OPCODES.get(opcode, ("INVALID", 0, 0))[0]
            immediate = int.from_bytes(raw[pc + 1 : pc + 1 + width], "big") if width else None
            code.ops[pc] = (name, immediate)
            if previous is not None:
                code.following[previous] = pc
                code.preceding[pc] = previous
            previous, pc = pc, pc + 1 + width
        code.jumpdests = frozenset(p for p, (name, _) in code.ops.items() if name == "JUMPDEST")
        return code

    def jumpis(self):
        return sorted(pc for pc, (name, _) in self.ops.items() if name == "JUMPI")

    def jumpi_target(self, pc):
        """Static destination of a `PUSH dest; JUMPI`, or None for a computed one."""
        previous = self.preceding.get(pc)
        return self.ops[previous][1] if previous is not None else None


def rpc_url():
    for name in ("ETH_RPC_URL", "MAINNET_RPC_URL"):
        if os.environ.get(name):
            return os.environ[name]
    sys.exit("set ETH_RPC_URL to the chain the contract lives on (foundry convention)")


def rpc(payload, timeout=600):
    """POST one JSON-RPC request or batch, retrying rate limits (429/503) with backoff."""
    body = json.dumps(payload).encode()
    req = urllib.request.Request(rpc_url(), body, {"Content-Type": "application/json"})
    for attempt in range(7):
        try:
            return json.load(urllib.request.urlopen(req, timeout=timeout))
        except urllib.error.HTTPError as err:
            if err.code not in (429, 503):
                raise
            time.sleep(0.5 * 2**attempt)
    return json.load(urllib.request.urlopen(req, timeout=timeout))


def load_code(target):
    """Runtime bytecode from a hex file, or from eth_getCode when `target` is an address."""
    if target.startswith("0x") and len(target) == 42:
        reply = rpc(
            {"jsonrpc": "2.0", "id": 1, "method": "eth_getCode", "params": [target, "latest"]}
        )
        code_hex = reply["result"]
    else:
        with open(target) as handle:
            code_hex = handle.read().strip()
    raw = bytes.fromhex(code_hex.removeprefix("0x"))
    if not raw:
        sys.exit(f"{target}: no code")
    return raw


# --- abstract execution ------------------------------------------------------------------------


class Memory:
    """Byte-level memory where unknown bytes are absent; writes at unknown offsets forget all."""

    def __init__(self, known=None):
        self.known = dict(known or {})

    def copy(self):
        return Memory(self.known)

    def forget(self, offset, size):
        if offset is None or size is None:
            self.known.clear()
            return
        for k in range(offset, offset + min(size, 1 << 16)):
            self.known.pop(k, None)

    def store(self, offset, value, size=32):
        if offset is None or value is None:
            self.forget(offset, size)
            return
        for k in range(size):
            self.known[offset + k] = (value >> (8 * (size - 1 - k))) & 0xFF

    def load(self, offset, size=32):
        if offset is None or any(offset + k not in self.known for k in range(size)):
            return None
        return int.from_bytes(bytes(self.known[offset + k] for k in range(size)), "big")

    def read(self, offset, size):
        return [self.known.get(offset + k) for k in range(min(size, 1 << 12))]


def _initial_memory():
    memory = Memory()
    memory.store(0x40, FAKE_FREE_POINTER)
    memory.store(0x60, 0)
    return memory


def _pop(stack, n):
    """Pop n values, top first; below the known stack every value is unknown."""
    return [stack.pop() if stack else None for _ in range(n)]


def _memory_effect(code, name, args, memory):
    """Apply a memory-writing opcode to `memory`; returns the pushed value for MLOAD."""
    if name == "MLOAD":
        return memory.load(args[0])
    if name == "MSTORE":
        memory.store(args[0], args[1])
    elif name == "MSTORE8":
        memory.store(args[0], None if args[1] is None else args[1] & 0xFF, 1)
    elif name == "CODECOPY" and None not in args:
        dest, src, size = args
        for k in range(min(size, 1 << 16)):
            memory.known[dest + k] = code.raw[src + k] if src + k < len(code.raw) else 0
    elif name in ("CALLDATACOPY", "RETURNDATACOPY", "CODECOPY", "MCOPY"):
        memory.forget(args[0], args[2])
    elif name == "EXTCODECOPY":
        memory.forget(args[1], args[3])
    elif name in ("CALL", "CALLCODE", "DELEGATECALL", "STATICCALL"):
        memory.forget(args[-2], args[-1])
    return None


def _stack_op(name, immediate, stack):
    if name.startswith("PUSH"):
        stack.append(immediate or 0)
    elif name.startswith("DUP"):
        n = int(name[3:])
        stack.append(stack[-n] if len(stack) >= n else None)
    else:
        n = int(name[4:])
        while len(stack) < n + 1:
            stack.insert(0, None)
        stack[-1], stack[-1 - n] = stack[-1 - n], stack[-1]


def execute(code, name, immediate, stack, memory):
    """One non-control-flow instruction on an abstract stack of ints and None (unknown).

    With `memory` None the machine only tracks jump destinations: pushed constants are kept and
    computed values become unknown, except AND (solc masks internal function pointers with
    0xffffffff), which keeps the state space finite."""
    if name.startswith(("PUSH", "DUP", "SWAP")):
        _stack_op(name, immediate, stack)
        return
    pops, pushes = OPCODE_ARITY[name]
    args = _pop(stack, pops)
    result = None
    if memory is None:
        if name == "AND" and None not in args:
            result = args[0] & args[1]
    else:
        if name in ARITH and None not in args:
            result = ARITH[name](*args)
        else:
            result = _memory_effect(code, name, args, memory)
    if pushes:
        stack.append(result)


OPCODE_ARITY = {name: (pops, pushes) for name, pops, pushes in OPCODES.values()}


def decode_revert(data):
    """Readable reason for revert data given as a list of bytes (None for unknown)."""
    if not data:
        return "bare revert"
    if None in data[:4] or len(data) < 4:
        return "revert ?"
    selector = int.from_bytes(bytes(data[:4]), "big")
    if selector == PANIC:
        code = data[4:36]
        return f"panic {int.from_bytes(bytes(code), 'big'):#x}" if None not in code else "panic ?"
    if selector == ERROR_STRING:
        head = data[36:68]
        if None in head:
            return "error ?"
        length = int.from_bytes(bytes(head), "big")
        text = data[68 : 68 + length]
        if None in text or length > 256:
            return "error ?"
        return "error " + json.dumps(bytes(text).decode(errors="replace"))
    return f"custom {selector:#010x}"


@dataclass
class EdgeFate:
    doomed: bool
    reasons: tuple = ()  # revert reasons of the paths explored, when doomed
    decided: bool = True  # False when the budget ran out before every path ended


def _jump_successors(code, pc, name, stack):
    """(successors, returns, bad) after a JUMP/JUMPI in `edge_fate`: `returns` when a possible
    destination is unknown (a return into the caller), `bad` for a non-JUMPDEST destination."""
    dest, cond = _pop(stack, 2) if name == "JUMPI" else (_pop(stack, 1)[0], 1)
    successors = []
    if name == "JUMPI" and cond in (None, 0) and pc in code.following:
        successors.append(code.following[pc])
    if cond == 0:
        return successors, False, False
    if dest is None:
        return successors, True, False
    if dest not in code.jumpdests:
        return successors, False, True
    return [dest, *successors], False, False


def _mark_block(code, pc, stack, seen, visits):
    """Record entering a block; the stack to go on with (constants that are not jump
    destinations dropped after WIDEN_AFTER visits, so loops end), or None if seen before."""
    visits[pc] += 1
    if visits[pc] > WIDEN_AFTER:
        stack = [v if v in code.jumpdests else None for v in stack]
    if (pc, tuple(stack)) in seen:
        return None
    seen.add((pc, tuple(stack)))
    return stack


BUBBLE = "bubbles the callee's revert"


def _copies_returndata(code, pc):
    """Whether the basic block ending at `pc` copies return data (a bubbled callee revert)."""
    cursor = _block_start(code, pc)
    while cursor != pc:
        if code.ops[cursor][0] == "RETURNDATACOPY":
            return True
        cursor = code.following[cursor]
    return False


def _revert_reason(code, pc, stack, memory):
    if code.ops[pc][0] == "INVALID":
        return "invalid opcode"
    offset, size = _pop(stack, 2)
    if offset is None or size is None:
        return BUBBLE if _copies_returndata(code, pc) else "revert ?"
    return decode_revert(memory.read(offset, size))


def edge_fate(code, start):
    """Whether every path from `start` ends in REVERT/INVALID, with the revert reasons.

    Starts with an unknown stack and the free memory pointer at 0x80: a jump to an unknown
    destination returns into the caller, which counts as not doomed."""
    work = [(start, [], _initial_memory())]
    seen, visits, reasons = set(), defaultdict(int), set()
    while work:
        pc, stack, memory = work.pop()
        while pc is not None:
            name, immediate = code.ops.get(pc, ("STOP", None))
            if name == "JUMPDEST" or pc == start:
                stack = _mark_block(code, pc, stack, seen, visits)
                if stack is None:
                    break
                if len(seen) > DOOM_BUDGET:
                    return EdgeFate(False, decided=False)
            if name in ("REVERT", "INVALID"):
                reasons.add(_revert_reason(code, pc, stack, memory))
                break
            if name in ("JUMP", "JUMPI"):
                successors, returns, bad = _jump_successors(code, pc, name, stack)
                if returns:
                    return EdgeFate(False)
                reasons.update(["bad jump"] if bad else [])
                work.extend((target, list(stack), memory.copy()) for target in successors[1:])
                pc = successors[0] if successors else None
                continue
            if name in SUCCESS_ENDS or pc not in code.following:
                return EdgeFate(False)
            execute(code, name, immediate, stack, memory)
            pc = code.following[pc]
    return EdgeFate(True, tuple(sorted(reasons)))


@dataclass
class Reach:
    jumpis: set = field(default_factory=set)
    jump_targets: dict = field(default_factory=lambda: defaultdict(set))  # JUMP pc -> dests
    unresolved: set = field(default_factory=set)  # jumps whose destination stayed unknown
    complete: bool = True


def _follow_jump(code, pc, name, stack, reach, stop_at):
    """Destinations to continue at after a JUMP/JUMPI, recording what was seen."""
    dest, cond = _pop(stack, 2) if name == "JUMPI" else (_pop(stack, 1)[0], 1)
    targets = []
    if name == "JUMPI":
        reach.jumpis.add(pc)
        if code.following.get(pc) is not None:
            targets.append(code.following[pc])
    if dest is None:
        reach.unresolved.add(pc)
    elif dest in code.jumpdests and cond != 0:
        if name == "JUMP":
            reach.jump_targets[pc].add(dest)
        if dest not in stop_at:
            targets.append(dest)
    return targets


def reachable(code, start, stop_at=frozenset()):
    """JUMPIs reachable from `start`, following return addresses kept on the stack."""
    reach = Reach()
    work, seen = [(start, ())], set()
    while work:
        pc, frozen = work.pop()
        stack = list(frozen)
        while pc is not None:
            name, immediate = code.ops.get(pc, ("STOP", None))
            if name == "JUMPDEST" or pc == start:
                key = (pc, tuple(stack))
                if key in seen:
                    break
                seen.add(key)
                if len(seen) > REACH_BUDGET:
                    reach.complete = False
                    return reach
            if name in SUCCESS_ENDS or name in ("REVERT", "INVALID"):
                break
            if name in ("JUMP", "JUMPI"):
                targets = _follow_jump(code, pc, name, stack, reach, stop_at)
                work.extend((target, tuple(stack)) for target in targets)
                break
            execute(code, name, immediate, stack, None)
            pc = code.following.get(pc)
    return reach


# --- classification ----------------------------------------------------------------------------


class Expr:
    __slots__ = ("args", "op", "value")

    def __init__(self, op, args=(), value=None):
        self.op, self.args, self.value = op, args, value


def _block_start(code, pc):
    while pc in code.preceding:
        before = code.preceding[pc]
        if code.ops[pc][0] == "JUMPDEST" or code.ops[before][0] in (
            "JUMP", "JUMPI", "STOP", "RETURN", "REVERT", "INVALID", "SELFDESTRUCT"
        ):  # fmt: skip
            return pc
        pc = before
    return pc


CONDITION_BLOCKS = 4


def _condition_start(code, pc):
    """Start of the chain of up to CONDITION_BLOCKS basic blocks ending at `pc` in which every
    later block is entered only by falling through the JUMPI that ends the one before."""
    start = _block_start(code, pc)
    for _ in range(CONDITION_BLOCKS - 1):
        before = code.preceding.get(start)
        if before is None or code.ops[start][0] == "JUMPDEST" or code.ops[before][0] != "JUMPI":
            break
        start = _block_start(code, before)
    return start


def jumpi_conditions(code, pc):
    """Conditions of the JUMPIs from `_condition_start` to the one at `pc` (last), as
    expressions over the values the first block started with. Identical values are the same
    Expr object, so a call's success flag checked twice is recognisable."""
    stack, conditions, cursor = [], [], _condition_start(code, pc)
    while True:
        name, immediate = code.ops[cursor]
        if name.startswith("PUSH"):
            stack.append(Expr("const", value=immediate or 0))
        elif name.startswith(("DUP", "SWAP")):
            n = int(name[3:] if name.startswith("DUP") else name[4:]) + name.startswith("SWAP")
            while len(stack) < n:
                stack.insert(0, Expr("in"))
            if name.startswith("DUP"):
                stack.append(stack[-n])
            else:
                stack[-1], stack[-n] = stack[-n], stack[-1]
        else:
            pops, pushes = OPCODE_ARITY[name]
            while len(stack) < pops:
                stack.insert(0, Expr("in"))
            args = tuple(stack.pop() for _ in range(pops))
            if name == "JUMPI":
                conditions.append(args[1])
            if pushes and name in ARITH and all(_const(a) for a in args):
                stack.append(Expr("const", value=ARITH[name](*(a.value for a in args))))
            elif pushes:
                stack.append(Expr(name, args))
        if cursor == pc:
            return conditions
        cursor = code.following[cursor]


ABI_LEAVES = {"CALLVALUE", "CALLDATASIZE", "RETURNDATASIZE"}
CALL_OPS = {"CALL", "STATICCALL", "DELEGATECALL", "CALLCODE"}


def _mentions(expr, ops, depth=0):
    if expr.op in ops:
        return True
    return depth < 12 and any(_mentions(a, ops, depth + 1) for a in expr.args)


U64_MAX = (1 << 64) - 1


def _const(expr, value=None):
    return expr.op == "const" and (value is None or expr.value == value)


def _is_bounds_check(expr):
    """solc's ABI decoder bounds checks, which revert without data.

    `dataEnd - headStart < N` (also with N or dataEnd from an earlier block, and as
    `calldatasize + (-4) < N`), `offset > 2**64 - 1`, `offset + 0x1f < dataEnd`, and
    `start + 0x20 + length > dataEnd` (both operand orders)."""
    if expr.op not in ("LT", "GT", "SLT", "SGT"):
        return False
    a, b = expr.args
    if expr.op == "SLT" and a.op == "SUB":
        return True
    if expr.op == "SLT" and a.op == "ADD" and any(_const(y) and y.value >> 255 for y in a.args):
        return True
    if expr.op == "GT" and _const(b, U64_MAX):
        return True
    for x in (a, b):
        if x.op == "ADD" and any(_const(y, 0x1F) or _const(y, 0x20) for y in x.args):
            return True
        if x.op == "ADD" and any(y.op == "ADD" and _const(y.args[1], 0x1F) for y in x.args):
            return True
    return False


def _strip_iszero(expr):
    while expr.op == "ISZERO":
        expr = expr.args[0]
    return expr


def is_abi_check(expr, bare_revert):
    """CALLVALUE/CALLDATASIZE/RETURNDATASIZE checks, `x == cleanup(x)` validators (and the
    via-IR `x - cleanup(x)`, `x ^ cleanup(x)` forms), and, for guards that revert without data,
    the ABI decoder's bounds checks."""
    expr = _strip_iszero(expr)
    if _mentions(expr, ABI_LEAVES):
        return True
    if bare_revert and _is_bounds_check(expr):
        return True
    if expr.op not in ("EQ", "SUB", "XOR"):
        return False
    a, b = expr.args
    for x, y in ((a, b), (b, a)):
        if y.op in ("AND", "SIGNEXTEND") and x in y.args:
            return True
        if y.op == "ISZERO" and y.args[0].op == "ISZERO" and y.args[0].args[0] is x:
            return True
    return False


@dataclass
class JumpiInfo:
    pc: int
    kind: str  # fork, guard, both-revert, dispatcher, abi, unreached
    guard: str = ""  # panic, error-string, custom-error, other-revert (guards only)
    doomed_edge: str = ""  # taken or fallthrough (guards only)
    reason: str = ""
    functions: tuple = ()
    undecided: bool = False


def _guard_class(reasons):
    if all(r.startswith("panic") for r in reasons):
        return "panic"
    if any(r.startswith("error") for r in reasons):
        return "error-string"
    if any(r.startswith("custom") for r in reasons):
        return "custom-error"
    return "other-revert"


def is_call_recheck(conditions):
    """The JUMPI tests a call's success flag that a JUMPI it fell through already tested: solc's
    second `if success` after `if iszero(success) { revert }`."""
    flag = _strip_iszero(conditions[-1])
    return flag.op in CALL_OPS and any(_strip_iszero(c) is flag for c in conditions[:-1])


def is_code_check(condition, reason):
    """solc's `extcodesize(target) == 0` check before a high-level call, which reverts without
    data (a `require(target.code.length > 0, ...)` carries a reason and stays a behavior)."""
    return _strip_iszero(condition).op == "EXTCODESIZE" and reason in ("bare revert", "revert ?")


def classify_jumpi(code, pc):
    dest = code.jumpi_target(pc)
    taken = edge_fate(code, dest) if dest in code.jumpdests else EdgeFate(True, ("bad jump",))
    fallthrough = edge_fate(code, code.following.get(pc, pc + 1))
    info = JumpiInfo(pc, "fork", undecided=not (taken.decided and fallthrough.decided))
    if taken.doomed and fallthrough.doomed:
        info.kind, info.reason = "both-revert", "; ".join(taken.reasons + fallthrough.reasons)
    elif taken.doomed or fallthrough.doomed:
        fate = taken if taken.doomed else fallthrough
        info.kind, info.doomed_edge = "guard", "taken" if taken.doomed else "fallthrough"
        info.guard, info.reason = _guard_class(fate.reasons), "; ".join(fate.reasons)
    conditions = jumpi_conditions(code, pc)
    if (
        is_abi_check(conditions[-1], info.reason == "bare revert")
        or (info.kind == "guard" and info.reason == BUBBLE)
        or (info.kind == "guard" and is_code_check(conditions[-1], info.reason))
        or is_call_recheck(conditions)
    ):
        info.kind = "abi"
    return info


def function_entries(raw):
    """{bytecode offset: selector} of the external functions evmole finds."""
    # Imported here: symreplay imports this module and runs without evmole.
    import evmole

    info = evmole.contract_info(raw, selectors=True)
    return {f.bytecode_offset: f.selector for f in info.functions or []}


def classify(raw, selectors=None):
    """JumpiInfo for every JUMPI of `raw`, with the external functions that reach it; with
    `selectors`, only those functions are followed (the others' JUMPIs count as unreached)."""
    code = Code.from_bytes(raw)
    entries = function_entries(raw)
    dispatcher = reachable(code, 0, frozenset(entries))
    owners = defaultdict(set)
    incomplete = []
    for offset, selector in entries.items():
        if selectors is not None and selector not in selectors:
            continue
        reach = reachable(code, offset)
        incomplete += [] if reach.complete else [selector]
        for pc in reach.jumpis:
            owners[pc].add(selector)
    if incomplete:
        print(f"warning: reachability budget exhausted for {incomplete}", file=sys.stderr)
    table = {}
    for pc in code.jumpis():
        info = classify_jumpi(code, pc)
        info.functions = tuple(sorted(owners[pc]))
        if pc in dispatcher.jumpis:
            info.kind = "dispatcher"
        elif not info.functions:
            info.kind = "unreached"
        table[pc] = info
    return table


def panic_guards(raw):
    """PCs of JUMPIs whose doomed edge ends only in Panic(uint256); no evmole needed."""
    code = Code.from_bytes(raw)
    found = set()
    for pc in code.jumpis():
        info = classify_jumpi(code, pc)
        if info.kind == "guard" and info.guard == "panic":
            found.add(pc)
    return found


# --- dynamic coverage --------------------------------------------------------------------------

# Keeps "address:selector:pc:taken" -> count for JUMPIs executed by frames running the code of
# one of TARGETS; the selector is the first 4 bytes of the input of the frame that entered that
# code. `String(peek)` because geth returns a big.Int (`isZero`) and anvil a JS bigint.
JS_TRACER = """{
  edges: {}, frames: [], top: null, targets: TARGETS,
  enter: function(f) {
    var input = toHex(f.getInput());
    this.frames.push({a: toHex(f.getTo()), s: input.slice(2, 10)});
  },
  exit: function() { this.frames.pop(); },
  step: function(log) {
    if (log.op.toNumber() != 0x57) return;
    var depth = log.getDepth();
    if (this.top === null) this.top = {a: toHex(log.contract.getAddress()),
                                       s: toHex(log.contract.getInput()).slice(2, 10)};
    var frame = depth == 1 ? this.top : this.frames[depth - 2];
    if (!this.targets[frame.a]) return;
    var key = frame.a + ":" + frame.s + ":" + log.getPC() + ":" +
              (String(log.stack.peek(1)) == "0" ? 0 : 1);
    this.edges[key] = (this.edges[key] || 0) + 1;
  },
  fault: function() {},
  result: function(ctx) { return {edges: this.edges, error: ctx.error || null}; }
}"""


def _hexq(value):
    return value if isinstance(value, str) else hex(value)


def _trace_request(case, tracer):
    config = {"tracer": tracer, "timeout": "60s"}
    if "tx" in case:
        return {"method": "debug_traceTransaction", "params": [case["tx"], config]}
    call = {"to": case["to"], "data": case["data"]}
    if case.get("from"):
        call["from"] = case["from"]
    if case.get("gas_price") is not None:
        call["gasPrice"] = _hexq(case["gas_price"])
    state = {}
    for address, slots in (case.get("overrides") or {}).items():
        diff = {
            f"0x{int(s, 16):064x}": f"0x{int(v, 16) % (1 << 256):064x}" for s, v in slots.items()
        }
        state[address.lower()] = {"stateDiff": diff}
    for address, code in (case.get("code") or {}).items():
        state.setdefault(address.lower(), {})["code"] = code
    if case.get("gas_price") is not None and case.get("from"):
        state.setdefault(case["from"].lower(), {})["balance"] = hex(1 << 200)
    if state:
        config["stateOverrides"] = state
    block = {k: _hexq(case[k]) for k in ("number", "time") if case.get(k) is not None}
    if block:
        config["blockOverrides"] = block
    return {"method": "debug_traceCall", "params": [call, _hexq(case["block"]), config]}


def _trace_batch(requests):
    """Results of one batch; requests the node refuses (busy, limits) are retried alone."""
    payload = [{"jsonrpc": "2.0", "id": i, **r} for i, r in enumerate(requests)]
    replies = sorted(rpc(payload), key=lambda r: r["id"])
    results = [r.get("result") or {"rpc_error": r.get("error")} for r in replies]
    for index, result in enumerate(results):
        for attempt in range(6):
            if "rpc_error" not in result:
                break
            time.sleep(0.5 * 2**attempt)
            reply = rpc({"jsonrpc": "2.0", "id": 0, **requests[index]})
            result = reply.get("result") or {"rpc_error": reply.get("error")}
        results[index] = result
    return results


def trace_cases(targets, cases, workers=4, batch=25):
    """{target: {"target", "edges": {selector: {pc: [not taken, taken]}}, "cases", "errors"}}
    over `cases`, one trace pass for every target."""
    targets = [t.lower() for t in targets]
    tracer = JS_TRACER.replace("TARGETS", json.dumps(dict.fromkeys(targets, True)))
    requests = [_trace_request(case, tracer) for case in cases]
    chunks = [requests[i : i + batch] for i in range(0, len(requests), batch)]
    with ThreadPoolExecutor(workers) as pool:
        results = list(pool.map(_trace_batch, chunks))
    edges = {t: defaultdict(lambda: defaultdict(lambda: [0, 0])) for t in targets}
    errors = defaultdict(int)
    for reply in (r for chunk in results for r in chunk):
        if "rpc_error" in reply:
            errors["rpc: " + str(reply["rpc_error"])[:120]] += 1
            continue
        errors[reply["error"] or "ok"] += 1
        for key, count in reply["edges"].items():
            address, selector, pc, taken = key.split(":")
            edges[address][selector][int(pc)][int(taken)] += count
    return {
        t: {"target": t, "cases": len(cases), "errors": dict(errors), "edges": edges[t]}
        for t in targets
    }


# --- report ------------------------------------------------------------------------------------


def load_seen(paths):
    """Merge SEEN files into {selector: {pc: [not taken, taken]}}, checking they share a target."""
    merged = defaultdict(lambda: defaultdict(lambda: [0, 0]))
    targets, cases = set(), 0
    for path in paths:
        with open(path) as handle:
            seen = json.load(handle)
        targets.add(seen["target"])
        cases += seen["cases"]
        for selector, pcs in seen["edges"].items():
            for pc, counts in pcs.items():
                merged[selector][int(pc)][0] += counts[0]
                merged[selector][int(pc)][1] += counts[1]
    if len(targets) > 1:
        sys.exit(f"SEEN files trace different contracts: {sorted(targets)}")
    return merged, cases


def render(node, depth=0):
    """One-line form of a symreplay expression, cut at depth 4."""
    if node.op == "const":
        return str(node.val) if node.val < 1 << 64 else hex(node.val)
    if node.op == "leaf":
        return node.name
    if depth >= 4:
        return node.op + "(..)"
    return node.op + "(" + ", ".join(render(a, depth + 1) for a in node.args) + ")"


def trace_conditions(code, paths, arg_names):
    """{JUMPI pc: condition} from struct-log traces whose depth-1 frame runs `code`."""
    import symreplay

    names = {4 + 32 * i: n for i, n in enumerate(arg_names)}
    conditions = {}
    for path in paths:
        with open(path) as handle:
            logs = json.load(handle)
        logs = logs.get("result", logs)["structLogs"]
        top = {s["pc"] for s in logs if s["depth"] == 1 and s["op"] == "JUMPI"}
        if not top <= set(code.jumpis()):
            print(f"skipping {path}: its depth-1 frame runs other code", file=sys.stderr)
            continue
        with contextlib.redirect_stdout(io.StringIO()):
            conds, _ = symreplay.replay(path, names)
        for pc, node, taken in conds:
            if not isinstance(taken, tuple):
                conditions.setdefault(pc, render(node))
    return conditions


def edge_name(info, taken):
    if info.kind != "guard":
        return "taken" if taken else "fallthrough"
    doomed = (info.doomed_edge == "taken") == bool(taken)
    return f"revert ({info.reason})" if doomed else "pass"


def edge_group(info, taken):
    """What an unseen edge does, to group the report by: the revert reason or the edge kind."""
    if info.kind == "both-revert":
        return f"revert ({info.reason})"
    if info.kind != "guard":
        return "fork edge"
    doomed = (info.doomed_edge == "taken") == bool(taken)
    return f"revert ({info.reason})" if doomed else "guard passes"


def unseen_edges(info, counts, elsewhere):
    """[(description, group)] of the edges of one JUMPI that no call took, with the other
    functions whose calls took them (`elsewhere`: {selector: counts})."""
    missing = []
    for taken in (0, 1):
        if counts[taken]:
            continue
        others = ",".join(s for s, c in sorted(elsewhere.items()) if c[taken])
        name = edge_name(info, taken) if any(counts) else "never reached; " + edge_name(info, taken)
        text = f"{name} never taken" + (f" (taken under {others})" if others else "")
        missing.append((text, edge_group(info, taken)))
    return missing


def report_function(selector, table, all_edges, conditions):
    """Print one function's coverage; returns [(group, pc)] of its unseen behavior edges.
    `conditions` maps a pc to lines to print under it (symreplay or gigahorse)."""
    mine = [i for i in table.values() if selector in i.functions]
    edges = all_edges.get(selector, {})
    counts = {i.pc: edges.get(i.pc, [0, 0]) for i in mine}
    behaviors = [i for i in mine if i.kind in ("fork", "both-revert") or
                 (i.kind == "guard" and i.guard != "panic")]  # fmt: skip
    panics = [i for i in mine if i.kind == "guard" and i.guard == "panic"]
    skipped = [i for i in mine if i.kind in ("dispatcher", "abi")]
    both = sum(all(counts[i.pc]) for i in behaviors)
    fired = sum(counts[i.pc][i.doomed_edge == "taken"] > 0 for i in panics)
    print(
        f"\n== {selector}: {len(mine)} JUMPIs reachable, {len(behaviors)} behaviors "
        f"(forks and non-panic guards), both edges seen for {both}"
    )
    print(
        f"   panic guards: {len(panics)}, reached {sum(any(counts[i.pc]) for i in panics)}, "
        f"fired {fired}; dispatcher/ABI checks not counted: {len(skipped)}"
    )
    unseen = []
    for info in sorted(behaviors, key=lambda i: i.pc):
        elsewhere = {s: e[info.pc] for s, e in all_edges.items() if s != selector and info.pc in e}
        missing = unseen_edges(info, counts[info.pc], elsewhere)
        if not missing:
            continue
        unseen += [(group, info.pc) for _, group in missing]
        label = info.kind if info.kind != "guard" else f"guard {info.guard}"
        print(f"   pc {info.pc:5d} {label:22s} seen {counts[info.pc]}")
        for text, _ in missing:
            print(f"            {text}")
        for line in conditions.get(info.pc, []):
            print(f"            {line[:150]}")
    return unseen


def print_groups(unseen):
    """The unseen edges of every reported function grouped by revert reason or edge kind."""
    groups = defaultdict(set)
    for group, pc in unseen:
        groups[group].add(pc)
    print("\nunseen behavior edges by what they do (JUMPI pcs):")
    for group, pcs in sorted(groups.items(), key=lambda kv: -len(kv[1])):
        print(f"  {len(pcs):4d} {group}: {', '.join(str(pc) for pc in sorted(pcs))}")


def selected_functions(wanted, known, edges):
    """Normalised selectors from --function (with or without 0x), or every traced one;
    `known` holds the code's entry points."""
    if not wanted:
        return sorted(edges)
    selectors = []
    for value in wanted:
        selector = value.lower().removeprefix("0x").rjust(8, "0")
        if selector not in known:
            sys.exit(f"--function {value}: no such entry point; the code has {sorted(known)}")
        selectors.append(selector)
    return selectors


def edge_annotations(code, args):
    """{pc: lines} from --trace (symreplay conditions) and --tac (gigahorse statements)."""
    notes = defaultdict(list)
    names = [n for n in args.args.split(",") if n]
    for pc, condition in trace_conditions(code, args.trace or [], names).items():
        notes[pc].append(f"if {condition}")
    if args.tac:
        import tac

        for pc, lines in tac.jumpi_statements(tac.find_results(Path(args.tac))).items():
            notes[pc] += [f"tac: {line}" for line in lines]
    return notes


def run_report(args):
    raw = load_code(args.code)
    edges, cases = load_seen(args.seen)
    selectors = selected_functions(args.function, set(function_entries(raw).values()), edges)
    table = classify(raw, set(selectors))
    notes = edge_annotations(Code.from_bytes(raw), args)
    print(f"{cases} traced cases; JUMPI fork/guard edges per function, 'seen [not taken, taken]'")
    unseen = [edge for s in selectors for edge in report_function(s, table, edges, notes)]
    if unseen:
        print_groups(unseen)
    print(f"\nunseen behavior edges: {len(unseen)}")
    return 1 if unseen else 0


def run_classify(args):
    table = classify(load_code(args.code))
    kinds = defaultdict(int)
    for info in table.values():
        kinds[info.kind + (f" {info.guard}" if info.guard and info.kind == "guard" else "")] += 1
    print(f"JUMPIs: {len(table)}")
    for kind, n in sorted(kinds.items(), key=lambda kv: -kv[1]):
        print(f"  {n:5d} {kind}")
    undecided = [pc for pc, i in table.items() if i.undecided]
    if undecided:
        print(f"  undecided (budget ran out, counted as not reverting): {undecided}")
    if args.list:
        for info in table.values():
            owners = ",".join(info.functions[:4]) + ("..." if len(info.functions) > 4 else "")
            line = f"{info.pc:6d} {info.kind:11s} {info.guard:13s} {info.reason[:60]:60s} {owners}"
            print(line.rstrip())
    return 0


def run_trace(args):
    if bool(args.cases) == bool(args.txs):
        sys.exit("pass exactly one of --cases or --txs")
    if args.cases:
        with open(args.cases) as handle:
            cases = json.load(handle)
    else:
        with open(args.txs) as handle:
            cases = [{"tx": line.strip()} for line in handle if line.strip()]
    targets = [a for a in args.address.split(",") if a]
    started = time.time()
    seen = trace_cases(targets, cases, workers=args.workers)
    if len(targets) == 1:
        paths = {targets[0].lower(): args.out}
    else:
        os.makedirs(args.out, exist_ok=True)
        paths = {t: os.path.join(args.out, f"{t}.json") for t in seen}
    for target, path in paths.items():
        with open(path, "w") as handle:
            json.dump(seen[target], handle)
        edges = sum(len(pcs) for pcs in seen[target]["edges"].values())
        print(f"{target}: {edges} (function, JUMPI) pairs seen -> {path}")
    errors = next(iter(seen.values()))["errors"]
    print(f"{len(cases)} cases in {time.time() - started:.1f}s; outcomes: {errors}")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    commands = parser.add_subparsers(dest="command", required=True)
    p = commands.add_parser("classify")
    p.add_argument("code")
    p.add_argument("--list", action="store_true", help="one line per JUMPI")
    p = commands.add_parser("trace")
    p.add_argument("address", help="code address(es) to cover, comma-separated")
    p.add_argument("out", help="SEEN file for one address, directory for several")
    p.add_argument("--cases")
    p.add_argument("--txs", help="file with one transaction hash per line")
    p.add_argument("--workers", type=int, default=4)
    p = commands.add_parser("report")
    p.add_argument("code")
    p.add_argument("seen", nargs="+")
    p.add_argument("--function", action="extend", nargs="+", help="selectors, 0x optional")
    p.add_argument("--trace", action="append", help="struct-log trace for conditions")
    p.add_argument("--args", default="", help="ABI argument names for --trace")
    p.add_argument("--tac", help="gigahorse results dir: print the TAC at each unseen JUMPI")
    args = parser.parse_args()
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    run = {"classify": run_classify, "trace": run_trace, "report": run_report}[args.command]
    sys.exit(run(args))


if __name__ == "__main__":
    main()
