import json
from typing import Any

PARAMETERS = "params.json"
EMPTY = "0x0000000000000000000000000000000000000000"


def encode_erc4626_params(params: list[dict[str, Any]]) -> str:
    encoded_params = []

    for i, param in enumerate(params):
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

            encoded_param = (
                f"{encoded_address}&"
                f"{encoded_tx}&"
                f"{encoded_asset}&"
                f"{encoded_block}&"
                f"{encoded_static_attributes}"
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
