#!/usr/bin/env python3
"""Run the routing example through a read-only, block-pinned Base RPC proxy.

RPC_URL must support debug_traceCall with state overrides. Arguments after --
are forwarded to the example.
"""

import argparse
import copy
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError, URLError
from urllib.parse import urlsplit
from urllib.request import Request, urlopen


RPC_TIMEOUT = 45
EXAMPLE_TIMEOUT = 210
MAX_BODY_BYTES = 16 * 1024 * 1024
READ_METHODS = frozenset({
    "eth_chainId", "net_version", "web3_clientVersion", "eth_blockNumber",
    "eth_getBlockByNumber", "eth_getBlockByHash", "eth_call", "debug_traceCall",
    "eth_getCode", "eth_getBalance", "eth_getTransactionCount", "eth_getStorageAt",
    "eth_estimateGas",
})
BLOCK_PARAMETER = {
    "eth_call": 1,
    "debug_traceCall": 1,
    "eth_getCode": 1,
    "eth_getBalance": 1,
    "eth_getTransactionCount": 1,
    "eth_getStorageAt": 2,
    "eth_estimateGas": 1,
}


class RunnerError(Exception):
    """Failure safe to display without leaking the upstream URL."""


def redact(value):
    """Remove endpoint URLs from RPC errors."""
    if isinstance(value, str):
        return re.sub(r"https?://[^\s<>\"'`]+", "[RPC endpoint]", value)
    if isinstance(value, list):
        return [redact(item) for item in value]
    if isinstance(value, dict):
        return {key: redact(item) for key, item in value.items()}
    return value


class Upstream:
    def __init__(self, url):
        parsed = urlsplit(url)
        if parsed.scheme not in {"http", "https"} or not parsed.hostname:
            raise RunnerError("RPC_URL / BASE_RPC_URL must be an HTTP(S) RPC endpoint")
        self._url = url

    def request(self, payload):
        request = Request(
            self._url,
            data=json.dumps(payload).encode(),
            headers={"Content-Type": "application/json", "Accept": "application/json"},
            method="POST",
        )
        try:
            with urlopen(request, timeout=RPC_TIMEOUT) as response:
                raw = response.read(MAX_BODY_BYTES + 1)
            if len(raw) > MAX_BODY_BYTES:
                raise RunnerError("Upstream RPC response exceeded 16 MiB")
            result = json.loads(raw)
            if not isinstance(result, (dict, list)):
                raise RunnerError("Upstream returned an invalid JSON-RPC response")
            # Errors can contain credentials in endpoint URLs.
            responses = result if isinstance(result, list) else [result]
            for item in responses:
                if isinstance(item, dict) and "error" in item:
                    item["error"] = redact(item["error"])
            return result
        except HTTPError as error:
            raise RunnerError(f"Upstream RPC returned HTTP {error.code}") from None
        except (URLError, TimeoutError, OSError):
            raise RunnerError("Upstream RPC connection failed or timed out") from None
        except (ValueError, UnicodeError):
            raise RunnerError("Upstream RPC response was not valid JSON") from None

    def call(self, method, params):
        response = self.request({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
        if not isinstance(response, dict) or response.get("id") != 1:
            raise RunnerError(f"Upstream returned an invalid response to {method}")
        if "error" in response:
            error = response["error"]
            code = error.get("code", "unknown") if isinstance(error, dict) else "unknown"
            raise RunnerError(f"Upstream rejected {method} (RPC error {code})")
        if "result" not in response:
            raise RunnerError(f"Upstream omitted the result of {method}")
        return response["result"]


def rpc_error(request_id, code, message):
    return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}


class PinnedRPC:
    def __init__(self, upstream, header):
        self.upstream = upstream
        self.block = header["number"]
        self.block_hash = header["hash"]

    def prepare(self, request):
        if not isinstance(request, dict):
            return None, rpc_error(None, -32600, "Invalid JSON-RPC request")
        request_id = request.get("id")
        method = request.get("method")
        if request.get("jsonrpc") != "2.0" or not isinstance(method, str):
            return None, rpc_error(request_id, -32600, "Invalid JSON-RPC request")
        if method not in READ_METHODS:
            return None, rpc_error(request_id, -32601, "Method rejected by the read-only RPC proxy")
        params = request.get("params", [])
        if not isinstance(params, list):
            return None, rpc_error(request_id, -32602, "JSON-RPC params must be an array")
        pinned = copy.deepcopy(request)
        pinned["params"] = params = copy.deepcopy(params)
        if method == "eth_blockNumber":
            return None, {"jsonrpc": "2.0", "id": request_id, "result": self.block}
        if method == "eth_getBlockByNumber":
            if not params:
                return None, rpc_error(request_id, -32602, "Missing block parameter")
            params[0] = self.block
        elif method == "eth_getBlockByHash":
            if not params or not isinstance(params[0], str) or params[0].lower() != self.block_hash.lower():
                return None, rpc_error(request_id, -32602, "Only the frozen block hash may be read")
        elif method in BLOCK_PARAMETER:
            index = BLOCK_PARAMETER[method]
            if len(params) < index:
                return None, rpc_error(request_id, -32602, "Missing contract read parameters")
            if len(params) == index:
                params.append(self.block)
            else:
                params[index] = self.block
        return pinned, None

    def process(self, payload):
        if not isinstance(payload, list):
            pinned, response = self.prepare(payload)
            if response is not None:
                return response
            try:
                return self.upstream.request(pinned)
            except RunnerError as error:
                return rpc_error(pinned.get("id"), -32000, str(error))
        if not payload:
            return rpc_error(None, -32600, "Empty JSON-RPC batch")
        forwarded = []
        responses = []
        for request in payload:
            pinned, response = self.prepare(request)
            if response is not None:
                responses.append(response)
            else:
                forwarded.append(pinned)
        if forwarded:
            try:
                result = self.upstream.request(forwarded)
                if not isinstance(result, list):
                    raise RunnerError("Upstream did not return a JSON-RPC batch response")
                responses.extend(result)
            except RunnerError as error:
                responses.extend(rpc_error(request.get("id"), -32000, str(error)) for request in forwarded)
        return responses


def handler_for(proxy):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            try:
                length = int(self.headers.get("Content-Length", "0"))
                if length <= 0 or length > MAX_BODY_BYTES:
                    response = rpc_error(None, -32600, "Invalid JSON-RPC request size")
                else:
                    payload = json.loads(self.rfile.read(length))
                    response = proxy.process(payload)
            except (ValueError, UnicodeError):
                response = rpc_error(None, -32700, "Invalid JSON")
            raw = json.dumps(response).encode()
            try:
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)
            except (BrokenPipeError, ConnectionResetError):
                pass

    return Handler


def wait_for_process(command, cwd, process_env=None, timeout=None):
    """Reap children on exit, timeout or interruption."""
    process = subprocess.Popen(command, cwd=cwd, env=process_env)
    try:
        return process.wait(timeout=timeout)
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--no-build", action="store_true", help="Reuse the compiled routing example")
    parser.add_argument("--offline", action="store_true", help="Build without fetching Cargo dependencies")
    args, example_args = parser.parse_known_args()
    if example_args[:1] == ["--"]:
        example_args = example_args[1:]
    upstream_url = os.environ.get("RPC_URL") or os.environ.get("BASE_RPC_URL")
    if not upstream_url:
        raise RunnerError("Set RPC_URL or BASE_RPC_URL to a Base RPC supporting debug_traceCall")
    upstream = Upstream(upstream_url)
    chain_id = upstream.call("eth_chainId", [])
    if not isinstance(chain_id, str) or int(chain_id, 16) != 8453:
        raise RunnerError("The upstream RPC must serve Base chain ID 8453")
    root = Path(__file__).resolve().parents[3]
    if not args.no_build:
        command = ["cargo", "build", "-p", "protocol-testing", "--example", "elfomofi_routing"]
        if args.offline:
            command.append("--offline")
        result = wait_for_process(command, root)
        if result:
            return result
    target = Path(os.environ.get("CARGO_TARGET_DIR", "target"))
    if not target.is_absolute():
        target = root / target
    if os.environ.get("CARGO_BUILD_TARGET"):
        target /= os.environ["CARGO_BUILD_TARGET"]
    executable = target / "debug" / "examples" / "elfomofi_routing"
    if not executable.is_file():
        raise RunnerError("Routing example executable is missing; rerun without --no-build")
    header = upstream.call("eth_getBlockByNumber", ["latest", False])
    if (not isinstance(header, dict)
            or not isinstance(header.get("number"), str)
            or not isinstance(header.get("timestamp"), str)
            or not isinstance(header.get("hash"), str)
            or not re.fullmatch(r"0x[0-9a-fA-F]{64}", header["hash"])):
        raise RunnerError("Upstream did not return a complete real block header")
    try:
        block_number = int(header["number"], 16)
        block_timestamp = int(header["timestamp"], 16)
    except ValueError:
        raise RunnerError("Upstream block number or timestamp is invalid") from None
    print(f"Frozen real Base block {block_number} ({header['hash']}), timestamp {block_timestamp}", flush=True)
    proxy = PinnedRPC(upstream, header)
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_for(proxy))
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True)
    thread.start()
    child_env = os.environ.copy()
    child_env["RPC_URL"] = f"http://127.0.0.1:{server.server_port}"
    child_env.pop("WS_RPC_URL", None)
    child_env.pop("BASE_RPC_URL", None)
    try:
        return wait_for_process(
            [str(executable), "--poll-ms", "1000", *example_args],
            root,
            child_env,
            EXAMPLE_TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        raise RunnerError("Routing example exceeded its 210-second timeout") from None
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    try:
        status = main()
    except KeyboardInterrupt:
        print("Routing test interrupted; child process and proxy stopped", file=sys.stderr)
        status = 130
    except (RunnerError, OSError, ValueError) as error:
        # Setup errors may contain secret URLs.
        message = str(error) if isinstance(error, RunnerError) else "Routing runner setup failed"
        print(message, file=sys.stderr)
        status = 1
    sys.exit(status if status >= 0 else 128 - status)
