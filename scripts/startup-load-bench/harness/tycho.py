"""Requests to the target's RPC API, with the same headers a client sends."""
import json, urllib.request
from config import key

try:
    import zstandard
except ImportError:      # without zstd the API caps pages at 500: slower plan build, same plan
    zstandard = None
FETCH_PAGE = 2550 if zstandard else 500


def headers(cfg):
    h = {"Authorization": key("TYCHO_API_KEY"), "Content-Type": "application/json", "User-Agent": "startup-load-bench/1"}
    if cfg["target"]["plan_header"]:
        h["X-User-Plan"] = cfg["target"]["plan_header"]
    if zstandard:
        h["Accept-Encoding"] = "zstd"
    return h


def post(cfg, path, body, timeout=300):
    req = urllib.request.Request(cfg["target"]["url"] + path, data=json.dumps(body).encode(), method="POST", headers=headers(cfg))
    with urllib.request.urlopen(req, timeout=timeout) as r:
        raw = r.read()
        if r.headers.get("Content-Encoding") == "zstd":
            raw = zstandard.ZstdDecompressor().decompressobj().decompress(raw)
    return json.loads(raw)


def health(cfg, timeout=30):
    """(seconds, http status) of one health probe."""
    import time, urllib.error
    h = {k: v for k, v in headers(cfg).items() if k in ("Authorization", "X-User-Plan")}
    h["Connection"] = "close"
    t = time.time()
    try:
        with urllib.request.urlopen(urllib.request.Request(cfg["target"]["url"] + "/v1/health", headers=h), timeout=timeout) as r:
            r.read()
            return time.time() - t, r.status
    except urllib.error.HTTPError as e:
        return time.time() - t, e.code
    except Exception:
        return time.time() - t, 0
