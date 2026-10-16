import json

import pytest
from hexbytes import HexBytes
from pydantic import ValidationError

from tycho_indexer_client.dto import (
    AccountBalance,
    BlockChanges,
    Chain,
    ContractId,
    ContractStateParams,
    CustomChain,
    ExtractorIdentity,
    FeedMessage,
    RPCTracerParams,
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


def test_decode_extractor_identity_arc_round_trip():
    ident = ExtractorIdentity(chain="arc", name="uniswap_v4")

    assert ident.chain == Chain.arc
    assert ident.json() == '{"chain": "arc", "name": "uniswap_v4"}'
    assert ExtractorIdentity.parse_raw(ident.json()) == ident


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


def test_decode_block_changes(asset_dir):
    # Serialised from the Rust `BlockAggregatedChanges` DTO.
    with (asset_dir / "block_changes.json").open("r") as fp:
        data = json.load(fp)

    msg = BlockChanges(**data)

    pool = "0x" + "b1" * 20
    vault, weth = HexBytes("0x" + "d4" * 20), HexBytes("0x" + "c0" * 20)
    assert msg.account_balances[vault][weth] == AccountBalance(
        account=vault,
        token=weth,
        balance="0x0de0b6b3a7640000",
        modify_tx="0x" + "e4" * 32,
    )
    assert msg.partial_block_index is None
    entry_point_id = "0x" + "f5" * 20 + ":getRate()"
    dci = msg.dci_update
    assert [ep.external_id for ep in dci.new_entrypoints[pool]] == [entry_point_id]
    [(tracer, component)] = dci.new_entrypoint_params[entry_point_id]
    assert component == pool
    assert tracer.calldata == HexBytes("0x679aefce")
    diff = tracer.state_overrides[HexBytes("0x" + "f5" * 20)].slots
    assert diff.diff == {HexBytes("0x" + "00" * 32): HexBytes("0x" + "01" * 32)}
    assert diff.replace is None
    assert tracer.state_overrides[HexBytes("0x" + "f6" * 20)].slots.replace == {}
    [(address, location)] = dci.trace_results[entry_point_id].retriggers
    assert address == HexBytes("0x" + "f5" * 20)
    assert location.offset == 12
    assert dci.trace_results[entry_point_id].accessed_slots == {
        HexBytes("0x" + "f5" * 20): {HexBytes("0x" + "00" * 32)}
    }


def test_decode_partial_block_changes(asset_dir):
    with (asset_dir / "block_changes.json").open("r") as fp:
        data = json.load(fp)

    msg = BlockChanges(**data, partial_block_index=2)

    assert msg.partial_block_index == 2



def test_decode_tracing_params_of_an_unknown_method():
    with pytest.raises(ValidationError):
        RPCTracerParams(method="other", calldata="0x00")
