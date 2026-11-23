import json
from unittest.mock import patch, Mock

from tycho_indexer_client.dto import (
    ProtocolComponent,
    ResponseProtocolState,
    ResponseAccount,
    ProtocolComponentsParams,
    ProtocolStateParams,
    ContractStateParams,
    TokensParams,
    ResponseToken,
)
from tycho_indexer_client.rpc_client import TychoRPCClient


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
    params = ContractStateParams(protocol_system="vm:balancer_v2")
    result = client.get_contract_state(params)

    mock_post.assert_called_once_with(
        "http://0.0.0.0:4242/v1/contract_state",
        headers={"accept": "application/json", "Content-Type": "application/json"},
        data='{"protocol_system": "vm:balancer_v2", "chain": "ethereum"}',
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
