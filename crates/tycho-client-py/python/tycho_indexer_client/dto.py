from datetime import datetime
from enum import Enum
from typing import Dict, List, Optional, Set, Union
from uuid import UUID

from hexbytes import HexBytes as _HexBytes
from pydantic import BaseModel, Field, root_validator


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


class ChainTokenConfig(BaseModel):
    address: str
    symbol: str
    decimals: int


class TvlThresholds(BaseModel):
    low: int
    medium: int


class CustomChainConfig(BaseModel):
    name: str
    chain_id: int
    block_time_secs: int
    native: ChainTokenConfig
    wrapped_native: ChainTokenConfig
    default_tvl_thresholds: TvlThresholds

    def __str__(self) -> str:
        return self.name

    @root_validator(pre=True)
    def _unwrap_serde_tag(cls, values):
        # Rust serialises Custom(cfg) as {"custom": {...}} — unwrap the tag.
        if "custom" in values and len(values) == 1:
            return values["custom"]
        return values




class ChangeType(str, Enum):
    update = "Update"
    deletion = "Deletion"
    creation = "Creation"
    unspecified = "Unspecified"


class ExtractorIdentity(BaseModel):
    chain: Union[Chain, CustomChainConfig]
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
    chain: Union[Chain, CustomChainConfig]
    ts: datetime


class BlockParam(BaseModel):
    hash: Optional[HexBytes] = None
    chain: Optional[Union[Chain, CustomChainConfig]] = None
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
    chain: Union[Chain, CustomChainConfig]
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
    chain: Union[Chain, CustomChainConfig]
    tokens: List[HexBytes]
    contract_ids: List[HexBytes]
    static_attributes: Dict[str, HexBytes]
    change: ChangeType
    creation_tx: HexBytes
    created_at: datetime


class ResponseToken(BaseModel):
    chain: Union[Chain, CustomChainConfig]
    address: HexBytes = Field(..., example="0xc9f2e6ea1637E499406986ac50ddC92401ce1f58")
    symbol: str = Field(..., example="WETH")
    decimals: int
    tax: int
    gas: List[Optional[int]]
    quality: int


class BlockChanges(BaseModel):
    extractor: str
    chain: Union[Chain, CustomChainConfig]
    block: Block
    finalized_block_height: int
    revert: bool
    new_tokens: Dict[HexBytes, ResponseToken] = Field(default_factory=dict)
    account_updates: Dict[HexBytes, AccountUpdate]
    state_updates: Dict[str, ProtocolStateDelta]
    new_protocol_components: Dict[str, ProtocolComponent]
    deleted_protocol_components: Dict[str, ProtocolComponent]
    component_balances: Dict[str, TokenBalances]
    account_balances: Dict[HexBytes, Dict[HexBytes, HexBytes]] = Field(
        default_factory=dict
    )
    component_tvl: Dict[str, float]


class ResponseProtocolState(BaseModel):
    component_id: str
    attributes: Dict[str, HexBytes]
    balances: Dict[HexBytes, HexBytes]


class ComponentWithState(BaseModel):
    state: ResponseProtocolState
    component: ProtocolComponent


class ResponseAccount(BaseModel):
    chain: Union[Chain, CustomChainConfig]
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
    chain: Union[Chain, CustomChainConfig]
    id: str


class ContractId(BaseModel):
    address: HexBytes
    chain: Union[Chain, CustomChainConfig]


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
    chain: Optional[Union[Chain, CustomChainConfig]] = None
    pagination: Optional[PaginationParams] = None

    class Config:
        allow_population_by_field_name = True


class ComponentTvlParams(BaseModel):
    chain: Optional[Union[Chain, CustomChainConfig]] = None
    protocol_system: Optional[str] = Field(default=None, alias="protocolSystem")
    component_ids: Optional[List[str]] = Field(default=None)
    pagination: Optional[PaginationParams] = None

    class Config:
        allow_population_by_field_name = True


class TracedEntryPointParams(BaseModel):
    chain: Optional[Union[Chain, CustomChainConfig]] = None
    protocol_system: str
    component_ids: Optional[List[str]] = Field(default=None)
    pagination: Optional[PaginationParams] = None

    class Config:
        allow_population_by_field_name = True


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
