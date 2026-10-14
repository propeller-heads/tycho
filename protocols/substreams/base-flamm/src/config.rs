// Copyright (c) 2026 Everlong Labs Limited
//! The manifest params. One string, shared by every module, `key=value` pairs joined by `&`:
//!
//! | key | value | what |
//! |---|---|---|
//! | `factory` | address | the `FLAMMFactory` whose `PoolCreated` logs create components |
//! | `deployments` | `role:0x…,…` | runtime codehash → role registry: contracts created with one of these codes are tracked from their creation, and a pool is admitted only when each of its contracts was (see [`crate::flamm::Role`]); the `hook` entries are therefore the invariant-hook allowlist |
//! | `aggregators` | `0x…:kind,…` | Chainlink aggregator address → layout kind (`ocr2`, `uptime`, `dual`); every storage write of a listed aggregator is tracked from `initialBlock`, its round words having per-round keys |
//! | `words` | `0x<addr>:0x<slot>:0x<value>,…` | the seeds: external words as of `initialBlock - 1`, tracked from `initialBlock` |
//! | `immutables` | `0x<pool>:name=0x…;name=0x…,…` | per pool, the static attributes that are immutables of contracts the stream cannot read (asserted by the range test) |
//!
//! Every value is verifiable with `eth_call` / `eth_getStorageAt` / `eth_getCode` at `initialBlock
//! - 1`; the README lists the call behind each one.
use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};

use crate::flamm::{
    feeds::FeedKind,
    keys::{parse_address, parse_word, Address, Word},
    Role,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub factory: Address,
    pub deployments: HashMap<Word, Role>,
    pub aggregators: HashMap<Address, FeedKind>,
    pub words: HashMap<(Address, Word), Word>,
    pub immutables: HashMap<Address, Vec<(String, Vec<u8>)>>,
}

impl Config {
    pub fn parse(params: &str) -> Result<Self> {
        let mut cfg = Self::default();
        let mut seen_factory = false;
        for pair in params
            .split('&')
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("param `{pair}` is not key=value"))?;
            match key {
                "factory" => {
                    cfg.factory = parse_address(value)?;
                    seen_factory = true;
                }
                "deployments" => {
                    for entry in list(value) {
                        let (role, codehash) = entry
                            .split_once(':')
                            .ok_or_else(|| anyhow!("deployment `{entry}` is not role:codehash"))?;
                        let role = Role::parse(role)?;
                        let codehash = parse_word(codehash)?;
                        if let Some(previous) = cfg.deployments.insert(codehash, role) {
                            bail!("codehash {codehash:?} listed for both {previous} and {role}");
                        }
                    }
                }
                "aggregators" => {
                    for entry in list(value) {
                        let (address, kind) = entry
                            .split_once(':')
                            .ok_or_else(|| anyhow!("aggregator `{entry}` is not address:kind"))?;
                        cfg.aggregators
                            .insert(parse_address(address)?, FeedKind::parse(kind)?);
                    }
                }
                "words" => {
                    for entry in list(value) {
                        let parts: Vec<&str> = entry.split(':').collect();
                        if parts.len() != 3 {
                            bail!("seed `{entry}` is not address:slot:value");
                        }
                        cfg.words.insert(
                            (parse_address(parts[0])?, parse_word(parts[1])?),
                            parse_word(parts[2])?,
                        );
                    }
                }
                "immutables" => {
                    for entry in list(value) {
                        let (pool, fields) = entry
                            .split_once(':')
                            .ok_or_else(|| anyhow!("immutables `{entry}` is not pool:fields"))?;
                        let fields = fields
                            .split(';')
                            .filter(|f| !f.is_empty())
                            .map(|f| {
                                let (name, hex_value) = f
                                    .split_once('=')
                                    .ok_or_else(|| anyhow!("immutable `{f}` is not name=value"))?;
                                let raw = hex::decode(
                                    hex_value
                                        .strip_prefix("0x")
                                        .unwrap_or(hex_value),
                                )?;
                                Ok((name.to_string(), raw))
                            })
                            .collect::<Result<Vec<_>>>()?;
                        cfg.immutables
                            .insert(parse_address(pool)?, fields);
                    }
                }
                other => bail!("unknown base-flamm param `{other}`"),
            }
        }
        if !seen_factory {
            bail!("the `factory` param is required");
        }
        Ok(cfg)
    }

    /// The seeded `(address, slot)` set as a lookup.
    pub fn seeded(&self, address: &Address, key: &Word) -> bool {
        self.words
            .contains_key(&(*address, *key))
    }

    /// Whether every storage write of `address` is tracked. An aggregator's rounds, ring and
    /// access pair are read from its words at per-round keys the manifest cannot enumerate (the
    /// creation snapshot, a rotation to it, the ring window at the start of a block), so listing
    /// a layout kind for it is what tracks its writes; nothing else is read that way.
    pub fn tracks_address(&self, address: &Address) -> bool {
        self.aggregators.contains_key(address)
    }
}

fn list(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_param() {
        let cfg = Config::parse(
            "factory=0x1bfce014774d0dd7e04bc595d46fa09f7dccf45f\
             &deployments=hook:0x63ca81587b713df89dc9a657dae2cb5a70910cbafbe23b9ebbbdedc1bfdc3e1d,\
             router:0x6ca3c38096320c757612b113e543c37862f48bce5a2f19de2375357bc31ddcc8\
             &aggregators=0x51ce3091cf646587e02cad83b580992f8723e718:ocr2,\
             0x606c6ecbd272e2174f6710b5974f23fe9899602e:uptime,0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1:dual\
             &words=0xbbbbbbbbbb9cc5e90e3b3af64bdaf62c37eeffcb:\
             0xb37d8d77c527a1e411d2abd81f103dee202b9a350a1fcbf567227a9222316a6a:\
             0x000000000000004c867b33d58d6db8c4000000000000000000058a99d36cf553\
             &immutables=0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572:hook_loan_scale=0xe8d4a51000;\
             factory_upgrade_delay=0x02a300",
        )
        .unwrap();
        assert_eq!(hex::encode(cfg.factory), "1bfce014774d0dd7e04bc595d46fa09f7dccf45f");
        assert_eq!(cfg.deployments.len(), 2);
        assert_eq!(
            cfg.deployments[&parse_word(
                "0x63ca81587b713df89dc9a657dae2cb5a70910cbafbe23b9ebbbdedc1bfdc3e1d"
            )
            .unwrap()],
            Role::Hook
        );
        assert_eq!(
            cfg.deployments[&parse_word(
                "0x6ca3c38096320c757612b113e543c37862f48bce5a2f19de2375357bc31ddcc8"
            )
            .unwrap()],
            Role::Router
        );
        assert_eq!(cfg.aggregators.len(), 3);
        assert_eq!(
            cfg.aggregators[&parse_address("0xe5ec87a39445b8d5b751b116802a53c5ae7e9df1").unwrap()],
            FeedKind::Dual
        );
        assert!(cfg
            .tracks_address(&parse_address("0x51ce3091cf646587e02cad83b580992f8723e718").unwrap()));
        assert!(!cfg
            .tracks_address(&parse_address("0x1bfce014774d0dd7e04bc595d46fa09f7dccf45f").unwrap()));
        assert_eq!(cfg.words.len(), 1);
        let pool = parse_address("0xc0fdcb1799ccc2cebaa1fe247157b0df33d57572").unwrap();
        assert_eq!(cfg.immutables[&pool].len(), 2);
        assert_eq!(
            cfg.immutables[&pool][0],
            ("hook_loan_scale".to_string(), hex::decode("e8d4a51000").unwrap())
        );
    }

    #[test]
    fn rejects_unknown_and_missing() {
        assert!(
            Config::parse("aggregators=0x51ce3091cf646587e02cad83b580992f8723e718:ocr2").is_err()
        );
        assert!(
            Config::parse("factory=0x1bfce014774d0dd7e04bc595d46fa09f7dccf45f&bogus=1").is_err()
        );
        assert!(Config::parse(
            "factory=0x1bfce014774d0dd7e04bc595d46fa09f7dccf45f&deployments=nope:0x01"
        )
        .is_err());
        assert!(Config::parse(
            "factory=0x1bfce014774d0dd7e04bc595d46fa09f7dccf45f\
             &deployments=hook:0x63ca81587b713df89dc9a657dae2cb5a70910cbafbe23b9ebbbdedc1bfdc3e1d,\
             router:0x63ca81587b713df89dc9a657dae2cb5a70910cbafbe23b9ebbbdedc1bfdc3e1d"
        )
        .is_err());
    }
}
