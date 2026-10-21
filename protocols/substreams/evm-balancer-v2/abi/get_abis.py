#!/usr/bin/python
import json
import os
import re
import time
import urllib.request

# Exports contract ABI in JSON

abis = {
    # Factories
    "WeightedPoolFactoryV1": "0x8E9aa87E45e92bad84D5F8DD1bff34Fb92637dE9",
    "WeightedPoolFactoryV2": "0xcC508a455F5b0073973107Db6a878DdBDab957bC",
    "WeightedPoolFactoryV3": "0x5Dd94Da3644DDD055fcf6B3E1aa310Bb7801EB8b",
    "WeightedPoolFactoryV4": "0x897888115Ada5773E02aA29F775430BFB5F34c51",
    "WeightedPool2TokensFactory": "0xA5bf2ddF098bb0Ef6d120C98217dD6B141c74EE0",  # 80Bal-20WETH
    "ComposableStablePoolFactory (v5)": "0xDB8d758BCb971e482B2C45f7F8a7740283A1bd3A",
    "ERC4626LinearPoolFactory (v4)": "0x813EE7a840CE909E7Fea2117A44a90b8063bd4fd",
    "EulerLinearPoolFactory": "0x5F43FBa61f63Fa6bFF101a0A0458cEA917f6B347",
    # "GearboxLinearPoolFactory (v2)": "0x39A79EB449Fc05C92c39aA6f0e9BfaC03BE8dE5B",
    "ManagedPoolFactory (v2)": "0xBF904F9F340745B4f0c4702c7B6Ab1e808eA6b93",
    "SiloLinearPoolFactory (v2)": "0x4E11AEec21baF1660b1a46472963cB3DA7811C89",
    "YearnLinearPoolFactory (v2)": "0x5F5222Ffa40F2AEd6380D022184D6ea67C776eE0",
    # Vault
    "Vault": "0xBA12222222228d8Ba445958a75a0704d566BF2C8",
}

ABI_ENDPOINT = (
    "https://api.etherscan.io/api?module=contract&action=getabi&address={address}"
)

if etherscan_key := os.environ.get("ETHERSCAN_API_TOKEN"):
    print("API KEY Loaded!")
    ABI_ENDPOINT += f"&apikey={etherscan_key}"


def __main__():
    for name, addr in abis.items():
        normalized_name = "_".join(re.findall(r"[A-Z]+[a-z]*\d*", name)).lower()
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
