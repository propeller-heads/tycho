// Copyright (c) 2026 Everlong Labs Limited
//! `FLAMMFactory.PoolCreated` and the `createPool` calldata that accompanies it
//! (`src/factory/FLAMMFactory.sol:65-79` and `:212-259`).
use anyhow::{anyhow, bail, Result};
use ethabi::{ParamType, Token};
use substreams::hex;

use crate::flamm::keys::{keccak256, Address, Word};

/// `PoolCreated(address indexed pool, address indexed core, address indexed creator, address
/// implementation, address account, address invariantHook, address feeHook, address recenterHook,
/// bytes32 invariantCodehash, bytes32 feeCodehash, bytes32 recenterCodehash, bytes32 hookSetHash,
/// bytes32 riskHash)`.
pub const POOL_CREATED_TOPIC: [u8; 32] =
    hex!("b0dad6ac559b5d5e2dcc1a86abc22ff2097a1fb0da39b753242c799feccb6d09");
pub const POOL_CREATED_SIGNATURE: &str =
    "PoolCreated(address,address,address,address,address,address,address,address,bytes32,bytes32,bytes32,bytes32,bytes32)";

/// `createPool(PoolParams,HookSet,bytes32,VenueInit[])`.
pub const CREATE_POOL_SELECTOR: [u8; 4] = hex!("c8bf45c3");
pub const CREATE_POOL_SIGNATURE: &str = "createPool((address,address,address,address,address,string,string,(uint64,uint64,uint64,uint64,uint64,uint64,uint64,uint32,uint64,uint64,uint64,uint64,uint64,uint256,uint256,uint256,uint16,uint32,uint32,uint256,bool),uint64,uint64,uint256,address),(address,address,address,address,address,address,address),bytes32,(uint8,uint8,bytes,bool,bool,uint128,uint128,uint64)[])";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolCreated {
    pub pool: Address,
    pub core: Address,
    pub creator: Address,
    pub implementation: Address,
    pub account: Address,
    pub invariant_hook: Address,
    pub fee_hook: Address,
    pub recenter_hook: Address,
    pub invariant_codehash: Word,
    pub fee_codehash: Word,
    pub recenter_codehash: Word,
    pub hook_set_hash: Word,
    pub risk_hash: Word,
}

/// `IFLAMM.HookSet` (`src/interfaces/core/flamm/IFLAMM.sol:31-39`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookSet {
    pub invariant_hook: Address,
    pub fee_hook: Address,
    pub recenter_hook: Address,
    pub controller_hook: Address,
    pub leverage_hook: Address,
    pub spread_hook: Address,
    pub loan_swap_hook: Address,
}

/// `IFLAMM.VenueInit` (`IFLAMM.sol:41-50`); `venue_params` is
/// `abi.encode(IMorphoBlue.MarketParams)` for kind 0 and the venue id is its keccak
/// (`MorphoBlueAccount.sol:107-109`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VenueInit {
    pub kind: u8,
    pub loan_index: u8,
    pub venue_params: Vec<u8>,
}

impl VenueInit {
    /// `keccak256(abi.encode(m))`, the Morpho market id (`MorphoBlueAccount.sol:109`).
    pub fn market_id(&self) -> Word {
        keccak256(&self.venue_params)
    }

    /// `(loanToken, collateralToken, oracle, irm, lltv)` when the params decode as `MarketParams`.
    pub fn market_params(&self) -> Option<MarketParams> {
        let types = [
            ParamType::Address,
            ParamType::Address,
            ParamType::Address,
            ParamType::Address,
            ParamType::Uint(256),
        ];
        let tokens = ethabi::decode(&types, &self.venue_params).ok()?;
        Some(MarketParams {
            loan_token: address(tokens.first()?).ok()?,
            collateral_token: address(tokens.get(1)?).ok()?,
            oracle: address(tokens.get(2)?).ok()?,
            irm: address(tokens.get(3)?).ok()?,
            lltv: word(tokens.get(4)?).ok()?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketParams {
    pub loan_token: Address,
    pub collateral_token: Address,
    pub oracle: Address,
    pub irm: Address,
    pub lltv: Word,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatePool {
    pub core: Address,
    pub pool_asset: Address,
    pub loan_asset: Address,
    pub price_feed: Address,
    pub hooks: HookSet,
    pub salt: Word,
    pub venues: Vec<VenueInit>,
}

pub fn decode_pool_created(topics: &[Vec<u8>], data: &[u8]) -> Result<PoolCreated> {
    if topics.len() != 4 || topics[0] != POOL_CREATED_TOPIC {
        bail!("not a PoolCreated log");
    }
    let types = [
        ParamType::Address,
        ParamType::Address,
        ParamType::Address,
        ParamType::Address,
        ParamType::Address,
        ParamType::FixedBytes(32),
        ParamType::FixedBytes(32),
        ParamType::FixedBytes(32),
        ParamType::FixedBytes(32),
        ParamType::FixedBytes(32),
    ];
    let t = ethabi::decode(&types, data)?;
    Ok(PoolCreated {
        pool: topic_address(&topics[1])?,
        core: topic_address(&topics[2])?,
        creator: topic_address(&topics[3])?,
        implementation: address(&t[0])?,
        account: address(&t[1])?,
        invariant_hook: address(&t[2])?,
        fee_hook: address(&t[3])?,
        recenter_hook: address(&t[4])?,
        invariant_codehash: word(&t[5])?,
        fee_codehash: word(&t[6])?,
        recenter_codehash: word(&t[7])?,
        hook_set_hash: word(&t[8])?,
        risk_hash: word(&t[9])?,
    })
}

fn risk_init() -> ParamType {
    ParamType::Tuple(vec![
        ParamType::Uint(64),
        ParamType::Uint(64),
        ParamType::Uint(64),
        ParamType::Uint(64),
        ParamType::Uint(64),
        ParamType::Uint(64),
        ParamType::Uint(64),
        ParamType::Uint(32),
        ParamType::Uint(64),
        ParamType::Uint(64),
        ParamType::Uint(64),
        ParamType::Uint(64),
        ParamType::Uint(64),
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Uint(256),
        ParamType::Uint(16),
        ParamType::Uint(32),
        ParamType::Uint(32),
        ParamType::Uint(256),
        ParamType::Bool,
    ])
}

fn pool_params() -> ParamType {
    ParamType::Tuple(vec![
        ParamType::Address,
        ParamType::Address,
        ParamType::Address,
        ParamType::Address,
        ParamType::Address,
        ParamType::String,
        ParamType::String,
        risk_init(),
        ParamType::Uint(64),
        ParamType::Uint(64),
        ParamType::Uint(256),
        ParamType::Address,
    ])
}

fn hook_set() -> ParamType {
    ParamType::Tuple(vec![ParamType::Address; 7])
}

fn venue_init() -> ParamType {
    ParamType::Tuple(vec![
        ParamType::Uint(8),
        ParamType::Uint(8),
        ParamType::Bytes,
        ParamType::Bool,
        ParamType::Bool,
        ParamType::Uint(128),
        ParamType::Uint(128),
        ParamType::Uint(64),
    ])
}

/// Decodes a `createPool` call's input (selector included).
pub fn decode_create_pool(input: &[u8]) -> Result<CreatePool> {
    if input.len() < 4 || input[..4] != CREATE_POOL_SELECTOR {
        bail!("not a createPool call");
    }
    let types = [
        pool_params(),
        hook_set(),
        ParamType::FixedBytes(32),
        ParamType::Array(Box::new(venue_init())),
    ];
    let t = ethabi::decode(&types, &input[4..])?;
    let p = tuple(&t[0])?;
    let h = tuple(&t[1])?;
    let venues = match &t[3] {
        Token::Array(items) => items
            .iter()
            .map(|v| {
                let v = tuple(v)?;
                Ok(VenueInit {
                    kind: small(&v[0])?,
                    loan_index: small(&v[1])?,
                    venue_params: bytes(&v[2])?,
                })
            })
            .collect::<Result<Vec<_>>>()?,
        _ => bail!("venues is not an array"),
    };
    Ok(CreatePool {
        core: address(&p[0])?,
        pool_asset: address(&p[1])?,
        loan_asset: address(&p[2])?,
        price_feed: address(&p[3])?,
        hooks: HookSet {
            invariant_hook: address(&h[0])?,
            fee_hook: address(&h[1])?,
            recenter_hook: address(&h[2])?,
            controller_hook: address(&h[3])?,
            leverage_hook: address(&h[4])?,
            spread_hook: address(&h[5])?,
            loan_swap_hook: address(&h[6])?,
        },
        salt: word(&t[2])?,
        venues,
    })
}

fn tuple(t: &Token) -> Result<&Vec<Token>> {
    match t {
        Token::Tuple(items) => Ok(items),
        other => Err(anyhow!("expected a tuple, got {other:?}")),
    }
}

fn address(t: &Token) -> Result<Address> {
    match t {
        Token::Address(a) => Ok(a.0),
        other => Err(anyhow!("expected an address, got {other:?}")),
    }
}

fn word(t: &Token) -> Result<Word> {
    match t {
        Token::FixedBytes(b) if b.len() == 32 => Ok(b.as_slice().try_into()?),
        Token::Uint(u) => {
            let mut w = [0u8; 32];
            u.to_big_endian(&mut w);
            Ok(w)
        }
        other => Err(anyhow!("expected a 32-byte word, got {other:?}")),
    }
}

fn small(t: &Token) -> Result<u8> {
    match t {
        Token::Uint(u) if *u <= ethabi::ethereum_types::U256::from(u8::MAX) => {
            Ok(u.low_u32() as u8)
        }
        other => Err(anyhow!("expected a uint8, got {other:?}")),
    }
}

fn bytes(t: &Token) -> Result<Vec<u8>> {
    match t {
        Token::Bytes(b) => Ok(b.clone()),
        other => Err(anyhow!("expected bytes, got {other:?}")),
    }
}

fn topic_address(topic: &[u8]) -> Result<Address> {
    if topic.len() != 32 {
        bail!("topic is not 32 bytes");
    }
    Ok(crate::flamm::keys::address_in_word(topic))
}
