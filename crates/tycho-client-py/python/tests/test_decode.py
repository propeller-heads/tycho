import json

from hexbytes import HexBytes

from tycho_indexer_client.dto import (
    Chain,
    ContractId,
    ContractStateParams,
    CustomChain,
    ExtractorIdentity,
    FeedMessage,
    SynchronizerState,
    SynchronizerStateEnum,
    Header,
)


def test_decode_snapshot(asset_dir):
    with (asset_dir / "snapshot.json").open("r") as fp:
        data = json.load(fp)

    msg = FeedMessage(**data)

    assert msg.sync_states["uniswap_v2"] == SynchronizerState(
        status=SynchronizerStateEnum.ready,
        header=Header(
            number=20440688,
            hash="0xc30593d774626d6175394d7d33c74bd40e7ddd2d2891eab0cc54032eafe15b98",
            parent_hash="0x954db41ab1ef9c32457b52c170d83f71f82a065ba0b4eeda8efb2da4b9e18162",
            revert=False,
        ),
    )


def test_decode_deltas(asset_dir):
    with (asset_dir / "deltas.json").open("r") as fp:
        data = json.load(fp)

    msg = FeedMessage(**data)

    assert msg.sync_states["uniswap_v2"] == SynchronizerState(
        status=SynchronizerStateEnum.ready,
        header=Header(
            number=20440962,
            hash="0x00d378b96546bc5ec4c64e225d2bb716867fd837d309bc79c5db321ecb1357ae",
            parent_hash="0x05d53b6b3b127ae642c409aeda425de3e5e3e444109d26cc54ca740be9a60316",
            revert=False,
        ),
    )


# JSON shaped exactly as Tycho serialises `Chain::Custom("name")`: externally tagged with the chain
# name as a string. If the Rust wire format changes, this fixture must change with it.
CUSTOM_CHAIN_JSON = {"custom": "testchain"}


def test_decode_extractor_identity_known_chain():
    # Known chains serialise as a bare lowercase string.
    ident = ExtractorIdentity(chain="ethereum", name="uniswap_v2")

    assert ident.chain == Chain.ethereum


def test_decode_extractor_identity_custom_chain():
    ident = ExtractorIdentity(chain=CUSTOM_CHAIN_JSON, name="my_extractor")

    assert isinstance(ident.chain, CustomChain)
    assert ident.chain == CustomChain(name="testchain")
    assert ident.chain.name == "testchain"
    assert str(ident.chain) == "testchain"


def test_decode_contract_state_params_backward_compatibility():
    data_new = {"contract_ids": ['0xba12222222228d8ba445958a75a0704d566bf2c8', '0xe96a45f66bdda121b24f0a861372a72e8889523d']}
    msg_new = ContractStateParams(**data_new)
    
    data_old = {"contract_ids": [ContractId(chain=Chain.ethereum, address=HexBytes('0xba12222222228d8ba445958a75a0704d566bf2c8')), ContractId(chain=Chain.ethereum, address=HexBytes('0xe96a45f66bdda121b24f0a861372a72e8889523d'))]}
    msg_old = ContractStateParams(**data_old)

    assert msg_new == ContractStateParams(
        contract_ids = ['0xba12222222228d8ba445958a75a0704d566bf2c8', '0xe96a45f66bdda121b24f0a861372a72e8889523d'])
    assert msg_old == ContractStateParams(
        contract_ids = ['0xba12222222228d8ba445958a75a0704d566bf2c8', '0xe96a45f66bdda121b24f0a861372a72e8889523d'])