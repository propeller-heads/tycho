import json
from typing import Optional, Union

import requests

from .dto import (
    Chain,
    CustomChainConfig,
    ProtocolComponentsParams,
    ProtocolStateParams,
    ContractStateParams,
    TokensParams,
    HexBytes,
    ProtocolSystemsParams,
    ComponentTvlParams,
    TracedEntryPointParams,
    ProtocolSystemsResponse,
    ComponentTvlResponse,
    ProtocolComponentsResponse,
    ProtocolStateResponse,
    ContractStateResponse,
    TokensResponse,
    TracedEntryPointsResponse,
    PaginationParams,
)


class HexBytesEncoder(json.JSONEncoder):
    def default(self, obj):
        if isinstance(obj, HexBytes):
            return obj.hex()
        return super().default(obj)


class TychoRPCError(Exception):
    """Custom exception for Tycho RPC errors."""

    def __init__(
        self,
        message: str,
        status_code: Optional[int] = None,
        response_data: Optional[dict] = None,
    ):
        self.message = message
        self.status_code = status_code
        self.response_data = response_data
        super().__init__(self.message)


class TychoRPCClient:
    """
    A client for interacting with the Tycho RPC server.

    This client provides methods to interact with all available Tycho RPC endpoints.
    """

    def __init__(
        self,
        rpc_url: str = "http://0.0.0.0:4242",
        auth_token: str = None,
        chain: Union[Chain, CustomChainConfig] = Chain.ethereum,
    ):
        """
        Initialize the Tycho RPC client.

        Args:
            rpc_url: The base URL of the Tycho RPC server
            auth_token: Optional authentication token
            chain: The default chain to use for requests
        """
        self.rpc_url = rpc_url.rstrip("/")
        self._headers = {
            "accept": "application/json",
            "Content-Type": "application/json",
        }
        if auth_token:
            self._headers["Authorization"] = auth_token
        self._chain = chain

    def _post_request(
        self, endpoint: str, params: dict = None, body: dict = None
    ) -> dict:
        """
        Sends a POST request to the given endpoint and returns the response.

        Args:
            endpoint: The API endpoint path
            params: Optional query parameters
            body: Optional request body

        Returns:
            The JSON response from the server

        Raises:
            TychoRPCError: If the request fails or returns an error
        """
        url = f"{self.rpc_url}{endpoint}"

        # Convert to JSON strings to cast booleans to strings
        if body is not None:
            body = json.dumps(body, cls=HexBytesEncoder)
        if params:
            params = json.dumps(params, cls=HexBytesEncoder)

        try:
            response = requests.post(
                url, headers=self._headers, data=body or "{}", params=params or {}
            )
            response.raise_for_status()
            return response.json()
        except requests.exceptions.RequestException as e:
            status_code = (
                getattr(e.response, "status_code", None)
                if hasattr(e, "response")
                else None
            )
            response_data = None
            if hasattr(e, "response") and e.response is not None:
                try:
                    response_data = e.response.json()
                except:
                    response_data = {"error": e.response.text}

            raise TychoRPCError(
                f"Request failed: {str(e)}",
                status_code=status_code,
                response_data=response_data,
            )

    def _get_request(self, endpoint: str, params: dict = None) -> dict:
        """
        Sends a GET request to the given endpoint and returns the response.

        Args:
            endpoint: The API endpoint path
            params: Optional query parameters

        Returns:
            The JSON response from the server

        Raises:
            TychoRPCError: If the request fails or returns an error
        """
        url = f"{self.rpc_url}{endpoint}"

        try:
            response = requests.get(url, headers=self._headers, params=params or {})
            response.raise_for_status()
            return response.json()
        except requests.exceptions.RequestException as e:
            status_code = (
                getattr(e.response, "status_code", None)
                if hasattr(e, "response")
                else None
            )
            response_data = None
            if hasattr(e, "response") and e.response is not None:
                try:
                    response_data = e.response.json()
                except:
                    response_data = {"error": e.response.text}

            raise TychoRPCError(
                f"Request failed: {str(e)}",
                status_code=status_code,
                response_data=response_data,
            )

    def get_protocol_components(
        self, params: ProtocolComponentsParams
    ) -> ProtocolComponentsResponse:
        """
        Get protocol components based on the provided parameters.

        Args:
            params: Parameters for filtering protocol components

        Returns:
            Protocol components response with pagination
        """
        params_dict = params.dict(exclude_none=True)
        params_dict["chain"] = self._chain

        res = self._post_request("/v1/protocol_components", body=params_dict)
        return ProtocolComponentsResponse(**res)

    def get_protocol_state(self, params: ProtocolStateParams) -> ProtocolStateResponse:
        """
        Get protocol state based on the provided parameters.

        Args:
            params: Parameters for filtering protocol state

        Returns:
            Protocol state response with pagination
        """
        params_dict = params.dict(exclude_none=True)
        params_dict["chain"] = self._chain

        res = self._post_request("/v1/protocol_state", body=params_dict)
        return ProtocolStateResponse(**res)

    def get_contract_state(self, params: ContractStateParams) -> ContractStateResponse:
        """
        Get contract state based on the provided parameters.

        Args:
            params: Parameters for filtering contract state

        Returns:
            Contract state response with pagination
        """
        params_dict = params.dict(exclude_none=True)
        params_dict["chain"] = self._chain

        res = self._post_request("/v1/contract_state", body=params_dict)
        return ContractStateResponse(**res)

    def get_tokens(self, params: TokensParams) -> TokensResponse:
        """
        Get tokens based on the provided parameters.

        Args:
            params: Parameters for filtering tokens

        Returns:
            Tokens response with pagination
        """
        params_dict = params.dict(exclude_none=True)
        params_dict["chain"] = self._chain

        res = self._post_request("/v1/tokens", body=params_dict)
        return TokensResponse(**res)

    def get_protocol_systems(
        self, params: ProtocolSystemsParams
    ) -> ProtocolSystemsResponse:
        """
        Get list of supported protocol systems.

        Args:
            params: Parameters for filtering protocol systems

        Returns:
            Protocol systems response with pagination
        """
        params_dict = params.dict(exclude_none=True)
        params_dict["chain"] = self._chain

        res = self._post_request("/v1/protocol_systems", body=params_dict)
        return ProtocolSystemsResponse(**res)

    def get_component_tvl(self, params: ComponentTvlParams) -> ComponentTvlResponse:
        """
        Get component TVL data.

        Args:
            params: Parameters for filtering component TVL

        Returns:
            Component TVL response with pagination
        """
        params_dict = params.dict(exclude_none=True)
        params_dict["chain"] = self._chain

        res = self._post_request("/v1/component_tvl", body=params_dict)
        return ComponentTvlResponse(**res)

    def get_traced_entry_points(
        self, params: TracedEntryPointParams
    ) -> TracedEntryPointsResponse:
        """
        Get traced entry points.

        Args:
            params: Parameters for filtering traced entry points

        Returns:
            Traced entry points response with pagination
        """
        params_dict = params.dict(exclude_none=True)
        params_dict["chain"] = self._chain

        res = self._post_request("/v1/traced_entry_points", body=params_dict)
        return TracedEntryPointsResponse(**res)

    def health(self) -> dict:
        """
        Get server health status.

        Returns:
            Health status response
        """
        return self._get_request("/v1/health")


if __name__ == "__main__":
    # Example usage of the Tycho RPC client
    client = TychoRPCClient(
        rpc_url="https://tycho-beta.propellerheads.xyz",
        chain=Chain.ethereum,
        auth_token="sampletoken",
    )

    print("Tycho RPC Client Example Usage")
    print("=" * 50)

    try:
        # Example 1: Check server health
        print("1. Checking server health...")
        health = client.health()
        print(f"   Health status: {health}")
        print()

        # Example 2: Get list of supported protocol systems
        print("2. Getting protocol systems...")
        systems_params = ProtocolSystemsParams(
            chain=Chain.ethereum, pagination=PaginationParams(page=0, page_size=10)
        )
        systems = client.get_protocol_systems(systems_params)
        print(f"   Found {len(systems.protocol_systems)} protocol systems")
        print(
            f"   Pagination: page {systems.pagination.page}, size {systems.pagination.page_size}, total {systems.pagination.total}"
        )
        if systems.protocol_systems:
            print(f"   First system: {systems.protocol_systems[0]}")
        print()

        # Example 3: Get tokens with quality and trading filters
        print("3. Getting tokens with filters...")
        tokens_params = TokensParams(
            min_quality=10,
            traded_n_days_ago=30,
            pagination=PaginationParams(page=0, page_size=5),
        )
        tokens = client.get_tokens(tokens_params)
        print(f"   Found {len(tokens.tokens)} tokens")
        print(
            f"   Pagination: page {tokens.pagination.page}, size {tokens.pagination.page_size}, total {tokens.pagination.total}"
        )
        if tokens.tokens:
            print(
                f"   First token: {tokens.tokens[0].symbol} ({tokens.tokens[0].address.hex()})"
            )
        print()

        # Example 4: Get protocol components for a specific system
        if systems.protocol_systems:
            print("4. Getting protocol components...")
            components_params = ProtocolComponentsParams(
                protocol_system=systems.protocol_systems[0],
                pagination=PaginationParams(page=0, page_size=5),
            )
            components = client.get_protocol_components(components_params)
            print(f"   Found {len(components.protocol_components)} protocol components")
            print(
                f"   Pagination: page {components.pagination.page}, size {components.pagination.page_size}, total {components.pagination.total}"
            )
            if components.protocol_components:
                print(f"   First component: {components.protocol_components[0].id}")
            print()

            # Example 5: Get component TVL data
            print("5. Getting component TVL data...")
            tvl_params = ComponentTvlParams(
                protocol_system=systems.protocol_systems[0],
                pagination=PaginationParams(page=0, page_size=10),
            )
            tvl = client.get_component_tvl(tvl_params)
            print(f"   Found TVL data for {len(tvl.tvl)} components")
            print(
                f"   Pagination: page {tvl.pagination.page}, size {tvl.pagination.page_size}, total {tvl.pagination.total}"
            )
            if tvl.tvl:
                first_component = list(tvl.tvl.keys())[0]
                print(
                    f"   First component TVL: {first_component} = {tvl.tvl[first_component]}"
                )
            print()

            # Example 6: Get traced entry points
            print("6. Getting traced entry points...")
            entry_points_params = TracedEntryPointParams(
                protocol_system=systems.protocol_systems[0],
                pagination=PaginationParams(page=0, page_size=5),
            )
            entry_points = client.get_traced_entry_points(entry_points_params)
            print(
                f"   Found traced entry points for {len(entry_points.traced_entry_points)} components"
            )
            print(
                f"   Pagination: page {entry_points.pagination.page}, size {entry_points.pagination.page_size}, total {entry_points.pagination.total}"
            )
            print()

        # Example 7: Get contract state
        print("7. Getting contract state...")
        contract_params = ContractStateParams(
            pagination=PaginationParams(page=0, page_size=5)
        )
        contracts = client.get_contract_state(contract_params)
        print(f"   Found {len(contracts.accounts)} contracts")
        print(
            f"   Pagination: page {contracts.pagination.page}, size {contracts.pagination.page_size}, total {contracts.pagination.total}"
        )
        if contracts.accounts:
            print(f"   First contract: {contracts.accounts[0].address.hex()}")
        print()

        # Example 8: Get protocol state with balances
        if systems.protocol_systems:
            print("8. Getting protocol state with balances...")
            state_params = ProtocolStateParams(
                protocol_system=systems.protocol_systems[0],
                include_balances=True,
                pagination=PaginationParams(page=0, page_size=5),
            )
            states = client.get_protocol_state(state_params)
            print(f"   Found {len(states.states)} protocol states")
            print(
                f"   Pagination: page {states.pagination.page}, size {states.pagination.page_size}, total {states.pagination.total}"
            )
            if states.states:
                print(f"   First state: {states.states[0].component_id}")
            print()

        print("All examples completed successfully!")

    except TychoRPCError as e:
        print(f"RPC Error: {e.message}")
        if e.status_code:
            print(f"Status code: {e.status_code}")
        if e.response_data:
            print(f"Response data: {e.response_data}")
    except Exception as e:
        print(f"Unexpected error: {e}")
        import traceback

        traceback.print_exc()
