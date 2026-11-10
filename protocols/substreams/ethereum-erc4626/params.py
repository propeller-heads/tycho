import json
from typing import Any

PARAMETERS = "params.json"
EMPTY = "0x0000000000000000000000000000000000000000"
VAULT_KEYS = {"name", "address", "tx_hash", "asset", "block", "extra_entrypoints"}
ENTRYPOINT_KEYS = {"signature", "calldata"}


def reject_unknown_keys(entry: dict[str, Any], known: set[str], where: str) -> None:
    unknown = entry.keys() - known
    if unknown:
        raise ValueError(
            f"Unknown key(s) {sorted(unknown)} in {where} of {PARAMETERS}; "
            f"expected only {sorted(known)}"
        )


def encode_erc4626_params(params: list[dict[str, Any]]) -> str:
    encoded_params = []

    for i, param in enumerate(params):
        reject_unknown_keys(param, VAULT_KEYS, f"vault `{i}`")
        try:
            name: str = param["name"]
            address: str = param["address"]
            tx_hash: str = param["tx_hash"]
            asset: str = param["asset"]
            block: int = param["block"]

            # static attributes
            static_attributes = {
                "name": name,
            }

            encoded_address = f"address={address}"
            encoded_tx = f"tx_hash={tx_hash}"
            encoded_asset = f"asset={asset}"
            encoded_block = f"block={block}"

            encoded_static_attributes = "&".join(
                [
                    f"static_attribute_keys[]={key}&static_attribute_vals[]={value}"
                    for key, value in static_attributes.items()
                ]
            )

            extra_entrypoints = param.get("extra_entrypoints", [])
            for j, entrypoint in enumerate(extra_entrypoints):
                reject_unknown_keys(
                    entrypoint, ENTRYPOINT_KEYS, f"vault `{i}` extra entrypoint `{j}`"
                )
            encoded_extra_entrypoints = "&".join(
                [
                    f"extra_entrypoints[{j}][signature]={entrypoint['signature']}"
                    f"&extra_entrypoints[{j}][calldata]={entrypoint['calldata']}"
                    for j, entrypoint in enumerate(extra_entrypoints)
                ]
            )

            encoded_param = (
                f"{encoded_address}&"
                f"{encoded_tx}&"
                f"{encoded_asset}&"
                f"{encoded_block}&"
                f"{encoded_static_attributes}&"
                f"{encoded_extra_entrypoints}"
            ).rstrip("&")

            encoded_params.append(encoded_param)

        except KeyError as err:
            raise KeyError(
                f"Missing key in {PARAMETERS}.\n"
                f"Index `{i}` missing required field.\n\n"
                f"Missing: {err.args[0]}"
            )

    return "#".join(encoded_params)


def main():
    with open(PARAMETERS, "r") as f:
        params = json.load(f)

    print(encode_erc4626_params(params))


if __name__ == "__main__":
    main()
