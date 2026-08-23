use anyhow::Result;
use substreams::{log, scalar::BigInt};
use substreams_ethereum::{
    pb::eth::{rpc::RpcResponse, v2 as eth},
    rpc::RpcBatch,
    Event,
};
use tycho_substreams::{abi::erc20, prelude::*};

use crate::{
    abi::{pendle_market, pendle_market_factory, pendle_market_factory_v1, pendle_sy},
    consts::{MARKET_FACTORIES_V3_PLUS, MARKET_FACTORY_V1, PENDLE_MARKET, PENDLE_SY},
    sy::{classify_deposit, classify_redeem, pow10, TokenClass},
};

/// Discovers markets from the factories' creation events and resolves what the events omit.
///
/// `CreateNewMarket` carries neither the market's `expiry` nor its SY and YT, and the two
/// factory generations do not even agree on the event's shape, so each creation is followed by
/// `eth_call`s against the market and its SY. These reads are immutable per market, so this is
/// one burst per market lifetime rather than per block.
///
/// Emits two component types: the market itself, and the SY behind it. The SY is emitted on
/// every market that references it; `map_protocol_components` drops the repeats.
#[substreams::handlers::map]
pub fn map_market_components(block: eth::Block) -> Result<BlockTransactionProtocolComponents> {
    let mut tx_components = Vec::new();
    for tx in block.transactions() {
        let mut components = Vec::new();
        for log in tx.logs_with_calls().map(|(log, _)| log) {
            let Some(creation) = decode_creation(log) else { continue };
            components.extend(build_components(&creation));
        }
        if !components.is_empty() {
            tx_components.push(TransactionProtocolComponents { tx: Some(tx.into()), components });
        }
    }
    Ok(BlockTransactionProtocolComponents { tx_components })
}

/// A market creation, normalised across the two factory generations.
struct MarketCreation {
    factory: Vec<u8>,
    market: Vec<u8>,
    scalar_root: BigInt,
    initial_anchor: BigInt,
    /// The rate the market was deployed with. The original factory does not put it in the event,
    /// so it is zero there and the fee has to come from `getMarketConfig` instead.
    ln_fee_rate_root: BigInt,
}

fn decode_creation(log: &eth::Log) -> Option<MarketCreation> {
    let address: [u8; 20] = log.address.as_slice().try_into().ok()?;
    if address == MARKET_FACTORY_V1 {
        let event = pendle_market_factory_v1::events::CreateNewMarket::match_and_decode(log)?;
        return Some(MarketCreation {
            factory: log.address.clone(),
            market: event.market,
            scalar_root: event.scalar_root,
            initial_anchor: event.initial_anchor,
            ln_fee_rate_root: BigInt::zero(),
        });
    }
    if MARKET_FACTORIES_V3_PLUS.contains(&address) {
        let event = pendle_market_factory::events::CreateNewMarket::match_and_decode(log)?;
        return Some(MarketCreation {
            factory: log.address.clone(),
            market: event.market,
            scalar_root: event.scalar_root,
            initial_anchor: event.initial_anchor,
            ln_fee_rate_root: event.ln_fee_rate_root,
        });
    }
    None
}

fn build_components(creation: &MarketCreation) -> Vec<ProtocolComponent> {
    let market_hex = format!("0x{}", hex::encode(&creation.market));

    let responses = RpcBatch::new()
        .add(pendle_market::functions::ReadTokens {}, creation.market.clone())
        .add(pendle_market::functions::Expiry {}, creation.market.clone())
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let Some((sy_address, pt_address, yt_address)) = responses
        .first()
        .and_then(RpcBatch::decode::<_, pendle_market::functions::ReadTokens>)
    else {
        log::info!("Skipping market {}: readTokens() did not resolve", market_hex);
        return vec![];
    };
    let Some(expiry) = responses
        .get(1)
        .and_then(RpcBatch::decode::<_, pendle_market::functions::Expiry>)
    else {
        log::info!("Skipping market {}: expiry() did not resolve", market_hex);
        return vec![];
    };

    let Some(sy) = profile_sy(&sy_address) else {
        log::info!(
            "Skipping market {}: SY {} did not resolve",
            market_hex,
            hex::encode(&sy_address)
        );
        return vec![];
    };

    let market = ProtocolComponent::at_contract(&creation.market)
        .with_tokens(&[sy_address.clone(), pt_address.clone(), yt_address.clone()])
        .with_attributes(&[
            (
                "scalar_root",
                creation
                    .scalar_root
                    .to_signed_bytes_be(),
            ),
            (
                "initial_anchor",
                creation
                    .initial_anchor
                    .to_signed_bytes_be(),
            ),
            ("expiry", expiry.to_signed_bytes_be()),
            ("factory", creation.factory.clone()),
            (
                "ln_fee_rate_root_at_creation",
                creation
                    .ln_fee_rate_root
                    .to_signed_bytes_be(),
            ),
            ("sy_address", sy_address.clone()),
            ("pt_address", pt_address),
            ("yt_address", yt_address),
            ("sy_decimals", BigInt::from(sy.decimals).to_signed_bytes_be()),
            ("asset_decimals", BigInt::from(sy.asset_decimals).to_signed_bytes_be()),
        ])
        .as_swap_type(PENDLE_MARKET, ImplementationType::Custom);

    // An SY whose conversions none of the two closed forms explain contributes no wrap edges,
    // so it gets no component — but its market still quotes PT against SY.
    if sy.tokens_in.is_empty() && sy.tokens_out.is_empty() {
        log::info!(
            "Market {}: SY 0x{} has no quotable tokens, emitting the market only",
            market_hex,
            hex::encode(&sy.address)
        );
        return vec![market];
    }

    vec![market, sy.into_component()]
}

/// An SY resolved far enough to quote its wrap edges.
struct SyProfile {
    address: Vec<u8>,
    decimals: u32,
    asset: Vec<u8>,
    asset_decimals: u32,
    /// Quotable entry tokens and the conversion each uses.
    tokens_in: Vec<(Vec<u8>, TokenClass)>,
    /// Quotable exit tokens and the conversion each uses.
    tokens_out: Vec<(Vec<u8>, TokenClass)>,
}

impl SyProfile {
    fn into_component(self) -> ProtocolComponent {
        let mut tokens = vec![self.address.clone()];
        for (token, _) in self
            .tokens_in
            .iter()
            .chain(self.tokens_out.iter())
        {
            if !tokens.contains(token) {
                tokens.push(token.clone());
            }
        }

        // An entry token and an exit token can convert differently on the same SY, so the class
        // is recorded per direction rather than once per token.
        let mut attributes = vec![
            ("asset_address".to_string(), self.asset.clone()),
            ("asset_decimals".to_string(), BigInt::from(self.asset_decimals).to_signed_bytes_be()),
            ("sy_decimals".to_string(), BigInt::from(self.decimals).to_signed_bytes_be()),
        ];
        for (token, class) in &self.tokens_in {
            attributes.push((
                format!("token_in_class_0x{}", hex::encode(token)),
                class.as_str().as_bytes().to_vec(),
            ));
        }
        for (token, class) in &self.tokens_out {
            attributes.push((
                format!("token_out_class_0x{}", hex::encode(token)),
                class.as_str().as_bytes().to_vec(),
            ));
        }

        ProtocolComponent::at_contract(&self.address)
            .with_tokens(&tokens)
            .with_attributes(&attributes)
            .as_swap_type(PENDLE_SY, ImplementationType::Custom)
    }
}

fn profile_sy(sy_address: &[u8]) -> Option<SyProfile> {
    let responses = RpcBatch::new()
        .add(pendle_sy::functions::Decimals {}, sy_address.to_vec())
        .add(pendle_sy::functions::AssetInfo {}, sy_address.to_vec())
        .add(pendle_sy::functions::ExchangeRate {}, sy_address.to_vec())
        .add(pendle_sy::functions::GetTokensIn {}, sy_address.to_vec())
        .add(pendle_sy::functions::GetTokensOut {}, sy_address.to_vec())
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let decimals = decode_decimals(responses.first()?)?;
    let (_asset_type, asset, asset_decimals) = responses
        .get(1)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::AssetInfo>)?;
    let exchange_rate = responses
        .get(2)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::ExchangeRate>)?;
    // The token lists are optional: an SY that will not enumerate them still yields a quotable
    // market, it just contributes no wrap edges.
    let tokens_in = responses
        .get(3)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::GetTokensIn>)
        .unwrap_or_default();
    let tokens_out = responses
        .get(4)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::GetTokensOut>)
        .unwrap_or_default();

    let asset_decimals = asset_decimals.to_u64() as u32;
    let token_decimals = read_token_decimals(&tokens_in, &tokens_out);

    let mut quotable_in = Vec::new();
    for (token, probe) in probe_deposits(sy_address, &tokens_in, &token_decimals) {
        let Some(class) = classify_deposit(&probe, decimals, asset_decimals, &exchange_rate) else {
            continue;
        };
        quotable_in.push((token, class));
    }

    let mut quotable_out = Vec::new();
    for (token, token_decimals, probe) in
        probe_redeems(sy_address, &tokens_out, &token_decimals, decimals)
    {
        let Some(class) =
            classify_redeem(&probe, token_decimals, decimals, asset_decimals, &exchange_rate)
        else {
            continue;
        };
        quotable_out.push((token, class));
    }

    Some(SyProfile {
        address: sy_address.to_vec(),
        decimals,
        asset,
        asset_decimals,
        tokens_in: quotable_in,
        tokens_out: quotable_out,
    })
}

fn decode_decimals(response: &RpcResponse) -> Option<u32> {
    RpcBatch::decode::<_, pendle_sy::functions::Decimals>(response).map(|d| d.to_u64() as u32)
}

/// Reads `decimals()` for every token either side of the SY in one batch.
///
/// Native ETH is listed by some SYs as the zero address and has no `decimals()`; it drops out
/// here and is therefore never classified or declared.
fn read_token_decimals(tokens_in: &[Vec<u8>], tokens_out: &[Vec<u8>]) -> Vec<(Vec<u8>, u32)> {
    let mut unique: Vec<Vec<u8>> = Vec::new();
    for token in tokens_in
        .iter()
        .chain(tokens_out.iter())
    {
        if token.iter().all(|b| *b == 0) || unique.contains(token) {
            continue;
        }
        unique.push(token.clone());
    }

    let mut batch = RpcBatch::new();
    for token in &unique {
        batch = batch.add(erc20::functions::Decimals {}, token.clone());
    }
    let responses = batch
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let mut decimals = Vec::new();
    for (token, response) in unique.into_iter().zip(responses.iter()) {
        let Some(value) = RpcBatch::decode::<_, erc20::functions::Decimals>(response) else {
            continue;
        };
        decimals.push((token, value.to_u64() as u32));
    }
    decimals
}

/// Probes `previewDeposit` with one whole unit of each entry token.
fn probe_deposits(
    sy_address: &[u8],
    tokens_in: &[Vec<u8>],
    token_decimals: &[(Vec<u8>, u32)],
) -> Vec<(Vec<u8>, BigInt)> {
    let mut probes = Vec::new();
    for token in tokens_in {
        let Some((_, decimals)) = token_decimals
            .iter()
            .find(|(t, _)| t == token)
        else {
            continue;
        };
        probes.push((token.clone(), *decimals));
    }

    let mut batch = RpcBatch::new();
    for (token, decimals) in &probes {
        batch = batch.add(
            pendle_sy::functions::PreviewDeposit {
                token_in: token.clone(),
                amount_token_to_deposit: pow10(*decimals),
            },
            sy_address.to_vec(),
        );
    }

    let mut results = Vec::new();
    for ((token, _), value) in probes
        .into_iter()
        .zip(execute_probes::<pendle_sy::functions::PreviewDeposit>(batch))
    {
        let Some(value) = value else { continue };
        results.push((token, value));
    }
    results
}

/// Probes `previewRedeem` with one whole SY unit for each exit token.
///
/// Returns each token with its own decimals, which the redeem conversion needs to rescale the
/// asset-unit result the SY reports.
fn probe_redeems(
    sy_address: &[u8],
    tokens_out: &[Vec<u8>],
    token_decimals: &[(Vec<u8>, u32)],
    sy_decimals: u32,
) -> Vec<(Vec<u8>, u32, BigInt)> {
    let mut probes = Vec::new();
    for token in tokens_out {
        let Some((_, decimals)) = token_decimals
            .iter()
            .find(|(t, _)| t == token)
        else {
            continue;
        };
        probes.push((token.clone(), *decimals));
    }

    let mut batch = RpcBatch::new();
    for (token, _) in &probes {
        batch = batch.add(
            pendle_sy::functions::PreviewRedeem {
                token_out: token.clone(),
                amount_shares_to_redeem: pow10(sy_decimals),
            },
            sy_address.to_vec(),
        );
    }

    let mut results = Vec::new();
    for ((token, decimals), value) in probes
        .into_iter()
        .zip(execute_probes::<pendle_sy::functions::PreviewRedeem>(batch))
    {
        let Some(value) = value else { continue };
        results.push((token, decimals, value));
    }
    results
}

/// Executes a probe batch, yielding one entry per call in the order they were added.
///
/// A reverting probe — a paused SY, a token the view rejects — yields `None`, which leaves that
/// token out of the component rather than guessed at.
fn execute_probes<F>(batch: RpcBatch) -> Vec<Option<BigInt>>
where
    F: substreams_ethereum::Function + substreams_ethereum::rpc::RPCDecodable<BigInt>,
{
    batch
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default()
        .iter()
        .map(RpcBatch::decode::<_, F>)
        .collect()
}
