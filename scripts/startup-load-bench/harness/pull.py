#!/usr/bin/env python3
"""Copies the plans and wave results out of the runner pod while it runs or after it is done.

usage: pull.py <local dir> [--once]      default: pull every 2 minutes until interrupted
       pull.py <local dir> --url http://host:port     (no port-forward; e.g. a local http.server)

Opens a port-forward to the pod's file server (needs only get + portforward on pods, which the
read-only debug role has) and fetches every file under load/ that is new or changed size.
"""
import html.parser, os, subprocess, sys, time, urllib.request

NS, POD, PORT = "prod-tycho-bench", "tycho-bench", 8000


class Links(html.parser.HTMLParser):
    def __init__(self):
        super().__init__(); self.hrefs = []

    def handle_starttag(self, tag, attrs):
        if tag == "a":
            self.hrefs += [v for k, v in attrs if k == "href"]


def listing(base, path):
    p = Links(); p.feed(urllib.request.urlopen(base + path, timeout=30).read().decode())
    return [h for h in p.hrefs if not h.startswith("?") and not h.startswith("/")]


def walk(base, path=""):
    for h in listing(base, "/" + path):
        if h.endswith("/"):
            yield from walk(base, path + h)
        elif not h.endswith(".tmp") and not h.endswith(".lg"):
            yield path + h


def pull(base, dest):
    got = 0
    for rel in walk(base):
        local = os.path.join(dest, rel)
        size = int(urllib.request.urlopen(urllib.request.Request(base + "/" + rel, method="HEAD"), timeout=30).headers["Content-Length"])
        if os.path.exists(local) and os.path.getsize(local) == size:
            continue
        os.makedirs(os.path.dirname(local), exist_ok=True)
        with urllib.request.urlopen(base + "/" + rel, timeout=600) as r, open(local + ".part", "wb") as f:
            while chunk := r.read(1 << 20):
                f.write(chunk)
        os.replace(local + ".part", local)
        got += 1
        print(f"pulled {rel} ({size / 1e6:.1f} MB)", flush=True)
    return got


def main():
    args = sys.argv[1:]
    dest = args[0]
    once = "--once" in args
    url = args[args.index("--url") + 1] if "--url" in args else None
    while True:
        pf = None
        try:
            if url:
                base = url
            else:
                local_port = 18000 + os.getpid() % 1000
                pf = subprocess.Popen(["kubectl", "-n", NS, "port-forward", f"pod/{POD}", f"{local_port}:{PORT}"],
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                time.sleep(3)
                base = f"http://127.0.0.1:{local_port}"
            n = pull(base, dest)
            print(time.strftime("%H:%M:%S"), f"{n} new file(s)", flush=True)
        except Exception as e:
            print(time.strftime("%H:%M:%S"), "pull failed:", e, flush=True)
        finally:
            if pf:
                pf.terminate()
        if once:
            break
        time.sleep(120)


if __name__ == "__main__":
    main()
