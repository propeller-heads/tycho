#!/usr/bin/python
import json
import os
import re
import time
import urllib.request

# Exports contract ABI in JSON

abis = {
    # Factories
    "CryptoSwapRegistry": "0x9a32aF1A11D9c937aEa61A3790C2983257eA8Bc0",
    "MainRegistry": "0x90E00ACe148ca3b23Ac1bC8C240C2a7Dd9c2d7f5",
    "MetaPoolFactory": "0xB9fC157394Af804a3578134A6585C0dc9cc990d4",
    "CryptoPoolFactory": "0xF18056Bbd320E96A48e3Fbf8bC061322531aac99",
    "TwocryptoFactory": "0x98EE851a00abeE0d95D08cF4CA2BdCE32aeaAF7F",
    # pool
    "Pool": "0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7",
    "3Pool": "0x5F890841f657d90E081bAbdB532A05996Af79Fe6",
}

ABI_ENDPOINT = (
    "https://api.etherscan.io/api?module=contract&action=getabi&address={address}"
)

if etherscan_key := os.environ.get("ETHERSCAN_API_TOKEN"):
    print("API KEY Loaded!")
    ABI_ENDPOINT += f"&apikey={etherscan_key}"


def __main__():
    for name, addr in abis.items():
        normalized_name = "_".join(re.findall(r"[A-Z]+[a-z]*", name)).lower()
        print(f"Getting ABI for {name} at {addr} ({normalized_name})")

        try:
            with urllib.request.urlopen(ABI_ENDPOINT.format(address=addr)) as response:
                response_json = json.loads(response.read().decode())
                abi_json = json.loads(response_json["result"])
                result = json.dumps(abi_json, indent=4, sort_keys=True)
                with open(f"{normalized_name}.json", "w") as f:
                    f.write(result)
        except Exception as err:
            print(response.content)
            raise err
        time.sleep(0.25)


if __name__ == "__main__":
    __main__()
