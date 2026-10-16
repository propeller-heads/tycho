from datetime import datetime
from enum import Enum
from typing import Dict, List, Optional, Set, Tuple, Union
from uuid import UUID

from hexbytes import HexBytes as _HexBytes
from pydantic import BaseModel, Field, root_validator

# pydantic 1.8 compares Literal fields against typing_extensions.Literal, which below Python
# 3.10.1 is not typing.Literal.
from typing_extensions import Literal


class HexBytes(_HexBytes):
    @classmethod
    def __get_validators__(cls):
        # one or more validators may be yielded which will be called in the
        # order to validate the input, each validator will receive as an input
        # the value returned from the previous validator
        yield cls.validate

    @classmethod
    def __modify_schema__(cls, field_schema):
        # __modify_schema__ should mutate the dict it receives in place,
        # the returned value will be ignored
        field_schema.update(
            # some example postcodes
            examples=["0xBadBad"]
        )

    @classmethod
    def validate(cls, v: str):
        data = _HexBytes(v)

        return cls(data)

    def __int__(self):
        if self.hex() != "0x":
            return int(self.hex(), 16)
        else:
            return 0


class Chain(str, Enum):
    ethereum = "ethereum"
    starknet = "starknet"
    arbitrum = "arbitrum"
    base = "base"
    bsc = "bsc"
    zksync = "zksync"
    unichain = "unichain"
    polygon = "polygon"
    plasma = "plasma"
    robinhood = "robinhood"
    arc = "arc"


class CustomChain(BaseModel):
    """A user-defined chain. Only the chain name travels on the wire; the full config lives
    server-side. Rust serialises ``Custom("name")`` as ``{"custom": "name"}``."""

    name: str

    def __str__(self) -> str:
        return self.name

    @root_validator(pre=True)
    def _unwrap_serde_tag(cls, values):
        # Rust serialises Custom("name") as {"custom": "name"} — unwrap the tag to the name.
        if "custom" in values and len(values) == 1:
            return {"name": values["custom"]}
        return values


class ChangeType(str, Enum):
    update = "Update"
    deletion = "Deletion"
    creation = "Creation"
    unspecified = "Unspecified"


class ExtractorIdentity(BaseModel):
    chain: Union[Chain, CustomChain]
    name: str


class Command(BaseModel):
    class Subscribe(BaseModel):
        extractor_id: ExtractorIdentity
        include_state: bool

    class Unsubscribe(BaseModel):
        subscription_id: UUID

    method: Union[Subscribe, Unsubscribe]


class Response(BaseModel):
    class NewSubscription(BaseModel):
        extractor_id: ExtractorIdentity
        subscription_id: UUID
        fingerprint: Optional[HexBytes] = None

    class SubscriptionEnded(BaseModel):
        subscription_id: UUID

    method: Union[NewSubscription, SubscriptionEnded]


class Header(BaseModel):
    number: int
    hash: HexBytes
    parent_hash: HexBytes
    revert: bool


class Block(BaseModel):
    number: int
    hash: HexBytes
    parent_hash: HexBytes
    chain: Union[Chain, CustomChain]
    ts: datetime


class BlockParam(BaseModel):
    hash: Optional[HexBytes] = None
    chain: Optional[Union[Chain, CustomChain]] = None
    number: Optional[int] = None


class ComponentBalance(BaseModel):
    token: HexBytes
    balance: HexBytes
    balance_float: float
    modify_tx: HexBytes
    component_id: str


class TokenBalances(BaseModel):
    __root__: Dict[HexBytes, ComponentBalance]

    def items(self):
        return self.__root__.items()


class AccountUpdate(BaseModel):
    address: HexBytes
    chain: Union[Chain, CustomChain]
    slots: Dict[HexBytes, HexBytes]
    balance: Optional[HexBytes] = None
    code: Optional[HexBytes] = None
    change: ChangeType


class ProtocolStateDelta(BaseModel):
    component_id: str
    updated_attributes: Dict[str, HexBytes]
    deleted_attributes: Set[str]


class ProtocolComponent(BaseModel):
    id: str
    protocol_system: str
    protocol_type_name: str
    chain: Union[Chain, CustomChain]
    tokens: List[HexBytes]
    contract_ids: List[HexBytes]
    static_attributes: Dict[str, HexBytes]
    change: ChangeType
    creation_tx: HexBytes
    created_at: datetime


class ResponseToken(BaseModel):
    chain: Union[Chain, CustomChain]
    address: HexBytes = Field(..., example="0xc9f2e6ea1637E499406986ac50ddC92401ce1f58")
    symbol: str = Field(..., example="WETH")
    decimals: int
    tax: int
    gas: List[Optional[int]]
    quality: int


class AccountBalance(BaseModel):
    account: HexBytes
    token: HexBytes
    balance: HexBytes
    modify_tx: HexBytes


class EntryPoint(BaseModel):
    external_id: str
    target: HexBytes
    signature: str


class StorageOverride(BaseModel):
    """Rust serialises it as ``{"Diff": {...}}`` or ``{"Replace": {...}}``: one of the two is
    set. ``diff`` changes only the given slots; ``replace`` replaces the whole storage.
    """

    diff: Optional[Dict[HexBytes, HexBytes]] = Field(default=None, alias="Diff")
    replace: Optional[Dict[HexBytes, HexBytes]] = Field(default=None, alias="Replace")

    class Config:
        allow_population_by_field_name = True


class AccountOverrides(BaseModel):
    slots: Optional[StorageOverride] = None
    native_balance: Optional[HexBytes] = None
    code: Optional[HexBytes] = None


class RPCTracerParams(BaseModel):
    """The only tracing method (``TracingParams::RPCTracer`` in Rust)."""

    method: Literal["rpctracer"] = "rpctracer"
    caller: Optional[HexBytes] = None
    calldata: HexBytes
    state_overrides: Optional[Dict[HexBytes, AccountOverrides]] = None
    prune_addresses: Optional[List[HexBytes]] = None


class AddressStorageLocation(BaseModel):
    key: HexBytes
    offset: int


class TracingResult(BaseModel):
    retriggers: List[Tuple[HexBytes, AddressStorageLocation]] = Field(
        default_factory=list
    )
    accessed_slots: Dict[HexBytes, Set[HexBytes]] = Field(default_factory=dict)


class DCIUpdate(BaseModel):
    # component id -> entry points newly associated with it
    new_entrypoints: Dict[str, List[EntryPoint]] = Field(default_factory=dict)
    # entry point id -> new tracing params, each with the component they belong to
    new_entrypoint_params: Dict[str, List[Tuple[RPCTracerParams, str]]] = Field(
        default_factory=dict
    )
    # entry point id -> its trace result
    trace_results: Dict[str, TracingResult] = Field(default_factory=dict)


class BlockChanges(BaseModel):
    """One block's changes, as the WebSocket sends them (``BlockAggregatedChanges`` in Rust)."""

    extractor: str
    chain: Union[Chain, CustomChain]
    block: Block
    finalized_block_height: int
    revert: bool
    new_tokens: Dict[HexBytes, ResponseToken] = Field(default_factory=dict)
    account_updates: Dict[HexBytes, AccountUpdate]
    state_updates: Dict[str, ProtocolStateDelta]
    new_protocol_components: Dict[str, ProtocolComponent]
    deleted_protocol_components: Dict[str, ProtocolComponent]
    component_balances: Dict[str, TokenBalances]
    # account -> token -> balance
    account_balances: Dict[HexBytes, Dict[HexBytes, AccountBalance]] = Field(
        default_factory=dict
    )
    component_tvl: Dict[str, float]
    dci_update: DCIUpdate = Field(default_factory=DCIUpdate)
    # The index of a partial block; None for a full block.
    partial_block_index: Optional[int] = None


class ResponseProtocolState(BaseModel):
    component_id: str
    attributes: Dict[str, HexBytes]
    balances: Dict[HexBytes, HexBytes]


class ComponentWithState(BaseModel):
    state: ResponseProtocolState
    component: ProtocolComponent


class ResponseAccount(BaseModel):
    chain: Union[Chain, CustomChain]
    address: HexBytes
    title: str
    slots: Dict[HexBytes, HexBytes]
    native_balance: HexBytes = Field(alias="balance")
    token_balances: Dict[HexBytes, HexBytes]
    code: HexBytes
    code_hash: HexBytes
    balance_modify_tx: HexBytes
    code_modify_tx: HexBytes
    creation_tx: Optional[HexBytes] = None

    class Config:
        allow_population_by_field_name = True

    @property
    def balance(self) -> HexBytes:
        return self.native_balance

    @property
    def change(self) -> ChangeType:
        return ChangeType.creation


class Snapshot(BaseModel):
    states: Dict[str, ComponentWithState] = Field(default_factory=dict)
    vm_storage: Dict[HexBytes, ResponseAccount] = Field(default_factory=dict)


class StateSyncMessage(BaseModel):
    header: Header
    snapshots: Snapshot
    deltas: Optional[BlockChanges] = None
    removed_components: Dict[str, ProtocolComponent] = Field(default_factory=dict)


class SynchronizerStateEnum(str, Enum):
    started = "started"
    ready = "ready"
    stale = "stale"
    delayed = "delayed"
    advanced = "advanced"
    ended = "ended"


class SynchronizerState(BaseModel):
    status: SynchronizerStateEnum
    header: Optional[Header] = None

    def __init__(self, status: SynchronizerStateEnum, **kwargs):
        if len(kwargs) == 4:
            header = Header(**kwargs)
        else:
            header = kwargs.get("header", None)
        super().__init__(status=status, header=header)


class FeedMessage(BaseModel):
    state_msgs: dict[str, StateSyncMessage]
    sync_states: dict[str, SynchronizerState]


# Request Parameters


class PaginationParams(BaseModel):
    page: Optional[int] = None
    page_size: Optional[int] = None


class ProtocolId(BaseModel):
    chain: Union[Chain, CustomChain]
    id: str


class ContractId(BaseModel):
    address: HexBytes
    chain: Union[Chain, CustomChain]


class VersionParams(BaseModel):
    block: Optional[BlockParam] = None
    timestamp: Optional[datetime] = None


class ProtocolComponentsParams(BaseModel):
    protocol_system: Optional[str] = None
    component_addresses: Optional[List[HexBytes]] = Field(default=None)
    tvl_gt: Optional[int] = None
    pagination: Optional[PaginationParams] = None

    class Config:
        allow_population_by_field_name = True


class ProtocolStateParams(BaseModel):
    include_balances: Optional[bool] = True
    protocol_ids: Optional[List[ProtocolId]] = Field(default=None)
    protocol_system: Optional[str] = Field(default=None)
    version: Optional[VersionParams] = None
    pagination: Optional[PaginationParams] = None

    class Config:
        allow_population_by_field_name = True
        use_enum_values = True


class ContractStateParams(BaseModel):
    contract_ids: Optional[List[str]] = Field(default=None)
    protocol_system: Optional[str] = Field(default=None)
    version: Optional[VersionParams] = None
    pagination: Optional[PaginationParams] = None

    # Backward compatibility with old ContractId format
    # To be removed in the future
    @root_validator(pre=True)
    def handle_old_contract_id_format(cls, values):
        contract_ids = values.get("contract_ids")
        if contract_ids and isinstance(contract_ids[0], ContractId):
            # Handle old format: List of ContractId objects
            values["contract_ids"] = [c.address.hex() for c in contract_ids]
        return values

    class Config:
        allow_population_by_field_name = True


class TokensParams(BaseModel):
    min_quality: Optional[int] = None
    token_addresses: Optional[List[HexBytes]] = Field(default=None)
    traded_n_days_ago: Optional[int] = None
    pagination: Optional[PaginationParams] = None

    class Config:
        allow_population_by_field_name = True


class ProtocolSystemsParams(BaseModel):
    chain: Optional[Union[Chain, CustomChain]] = None
    pagination: Optional[PaginationParams] = None

    class Config:
        allow_population_by_field_name = True


class ComponentTvlParams(BaseModel):
    chain: Optional[Union[Chain, CustomChain]] = None
    protocol_system: Optional[str] = Field(default=None, alias="protocolSystem")
    component_ids: Optional[List[str]] = Field(default=None)
    pagination: Optional[PaginationParams] = None

    class Config:
        allow_population_by_field_name = True


class TracedEntryPointParams(BaseModel):
    chain: Optional[Union[Chain, CustomChain]] = None
    protocol_system: str
    component_ids: Optional[List[str]] = Field(default=None)
    pagination: Optional[PaginationParams] = None

    class Config:
        allow_population_by_field_name = True


class DeltaCatchUpParams(BaseModel):
    """Asks for the messages a WebSocket subscriber of ``protocol_system`` missed between the
    last block it applied and the block its new subscription continues from.
    ``TychoRPCClient`` adds its chain; the server rejects any other field."""

    protocol_system: str
    # Hash and number of the last block the client applied.
    from_block_hash: HexBytes
    from_block_number: int
    # The block the messages must lead to: the parent of the new subscription's first
    # message, or that message's target when it is a revert.
    to_block_hash: HexBytes
    # The fingerprint the subscription reported when the client synced
    # (`Response.NewSubscription.fingerprint`).
    fingerprint: HexBytes
    # Whether the messages carry state updates (`state_updates` and `account_updates`), like
    # the subscription option of the same name.
    include_state: bool = True


# Response objects


class PaginationResponse(BaseModel):
    page: int
    page_size: int
    total: int  # Total number of items across all pages

    def total_pages(self) -> int:
        """Calculate the total number of pages."""
        if self.page_size <= 0:
            return 0
        return (self.total + self.page_size - 1) // self.page_size

    def has_next_page(self) -> bool:
        """Check if there is a next page."""
        return self.page < self.total_pages() - 1

    def has_previous_page(self) -> bool:
        """Check if there is a previous page."""
        return self.page > 0


class ProtocolSystemsResponse(BaseModel):
    protocol_systems: List[str]
    pagination: PaginationResponse


class ProtocolComponentsResponse(BaseModel):
    protocol_components: List[ProtocolComponent]
    pagination: PaginationResponse


class ProtocolStateResponse(BaseModel):
    states: List[ResponseProtocolState]
    pagination: PaginationResponse


class ContractStateResponse(BaseModel):
    accounts: List[ResponseAccount]
    pagination: PaginationResponse


class TokensResponse(BaseModel):
    tokens: List[ResponseToken]
    pagination: PaginationResponse


class ComponentTvlResponse(BaseModel):
    tvl: Dict[str, float]
    pagination: PaginationResponse


class TracedEntryPointsResponse(BaseModel):
    traced_entry_points: Dict[
        str, List[tuple]
    ]  # component_id -> [(EntryPointWithTracingParams, TracingResult)]
    pagination: PaginationResponse


class DeltaCatchUpUnavailable(str, Enum):
    """Why the server cannot serve a delta catch-up. A reason this client does not know
    parses as ``unknown``; servers never send ``unknown`` themselves."""

    # The server runs no extractors (the standalone RPC server).
    no_window = "no_window"
    # The extractor's fingerprint differs from the request's, or it has none.
    fingerprint_changed = "fingerprint_changed"
    # The extractor's window holds no block: at startup, or right after a restart.
    empty_window = "empty_window"
    # `to_block_hash` is not a canonical block in the window (yet).
    to_not_in_window = "to_not_in_window"
    # `from_block_hash` is older than the parent of the window's oldest block.
    below_floor = "below_floor"
    # `from_block_hash` is neither in the window nor orphaned by a revert it remembers.
    unknown_hash = "unknown_hash"
    # The client received a revert the window has not applied yet.
    to_not_ahead = "to_not_ahead"
    # A reason added to the server after this client.
    unknown = "unknown"

    @classmethod
    def _missing_(cls, value):
        if isinstance(value, str):
            return cls.unknown
        return None


class DeltaCatchUp:
    """Answer of ``TychoRPCClient.get_delta_catch_up``: an instance of ``Available``,
    ``Unavailable`` or ``Unsupported``, each with a ``status`` naming it. Only ``Available``
    lets the client continue without a snapshot."""

    class Available(BaseModel):
        """The missed messages, oldest first, as the WebSocket sent them: revert messages
        and full blocks. Applied in order to the state at `from_block_hash`, they give the
        state at `to_block_hash`; the last one may be a revert to that block. Empty when the
        two blocks are the same."""

        status: Literal["available"] = "available"
        messages: List[BlockChanges]

    class Unavailable(BaseModel):
        """The server cannot serve the messages; resync from a snapshot."""

        status: Literal["unavailable"] = "unavailable"
        reason: DeltaCatchUpUnavailable

    class Unsupported(BaseModel):
        """The server predates delta catch-up (it answered 404); resync from a snapshot."""

        status: Literal["unsupported"] = "unsupported"


DeltaCatchUpResult = Union[
    DeltaCatchUp.Available, DeltaCatchUp.Unavailable, DeltaCatchUp.Unsupported
]
