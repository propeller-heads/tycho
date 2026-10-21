import sys
import json
from typing import Any

EMPTY = "0x0000000000000000000000000000000000000000"

def encode_protocol_params(protocol: dict[str, Any]) -> list[str]:
    """
    protocol_params[key]=value
    """
    parts: list[str] = []
    for k, v in protocol.items():
        if isinstance(v, list):
            for item in v:
                parts.append(f"protocol_params[{k}][]={item}")
        else:
            parts.append(f"protocol_params[{k}]={v}")
    return parts


def encode_pool(pool: dict[str, Any], index: int) -> list[str]:
    """
    pools[index][field]=value
    """
    parts: list[str] = []

    # required fields
    parts.append(f"pool_params[{index}][address]={pool['address']}")
    parts.append(f"pool_params[{index}][tx_hash]={pool['tx_hash']}")

    # optional: contracts
    for contract in pool.get("contracts", []):
        parts.append(f"pool_params[{index}][contracts][]={contract}")

    # tokens
    for token in pool["tokens"]:
        parts.append(f"pool_params[{index}][tokens][]={token}")

    # static attributes
    static_attrs = pool.get("static_attributes", {})
    static_attrs["name"] = pool["name"]
    static_attrs["factory_name"] = "NA"
    static_attrs["factory"] = EMPTY

    for k, v in static_attrs.items():
        parts.append(f"pool_params[{index}][static_attribute_keys][]={k}")
        parts.append(f"pool_params[{index}][static_attribute_vals][]={v}")

    # dynamic attributes
    for k, v in pool.get("attributes", {}).items():
        parts.append(f"pool_params[{index}][attribute_keys][]={k}")
        parts.append(f"pool_params[{index}][attribute_vals][]={v}")

    return parts

def encode_curve_params(config: dict[str, Any]) -> str:
    """Encode entire config (protocol + pools)"""
    parts: list[str] = []

    parts.extend(encode_protocol_params(config["protocol_params"]))

    for i, pool in enumerate(config["pools"]):
        parts.extend(encode_pool(pool, i))

    return "&".join(parts)

def main():
    if len(sys.argv) != 2:
        print("Usage: python encode.py <config.json>")
        sys.exit(1)

    json_file = sys.argv[1]

    with open(json_file, "r") as f:
        config = json.load(f)

    encoded = encode_curve_params(config)
    print(encoded)


if __name__ == "__main__":
    main()
