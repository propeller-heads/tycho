// Copyright (c) 2026 Everlong Labs Limited
//! Everything the modules share: the contract roles, the per-pool tracking configuration, attribute
//! names and encodings, and the word view that answers "what is this storage word after transaction
//! `i`".
pub mod balances;
pub mod calldata;
#[cfg(test)]
pub mod feed_events;
pub mod feeds;
pub mod keys;
pub mod statics;
pub mod words;

use std::fmt;

use anyhow::{anyhow, bail, Result};
use tycho_substreams::prelude::{Attribute, ChangeType};

use crate::flamm::keys::{hex_address, hex_word, parse_address, parse_word, Address, Word};

pub const PROTOCOL_TYPE_NAME: &str = "flamm_pool";

/// `pool(20) || 0x00000000 || uint64(1)`: the lever-up component of a pool (design section 5).
pub const LEVER_UP_DISCRIMINATOR: u64 = 1;

pub fn swap_component_id(pool: &Address) -> String {
    hex_address(pool)
}

pub fn lever_up_component_id(pool: &Address) -> String {
    let mut id = [0u8; 32];
    id[..20].copy_from_slice(pool);
    id[24..].copy_from_slice(&LEVER_UP_DISCRIMINATOR.to_be_bytes());
    hex_word(&id)
}

/// The contracts a pool is made of, by the role their storage plays for the decoder. The registry
/// param maps runtime codehashes to roles; the attribute prefix is the schema's (`pricefeed:` for
/// the price feed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Implementation,
    Pool,
    Hook,
    LeverageHook,
    SpreadHook,
    Router,
    PriceFeed,
    Factory,
    Account,
}

impl Role {
    pub const ALL: [Role; 9] = [
        Role::Implementation,
        Role::Pool,
        Role::Hook,
        Role::LeverageHook,
        Role::SpreadHook,
        Role::Router,
        Role::PriceFeed,
        Role::Factory,
        Role::Account,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Role::Implementation => "implementation",
            Role::Pool => "pool",
            Role::Hook => "hook",
            Role::LeverageHook => "leverage_hook",
            Role::SpreadHook => "spread_hook",
            Role::Router => "router",
            Role::PriceFeed => "price_feed",
            Role::Factory => "factory",
            Role::Account => "account",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Role::ALL
            .into_iter()
            .find(|r| r.name() == s)
            .ok_or_else(|| anyhow!("unknown role `{s}`"))
    }

    /// The attribute prefix of the role's storage words, `None` for contracts whose state is all
    /// immutables.
    pub fn attribute_prefix(self) -> Option<&'static str> {
        match self {
            Role::Pool => Some("pool"),
            Role::Hook => Some("hook"),
            Role::SpreadHook => Some("spread"),
            Role::Router => Some("router"),
            Role::PriceFeed => Some("pricefeed"),
            Role::Factory => Some("factory"),
            Role::Account => Some("account"),
            Role::Implementation | Role::LeverageHook => None,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One financing venue of a pool: the account that holds its Morpho position and the market it is
/// bound to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VenueConfig {
    pub account: Address,
    pub market_id: Word,
    pub morpho: Address,
    pub irm: Address,
    pub oracle: Address,
}

/// One Chainlink proxy a pool's quotes read through: `asset`, `loan<i>` (the `PriceFeed` USD
/// feeds), `seq` (the sequencer uptime feed) and `mo<v>` (the base feed of venue `v`'s Morpho
/// oracle).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedConfig {
    pub role: String,
    pub proxy: Address,
}

/// What the package tracks for one pool, fixed at creation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolConfig {
    pub pool: Address,
    pub hook: Address,
    pub leverage_hook: Address,
    pub spread_hook: Address,
    pub router: Address,
    pub price_feed: Address,
    pub factory: Address,
    pub pool_asset: Address,
    pub loan_assets: Vec<Address>,
    pub venues: Vec<VenueConfig>,
    pub feeds: Vec<FeedConfig>,
}

impl PoolConfig {
    pub fn component_ids(&self) -> [String; 2] {
        [swap_component_id(&self.pool), lever_up_component_id(&self.pool)]
    }

    pub fn tokens(&self) -> Vec<Address> {
        let mut tokens = vec![self.pool_asset];
        tokens.extend(self.loan_assets.iter().copied());
        tokens
    }

    /// The FLAMM-owned contracts whose storage becomes attributes, with their role.
    pub fn storage_contracts(&self) -> Vec<(Role, Address)> {
        let mut out = vec![
            (Role::Pool, self.pool),
            (Role::Hook, self.hook),
            (Role::Router, self.router),
            (Role::PriceFeed, self.price_feed),
            (Role::Factory, self.factory),
        ];
        if self.spread_hook != [0u8; 20] {
            out.push((Role::SpreadHook, self.spread_hook));
        }
        for v in &self.venues {
            if !out.iter().any(|(_, a)| *a == v.account) {
                out.push((Role::Account, v.account));
            }
        }
        out
    }

    pub fn feed(&self, role: &str) -> Option<&FeedConfig> {
        self.feeds
            .iter()
            .find(|f| f.role == role)
    }

    /// Every tracked storage word with the attribute it feeds: the FLAMM-owned words
    /// (`<role>:<slot>`), the Morpho and IRM words (`mm:<v>:…`, `irm:<v>:rate_at_target`) and
    /// the proxies' access controllers (`feed:<f>:access_controller`). Rotation words (proxy
    /// slot 2) and the aggregator-side access words are handled by the feed logic, since their
    /// meaning depends on the current aggregator.
    pub fn tracked_words(&self) -> Vec<(Address, Word, String)> {
        let mut out = Vec::new();
        let market_ids: Vec<Word> = self
            .venues
            .iter()
            .map(|v| v.market_id)
            .collect();
        for (role, address) in self.storage_contracts() {
            let Some(prefix) = role.attribute_prefix() else { continue };
            let keys = match role {
                Role::Pool => keys::pool_keys(),
                Role::Hook => keys::hook_keys(),
                Role::SpreadHook => keys::spread_keys(),
                Role::Router => keys::router_keys(&self.pool),
                Role::PriceFeed => keys::pricefeed_keys(&self.tokens()),
                Role::Factory => keys::factory_keys(&self.pool),
                Role::Account => keys::account_keys(&market_ids),
                Role::Implementation | Role::LeverageHook => Vec::new(),
            };
            for key in keys {
                out.push((address, key, format!("{prefix}:{}", hex_word(&key))));
            }
        }
        for (v, venue) in self.venues.iter().enumerate() {
            for (i, key) in keys::morpho_market_keys(&venue.market_id)
                .into_iter()
                .enumerate()
            {
                out.push((venue.morpho, key, format!("mm:{v}:market:{i}")));
            }
            for (i, key) in keys::morpho_position_keys(&venue.market_id, &venue.account)
                .into_iter()
                .enumerate()
            {
                out.push((venue.morpho, key, format!("mm:{v}:position:{i}")));
            }
            out.push((
                venue.irm,
                keys::irm_rate_key(&venue.market_id),
                format!("irm:{v}:rate_at_target"),
            ));
        }
        for feed in &self.feeds {
            out.push((
                feed.proxy,
                keys::slot(keys::PROXY_ACCESS_CONTROLLER_SLOT),
                format!("feed:{}:access_controller", feed.role),
            ));
        }
        out
    }

    /// Serialization for the pools store: `key=value|…` with lists comma-separated (`;` is the
    /// append store's own item separator).
    pub fn serialize(&self) -> String {
        let venues = self
            .venues
            .iter()
            .map(|v| {
                format!(
                    "{}:{}:{}:{}:{}",
                    hex_address(&v.account),
                    hex_word(&v.market_id),
                    hex_address(&v.morpho),
                    hex_address(&v.irm),
                    hex_address(&v.oracle)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let feeds = self
            .feeds
            .iter()
            .map(|f| format!("{}:{}", f.role, hex_address(&f.proxy)))
            .collect::<Vec<_>>()
            .join(",");
        let loans = self
            .loan_assets
            .iter()
            .map(|a| hex_address(a))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "pool={}|hook={}|leverage_hook={}|spread_hook={}|router={}|price_feed={}|factory={}|pool_asset={}|loan_assets={}|venues={}|feeds={}",
            hex_address(&self.pool),
            hex_address(&self.hook),
            hex_address(&self.leverage_hook),
            hex_address(&self.spread_hook),
            hex_address(&self.router),
            hex_address(&self.price_feed),
            hex_address(&self.factory),
            hex_address(&self.pool_asset),
            loans,
            venues,
            feeds
        )
    }

    pub fn parse(s: &str) -> Result<Self> {
        let mut fields = std::collections::HashMap::new();
        for pair in s.split('|').filter(|p| !p.is_empty()) {
            let (k, v) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("bad pool config field `{pair}`"))?;
            fields.insert(k, v);
        }
        let get = |k: &str| {
            fields
                .get(k)
                .copied()
                .ok_or_else(|| anyhow!("pool config without `{k}`"))
        };
        fn list(v: &str) -> Vec<&str> {
            v.split(',')
                .filter(|p| !p.is_empty())
                .collect()
        }
        let venues = list(get("venues")?)
            .into_iter()
            .map(|v| {
                let parts: Vec<&str> = v.split(':').collect();
                if parts.len() != 5 {
                    bail!("bad venue `{v}`");
                }
                Ok(VenueConfig {
                    account: parse_address(parts[0])?,
                    market_id: parse_word(parts[1])?,
                    morpho: parse_address(parts[2])?,
                    irm: parse_address(parts[3])?,
                    oracle: parse_address(parts[4])?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let feeds = list(get("feeds")?)
            .into_iter()
            .map(|f| {
                let (role, proxy) = f
                    .split_once(':')
                    .ok_or_else(|| anyhow!("bad feed `{f}`"))?;
                Ok(FeedConfig { role: role.to_string(), proxy: parse_address(proxy)? })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            pool: parse_address(get("pool")?)?,
            hook: parse_address(get("hook")?)?,
            leverage_hook: parse_address(get("leverage_hook")?)?,
            spread_hook: parse_address(get("spread_hook")?)?,
            router: parse_address(get("router")?)?,
            price_feed: parse_address(get("price_feed")?)?,
            factory: parse_address(get("factory")?)?,
            pool_asset: parse_address(get("pool_asset")?)?,
            loan_assets: list(get("loan_assets")?)
                .into_iter()
                .map(parse_address)
                .collect::<Result<Vec<_>>>()?,
            venues,
            feeds,
        })
    }
}

pub fn attribute(name: &str, value: Vec<u8>, change: ChangeType) -> Attribute {
    Attribute { name: name.to_string(), value, change: change.into() }
}

pub fn word_attribute(name: &str, value: &Word, change: ChangeType) -> Attribute {
    attribute(name, value.to_vec(), change)
}

/// A tracked word as its attribute: raw 32 bytes, except the proxies' access controllers, which the
/// schema carries as 20-byte addresses like the other `feed:<f>:*` address values.
pub fn tracked_attribute(name: &str, value: &Word, change: ChangeType) -> Attribute {
    if name.starts_with("feed:") && name.ends_with(":access_controller") {
        attribute(name, keys::address_in_word(value).to_vec(), change)
    } else {
        word_attribute(name, value, change)
    }
}

pub fn deleted_attribute(name: &str) -> Attribute {
    attribute(name, Vec::new(), ChangeType::Deletion)
}

/// Pads a storage value (Firehose strips leading zeros) to the 32-byte word the decoder expects.
pub fn pad_word(value: &[u8]) -> Word {
    let mut w = [0u8; 32];
    let take = value.len().min(32);
    w[32 - take..].copy_from_slice(&value[value.len() - take..]);
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_ids_follow_the_design() {
        let pool = parse_address("0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572").unwrap();
        assert_eq!(swap_component_id(&pool), "0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572");
        assert_eq!(
            lever_up_component_id(&pool),
            "0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572000000000000000000000001"
        );
    }

    #[test]
    fn pool_config_round_trips() {
        let cfg = PoolConfig {
            pool: [1u8; 20],
            hook: [2u8; 20],
            leverage_hook: [3u8; 20],
            spread_hook: [4u8; 20],
            router: [5u8; 20],
            price_feed: [6u8; 20],
            factory: [7u8; 20],
            pool_asset: [8u8; 20],
            loan_assets: vec![[9u8; 20]],
            venues: vec![VenueConfig {
                account: [10u8; 20],
                market_id: [11u8; 32],
                morpho: [12u8; 20],
                irm: [13u8; 20],
                oracle: [14u8; 20],
            }],
            feeds: vec![
                FeedConfig { role: "asset".into(), proxy: [15u8; 20] },
                FeedConfig { role: "mo0".into(), proxy: [16u8; 20] },
            ],
        };
        assert_eq!(PoolConfig::parse(&cfg.serialize()).unwrap(), cfg);
        assert_eq!(cfg.storage_contracts().len(), 7);
        let words = cfg.tracked_words();
        assert!(words
            .iter()
            .any(|(a, _, n)| *a == [12u8; 20] && n == "mm:0:position:1"));
        assert!(words
            .iter()
            .any(|(a, _, n)| *a == [16u8; 20] && n == "feed:mo0:access_controller"));
    }
}
