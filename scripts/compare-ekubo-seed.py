"""Compare the Ekubo v3 state of two Tycho instances at one block.

Meant for checking a Tycho synced from an `ethereum-ekubo-v3-seed` seed against one synced from the protocol's
first block: components, static attributes and entity attributes must agree, while creation
metadata and balances differ by design and are only reported.

    python scripts/compare-ekubo-seed.py --left http://localhost:4242/v1 \
        --right http://localhost:4243/v1 --block 25900000

Attribute values are compared as integers. `tick/*`, `rate_delta/*` and `liquidity` are signed,
everything else unsigned; an empty value counts as zero. `rate_delta/*/<t>` entries with
`t <= last_time` are dropped before comparing: the stock package keeps them after the chain has
consumed the time, a seed never has them.
"""

import argparse
import logging as log
import os
import sys
from typing import Any

import requests

PROTOCOL_SYSTEM = "ekubo_v3"
SIGNED_PREFIXES = ("tick/", "rate_delta/", "orders/", "ticks/")
SIGNED_NAMES = {"liquidity"}


class TychoClient:
    def __init__(self, base_url: str, chain: str, auth_token: str):
        self.base_url = base_url.rstrip("/")
        self.chain = chain
        self.headers = {
            "Content-Type": "application/json",
            "Accept": "application/json",
            "Authorization": auth_token,
        }

    def _post(self, path: str, payload: dict[str, Any]) -> dict[str, Any]:
        response = requests.post(
            f"{self.base_url}/{path}", json=payload, headers=self.headers
        )
        response.raise_for_status()
        return response.json()

    def components(self) -> dict[str, dict[str, Any]]:
        components: dict[str, dict[str, Any]] = {}
        page, page_size = 0, 500
        while True:
            data = self._post(
                "protocol_components",
                {
                    "chain": self.chain,
                    "protocol_system": PROTOCOL_SYSTEM,
                    "pagination": {"page": page, "page_size": page_size},
                },
            )
            results = data.get("protocol_components", [])
            for component in results:
                components[component["id"].lower()] = component
            if len(results) < page_size:
                return components
            page += 1

    def states(self, block: int) -> dict[str, dict[str, Any]]:
        states: dict[str, dict[str, Any]] = {}
        page, page_size = 0, 100
        while True:
            data = self._post(
                "protocol_state",
                {
                    "chain": self.chain,
                    "protocol_system": PROTOCOL_SYSTEM,
                    "include_balances": True,
                    "version": {"block": {"chain": self.chain, "number": block}},
                    "pagination": {"page": page, "page_size": page_size},
                },
            )
            results = data.get("states", [])
            for state in results:
                states[state["component_id"].lower()] = state
            if len(results) < page_size:
                return states
            page += 1


def as_int(name: str, value: str) -> int:
    raw = bytes.fromhex(value[2:] if value.startswith("0x") else value)
    if not raw:
        return 0
    signed = name in SIGNED_NAMES or name.startswith(SIGNED_PREFIXES)
    return int.from_bytes(raw, byteorder="big", signed=signed)


def normalized_attributes(attributes: dict[str, str]) -> dict[str, int]:
    values = {name: as_int(name, value) for name, value in attributes.items()}
    last_time = values.get("last_time", values.get("last_execution_time"))
    if last_time is None:
        return values
    return {
        name: value
        for name, value in values.items()
        if not (
            name.startswith(("rate_delta/", "orders/"))
            and int(name.rsplit("/", 1)[1]) <= last_time
        )
    }


def relative_deviation(left: int, right: int) -> float:
    if left == right:
        return 0.0
    return abs(left - right) / max(abs(left), abs(right))


def compare(left: TychoClient, right: TychoClient, block: int) -> int:
    left_components, right_components = left.components(), right.components()
    left_states, right_states = left.states(block), right.states(block)

    mismatches = 0
    only_left = set(left_components) - set(right_components)
    only_right = set(right_components) - set(left_components)
    for component_id in sorted(only_left):
        log.error(f"{component_id}: only on the left")
    for component_id in sorted(only_right):
        log.error(f"{component_id}: only on the right")
    mismatches += len(only_left) + len(only_right)

    balance_deviations: list[tuple[float, str, str]] = []
    for component_id in sorted(set(left_components) & set(right_components)):
        lc, rc = left_components[component_id], right_components[component_id]
        if sorted(t.lower() for t in lc["tokens"]) != sorted(
            t.lower() for t in rc["tokens"]
        ):
            log.error(f"{component_id}: tokens differ: {lc['tokens']} vs {rc['tokens']}")
            mismatches += 1
        left_static = {k: v.lower() for k, v in lc["static_attributes"].items()}
        right_static = {k: v.lower() for k, v in rc["static_attributes"].items()}
        if left_static != right_static:
            log.error(
                f"{component_id}: static attributes differ: {left_static} vs {right_static}"
            )
            mismatches += 1
        if lc.get("creation_tx") != rc.get("creation_tx"):
            log.info(f"{component_id}: creation_tx differs (expected for a seeded run)")

        ls, rs = left_states.get(component_id), right_states.get(component_id)
        if ls is None or rs is None:
            log.error(f"{component_id}: state missing on {'left' if ls is None else 'right'}")
            mismatches += 1
            continue

        left_attrs = normalized_attributes(ls["attributes"])
        right_attrs = normalized_attributes(rs["attributes"])
        for name in sorted(set(left_attrs) | set(right_attrs)):
            lv, rv = left_attrs.get(name), right_attrs.get(name)
            if lv != rv:
                log.error(f"{component_id}: {name}: {lv} vs {rv}")
                mismatches += 1

        for token in sorted(set(ls["balances"]) | set(rs["balances"])):
            lb = as_int(token, ls["balances"].get(token, "0x"))
            rb = as_int(token, rs["balances"].get(token, "0x"))
            balance_deviations.append((relative_deviation(lb, rb), component_id, token))

    balance_deviations.sort(reverse=True)
    for deviation, component_id, token in balance_deviations[:20]:
        log.info(f"balance deviation {deviation:.4%}: {component_id} {token}")
    if balance_deviations:
        mean = sum(d for d, _, _ in balance_deviations) / len(balance_deviations)
        log.info(f"mean balance deviation over {len(balance_deviations)} balances: {mean:.4%}")

    log.info(
        f"compared {len(set(left_components) & set(right_components))} components at block "
        f"{block}: {mismatches} mismatches"
    )
    return mismatches


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--left", required=True, help="Base URL of the first Tycho, e.g. http://localhost:4242/v1")
    parser.add_argument("--right", required=True, help="Base URL of the second Tycho")
    parser.add_argument("--block", type=int, required=True, help="Block to compare the state at")
    parser.add_argument("--chain", default="ethereum")
    parser.add_argument(
        "--auth-token", default=os.getenv("TYCHO_AUTH_TOKEN", "sampletoken")
    )
    args = parser.parse_args()

    log.basicConfig(level=log.INFO, format="%(levelname)s %(message)s")
    left = TychoClient(args.left, args.chain, args.auth_token)
    right = TychoClient(args.right, args.chain, args.auth_token)
    return 1 if compare(left, right, args.block) else 0


if __name__ == "__main__":
    sys.exit(main())
