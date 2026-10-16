import json
from unittest.mock import patch, Mock

import pytest
import requests
from pydantic import ValidationError

from tycho_indexer_client.dto import (
    BlockChanges,
    DeltaCatchUp,
    DeltaCatchUpParams,
    DeltaCatchUpUnavailable,
    HexBytes,
    ProtocolComponent,
    ResponseProtocolState,
    ResponseAccount,
    ProtocolComponentsParams,
    ProtocolStateParams,
    ContractStateParams,
    TokensParams,
    ResponseToken,
)
from tycho_indexer_client.rpc_client import TychoRPCClient, TychoRPCError


@patch("requests.post")
def test_get_protocol_components_returns_expected_result(mock_post, asset_dir):
    with (asset_dir / "rpc" / "protocol_components.json").open("r") as fp:
        data = json.load(fp)

    mock_response = Mock()
    mock_response.json.return_value = data
    mock_post.return_value = mock_response

    client = TychoRPCClient()
    params = ProtocolComponentsParams(protocol_system="uniswap_v2")
    result = client.get_protocol_components(params)

    mock_post.assert_called_once_with(
        "http://0.0.0.0:4242/v1/protocol_components",
        headers={"accept": "application/json", "Content-Type": "application/json"},
        data='{"protocol_system": "uniswap_v2", "chain": "ethereum"}',
        params={},
    )
    assert isinstance(result.protocol_components, list)
    assert isinstance(result.protocol_components[0], ProtocolComponent)
    assert len(result.protocol_components) == 2


@patch("requests.post")
def test_get_protocol_state_returns_expected_result(mock_post, asset_dir):
    with (asset_dir / "rpc" / "protocol_state.json").open("r") as fp:
        data = json.load(fp)

    mock_response = Mock()
    mock_response.json.return_value = data
    mock_post.return_value = mock_response

    client = TychoRPCClient()
    params = ProtocolStateParams()
    result = client.get_protocol_state(params)

    mock_post.assert_called_once_with(
        "http://0.0.0.0:4242/v1/protocol_state",
        headers={"accept": "application/json", "Content-Type": "application/json"},
        data='{"include_balances": true, "chain": "ethereum"}',
        params={},
    )
    assert isinstance(result.states, list)
    assert isinstance(result.states[0], ResponseProtocolState)
    assert len(result.states) == 1

    assert result.states[0].component_id == "0xe96a45f66bdda121b24f0a861372a72e8889523d"


@patch("requests.post")
def test_get_contract_state_returns_expected_result(mock_post, asset_dir):
    with (asset_dir / "rpc" / "contract_state.json").open("r") as fp:
        data = json.load(fp)

    mock_response = Mock()
    mock_response.json.return_value = data
    mock_post.return_value = mock_response

    client = TychoRPCClient()
    params = ContractStateParams()
    result = client.get_contract_state(params)

    mock_post.assert_called_once_with(
        "http://0.0.0.0:4242/v1/contract_state",
        headers={"accept": "application/json", "Content-Type": "application/json"},
        data='{"chain": "ethereum"}',
        params={},
    )

    assert isinstance(result.accounts, list)
    assert isinstance(result.accounts[0], ResponseAccount)
    assert len(result.accounts[0].slots) > 0


@patch("requests.post")
def test_get_tokens_returns_expected_result(mock_post, asset_dir):
    with (asset_dir / "rpc" / "tokens.json").open("r") as fp:
        data = json.load(fp)

    mock_response = Mock()
    mock_response.json.return_value = data
    mock_post.return_value = mock_response

    client = TychoRPCClient()
    params = TokensParams(min_quality=10, traded_n_days_ago=30)
    result = client.get_tokens(params)

    mock_post.assert_called_once_with(
        "http://0.0.0.0:4242/v1/tokens",
        headers={"accept": "application/json", "Content-Type": "application/json"},
        data='{"min_quality": 10, "traded_n_days_ago": 30, "chain": "ethereum"}',
        params={},
    )
    assert isinstance(result.tokens, list)
    assert isinstance(result.tokens[0], ResponseToken)
    assert len(result.tokens) == len(data["tokens"])


DELTA_CATCH_UP_URL = "http://0.0.0.0:4242/v1/delta_catch_up"


def delta_catch_up_params(**overrides) -> DeltaCatchUpParams:
    """A client that applied block 3 on a branch a revert to block 2 orphaned, and whose new
    subscription continues after block 3' (0xa3...)."""
    fields = dict(
        protocol_system="vm:balancer_v2",
        from_block_hash="0x" + "03" * 32,
        from_block_number=3,
        to_block_hash="0x" + "a3" * 32,
        fingerprint="0xabcdef",
    )
    fields.update(overrides)
    return DeltaCatchUpParams(**fields)


def http_response(status_code: int, body: str) -> requests.Response:
    response = requests.Response()
    response.status_code = status_code
    response._content = body.encode()
    response.url = DELTA_CATCH_UP_URL
    return response


@patch("requests.post")
def test_get_delta_catch_up_sends_the_request_body(mock_post):
    mock_post.return_value = http_response(
        200, '{"status": "available", "messages": []}'
    )

    TychoRPCClient().get_delta_catch_up(delta_catch_up_params(include_state=False))

    mock_post.assert_called_once()
    args, kwargs = mock_post.call_args
    assert args == (DELTA_CATCH_UP_URL,)
    assert kwargs["params"] == {}
    # The server rejects unknown fields, so the body must hold exactly these.
    assert json.loads(kwargs["data"]) == {
        "chain": "ethereum",
        "protocol_system": "vm:balancer_v2",
        "from_block_hash": "0x" + "03" * 32,
        "from_block_number": 3,
        "to_block_hash": "0x" + "a3" * 32,
        "fingerprint": "0xabcdef",
        "include_state": False,
    }


def test_delta_catch_up_params_include_state_by_default():
    assert delta_catch_up_params().include_state is True


@patch("requests.post")
def test_get_delta_catch_up_returns_the_missed_messages(mock_post, asset_dir):
    body = (asset_dir / "rpc" / "delta_catch_up_available.json").read_text()
    mock_post.return_value = http_response(200, body)

    result = TychoRPCClient().get_delta_catch_up(delta_catch_up_params())

    assert isinstance(result, DeltaCatchUp.Available)
    assert result.status == "available"
    revert, block = result.messages

    assert revert.revert
    assert revert.block.hash == HexBytes("0x" + "02" * 32)
    pool = "0x" + "b1" * 20
    assert revert.state_updates[pool].updated_attributes == {
        "reserve0": HexBytes("0x0de0b6b3a7640000")
    }
    assert revert.state_updates[pool].deleted_attributes == {"paused"}
    assert list(revert.deleted_protocol_components) == ["0x" + "c3" * 20]

    assert not block.revert
    assert block.block.parent_hash == revert.block.hash
    assert block.block.hash == HexBytes("0x" + "a3" * 32)
    # The block message holds every field; test_decode checks how it parses.
    with (asset_dir / "block_changes.json").open("r") as fp:
        assert block == BlockChanges(**json.load(fp))


@pytest.mark.parametrize(
    "sent, expected",
    [
        ("fingerprint_changed", DeltaCatchUpUnavailable.fingerprint_changed),
        ("to_not_in_window", DeltaCatchUpUnavailable.to_not_in_window),
        # A reason added to the server after this client.
        ("window_too_busy", DeltaCatchUpUnavailable.unknown),
    ],
)
@patch("requests.post")
def test_get_delta_catch_up_reports_why_it_is_unavailable(mock_post, sent, expected):
    mock_post.return_value = http_response(
        200, json.dumps({"status": "unavailable", "reason": sent})
    )

    result = TychoRPCClient().get_delta_catch_up(delta_catch_up_params())

    assert result == DeltaCatchUp.Unavailable(reason=expected)
    assert result.status == "unavailable"


@patch("requests.post")
def test_get_delta_catch_up_rejects_an_unknown_status(mock_post):
    mock_post.return_value = http_response(200, '{"status": "partial", "reason": "x"}')

    with pytest.raises(ValidationError):
        TychoRPCClient().get_delta_catch_up(delta_catch_up_params())


@patch("requests.post")
def test_get_delta_catch_up_from_a_server_without_the_endpoint_is_unsupported(
    mock_post,
):
    mock_post.return_value = http_response(404, "")

    result = TychoRPCClient().get_delta_catch_up(delta_catch_up_params())

    assert result == DeltaCatchUp.Unsupported()
    assert result.status == "unsupported"


@patch("requests.post")
def test_get_delta_catch_up_of_an_unknown_extractor_raises(mock_post):
    mock_post.return_value = http_response(
        400, "Unknown extractor: ethereum:vm:balancer_v2"
    )

    with pytest.raises(TychoRPCError) as error:
        TychoRPCClient().get_delta_catch_up(delta_catch_up_params())

    assert error.value.status_code == 400
    assert error.value.response_data == {
        "error": "Unknown extractor: ethereum:vm:balancer_v2"
    }
