"""Replay a geth struct-log trace symbolically for the depth-1 frame.

usage: symreplay.py TRACE_JSON [--args name1,name2,...] [--hide-panics CODE] [--from-pc PC]

Every stack value carries an expression tree. Leaves are calldata words (named by --args, in
ABI order starting at offset 4), storage reads S[slot], call results retN[word], block values
and constants. Prints an SSA listing of the executed path: every branch condition that depended
on a symbolic input, every SSTORE, and the RETURN/REVERT words.

Limits: one frame (depth 1) only; memory is tracked per aligned 32-byte word, so unaligned or
partial writes fall back to the concrete value; KECCAK256 results are concrete (use
storage_access.py for their preimages). Concrete values are cross-checked against the trace on
every step, so a wrong symbolic model degrades to constants instead of wrong expressions.

--hide-panics CODE (runtime-bytecode hex file or address) drops the BRANCH lines of JUMPIs whose
failing edge only reaches Panic(uint256), i.e. solc's checked-math and bounds checks, using the
classification of coverage.py.

--from-pc PC (decimal or 0x hex) prints only what happens after the depth-1 frame first reaches
PC, e.g. the external function's entry (the `Begin block` of `tac.py tree`), which drops the
dispatcher and ABI-decoding preamble. The replay itself still runs from the first step.
"""

import json
import os
import sys

M = (1 << 256) - 1


class Node:
    __slots__ = ("args", "id", "name", "op", "val")

    def __init__(self, op, args, val, name=None):
        self.op, self.args, self.val, self.name, self.id = op, args, val, name, None

    @property
    def const(self):
        return self.op == "const"


def C(v):
    return Node("const", (), v & M)


def leaf(name, v):
    return Node("leaf", (), v & M, name)


def s2i(x):
    return int(x, 16)


ARITH2 = {
    "ADD",
    "MUL",
    "SUB",
    "DIV",
    "SDIV",
    "MOD",
    "SMOD",
    "EXP",
    "SIGNEXTEND",
    "LT",
    "GT",
    "SLT",
    "SGT",
    "EQ",
    "AND",
    "OR",
    "XOR",
    "BYTE",
    "SHL",
    "SHR",
    "SAR",
}
ARITH3 = {"ADDMOD", "MULMOD"}
# Environment values kept as named leaves, because quotes may depend on them.
SYMBOLIC_ENV = {"TIMESTAMP", "NUMBER", "CALLER", "ORIGIN"}


def simplify(op, args, val):
    if all(a.const for a in args):
        return C(val)
    if op == "AND":
        a, b = args
        for x, y in ((a, b), (b, a)):
            if y.const and y.val == M:
                return x
            if y.const and y.val in ((1 << 160) - 1,) and x.op == "leaf":
                return x
    if op == "ISZERO" and args[0].op == "ISZERO" and args[0].args[0].op == "ISZERO":
        return args[0].args[0]
    if op in ("ADD", "OR", "XOR"):
        a, b = args
        if a.const and a.val == 0:
            return b
        if b.const and b.val == 0:
            return a
    if op == "SUB" and args[1].const and args[1].val == 0:
        return args[0]
    if op == "MUL":
        a, b = args
        if a.const and a.val == 1:
            return b
        if b.const and b.val == 1:
            return a
    if op == "DIV" and args[1].const and args[1].val == 1:
        return args[0]
    return Node(op, tuple(args), val)


def word_label(value, hashes):
    """Short, readable form of one 32-byte preimage word."""
    if value < 1 << 32:
        return str(value)
    derived = slot_label(value, hashes)
    if derived.startswith("keccak("):
        return derived
    if value < 1 << 32:
        return str(value)
    if value < 1 << 160:
        return f"0x{value:040x}"[:8] + ".."
    return f"{value:#x}"[:10] + ".."


def preimage_label(step, offset, size, hashes):
    """`keccak(a . b)` for the memory a KECCAK256 step hashes, one item per 32-byte word."""
    raw = "".join(w.removeprefix("0x") for w in step.get("memory") or [])
    data = raw[offset * 2 : (offset + size) * 2]
    words = [int(data[i : i + 64], 16) for i in range(0, len(data), 64) if data[i : i + 64]]
    return "keccak(" + " . ".join(word_label(w, hashes) for w in words) + ")"


def slot_label(slot, hashes):
    """Name a storage slot by the hash it was derived from: `keccak(..)+k`, or the plain number."""
    if slot < 1 << 32:
        return str(slot)
    for base, label in hashes.items():
        if 0 <= slot - base < 256:
            offset = slot - base
            return label if offset == 0 else f"{label}+{offset}"
    return f"{slot:#x}"


ENV_CONCRETE = {
    "ADDRESS",
    "CALLVALUE",
    "CALLDATASIZE",
    "RETURNDATASIZE",
    "GAS",
    "CHAINID",
    "SELFBALANCE",
    "CODESIZE",
}
# Opcodes whose result is kept as the traced constant, with the number of arguments they pop.
OPAQUE = {
    "MSIZE": 0, "PC": 0, "BALANCE": 1, "EXTCODESIZE": 1, "EXTCODEHASH": 1, "BLOCKHASH": 1,
    "BASEFEE": 0, "GASPRICE": 0, "COINBASE": 0, "GASLIMIT": 0, "PREVRANDAO": 0, "DIFFICULTY": 0,
    "TLOAD": 1, "BLOBBASEFEE": 0,
}  # fmt: skip


class Replay:
    """Symbolic replay of the depth-1 frame of one struct-log trace."""

    def __init__(self, logs, names, from_pc=None):
        self.logs, self.names, self.from_pc = logs, names, from_pc
        self.stack, self.mem, self.hashes = [], {}, {}
        self.conds, self.ret, self.calls = [], None, 0
        self.started = from_pc is None

    def pop(self, n):
        if n == 0:
            return []
        vals = self.stack[-n:][::-1]
        del self.stack[-n:]
        return vals

    def mwrite(self, off, node):
        self.mclear(off, 32)
        self.mem[off] = node

    def mclear(self, off, size):
        for k in [k for k in self.mem if k > off - 32 and k < off + size]:
            del self.mem[k]

    def resync(self, traced):
        """Replace values that drifted from the trace's stack by its constants."""
        if len(self.stack) != len(traced):
            self.stack[:] = [C(s2i(x)) for x in traced]
        for k, x in enumerate(traced):
            if self.stack[k].val != s2i(x):
                self.stack[k] = C(s2i(x))

    def run(self):
        for i, step in enumerate(self.logs):
            if step["depth"] != 1:
                continue
            op = step["op"]
            self.started = self.started or step["pc"] == self.from_pc
            nxt = self.logs[i + 1] if i + 1 < len(self.logs) else None
            out = s2i(nxt["stack"][-1]) if nxt and nxt["stack"] else 0
            self.resync(step["stack"])
            if self.stack_op(op, step, nxt, out) or self.memory_op(op, nxt, out):
                continue
            if op in ("STATICCALL", "CALL", "DELEGATECALL"):
                self.call(op, i, step)
            elif not self.control_op(op, step):
                break
        return self.conds, self.ret

    def stack_op(self, op, step, nxt, out):
        """Stack, arithmetic and environment opcodes; False when `op` is not one."""
        if op.startswith("PUSH"):
            self.stack.append(C(s2i(nxt["stack"][-1])))
        elif op.startswith("DUP"):
            self.stack.append(self.stack[-int(op[3:])])
        elif op.startswith("SWAP"):
            n = int(op[4:])
            self.stack[-1], self.stack[-1 - n] = self.stack[-1 - n], self.stack[-1]
        elif op == "POP":
            self.pop(1)
        elif op in ARITH2 or op in ARITH3 or op in ("ISZERO", "NOT"):
            arity = 2 if op in ARITH2 else 3 if op in ARITH3 else 1
            self.stack.append(simplify(op, self.pop(arity), out))
        elif op == "CALLDATALOAD":
            (o,) = self.pop(1)
            self.stack.append(leaf(self.names.get(o.val, f"cd[{o.val:#x}]"), out))
        elif op == "SLOAD":
            (k,) = self.pop(1)
            self.stack.append(leaf(f"S[{slot_label(k.val, self.hashes)}]", out))
        elif op in SYMBOLIC_ENV:
            self.stack.append(leaf(op.lower(), out))
        elif op in ENV_CONCRETE:
            self.stack.append(C(out))
        elif op in OPAQUE:
            self.pop(OPAQUE[op])
            self.stack.append(C(out))
        elif op in ("KECCAK256", "SHA3"):
            o, s = self.pop(2)
            self.hashes[out] = preimage_label(step, o.val, s.val, self.hashes)
            self.stack.append(C(out))
        else:
            return False
        return True

    def memory_op(self, op, nxt, out):
        """Memory opcodes; False when `op` is not one."""
        if op == "MLOAD":
            (o,) = self.pop(1)
            self.stack.append(self.mem.get(o.val, C(out)))
        elif op == "MSTORE":
            o, v = self.pop(2)
            self.mwrite(o.val, v)
        elif op == "MSTORE8":
            o, _ = self.pop(2)
            self.mclear(o.val, 1)
        elif op == "CALLDATACOPY":
            d, o, s = self.pop(3)
            self.mclear(d.val, s.val)
            for w in range(0, s.val, 32):
                self.mem[d.val + w] = leaf(
                    self.names.get(o.val + w, f"cd[{o.val + w:#x}]"),
                    s2i(nxt["memory"][(d.val + w) // 32]) if nxt.get("memory") else 0,
                )
        elif op in ("CODECOPY", "EXTCODECOPY"):
            a = self.pop(3 if op == "CODECOPY" else 4)
            d, s = (a[0], a[2]) if op == "CODECOPY" else (a[1], a[3])
            self.mclear(d.val, s.val)
        elif op == "RETURNDATACOPY":
            d, o, s = self.pop(3)
            self.mclear(d.val, s.val)
            memv = nxt.get("memory") or []
            for w in range(0, s.val, 32):
                idx = (d.val + w) // 32
                v = s2i(memv[idx]) if idx < len(memv) else 0
                self.mem[d.val + w] = leaf(f"ret{self.calls}[{(o.val + w) // 32}]", v)
        else:
            return False
        return True

    def call(self, op, i, step):
        a = self.pop(7 if op == "CALL" else 6)
        self.calls += 1
        ro, rs = a[-2].val, a[-1].val
        # returned words land in memory at ro after the call; read from the next depth-1 step
        j = i + 1
        while self.logs[j]["depth"] != 1:
            j += 1
        memv = self.logs[j].get("memory") or []
        self.mclear(ro, rs)
        for w in range(0, rs, 32):
            idx = (ro + w) // 32
            v = s2i(memv[idx]) if idx < len(memv) else 0
            self.mem[ro + w] = leaf(f"ret{self.calls}[{w // 32}]", v)
        self.stack.append(C(s2i(self.logs[j]["stack"][-1])))
        if self.started:
            print(f"# call{self.calls} {op} to={a[1].val:#x} pc={step['pc']}")

    def control_op(self, op, step):
        """Jumps, storage writes, logs and the frame's end; False when the frame ends."""
        if op == "JUMP":
            self.pop(1)
        elif op == "JUMPI":
            _, c = self.pop(2)
            if self.started and not c.const:
                self.conds.append((step["pc"], c, c.val != 0))
        elif op in ("RETURN", "REVERT"):
            o, s = self.pop(2)
            words = [self.mem.get(o.val + w, C(0)) for w in range(0, s.val, 32)]
            self.ret = (op, words) if self.started else None
            return False
        elif op in ("SSTORE", "TSTORE"):
            k, v = self.pop(2)
            if self.started:
                label = slot_label(k.val, self.hashes)
                self.conds.append((step["pc"], v, ("store", op, label)))
        elif op.startswith("LOG"):
            self.pop(2 + int(op[3:]))
        elif op not in ("JUMPDEST", "STOP"):
            raise SystemExit(f"unhandled {op} at pc {step['pc']}")
        return True


def replay(path, names, from_pc=None):
    """(conditions and stores, RETURN/REVERT words) of the trace's depth-1 frame."""
    with open(path) as f:
        data = json.load(f)
    return Replay(data.get("result", data)["structLogs"], names, from_pc).run()


def dump(conds, ret, hidden=frozenset()):
    counter = [0]
    lines = []

    def name(n):
        if n.const:
            v = n.val
            if v > (1 << 255):
                return f"-{(1 << 256) - v:#x}" if (1 << 256) - v < (1 << 128) else f"{v:#x}"
            return str(v) if v < 1 << 64 else f"{v:#x}"
        if n.op == "leaf":
            return n.name
        if n.id is None:
            argn = [name(a) for a in n.args]
            counter[0] += 1
            n.id = f"t{counter[0]}"
            value = n.val if n.val < 1 << 200 else hex(n.val)
            lines.append(f"{n.id} = {n.op}({', '.join(argn)})    ; {value}")
        return n.id

    out = []
    for pc, c, taken in conds:
        if pc in hidden and not isinstance(taken, tuple):
            continue
        nm = name(c)
        for ln in lines:
            out.append("  " + ln)
        lines.clear()
        if isinstance(taken, tuple):
            out.append(f"{taken[1]} pc={pc} [{taken[2]}] = {nm}")
            continue
        out.append(f"BRANCH pc={pc} if {nm} -> {'taken' if taken else 'fallthrough'}")
    if ret:
        kind, words = ret
        nms = [name(w) for w in words]
        for ln in lines:
            out.append("  " + ln)
        out.append(f"{kind} " + ", ".join(nms))
    print("\n".join(out))


if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument("trace")
    parser.add_argument("--args", default="", help="comma-separated names of the ABI arguments")
    parser.add_argument("--hide-panics", metavar="CODE", help="bytecode hex file or address")
    parser.add_argument("--from-pc", type=lambda v: int(v, 0), help="print from this pc on")
    opts = parser.parse_args()
    arg_names = [n for n in opts.args.split(",") if n]
    names = {4 + 32 * i: n for i, n in enumerate(arg_names)}
    hidden = frozenset()
    if opts.hide_panics:
        sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
        import coverage

        hidden = frozenset(coverage.panic_guards(coverage.load_code(opts.hide_panics)))
    conds, ret = replay(opts.trace, names, opts.from_pc)
    if opts.from_pc is not None and not conds and ret is None:
        sys.exit(f"the depth-1 frame never reaches pc {opts.from_pc:#x}")
    dump(conds, ret, hidden)
