//! Realistic concurrent client startups against one Tycho RPC host.
//!
//! Every startup picks a client profile (TVL threshold, protocol set, page and chunk sizes, old
//! or new client) from the plan by weight; the component and entry point id order is shuffled
//! per startup, as a real process iterating its HashMap does, while contract ids are sorted as
//! tycho-client sorts them; startups can start spread over a window instead of at one instant;
//! and every startup pins the block at its own start, or an older block up to the plan's
//! version-age limit. Every request is recorded with its timings, status and size.
//!
//! usage: startup-loadgen <plan.json> <n> <base_block> <out.json>
//! env:   TYCHO_API_KEY     API key sent as the Authorization header (required)
//!        SPREAD_S=0        start offsets drawn uniformly over this many seconds
//!        BLOCK_TIME_S=12   a startup that starts t seconds later pins base_block + t/BLOCK_TIME_S
//!        VERSION_AGE_BLOCKS=0  every startup pins this many blocks behind its start block
//!        PROFILE=<name>    force one profile for every startup
//!        SEED=1            shuffle seed
//!        SINGLE_ID_CALLS=0 per protocol, this many one-id components+state request pairs
use std::{sync::Arc, time::Instant};

use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{Mutex, Semaphore};

const CONCURRENCY: usize = 4;
const TOKENS_PAGE: usize = 12900;
const TIMEOUT_S: u64 = 300;

#[derive(Deserialize)]
struct Plan {
    chain: String,
    url: String,
    /// Sent as `X-User-Plan` when set: on a path without an auth-proxy the indexer applies this
    /// plan's restrictions from the header.
    #[serde(default)]
    user_plan: Option<String>,
    token_pages: usize,
    profiles: Vec<Profile>,
}

#[derive(Deserialize, Clone)]
struct Profile {
    name: String,
    weight: usize,
    tvl_gt: f64,
    comp_page_size: usize,
    state_chunk: usize,
    contract_chunk: usize,
    ep_chunk: usize,
    /// tycho-client sorts contract ids before chunking (`get_contract_state_paginated`); other
    /// clients send them in their own order.
    #[serde(default = "yes")]
    sort_contracts: bool,
    protocols: Vec<Protocol>,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize, Clone)]
struct Protocol {
    system: String,
    dci: bool,
    component_pages: usize,
    component_ids: Vec<String>,
    contract_ids: Vec<String>,
}

#[derive(Serialize, Clone)]
struct Record {
    startup: usize,
    profile: String,
    system: String,
    endpoint: &'static str,
    start_s: f64,
    ttfb_s: f64,
    total_s: f64,
    status: u16,
    bytes: u64,
    error: Option<String>,
}

struct Ctx {
    client: reqwest::Client,
    url: String,
    key: String,
    /// Sent as `X-User-Plan` when set (see `Plan::user_plan`).
    plan: Option<String>,
    t0: Instant,
    startup: usize,
    profile: String,
    records: Arc<Mutex<Vec<Record>>>,
}

/// xorshift64*, enough for a reproducible shuffle without a dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

impl Ctx {
    async fn post(&self, system: &str, endpoint: &'static str, body: Value) {
        let start = self.t0.elapsed().as_secs_f64();
        let t = Instant::now();
        let mut rec = Record {
            startup: self.startup,
            profile: self.profile.clone(),
            system: system.to_string(),
            endpoint,
            start_s: start,
            ttfb_s: 0.0,
            total_s: 0.0,
            status: 0,
            bytes: 0,
            error: None,
        };
        let mut req = self
            .client
            .post(format!("{}/v1/{}", self.url, endpoint))
            .header("Authorization", &self.key)
            .header("Content-Type", "application/json")
            .header("Accept-Encoding", "zstd");
        if let Some(plan) = &self.plan {
            req = req.header("X-User-Plan", plan);
        }
        let res = req.json(&body).send().await;
        match res {
            Ok(mut resp) => {
                rec.ttfb_s = t.elapsed().as_secs_f64();
                rec.status = resp.status().as_u16();
                loop {
                    match resp.chunk().await {
                        Ok(Some(c)) => rec.bytes += c.len() as u64,
                        Ok(None) => break,
                        Err(e) => {
                            rec.error = Some(format!("body: {e}"));
                            break;
                        }
                    }
                }
            }
            Err(e) => rec.error = Some(format!("send: {e}")),
        }
        rec.total_s = t.elapsed().as_secs_f64();
        self.records.lock().await.push(rec);
    }

    async fn fan_out(&self, system: &str, endpoint: &'static str, bodies: Vec<Value>) {
        let sem = Arc::new(Semaphore::new(CONCURRENCY));
        join_all(bodies.into_iter().map(|b| {
            let sem = sem.clone();
            async move {
                let _p = sem.acquire().await.unwrap();
                self.post(system, endpoint, b).await;
            }
        }))
        .await;
    }
}

struct Knobs {
    version_age_blocks: u64,
    single_id_calls: usize,
}

async fn startup(
    ctx: Ctx,
    plan: Arc<Plan>,
    profile: Profile,
    mut rng: Rng,
    block: u64,
    knobs: Arc<Knobs>,
) -> Value {
    let chain = plan.chain.as_str();
    let block = block.saturating_sub(knobs.version_age_blocks);
    let version = json!({"block": {"chain": chain, "number": block}});
    let pages = |n: usize, size: usize| -> Vec<Value> {
        (0..n.max(1))
            .map(|p| json!({"page": p, "page_size": size}))
            .collect()
    };
    let t_start = ctx.t0.elapsed().as_secs_f64();

    let tokens: Vec<Value> = pages(plan.token_pages, TOKENS_PAGE)
        .into_iter()
        .map(|p| json!({"chain": chain, "min_quality": 100, "traded_n_days_ago": 30, "pagination": p}))
        .collect();
    ctx.post("", "tokens", tokens[0].clone())
        .await;
    ctx.fan_out("", "tokens", tokens[1..].to_vec())
        .await;
    let tokens_s = ctx.t0.elapsed().as_secs_f64() - t_start;

    // Each protocol gets its own shuffled order, like a HashMap per tracker.
    let shuffled: Vec<(Protocol, Vec<String>, Vec<String>)> = profile
        .protocols
        .iter()
        .map(|p| {
            let mut comps = p.component_ids.clone();
            rng.shuffle(&mut comps);
            let mut contracts = p.contract_ids.clone();
            if profile.sort_contracts {
                contracts.sort();
            } else {
                rng.shuffle(&mut contracts);
            }
            (p.clone(), comps, contracts)
        })
        .collect();

    let per_protocol = shuffled.iter().map(|(p, comps, contracts)| {
        let ctx = &ctx;
        let version = version.clone();
        let profile = &profile;
        let knobs = knobs.clone();
        async move {
            let sys = p.system.as_str();
            let comp_pages: Vec<Value> = pages(p.component_pages, profile.comp_page_size)
                .into_iter()
                .map(|pg| json!({"chain": chain, "protocol_system": sys, "tvl_gt": profile.tvl_gt, "pagination": pg}))
                .collect();
            ctx.post(sys, "protocol_components", comp_pages[0].clone()).await;
            ctx.fan_out(sys, "protocol_components", comp_pages[1..].to_vec()).await;
            if comps.is_empty() {
                return json!({"system": sys, "done_s": ctx.t0.elapsed().as_secs_f64() - t_start});
            }
            if p.dci {
                let eps = comps
                    .chunks(profile.ep_chunk)
                    .map(|c| json!({"chain": chain, "protocol_system": sys, "component_ids": c,
                                    "pagination": {"page": 0, "page_size": profile.ep_chunk}}))
                    .collect();
                ctx.fan_out(sys, "traced_entry_points", eps).await;
            }
            let states = comps
                .chunks(profile.state_chunk)
                .map(|c| json!({"chain": chain, "protocol_system": sys, "protocol_ids": c,
                                "include_balances": true, "version": version,
                                "pagination": {"page": 0, "page_size": profile.state_chunk}}))
                .collect();
            ctx.fan_out(sys, "protocol_state", states).await;
            if !contracts.is_empty() {
                let bodies = contracts
                    .chunks(profile.contract_chunk)
                    .map(|c| json!({"chain": chain, "protocol_system": sys, "contract_ids": c,
                                    "version": version, "pagination": {"page": 0, "page_size": profile.contract_chunk}}))
                    .collect();
                ctx.fan_out(sys, "contract_state", bodies).await;
            }
            // Single-id lookups, the pattern of clients that poll one pool at a time.
            if knobs.single_id_calls > 0 {
                let mut bodies = vec![];
                for id in comps.iter().cycle().take(knobs.single_id_calls) {
                    bodies.push(json!({"chain": chain, "protocol_system": sys, "component_ids": [id],
                                       "pagination": {"page": 0, "page_size": 500}}));
                }
                ctx.fan_out(sys, "protocol_components", bodies).await;
                let bodies = comps
                    .iter()
                    .cycle()
                    .take(knobs.single_id_calls)
                    .map(|id| json!({"chain": chain, "protocol_system": sys, "protocol_ids": [id],
                                     "include_balances": true, "version": version,
                                     "pagination": {"page": 0, "page_size": 100}}))
                    .collect();
                ctx.fan_out(sys, "protocol_state", bodies).await;
            }
            json!({"system": sys, "done_s": ctx.t0.elapsed().as_secs_f64() - t_start})
        }
    });
    let systems = join_all(per_protocol).await;
    json!({"startup": ctx.startup, "profile": profile.name, "offset_s": t_start, "block": block,
           "tokens_s": tokens_s, "ready_s": ctx.t0.elapsed().as_secs_f64() - t_start, "systems": systems})
}

#[tokio::main]
async fn main() {
    let a: Vec<String> = std::env::args().collect();
    let plan: Arc<Plan> =
        Arc::new(serde_json::from_str(&std::fs::read_to_string(&a[1]).unwrap()).unwrap());
    let n: usize = a[2].parse().unwrap();
    let base_block: u64 = a[3].parse().unwrap();
    let env_f = |k: &str, d: f64| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let spread_s = env_f("SPREAD_S", 0.0);
    let block_time_s = env_f("BLOCK_TIME_S", 12.0);
    let seed = env_f("SEED", 1.0) as u64;
    let knobs = Arc::new(Knobs {
        version_age_blocks: env_f("VERSION_AGE_BLOCKS", 0.0) as u64,
        single_id_calls: env_f("SINGLE_ID_CALLS", 0.0) as usize,
    });
    let forced = std::env::var("PROFILE").ok();
    let key = std::env::var("TYCHO_API_KEY").expect("TYCHO_API_KEY");

    // Weighted, deterministic profile assignment: smooth weighted round-robin, so a wave of 4
    // already mixes profiles instead of taking the first profile's whole weight first.
    let eligible: Vec<(usize, i64)> = plan
        .profiles
        .iter()
        .enumerate()
        .filter(|(_, p)| {
            forced
                .as_deref()
                .is_none_or(|f| f == p.name)
        })
        .map(|(i, p)| (i, p.weight.max(1) as i64))
        .collect();
    assert!(!eligible.is_empty(), "no profile matches PROFILE");
    let total: i64 = eligible.iter().map(|e| e.1).sum();
    let mut current = vec![0i64; eligible.len()];
    let mut roster: Vec<usize> = vec![];
    for _ in 0..n.max(1) {
        for (k, e) in eligible.iter().enumerate() {
            current[k] += e.1;
        }
        let best = (0..eligible.len())
            .max_by_key(|&k| current[k])
            .unwrap();
        current[best] -= total;
        roster.push(eligible[best].0);
    }
    let mut seed_rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    let mut offsets: Vec<f64> = (0..n)
        .map(|_| seed_rng.unit() * spread_s)
        .collect();
    offsets.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if spread_s > 0.0 {
        offsets[0] = 0.0;
    }

    let records = Arc::new(Mutex::new(Vec::new()));
    let t0 = Instant::now();
    let wall_start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let runs = (0..n).map(|i| {
        // Plain-http URLs (an in-cluster auth-proxy) have no h2c; keep HTTP/1.1 there.
        let mut builder = reqwest::Client::builder();
        if plan.url.starts_with("https://") {
            builder = builder.http2_prior_knowledge();
        }
        let client = builder
            .no_zstd()
            .no_gzip()
            .timeout(std::time::Duration::from_secs(TIMEOUT_S))
            .build()
            .unwrap();
        let profile = plan.profiles[roster[i % roster.len()]].clone();
        let ctx = Ctx {
            client,
            url: plan.url.clone(),
            key: key.clone(),
            plan: plan.user_plan.clone(),
            t0,
            startup: i,
            profile: profile.name.clone(),
            records: records.clone(),
        };
        let rng = Rng((seed ^ (i as u64 + 1).wrapping_mul(0x9E3779B97F4A7C15)) | 1);
        let offset = offsets[i];
        let block = base_block + (offset / block_time_s) as u64;
        let plan = plan.clone();
        let knobs = knobs.clone();
        tokio::spawn(async move {
            if offset > 0.0 {
                tokio::time::sleep(std::time::Duration::from_secs_f64(offset)).await;
            }
            startup(ctx, plan, profile, rng, block, knobs).await
        })
    });
    let startups: Vec<Value> = join_all(runs)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    let records = records.lock().await.clone();
    let mut ready: Vec<f64> = startups
        .iter()
        .map(|s| s["ready_s"].as_f64().unwrap())
        .collect();
    ready.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let bad = records
        .iter()
        .filter(|r| r.status != 200 || r.error.is_some())
        .count();
    let bytes: u64 = records.iter().map(|r| r.bytes).sum();
    let wall = t0.elapsed().as_secs_f64();
    let mut mix: std::collections::BTreeMap<String, usize> = Default::default();
    for s in &startups {
        *mix.entry(
            s["profile"]
                .as_str()
                .unwrap()
                .to_string(),
        )
        .or_default() += 1;
    }
    eprintln!(
        "n={n} spread={spread_s}s ready p50={:.1}s max={:.1}s requests={} bad={bad} MB={:.0} wall={wall:.1}s mix={mix:?}",
        ready[ready.len() / 2], ready[ready.len() - 1], records.len(), bytes as f64 / 1e6
    );
    let out = json!({"n": n, "spread_s": spread_s, "block_time_s": block_time_s, "version_age_blocks": knobs.version_age_blocks,
                     "single_id_calls": knobs.single_id_calls, "profile": forced, "base_block": base_block, "wall_start": wall_start,
                     "wall_s": wall, "mix": mix, "startups": startups, "records": records});
    std::fs::write(&a[4], serde_json::to_vec(&out).unwrap()).unwrap();
}
