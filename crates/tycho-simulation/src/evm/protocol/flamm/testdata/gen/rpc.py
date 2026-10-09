# Copyright (c) 2026 Everlong Labs Limited
"""Minimal JSON-RPC client for Base with retry/backoff and endpoint fallback, plus keccak/ABI helpers.

Only eth_getStorageAt, eth_call, eth_getLogs, eth_getBlockByNumber and eth_getCode are used; no debug_* methods.
"""
import json
import time
import urllib.request
import urllib.error
from Crypto.Hash import keccak as _keccak

ENDPOINTS = [
    "https://mainnet.base.org",
    "https://base-rpc.publicnode.com",
    "https://base.llamarpc.com",
]
# llamarpc answers HTTP 525 intermittently; it is last and only reached after the other two fail.


def keccak256(data: bytes) -> bytes:
    h = _keccak.new(digest_bits=256)
    h.update(data)
    return h.digest()


def selector(sig: str) -> str:
    return "0x" + keccak256(sig.encode()).hex()[:8]


def topic(sig: str) -> str:
    return "0x" + keccak256(sig.encode()).hex()


def w256(x: int) -> bytes:
    return x.to_bytes(32, "big")


def addr_word(a: str) -> bytes:
    return bytes.fromhex(a[2:].lower().rjust(64, "0"))


def hexword(x: int) -> str:
    return "0x" + x.to_bytes(32, "big").hex()


def map_slot(key: bytes, slot: int) -> int:
    """Solidity mapping slot: keccak256(key(32) . slot(32))."""
    assert len(key) == 32
    return int.from_bytes(keccak256(key + w256(slot)), "big")


def array_base(slot: int) -> int:
    """Solidity dynamic array data base: keccak256(slot(32))."""
    return int.from_bytes(keccak256(w256(slot)), "big")


class RPC:
    def __init__(self, endpoints=None, verbose=False):
        self.endpoints = list(endpoints or ENDPOINTS)
        self.idx = 0
        self.id = 0
        self.verbose = verbose
        self.calls = 0
        self.batch_size = 4
        self.pace = 0.25
        self.min_pace = 0.2

    def _post(self, payload):
        """One JSON-RPC request (or batch). mainnet.base.org is the only public archive endpoint that answers
        eth_getStorageAt at historic blocks (publicnode refuses archive reads without a token, llamarpc answers
        HTTP 525 intermittently), so requests are paced and a rate-limit answer backs off instead of failing over."""
        last = None
        for attempt in range(20):
            url = self.endpoints[self.idx % len(self.endpoints)]
            try:
                time.sleep(self.pace)
                req = urllib.request.Request(url, data=json.dumps(payload).encode(),
                                             headers={"content-type": "application/json",
                                                      "user-agent": "curl/8.4.0"})
                with urllib.request.urlopen(req, timeout=40) as r:
                    body = json.loads(r.read())
                self.calls += 1
                if isinstance(body, list):
                    errs = [b for b in body if "error" in b]
                    if errs and any(self._rate_limited(e["error"]) for e in errs):
                        raise RuntimeError("rate limited: %s" % errs[0]["error"])
                elif "error" in body and self._rate_limited(body["error"]):
                    raise RuntimeError("rate limited: %s" % body["error"])
                self.pace = max(self.min_pace, self.pace * 0.9)
                return body
            except Exception as e:  # noqa: BLE001
                last = e
                if self.verbose:
                    print("rpc %s attempt %d: %s" % (url, attempt, e), flush=True)
                code = getattr(e, "code", None)
                if code == 429 or "rate limited" in str(e):
                    self.pace = min(self.pace * 2 + 0.1, 3.0)
                    time.sleep(min(3.0 * (attempt + 1), 20))
                    if attempt >= 6:
                        self.idx += 1
                else:
                    self.idx += 1
                    time.sleep(min(1.0 * (attempt + 1), 6))
        raise RuntimeError("rpc failed: %s" % last)

    @staticmethod
    def _rate_limited(err):
        msg = str(err.get("message", "")).lower()
        return err.get("code") in (-32005, 429, -32016) or "invalid block range" in msg or "rate" in msg or "limit" in msg or "too many" in msg \
            or "over capacity" in msg

    def call_raw(self, method, params):
        self.idx = 0  # every request starts on the archive endpoint; fallbacks are per-request only
        self.id += 1
        body = self._post({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})
        if "error" in body:
            raise RuntimeError("%s %s -> %s" % (method, params, body["error"]))
        return body["result"]

    def batch(self, items):
        """items: list of (method, params). Returns list of results (or {'error':..} dicts)."""
        out = []
        self.idx = 0
        for i in range(0, len(items), self.batch_size):
            chunk = items[i:i + self.batch_size]
            payload = []
            for j, (m, p) in enumerate(chunk):
                self.id += 1
                payload.append({"jsonrpc": "2.0", "id": self.id, "method": m, "params": p})
            res = self._post(payload)
            by_id = {r["id"]: r for r in res}
            for p in payload:
                r = by_id.get(p["id"])
                if r is None:
                    raise RuntimeError("missing batch response")
                out.append(r["result"] if "result" in r else {"error": r["error"]})
        return out

    def storage_at(self, addr, slot: int, block: int) -> int:
        r = self.call_raw("eth_getStorageAt", [addr, hexword(slot), hex(block)])
        return int(r, 16)

    def storage_many(self, reads, block: int):
        """reads: list of (addr, slot int). Returns list of ints."""
        items = [("eth_getStorageAt", [a, hexword(s), hex(block)]) for a, s in reads]
        res = self.batch(items)
        out = []
        for r in res:
            if isinstance(r, dict):
                raise RuntimeError("storage read failed: %s" % r)
            out.append(int(r, 16))
        return out

    def eth_call(self, to, data, block: int, timestamp_override=None):
        params = [{"to": to, "data": data}, hex(block)]
        r = self.call_raw("eth_call", params)
        return r

    def try_call(self, to, data, block: int):
        """Returns (ok, hexdata-or-error-string)."""
        try:
            return True, self.eth_call(to, data, block)
        except RuntimeError as e:
            return False, str(e)

    def calls_many(self, items, block: int):
        """items: list of (to, data). Returns list of (ok, result)."""
        req = [("eth_call", [{"to": to, "data": data}, hex(block)]) for to, data in items]
        res = self.batch(req)
        return [(not isinstance(r, dict), r if not isinstance(r, dict) else str(r["error"])) for r in res]

    def get_logs(self, address, from_block, to_block, topics=None):
        f = {"address": address, "fromBlock": hex(from_block), "toBlock": hex(to_block)}
        if topics:
            f["topics"] = topics
        return self.call_raw("eth_getLogs", [f])

    def block(self, number):
        return self.call_raw("eth_getBlockByNumber", [hex(number) if isinstance(number, int) else number, False])

    def code(self, addr, block):
        return self.call_raw("eth_getCode", [addr, hex(block)])


def dec_words(hexdata: str):
    b = bytes.fromhex(hexdata[2:])
    return [int.from_bytes(b[i:i + 32], "big") for i in range(0, len(b) - len(b) % 32, 32)]


def to_addr(w: int) -> str:
    return "0x" + (w & ((1 << 160) - 1)).to_bytes(20, "big").hex()


def field(w: int, byte_off: int, nbytes: int) -> int:
    """Solidity packed field: byte offset from the low end, width in bytes."""
    return (w >> (8 * byte_off)) & ((1 << (8 * nbytes)) - 1)


def signed(w: int, bits=256) -> int:
    if w >= 1 << (bits - 1):
        return w - (1 << bits)
    return w


def enc_call(sig: str, *args) -> str:
    """ABI-encode static args only (uint/int/address/bytes32/bool)."""
    data = bytes.fromhex(selector(sig)[2:])
    for a in args:
        if isinstance(a, bool):
            data += w256(1 if a else 0)
        elif isinstance(a, int):
            data += w256(a % (1 << 256))
        elif isinstance(a, str) and a.startswith("0x") and len(a) == 42:
            data += addr_word(a)
        elif isinstance(a, str) and a.startswith("0x") and len(a) == 66:
            data += bytes.fromhex(a[2:])
        elif isinstance(a, bytes) and len(a) == 32:
            data += a
        else:
            raise TypeError("unsupported arg %r" % (a,))
    return "0x" + data.hex()
