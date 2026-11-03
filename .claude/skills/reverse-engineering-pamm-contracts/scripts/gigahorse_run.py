"""Decompile a contract with gigahorse: TAC (contract.tac) plus the storage client's facts.

usage: python3 -I gigahorse_run.py INSTALL_DIR ADDRESS|HEXFILE OUT_DIR

INSTALL_DIR comes from gigahorse_setup.sh. An address is fetched with eth_getCode from
ETH_RPC_URL; a file holds runtime bytecode as hex. Each contract gets its own directory
OUT_DIR/NAME (NAME is the lowercase address or the file's stem), so one OUT_DIR can hold many
contracts; running the same contract again replaces only its own directory. The results are in
OUT_DIR/NAME/work/NAME/out: contract.tac and one CSV per relation. tac.py accepts that path or
OUT_DIR/NAME. The first run compiles the Datalog analyses (3-5 minutes); later runs reuse them
(seconds per contract).
"""

import argparse
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

SKILL_DIR = Path(__file__).resolve().parent.parent
CLIENTS = "clients/visualizeout.py,myclients/storage_dump.dl"


def rpc(method, params):
    url = os.environ.get("ETH_RPC_URL") or sys.exit("set ETH_RPC_URL to fetch code by address")
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, body, {"Content-Type": "application/json"})
    return json.load(urllib.request.urlopen(req, timeout=60))["result"]


def read_bytecode(target):
    """(name, runtime bytecode hex) of a hex file or of an address's code at latest."""
    path = Path(target)
    if path.is_file():
        code = path.read_text().strip().removeprefix("0x")
        name = path.stem
    elif re.fullmatch(r"0x[0-9a-fA-F]{40}", target):
        print(f"chain id: {int(rpc('eth_chainId', []), 16)}")
        code = rpc("eth_getCode", [target, "latest"]).removeprefix("0x")
        name = target.lower()
    else:
        sys.exit(f"{target} is neither a file nor a 0x-prefixed 20-byte address")
    if not code:
        sys.exit(f"{target}: no code (EOA, self-destructed, or wrong chain)")
    return re.sub(r"[^A-Za-z0-9_.-]", "_", name), code


def gigahorse_env(repo):
    env = dict(os.environ)
    if platform.system() == "Darwin":
        brew = subprocess.run(["brew", "--prefix"], capture_output=True, text=True, check=True)
        env["HOMEBREW_PREFIX"] = brew.stdout.strip()
        env["PATH"] = f"{SKILL_DIR / 'gigahorse' / 'macos-shims'}:{env['PATH']}"
        env["DYLD_LIBRARY_PATH"] = str(repo / "souffle-addon")
    return env


def souffle_binary(install_dir):
    copied = install_dir / "souffle" / "bin" / "souffle"
    if copied.is_file():
        return str(copied)
    return shutil.which("souffle") or sys.exit("souffle not found; run gigahorse_setup.sh")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("install_dir", type=Path)
    p.add_argument("target", help="contract address or hex file")
    p.add_argument("out_dir", type=Path)
    a = p.parse_args()
    repo = a.install_dir.resolve() / "gigahorse-toolchain"
    if not (repo / "gigahorse.py").is_file():
        sys.exit(f"no gigahorse in {a.install_dir}; run gigahorse_setup.sh {a.install_dir}")
    name, code = read_bytecode(a.target)
    out_dir = a.out_dir.resolve() / name
    out_dir.mkdir(parents=True, exist_ok=True)
    hex_path = out_dir / f"{name}.hex"
    hex_path.write_text(code + "\n")
    shutil.copyfile(
        SKILL_DIR / "gigahorse" / "storage_dump.dl", repo / "myclients" / "storage_dump.dl"
    )
    command = [
        "uv", "run", "--frozen", "python", "./gigahorse.py",
        "--souffle_bin", souffle_binary(a.install_dir.resolve()),
        "-w", str(out_dir / "work"), "--results_file", str(out_dir / "results.json"),
        "--restart", "-C", CLIENTS, str(hex_path),
    ]  # fmt: skip
    started = time.monotonic()
    log_path = out_dir / "gigahorse.log"
    with log_path.open("w") as log:
        status = subprocess.run(
            command, check=False, cwd=repo, env=gigahorse_env(repo), stdout=log, stderr=log
        )
    results = out_dir / "work" / hex_path.stem / "out"
    if status.returncode or not (results / "contract.tac").is_file():
        sys.exit(f"gigahorse failed (exit {status.returncode}); see {log_path}")
    if not (results / "StorageLoad.csv").is_file():
        sys.exit(f"storage client produced no output; see {log_path} and {results}/*.err")
    print(f"done in {time.monotonic() - started:.0f} s; results in {results}")
    print(f"next: python3 -I {SKILL_DIR / 'scripts' / 'tac.py'} {results} tree SELECTOR")


if __name__ == "__main__":
    main()
