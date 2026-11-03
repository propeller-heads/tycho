"""Read gigahorse output: a function's call tree, revert reasons, storage access per function.

usage: python3 -I tac.py RESULTS_DIR tree FUNCTION
       python3 -I tac.py RESULTS_DIR reverts
       python3 -I tac.py RESULTS_DIR storage [FUNCTION ...]

RESULTS_DIR is the `out` directory gigahorse_run.py prints (contract.tac plus CSV relations),
or the per-contract directory above it (OUT_DIR/NAME). FUNCTION is a selector (`c9e270d0` or
`0xc9e270d0`, also for functions gigahorse named), a name (`swap`, or `swap(address,uint256)`
when the name is overloaded) or a private function's entry (`0x26e0`). An unknown FUNCTION is
an error that lists the public functions.

tree     the function and every private function it reaches, each once, callers first. CFG
         bookkeeping, CONST and plain JUMP lines are dropped and variable names shortened;
         every computation, SLOAD, branch and revert stays.
reverts  short ASCII constants (2-8 bytes, such as Fermi's revert codes) and the functions
         that hold them.
storage  storage reads (R) and writes (W) per function, from the storage_dump.dl client;
         with FUNCTION, only those functions and the private functions they reach.
"""

import argparse
import csv
import re
import sys
from collections import defaultdict
from pathlib import Path

CALL_TARGET = re.compile(r"CALLPRIVATE v\w+\((0x[0-9a-f]+)\)")


def function_key(header):
    """`swap(address,uint256)(v1a2arg0, ..) public {` -> `swap(address,uint256)`;
    `0x977515f5(v119earg0, ..)` and `0x2b81(..)` -> `0x977515f5`, `0x2b81`. The same strings as
    gigahorse's HighLevelFunctionName relation."""
    header = header.removeprefix("function ")
    if header.startswith("0x"):
        return header.split("(")[0]
    return header[: header.index(")") + 1] if ")" in header else header


def read_functions(results_dir):
    """Map each function's key (see function_key) to its contract.tac lines."""
    functions, current = {}, None
    for line in (results_dir / "contract.tac").read_text().splitlines():
        if line.startswith("function "):
            current = function_key(line)
            functions[current] = []
        if current is not None:
            functions[current].append(line)
    return functions


def selector_names(results_dir):
    """{selector as 8 hex digits: function key} from PublicFunction and HighLevelFunctionName."""
    names = dict(read_relation(results_dir, "HighLevelFunctionName"))
    return {
        f"{int(selector, 16):08x}": names.get(function, function)
        for function, selector in read_relation(results_dir, "PublicFunction")
    }


def resolve(results_dir, functions, wanted):
    """The function key FUNCTION names: exact key, selector, bare name or private entry."""
    if wanted in functions:
        return wanted
    digits = wanted.lower().removeprefix("0x")
    if len(digits) == 8 and all(c in "0123456789abcdef" for c in digits):
        key = selector_names(results_dir).get(digits)
        if key in functions:
            return key
    if "0x" + digits in functions:
        return "0x" + digits
    named = [key for key in functions if key.split("(")[0] == wanted]
    if len(named) == 1:
        return named[0]
    if named:
        sys.exit(f"{wanted} is overloaded; pass one of: {', '.join(named)}")
    public = [f"{sel} {key}" for sel, key in sorted(selector_names(results_dir).items())]
    sys.exit(f"no function {wanted} in contract.tac; public functions:\n  " + "\n  ".join(public))


def reachable(functions, root):
    """Breadth-first: root, then the private functions it calls, in call order."""
    order, queue = [root], [root]
    while queue:
        for line in functions[queue.pop(0)]:
            for callee in CALL_TARGET.findall(line):
                if callee in functions and callee not in order:
                    order.append(callee)
                    queue.append(callee)
    return order


def compact(lines):
    out = []
    for line in lines:
        s = line.strip()
        if not s or s.startswith(("prev=", "====")) or re.search(r"= CONST\s*$", s):
            continue
        if re.match(r"^0x\S+: JUMP v\S+\(0x[0-9a-f]+\)$", s):
            continue
        s = re.sub(r"^0x[0-9a-fA-FSx_]+: ", "", s)
        s = re.sub(r"\b(v[0-9a-f_]+?)(?:V[0-9a-f]+)+\b", r"\1", s)
        s = re.sub(r"\bBegin block (0x[0-9a-f]+)(?:B0x[0-9a-f]+)+", r"Begin block \1", s)
        out.append(("" if s.startswith(("Begin", "function", "}")) else "  ") + s)
    return out


def print_tree(results_dir, wanted):
    functions = read_functions(results_dir)
    for key in reachable(functions, resolve(results_dir, functions, wanted)):
        print("\n".join(compact(functions[key])))


STATEMENT = re.compile(r"^\s*(0x[0-9a-f]+)(?:S0x[0-9a-f]+)*: (.*)$")
DEFINITION = re.compile(r"^(v\w+) = (\w+) ?(.*)$")
CLONE_SUFFIX = re.compile(r"\b(v[0-9a-f_]+?)(?:V[0-9a-f]+)+\b")


def _expand(token, definitions, depth):
    """`v12(0x20)` -> `0x20`; a variable -> its defining operation, `depth` levels deep."""
    token = token.strip()
    constant = re.fullmatch(r"v\w+\((0x[0-9a-f]+)\)", token)
    if constant:
        return constant[1]
    if depth == 0 or token not in definitions:
        return CLONE_SUFFIX.sub(r"\1", token)
    op, operands = definitions[token]
    if not operands:
        return op
    expanded = [_expand(t, definitions, depth - 1) for t in operands.split(", ")]
    return f"{op}({', '.join(expanded)})"


def jumpi_statements(results_dir, depth=3):
    """{bytecode pc of a JUMPI: ["if CONDITION", ...]}, the condition expanded `depth`
    definitions deep; one line per distinct form among gigahorse's clones of the statement."""
    definitions, jumpis = {}, defaultdict(list)
    for line in (results_dir / "contract.tac").read_text().splitlines():
        statement = STATEMENT.match(line)
        if not statement:
            continue
        definition = DEFINITION.match(statement[2])
        if definition:
            definitions[definition[1]] = (definition[2], definition[3])
        elif statement[2].startswith("JUMPI "):
            jumpis[int(statement[1], 16)].append(statement[2].removeprefix("JUMPI "))
    found = {}
    for pc, operand_lists in jumpis.items():
        forms = []
        for operands in operand_lists:
            condition = operands.split(", ", 1)[1]
            text = f"if {_expand(condition, definitions, depth)}"
            forms += [] if text in forms else [text]
        found[pc] = forms[:3]
    return found


def print_reverts(results_dir):
    found = defaultdict(set)
    for key, lines in read_functions(results_dir).items():
        for word in re.findall(r"\(0x([0-9a-f]{64})\)", "\n".join(lines)):
            raw = bytes.fromhex(word).rstrip(b"\0")
            if 1 < len(raw) <= 8 and all(0x20 < b < 0x7F for b in raw):
                found[raw.decode()].add(key)
    for text, keys in sorted(found.items()):
        print(text, sorted(keys))


def read_relation(results_dir, name):
    with (results_dir / f"{name}.csv").open() as f:
        return list(csv.reader(f, delimiter="\t"))


def print_storage(results_dir, wanted):
    block_of = dict(read_relation(results_dir, "TAC_Block"))
    function_of = dict(read_relation(results_dir, "InFunction"))
    names = dict(read_relation(results_dir, "HighLevelFunctionName"))
    access = defaultdict(set)
    for kind, relation in (("R", "StorageLoad"), ("W", "StorageStore")):
        for stmt, construct, *_ in read_relation(results_dir, relation):
            function = function_of.get(block_of.get(stmt, ""), "?")
            access[names.get(function, function)].add(f"{kind} {construct}")
    if not wanted:
        for key in sorted(access):
            print(key)
            for item in sorted(access[key]):
                print("   ", item)
        return
    functions = read_functions(results_dir)
    for root in wanted:
        key = resolve(results_dir, functions, root)
        reached = [k for k in reachable(functions, key) if access.get(k)]
        if not reached:
            print(f"{key}: no storage access found (also none in the functions it calls)")
        for k in reached:
            print(k)
            for item in sorted(access[k]):
                print("   ", item)


def find_results(path):
    """`path` itself, or the single work/NAME/out below a gigahorse_run.py contract directory."""
    if (path / "contract.tac").is_file():
        return path
    found = sorted(path.glob("work/*/out/contract.tac"))
    if len(found) == 1:
        return found[0].parent
    sys.exit(f"no contract.tac in {path}; pass the out dir gigahorse_run.py printed")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    p.add_argument("results_dir", type=Path)
    sub = p.add_subparsers(dest="command", required=True)
    sub.add_parser("tree").add_argument("function")
    sub.add_parser("reverts")
    sub.add_parser("storage").add_argument("functions", nargs="*")
    a = p.parse_args()
    a.results_dir = find_results(a.results_dir)
    if a.command == "tree":
        print_tree(a.results_dir, a.function)
    elif a.command == "reverts":
        print_reverts(a.results_dir)
    else:
        print_storage(a.results_dir, a.functions)


if __name__ == "__main__":
    main()
