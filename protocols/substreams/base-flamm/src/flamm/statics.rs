// Copyright (c) 2026 Everlong Labs Limited
//! The two components a `PoolCreated` log creates and their static attributes (schema 3.3).
//!
//! Every static attribute has one of three sources, listed per attribute in the README: the
//! creation log and calldata, the storage the creation transaction wrote (read through the word
//! view), the deployments registry (runtime codehashes of contracts created in the indexed range),
//! or the manifest `immutables` for values that live in bytecode. A pool is refused, with the
//! reason logged, when any source is missing: the decoder needs the full set, so a partial
//! component would only fail later.
use anyhow::{anyhow, bail, Result};
use tycho_substreams::prelude::{ImplementationType, ProtocolComponent};

use crate::{
    config::Config,
    flamm::{
        calldata::{CreatePool, PoolCreated},
        feeds,
        keys::{self, address_in_word, hex_address, Address, Word},
        words::WordView,
        FeedConfig, PoolConfig, Role, VenueConfig, PROTOCOL_TYPE_NAME,
    },
};

/// The immutables a pool's manifest entry must carry, in attribute order.
pub const REQUIRED_IMMUTABLES: [&str; 13] = [
    "hook_loan_scale",
    "hook_genesis_strategy_hash",
    "hook_genesis_params_hash",
    "leverage_hook_loan_scale",
    "leverage_hook_swap_hook",
    "price_feed_sequencer",
    "price_feed_sequencer_grace",
    "factory_upgrade_delay",
    "venue_0_oracle_scale_factor",
    "venue_0_oracle_base_feed_1",
    "feed_mo0_secondary_proxy",
    "feed_mo0_max_sync_iterations",
    "irm_codehash",
];

pub const COMPONENT_KIND_SWAP: u64 = 0;
pub const COMPONENT_KIND_LEVER_UP: u64 = 1;

/// The venue kind `MorphoBlueAccount` implements (`IMMRouter.VENUE_KIND_MORPHO_BLUE`).
pub const VENUE_KIND_MORPHO_BLUE: u8 = 0;

#[derive(Debug)]
pub struct Creation {
    pub config: PoolConfig,
    pub static_attributes: Vec<(String, Vec<u8>)>,
}

/// Builds the pool's tracking config and static attributes, or explains why the pool is refused.
pub fn creation(
    event: &PoolCreated,
    call: &CreatePool,
    config: &Config,
    deployment: &impl Fn(&Address) -> Option<(Role, Word)>,
    view: &WordView<'_>,
    tx_index: u64,
) -> Result<Creation> {
    if event.invariant_hook != call.hooks.invariant_hook {
        bail!("PoolCreated.invariantHook differs from the createPool calldata");
    }
    let immutables = config
        .immutables
        .get(&event.pool)
        .ok_or_else(|| anyhow!("no `immutables` entry for pool {}", hex_address(&event.pool)))?;
    for name in REQUIRED_IMMUTABLES {
        if !immutables
            .iter()
            .any(|(n, _)| n == name)
        {
            bail!("immutables entry for {} lacks `{name}`", hex_address(&event.pool));
        }
    }
    let immutable = |name: &str| -> Vec<u8> {
        immutables
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let codehash = |address: &Address, role: Role| -> Result<Word> {
        match deployment(address) {
            Some((r, h)) if r == role => Ok(h),
            Some((r, _)) => Err(anyhow!("{} was created as {r}, not {role}", hex_address(address))),
            None => Err(anyhow!(
                "{role} {} was not created with a registered code in the indexed range",
                hex_address(address)
            )),
        }
    };

    let pool = event.pool;
    let pool_word = |n: u64| -> Result<Word> {
        view.at(&pool, &keys::add(&keys::FLAMM_NS, n), tx_index)
            .ok_or_else(|| anyhow!("pool namespace word {n} not written at creation"))
    };
    let router = address_in_word(&pool_word(1)?);
    let price_feed = address_in_word(&pool_word(2)?);
    if price_feed != call.price_feed {
        bail!("pool priceFeed differs from the createPool calldata");
    }
    let hook = event.invariant_hook;
    let leverage_hook = call.hooks.leverage_hook;
    let spread_hook = call.hooks.spread_hook;
    // The leverage roles are bound as a pair or not at all (`FLAMMOpsLib._checkHookSet`,
    // FLAMMOpsLib.sol:209); the pool without them is valid, its lever paths revert
    // `LeverageDisabled` (FLAMMLeverLib.sol:81). Its words 24 and 25 are then never written (a
    // zero written over zero is no storage change), which reads as zero for a contract tracked
    // from its creation.
    let zero = [0u8; 20];
    if (leverage_hook == zero) != (spread_hook == zero) {
        bail!("leverage and spread hooks are not bound as a pair");
    }
    let stored_hook = |n: u64| -> Address {
        view.at(&pool, &keys::add(&keys::FLAMM_NS, n), tx_index)
            .map(|w| address_in_word(&w))
            .unwrap_or(zero)
    };
    if leverage_hook != stored_hook(24) || spread_hook != stored_hook(25) {
        bail!("pool leverage/spread hooks differ from the createPool calldata");
    }

    let implementation_codehash = codehash(&event.implementation, Role::Implementation)?;
    // The invariant-hook allowlist: the hook must have been created in the indexed range with a
    // code the manifest registers under `hook`, and the factory's own read of it
    // (`PoolCreated.invariantCodehash` = `hooks.invariantHook.codehash`, FLAMMFactory.sol:264)
    // must be that code. A pool on any other hook code is refused here.
    let hook_codehash = codehash(&hook, Role::Hook)?;
    if hook_codehash != event.invariant_codehash {
        bail!("hook code at creation differs from PoolCreated.invariantCodehash");
    }
    // A pool without the leverage pair carries zero hooks and zero codehashes; the decoder refuses
    // its lever-up component on the zero hook (`LeverageDisabled`), and `storage_contracts` tracks
    // no spread hook for it.
    let optional_codehash = |address: &Address, role: Role| -> Result<Word> {
        if *address == zero {
            Ok([0u8; 32])
        } else {
            codehash(address, role)
        }
    };
    let leverage_hook_codehash = optional_codehash(&leverage_hook, Role::LeverageHook)?;
    let spread_hook_codehash = optional_codehash(&spread_hook, Role::SpreadHook)?;
    let router_codehash = codehash(&router, Role::Router)?;
    codehash(&price_feed, Role::PriceFeed)?;
    codehash(&config.factory, Role::Factory)?;

    // Venue 0: the Morpho Blue account the factory bound (PoolCreated.account), its market from the
    // calldata. `initializeHooks` registers every entry of the array (FLAMMOpsLib.sol:177-179), but
    // the package models one venue: the event carries venue 0's account alone
    // (`ROUTER.accountOf(pool, 0, 0)`, FLAMMFactory.sol:245), the static attributes and
    // `pool_config_from_component` are `venue_0_*`, and `untracked_external_words` can only
    // name the venues the config carries. A further venue would therefore be indexed with its
    // Morpho market, position and IRM words untracked and its collateral and recognized supply
    // missing from the balances, so such a pool is refused until a package update models it.
    let Some(venue0) = call.venues.first() else { bail!("pool has no financing venue") };
    if call.venues.len() != 1 {
        bail!("pool has {} financing venues, and the package models one", call.venues.len());
    }
    if venue0.kind != VENUE_KIND_MORPHO_BLUE {
        bail!("venue 0 kind {} is not Morpho Blue", venue0.kind);
    }
    let account = event.account;
    let account_codehash = codehash(&account, Role::Account)?;
    let market_id = venue0.market_id();
    let params = venue0
        .market_params()
        .ok_or_else(|| anyhow!("venue 0 params do not decode as MarketParams"))?;
    let account_word = |key: &Word, what: &str| -> Result<Word> {
        view.at(&account, key, tx_index)
            .ok_or_else(|| anyhow!("account {what} not written at creation"))
    };
    let morpho = address_in_word(&account_word(&keys::slot(4), "MORPHO")?);
    let [market0, market1] = keys::account_market_keys(&market_id);
    let oracle = address_in_word(&account_word(&market0, "_markets[id].oracle")?);
    let irm = address_in_word(&account_word(&market1, "_markets[id].irm")?);
    if oracle != params.oracle || irm != params.irm {
        bail!("account market registration differs from the venue params");
    }
    let router_venue_id = view
        .at(&router, &keys::add(&keys::router_venue(&pool, 0), 1), tx_index)
        .ok_or_else(|| anyhow!("router venues[0].id not written at creation"))?;
    if router_venue_id != market_id {
        bail!("router venues[0].id differs from keccak(venue params)");
    }

    let feed_proxy = |token: &Address| -> Result<Address> {
        let [head, _] = keys::pricefeed_token_keys(token);
        view.at(&price_feed, &head, tx_index)
            .map(|w| address_in_word(&w))
            .filter(|a| *a != [0u8; 20])
            .ok_or_else(|| anyhow!("PriceFeed lists no aggregator for {}", hex_address(token)))
    };
    let feed_asset_proxy = feed_proxy(&call.pool_asset)?;
    let feed_loan0_proxy = feed_proxy(&call.loan_asset)?;
    let feed_seq_proxy = address_in_word(&immutable("price_feed_sequencer"));
    let feed_mo0_proxy = address_in_word(&immutable("venue_0_oracle_base_feed_1"));
    if feed_seq_proxy == [0u8; 20] || feed_mo0_proxy == [0u8; 20] {
        bail!("sequencer / oracle base feed immutables are zero");
    }

    let pool_config = PoolConfig {
        pool,
        hook,
        leverage_hook,
        spread_hook,
        router,
        price_feed,
        factory: config.factory,
        pool_asset: call.pool_asset,
        loan_assets: vec![call.loan_asset],
        venues: vec![VenueConfig { account, market_id, morpho, irm, oracle }],
        feeds: vec![
            FeedConfig { role: "asset".into(), proxy: feed_asset_proxy },
            FeedConfig { role: "loan0".into(), proxy: feed_loan0_proxy },
            FeedConfig { role: "seq".into(), proxy: feed_seq_proxy },
            FeedConfig { role: "mo0".into(), proxy: feed_mo0_proxy },
        ],
    };
    let untracked = untracked_external_words(&pool_config, config, view, tx_index);
    if !untracked.is_empty() {
        bail!("external words the manifest does not track: {}", untracked.join(", "));
    }

    let a = |v: &Address| v.to_vec();
    let w = |v: &Word| v.to_vec();
    let static_attributes: Vec<(String, Vec<u8>)> = vec![
        ("implementation".into(), a(&event.implementation)),
        ("implementation_codehash".into(), w(&implementation_codehash)),
        ("hook".into(), a(&hook)),
        ("hook_codehash".into(), w(&hook_codehash)),
        ("hook_loan_scale".into(), immutable("hook_loan_scale")),
        ("hook_genesis_strategy_hash".into(), immutable("hook_genesis_strategy_hash")),
        ("hook_genesis_params_hash".into(), immutable("hook_genesis_params_hash")),
        ("leverage_hook".into(), a(&leverage_hook)),
        ("leverage_hook_codehash".into(), w(&leverage_hook_codehash)),
        ("leverage_hook_loan_scale".into(), immutable("leverage_hook_loan_scale")),
        ("leverage_hook_swap_hook".into(), immutable("leverage_hook_swap_hook")),
        ("spread_hook".into(), a(&spread_hook)),
        ("spread_hook_codehash".into(), w(&spread_hook_codehash)),
        ("router".into(), a(&router)),
        ("router_codehash".into(), w(&router_codehash)),
        ("price_feed".into(), a(&price_feed)),
        ("price_feed_sequencer".into(), immutable("price_feed_sequencer")),
        ("price_feed_sequencer_grace".into(), immutable("price_feed_sequencer_grace")),
        ("factory".into(), a(&config.factory)),
        ("factory_upgrade_delay".into(), immutable("factory_upgrade_delay")),
        ("pool_asset".into(), a(&call.pool_asset)),
        ("loan_asset_0".into(), a(&call.loan_asset)),
        ("venue_0_account".into(), a(&account)),
        ("venue_0_account_codehash".into(), w(&account_codehash)),
        ("venue_0_market_id".into(), w(&market_id)),
        ("venue_0_morpho".into(), a(&morpho)),
        ("venue_0_irm".into(), a(&irm)),
        ("venue_0_oracle".into(), a(&oracle)),
        ("venue_0_oracle_scale_factor".into(), immutable("venue_0_oracle_scale_factor")),
        ("venue_0_oracle_base_feed_1".into(), immutable("venue_0_oracle_base_feed_1")),
        ("feed_mo0_secondary_proxy".into(), immutable("feed_mo0_secondary_proxy")),
        ("feed_mo0_max_sync_iterations".into(), immutable("feed_mo0_max_sync_iterations")),
        ("irm_codehash".into(), immutable("irm_codehash")),
        ("feed_asset_proxy".into(), a(&feed_asset_proxy)),
        ("feed_loan0_proxy".into(), a(&feed_loan0_proxy)),
        ("feed_seq_proxy".into(), a(&feed_seq_proxy)),
        ("feed_mo0_proxy".into(), a(&feed_mo0_proxy)),
    ];
    Ok(Creation { config: pool_config, static_attributes })
}

/// The external words a pool's quotes read that the stream cannot discover on its own, and that
/// the manifest must therefore track: each venue's Morpho market and position words and IRM rate
/// (`words` seeds), each feed proxy's rotation and access-controller words (`words` seeds), and
/// the aggregator currently behind each proxy (its layout kind in `aggregators`, which is also
/// what tracks every write of it). `store_words` keeps nothing else outside the registered
/// contracts, so a pool with an untracked word would have it valued as unknown in every block
/// after its creation: the balances would miss the venue, and a feed behind an unlisted
/// aggregator would carry `aggregator` / `phase` only and never quote. Such a pool is refused
/// until a package update seeds its words; the returned names say which.
pub fn untracked_external_words(
    cfg: &PoolConfig,
    config: &Config,
    view: &WordView<'_>,
    tx_index: u64,
) -> Vec<String> {
    let mut words: Vec<(Address, Word, String)> = Vec::new();
    for (v, venue) in cfg.venues.iter().enumerate() {
        for (i, key) in keys::morpho_market_keys(&venue.market_id)
            .into_iter()
            .enumerate()
        {
            words.push((venue.morpho, key, format!("mm:{v}:market:{i}")));
        }
        for (i, key) in keys::morpho_position_keys(&venue.market_id, &venue.account)
            .into_iter()
            .enumerate()
        {
            words.push((venue.morpho, key, format!("mm:{v}:position:{i}")));
        }
        words.push((
            venue.irm,
            keys::irm_rate_key(&venue.market_id),
            format!("irm:{v}:rate_at_target"),
        ));
    }
    for feed in &cfg.feeds {
        words.push((
            feed.proxy,
            keys::slot(keys::PROXY_PHASE_SLOT),
            format!("feed:{}:aggregator", feed.role),
        ));
        words.push((
            feed.proxy,
            keys::slot(keys::PROXY_ACCESS_CONTROLLER_SLOT),
            format!("feed:{}:access_controller", feed.role),
        ));
    }
    let mut out: Vec<String> = words
        .into_iter()
        .filter(|(address, key, _)| !config.seeded(address, key))
        .map(|(_, _, name)| name)
        .collect();
    for feed in &cfg.feeds {
        let aggregator = view
            .at(&feed.proxy, &keys::slot(keys::PROXY_PHASE_SLOT), tx_index)
            .map(|w| feeds::phase_and_aggregator(&w).1);
        if let Some(a) = aggregator {
            if !config.aggregators.contains_key(&a) {
                out.push(format!("feed:{}:kind ({})", feed.role, hex_address(&a)));
            }
        }
    }
    out
}

/// The swap and lever-up components of a created pool.
///
/// `contracts` is empty: the indexer resolves every listed address against the accounts the
/// stream created (`tycho-storage` `add_protocol_components` joins `contract_code` with `account`
/// and fails the block's write with `NotFound("Account")` otherwise), and a native integration
/// creates no accounts, its state being the components' attributes. The pool's contracts are the
/// address-valued static attributes (`hook`, `router`, `price_feed`, …).
pub fn components(creation: &Creation) -> [ProtocolComponent; 2] {
    let cfg = &creation.config;
    let tokens = cfg.tokens();
    let build = |id: String, kind: u64| {
        let mut attributes = creation.static_attributes.clone();
        attributes.push(("component_kind".into(), keys::word_from_u64(kind).to_vec()));
        ProtocolComponent::new(&id)
            .with_tokens(&tokens)
            .with_attributes(&attributes)
            .as_swap_type(PROTOCOL_TYPE_NAME, ImplementationType::Custom)
    };
    let [swap, lever] = cfg.component_ids();
    [build(swap, COMPONENT_KIND_SWAP), build(lever, COMPONENT_KIND_LEVER_UP)]
}

/// Rebuilds the tracking config from a component's static attributes (the pools store keeps the
/// serialized form; this is for the block that creates the pool, where only the component is at
/// hand).
pub fn pool_config_from_component(component: &ProtocolComponent) -> Result<PoolConfig> {
    let get = |name: &str| -> Result<Vec<u8>> {
        component
            .get_attribute_value(name)
            .ok_or_else(|| anyhow!("component {} lacks `{name}`", component.id))
    };
    let addr = |name: &str| -> Result<Address> { Ok(address_in_word(&get(name)?)) };
    let word = |name: &str| -> Result<Word> { Ok(crate::flamm::pad_word(&get(name)?)) };
    let id = hex::decode(
        component
            .id
            .strip_prefix("0x")
            .unwrap_or(&component.id),
    )?;
    let pool = address_in_word(
        id.get(..20)
            .ok_or_else(|| anyhow!("component id shorter than 20 bytes"))?,
    );
    Ok(PoolConfig {
        pool,
        hook: addr("hook")?,
        leverage_hook: addr("leverage_hook")?,
        spread_hook: addr("spread_hook")?,
        router: addr("router")?,
        price_feed: addr("price_feed")?,
        factory: addr("factory")?,
        pool_asset: addr("pool_asset")?,
        loan_assets: vec![addr("loan_asset_0")?],
        venues: vec![VenueConfig {
            account: addr("venue_0_account")?,
            market_id: word("venue_0_market_id")?,
            morpho: addr("venue_0_morpho")?,
            irm: addr("venue_0_irm")?,
            oracle: addr("venue_0_oracle")?,
        }],
        feeds: ["asset", "loan0", "seq", "mo0"]
            .into_iter()
            .map(|role| {
                Ok(FeedConfig {
                    role: role.to_string(),
                    proxy: addr(&format!("feed_{role}_proxy"))?,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    })
}
