//! SY (ERC-5115) profiling and per-token conversion classification.
//!
//! Every SY wraps a different protocol, so there is no single deposit/redeem formula. What is
//! generic is the *evidence*: `previewDeposit` / `previewRedeem` are pure views, so one probe of
//! a single token unit at creation, repeated on an upgrade or unpause, says which of two
//! closed-form conversions — if either — the SY implements for that token. Tokens matching neither
//! are left out of the component, which is what makes an unquotable SY edge absent rather than
//! silently wrong.
//!
//! Classification is per `(SY, token)` pair, not per SY: wrappers commonly accept their base
//! token 1:1 *and* the accounting asset at the index rate.

use substreams::{log, scalar::BigInt};
use substreams_ethereum::{pb::eth::rpc::RpcResponse, rpc::RpcBatch};
use tycho_substreams::{abi::erc20, prelude::*};

use crate::{
    abi::pendle_sy,
    consts::{ENTRY_FEE_PAYER_YIELD_TOKENS, PENDLE_SY},
    keys::contract_id,
};

/// Which closed-form conversion an SY applies to one specific token.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenClass {
    /// One token unit is one SY unit, up to a decimal rescale.
    OneToOne,
    /// The token is treated as the accounting asset and converted at `exchangeRate()`.
    IndexRate,
}

impl TokenClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            TokenClass::OneToOne => "one_to_one",
            TokenClass::IndexRate => "index_rate",
        }
    }
}

/// Fixed-point scale of `SY.exchangeRate()`. The rate is always 1e18-scaled even where the SY
/// and its accounting asset have different decimals — the decimal gap is absorbed into the rate
/// itself, so it must never be treated as `10^sy_decimals`.
const RATE_ONE: u64 = 1_000_000_000_000_000_000;

/// Relative tolerance for matching a probe against a prediction, as a reciprocal: a prediction is
/// accepted when it is within 1/1e6 of the observed value.
///
/// The bound is set by the spread the SYs charge on entry, not by rounding. Swept over the 46 live
/// SYs at one block, every `previewRedeem` probe matches its prediction to the unit, and the
/// `previewDeposit` probes that do not split in two: one or two wei where the SY converts through
/// the wrapped protocol's own floor-divide first, and seven SYs taking a real 1e-7 to 1e-6 haircut
/// on entry. Those seven are what a wei-level tolerance would drop. It stays far under a haircut
/// large enough to mean the SY is doing something else entirely (0.5% on `0x457904b5...59d6`), so
/// a token matching neither closed form is still excluded rather than rounded into the nearer
/// class, and the two predictions never both match unless they are arithmetically identical.
const TOLERANCE_RECIPROCAL: u64 = 1_000_000;

pub fn pow10(exponent: u32) -> BigInt {
    BigInt::from(10).pow(exponent)
}

/// True when `observed` is within the relative tolerance of `predicted`.
fn matches(observed: &BigInt, predicted: &BigInt) -> bool {
    if predicted.clone() == BigInt::zero() {
        return false;
    }
    let difference = if observed > predicted {
        observed.clone() - predicted.clone()
    } else {
        predicted.clone() - observed.clone()
    };
    difference * BigInt::from(TOLERANCE_RECIPROCAL) <= predicted.clone()
}

/// Picks the class whose prediction matches the probe.
///
/// `OneToOne` is only asserted where the two predictions genuinely diverge. Where they coincide
/// — an SY whose `exchangeRate` currently sits at exactly one unit — the evidence cannot
/// separate them, and the tie goes to `IndexRate`: it *is* the identity while the rate is one,
/// and it stays correct once the rate moves away. Freezing such an SY as `OneToOne` would leave
/// the label right today and quietly wrong by `rate - 1` forever after.
fn classify(observed: &BigInt, one_to_one: &BigInt, index_rate: &BigInt) -> Option<TokenClass> {
    if matches(observed, one_to_one) && !matches(one_to_one, index_rate) {
        return Some(TokenClass::OneToOne);
    }
    if matches(observed, index_rate) {
        return Some(TokenClass::IndexRate);
    }
    None
}

/// Classifies a `tokensIn` entry from a `previewDeposit` probe of one whole token unit.
///
/// `observed` must be the SY-unit output the SY predicts for exactly `10^token_decimals` of the
/// token. Both predictions are then independent of the token's own decimals, which cancel
/// against the probe amount.
pub fn classify_deposit(
    observed: &BigInt,
    sy_decimals: u32,
    asset_decimals: u32,
    exchange_rate: &BigInt,
) -> Option<TokenClass> {
    if exchange_rate.clone() == BigInt::zero() {
        return None;
    }
    let one_to_one = pow10(sy_decimals);
    // The token enters as the accounting asset, then converts to SY: `assetToSy` divides by the
    // rate, and the rate carries the sy/asset decimal gap, so the quotient is already in SY
    // units.
    let index_rate = pow10(asset_decimals) * BigInt::from(RATE_ONE) / exchange_rate.clone();
    classify(observed, &one_to_one, &index_rate)
}

/// Classifies a `tokensIn` entry of an SY that pays an entry fee, from a probe taken while the fee
/// may be charged: `index_rate`, when the probe sits at most [`MAX_ENTRY_FEE_BPS`] below that
/// prediction. The fee itself is state, followed from the vault's events.
pub fn classify_deposit_net_of_entry_fee(
    observed: &BigInt,
    asset_decimals: u32,
    exchange_rate: &BigInt,
) -> Option<TokenClass> {
    if exchange_rate.clone() == BigInt::zero() {
        return None;
    }
    let index_rate = pow10(asset_decimals) * BigInt::from(RATE_ONE) / exchange_rate.clone();
    let floor =
        index_rate.clone() * BigInt::from(10_000 - MAX_ENTRY_FEE_BPS) / BigInt::from(10_000);
    (*observed <= index_rate && *observed >= floor).then_some(TokenClass::IndexRate)
}

/// The largest entry fee the classification tolerates for an SY that pays one: 1%. The one known
/// is 25 basis points.
const MAX_ENTRY_FEE_BPS: i64 = 100;

/// The step a deposit mints in, from the probe of one whole token: `10^(sy - token decimals)` when
/// the SY's units are finer than the token's and the probe output is a whole multiple of that,
/// as an SY computing in the token's precision and scaling up mints. 1 otherwise.
///
/// An unrounded mint is a whole multiple of `10^k` by chance once in `10^k` probes, and `k` is
/// at least 12 for a 6-decimal token into an 18-decimal SY.
pub fn deposit_step(observed: &BigInt, sy_decimals: u32, token_decimals: u32) -> BigInt {
    if token_decimals >= sy_decimals || *observed == BigInt::zero() {
        return BigInt::one();
    }
    let step = pow10(sy_decimals - token_decimals);
    if observed.clone() % step.clone() == BigInt::zero() {
        step
    } else {
        BigInt::one()
    }
}

/// Classifies a `tokensOut` entry from a `previewRedeem` probe of one whole SY unit.
///
/// `observed` is the token output the SY predicts for `10^sy_decimals` of itself.
pub fn classify_redeem(
    observed: &BigInt,
    token_decimals: u32,
    sy_decimals: u32,
    asset_decimals: u32,
    exchange_rate: &BigInt,
) -> Option<TokenClass> {
    if exchange_rate.clone() == BigInt::zero() {
        return None;
    }
    let one_to_one = pow10(token_decimals);
    // Mirror of the deposit direction: `syToAsset` multiplies by the rate, leaving asset-decimal
    // units, which are then rescaled to the token's own decimals.
    let asset_amount = pow10(sy_decimals) * exchange_rate.clone() / BigInt::from(RATE_ONE);
    let index_rate = asset_amount * pow10(token_decimals) / pow10(asset_decimals);
    classify(observed, &one_to_one, &index_rate)
}

/// An SY resolved far enough to quote its wrap edges.
pub(crate) struct SyProfile {
    pub(crate) address: Vec<u8>,
    pub(crate) decimals: u32,
    pub(crate) asset: Vec<u8>,
    pub(crate) asset_decimals: u32,
    /// Quotable entry tokens and the conversion each uses.
    pub(crate) tokens_in: Vec<(Vec<u8>, TokenClass)>,
    /// Quotable exit tokens and the conversion each uses.
    pub(crate) tokens_out: Vec<(Vec<u8>, TokenClass)>,
    /// The deposit step of each quotable entry token. See [`deposit_step`].
    pub(crate) deposit_steps: Vec<(Vec<u8>, BigInt)>,
    /// Listed tokens whose probe did not answer. See [`Classification::unprobed`].
    pub(crate) unprobed: Vec<Vec<u8>>,
    /// `totalSupply()` at the creation block, the seed supply is accumulated from.
    pub(crate) total_supply: BigInt,
    /// The SY's `balanceOf` in each of its component tokens at the creation block, the seeds its
    /// balances are accumulated from. A token whose read failed is absent and starts at zero.
    pub(crate) holdings: Vec<(Vec<u8>, BigInt)>,
    /// `paused()` at the creation block; `Paused` / `Unpaused` move it afterwards.
    pub(crate) paused: bool,
    /// `yieldToken()`, the token the SY holds as its backing, when it answers.
    pub(crate) yield_token: Option<Vec<u8>>,
    /// The cap on the SY's own supply at the creation block, when it has one. See
    /// [`own_supply_cap`].
    pub(crate) supply_cap: Option<BigInt>,
}

/// State attribute carrying an SY's share supply, which bounds every redemption from it.
pub const SY_TOTAL_SUPPLY: &str = "sy_total_supply";

/// Static attribute carrying an SY's `totalSupply()` at creation. Consumed by
/// `map_reserve_deltas` and stripped before the component is emitted.
pub const CREATION_TOTAL_SUPPLY: &str = "creation_total_supply";
/// Prefix of the static attributes carrying an SY's holding of each token at creation, followed by
/// the token's hex address. Consumed by `map_relative_component_balance` and stripped before the
/// component is emitted.
pub const CREATION_BALANCE_PREFIX: &str = "creation_balance_0x";

/// Static attribute carrying whether an SY was paused at creation. Consumed by `map_sy_state` and
/// stripped before the component is emitted.
pub const CREATION_PAUSED: &str = "creation_paused";
/// State attribute: whether the SY is paused, which refuses every quote against it.
pub const PAUSED: &str = "paused";
/// Static attribute carrying whether a market's SY was paused when the market was created.
/// Consumed by `map_sy_state` and stripped before the component is emitted.
pub const CREATION_SY_PAUSED: &str = "creation_sy_paused";
/// State attribute on a market: whether its SY is paused. A paused SY reverts every transfer, and
/// every market swap moves SY, so this refuses every quote against the market.
pub const SY_PAUSED: &str = "sy_paused";
/// Static attribute: the token the SY holds as its backing (`yieldToken()`).
pub const YIELD_TOKEN: &str = "yield_token";
/// Static attribute carrying the cap on an SY's own supply at creation. Consumed by
/// `map_sy_state` and stripped before the component is emitted.
pub const CREATION_SUPPLY_CAP: &str = "creation_supply_cap";
/// State attribute: the most SY shares the SY lets exist. A deposit that would mint past it
/// reverts with `SupplyCapExceeded`. Absent for an SY without such a cap, and `type(uint256).max`
/// once `SupplyCapUpdated` lifts one.
pub const SY_SUPPLY_CAP: &str = "sy_supply_cap";
/// Prefixes of the per-token class attributes, followed by the token's hex address. Emitted as
/// state, since an upgrade can change how a token converts.
pub const TOKEN_IN_CLASS_PREFIX: &str = "token_in_class_0x";
/// Prefix of the per-token deposit step, followed by the token's hex address: the SY mints a
/// multiple of this many SY wei. Emitted with the classes, and 1 where the mint is not rounded.
pub const TOKEN_IN_STEP_PREFIX: &str = "token_in_step_0x";
pub const TOKEN_OUT_CLASS_PREFIX: &str = "token_out_class_0x";
/// The class a re-classification gives a listed token and direction that matches neither closed
/// form: the pair is unquotable, as if it had no class.
///
/// Classes and the supply cap are overwritten, never deleted. Tycho's storage fails the whole
/// write when it is asked to delete an attribute that has no row, and the package does not track
/// which of an SY's classes are set: a token that never had a class, or already lost it, would
/// otherwise be deleted again.
pub const NO_CLASS: &str = "none";

/// Whether a static attribute is a creation seed, which only seeds the stores and is never emitted.
pub fn is_creation_seed(name: &str) -> bool {
    name == CREATION_TOTAL_SUPPLY ||
        name == CREATION_PAUSED ||
        name == CREATION_SY_PAUSED ||
        name == CREATION_SUPPLY_CAP ||
        name.starts_with(CREATION_BALANCE_PREFIX)
}

/// Whether an attribute is a token class or a deposit step, which ride and are emitted together.
pub fn is_class_attribute(name: &str) -> bool {
    name.starts_with(TOKEN_IN_CLASS_PREFIX) ||
        name.starts_with(TOKEN_OUT_CLASS_PREFIX) ||
        name.starts_with(TOKEN_IN_STEP_PREFIX)
}

/// One deposit-step attribute per quotable entry token.
pub fn step_attributes(steps: &[(Vec<u8>, BigInt)]) -> Vec<(String, Vec<u8>)> {
    steps
        .iter()
        .map(|(token, step)| {
            (format!("{TOKEN_IN_STEP_PREFIX}{}", hex::encode(token)), step.to_signed_bytes_be())
        })
        .collect()
}

/// One class attribute per quotable token and direction: an entry token and an exit token can
/// convert differently on the same SY, so the class is recorded per direction.
pub fn class_attributes(
    tokens_in: &[(Vec<u8>, TokenClass)],
    tokens_out: &[(Vec<u8>, TokenClass)],
) -> Vec<(String, Vec<u8>)> {
    let mut attributes = Vec::new();
    for (token, class) in tokens_in {
        attributes.push((
            format!("{TOKEN_IN_CLASS_PREFIX}{}", hex::encode(token)),
            class.as_str().as_bytes().to_vec(),
        ));
    }
    for (token, class) in tokens_out {
        attributes.push((
            format!("{TOKEN_OUT_CLASS_PREFIX}{}", hex::encode(token)),
            class.as_str().as_bytes().to_vec(),
        ));
    }
    attributes
}

/// The SY's supply at creation, from its seed attribute.
pub fn supply_seed(component: &ProtocolComponent) -> Option<BigInt> {
    component
        .get_attribute_value(CREATION_TOTAL_SUPPLY)
        .map(|raw| BigInt::from_signed_bytes_be(&raw))
}

/// The SY's holding of each token at creation, from its seed attributes.
pub fn balance_seeds(component: &ProtocolComponent) -> Vec<(Vec<u8>, BigInt)> {
    let mut seeds = Vec::new();
    for attribute in &component.static_att {
        let Some(token) = attribute
            .name
            .strip_prefix(CREATION_BALANCE_PREFIX)
        else {
            continue;
        };
        let token = hex::decode(token).expect("creation balance attribute names a non-hex token");
        seeds.push((token, BigInt::from_signed_bytes_be(&attribute.value)));
    }
    seeds
}

impl SyProfile {
    /// The component's tokens: the SY itself, then every quotable or unprobed token once.
    fn component_tokens(&self) -> Vec<Vec<u8>> {
        component_tokens(&self.address, &self.tokens_in, &self.tokens_out, &self.unprobed)
    }

    pub(crate) fn into_component(self) -> ProtocolComponent {
        let tokens = self.component_tokens();

        // The classes ride here only as far as `map_sy_state`, which emits them as state so an
        // upgrade can change them; `map_protocol_changes` strips them from the component.
        let mut attributes = vec![
            ("asset_address".to_string(), self.asset.clone()),
            ("asset_decimals".to_string(), BigInt::from(self.asset_decimals).to_signed_bytes_be()),
            ("sy_decimals".to_string(), BigInt::from(self.decimals).to_signed_bytes_be()),
        ];
        attributes.extend(class_attributes(&self.tokens_in, &self.tokens_out));
        attributes.extend(step_attributes(&self.deposit_steps));
        attributes.push((CREATION_PAUSED.to_string(), vec![u8::from(self.paused)]));
        if let Some(yield_token) = &self.yield_token {
            attributes.push((YIELD_TOKEN.to_string(), yield_token.clone()));
        }
        if let Some(cap) = &self.supply_cap {
            attributes.push((CREATION_SUPPLY_CAP.to_string(), cap.to_signed_bytes_be()));
        }
        attributes
            .push((CREATION_TOTAL_SUPPLY.to_string(), self.total_supply.to_signed_bytes_be()));
        for (token, balance) in &self.holdings {
            attributes.push((
                format!("{CREATION_BALANCE_PREFIX}{}", hex::encode(token)),
                balance.to_signed_bytes_be(),
            ));
        }

        ProtocolComponent::new(&contract_id(&self.address))
            .with_tokens(&tokens)
            .with_attributes(&attributes)
            .as_swap_type(PENDLE_SY, ImplementationType::Custom)
    }
}

pub(crate) fn profile_sy(sy_address: &[u8]) -> Option<SyProfile> {
    let Classification {
        decimals,
        asset,
        asset_decimals,
        tokens_in,
        tokens_out,
        deposit_steps,
        unprobed,
        ..
    } = classify_sy(sy_address)?;

    let tokens = component_tokens(sy_address, &tokens_in, &tokens_out, &unprobed);
    let Some(Seeds { total_supply, holdings, paused, yield_token, supply_cap }) =
        read_seeds(sy_address, &tokens)
    else {
        log::info!("SY 0x{} did not answer totalSupply()", hex::encode(sy_address));
        return None;
    };

    Some(SyProfile {
        address: sy_address.to_vec(),
        decimals,
        asset,
        asset_decimals,
        tokens_in,
        tokens_out,
        deposit_steps,
        unprobed,
        total_supply,
        holdings,
        paused,
        yield_token,
        supply_cap,
    })
}

/// What a classification of an SY's tokens found: its units, and each quotable token's class.
pub(crate) struct Classification {
    pub(crate) decimals: u32,
    pub(crate) asset: Vec<u8>,
    pub(crate) asset_decimals: u32,
    /// Quotable entry tokens and the conversion each uses.
    pub(crate) tokens_in: Vec<(Vec<u8>, TokenClass)>,
    /// Quotable exit tokens and the conversion each uses.
    pub(crate) tokens_out: Vec<(Vec<u8>, TokenClass)>,
    /// The deposit step of each quotable entry token. See [`deposit_step`].
    pub(crate) deposit_steps: Vec<(Vec<u8>, BigInt)>,
    /// Entry tokens whose decimals or probe did not answer, so whether they still match a closed
    /// form is unknown. A pause does not land tokens here: Pendle's `SYBase` pauses transfers,
    /// not previews. A probe fails where the token or the protocol the SY wraps rejects it.
    pub(crate) undetermined_in: Vec<Vec<u8>>,
    /// Exit tokens whose decimals or probe did not answer.
    pub(crate) undetermined_out: Vec<Vec<u8>>,
    /// Whether `getTokensIn()` did not answer, which leaves every entry token undetermined.
    pub(crate) entry_list_unread: bool,
    /// Whether `getTokensOut()` did not answer, which leaves every exit token undetermined.
    pub(crate) exit_list_unread: bool,
    /// Undetermined tokens, in either direction, whose `decimals()` answered but whose probe did
    /// not. The component lists them without a class, so a re-classification on a later upgrade
    /// or unpause can classify them. A token without decimals is left out: the stream drops a
    /// component naming a token it does not know.
    pub(crate) unprobed: Vec<Vec<u8>>,
}

/// Classifies each token the SY lists by probing `previewDeposit` / `previewRedeem` against the
/// two closed forms, at the current block. `None` when the SY's units do not resolve.
pub(crate) fn classify_sy(sy_address: &[u8]) -> Option<Classification> {
    let responses = RpcBatch::new()
        .add(pendle_sy::functions::Decimals {}, sy_address.to_vec())
        .add(pendle_sy::functions::AssetInfo {}, sy_address.to_vec())
        .add(pendle_sy::functions::ExchangeRate {}, sy_address.to_vec())
        .add(pendle_sy::functions::GetTokensIn {}, sy_address.to_vec())
        .add(pendle_sy::functions::GetTokensOut {}, sy_address.to_vec())
        .add(pendle_sy::functions::YieldToken {}, sy_address.to_vec())
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let decimals = decode_decimals(responses.first()?)?;
    // An SY wrapping a vault that charges an entry fee may be probed while it does, which puts
    // its deposits below the closed form by the fee.
    let pays_entry_fee = responses
        .get(5)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::YieldToken>)
        .is_some_and(|yield_token| {
            ENTRY_FEE_PAYER_YIELD_TOKENS
                .iter()
                .any(|payer| yield_token.as_slice() == payer)
        });
    let (_asset_type, asset, asset_decimals) = responses
        .get(1)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::AssetInfo>)?;
    let exchange_rate = responses
        .get(2)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::ExchangeRate>)?;
    // The token lists are optional: an SY that will not enumerate them still yields a quotable
    // market, it just contributes no wrap edges. A list that does not answer is unknown, not
    // empty, which matters on a re-profile: its tokens keep their classes.
    let tokens_in = responses
        .get(3)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::GetTokensIn>);
    let tokens_out = responses
        .get(4)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::GetTokensOut>);
    let (entry_list_unread, exit_list_unread) = (tokens_in.is_none(), tokens_out.is_none());
    let (tokens_in, tokens_out) = (tokens_in.unwrap_or_default(), tokens_out.unwrap_or_default());

    let asset_decimals = bounded_decimals(&asset_decimals)?;
    let token_decimals = read_token_decimals(&tokens_in, &tokens_out);

    let mut quotable_in = Vec::new();
    let mut deposit_steps = Vec::new();
    let mut undetermined_in = Vec::new();
    for (token, probe) in probe_deposits(sy_address, &tokens_in, &token_decimals) {
        let Some(probe) = probe else {
            undetermined_in.push(token);
            continue;
        };
        let class =
            classify_deposit(&probe, decimals, asset_decimals, &exchange_rate).or_else(|| {
                pays_entry_fee
                    .then(|| {
                        classify_deposit_net_of_entry_fee(&probe, asset_decimals, &exchange_rate)
                    })
                    .flatten()
            });
        let Some(class) = class else { continue };
        let probed_decimals = token_decimals
            .iter()
            .find(|(listed, _)| *listed == token)
            .map_or(decimals, |(_, decimals)| *decimals);
        deposit_steps.push((token.clone(), deposit_step(&probe, decimals, probed_decimals)));
        quotable_in.push((token, class));
    }

    let mut quotable_out = Vec::new();
    let mut undetermined_out = Vec::new();
    for (token, probe) in probe_redeems(sy_address, &tokens_out, &token_decimals, decimals) {
        let Some((token_decimals, probe)) = probe else {
            undetermined_out.push(token);
            continue;
        };
        let Some(class) =
            classify_redeem(&probe, token_decimals, decimals, asset_decimals, &exchange_rate)
        else {
            continue;
        };
        quotable_out.push((token, class));
    }

    let unprobed = unprobed(&undetermined_in, &undetermined_out, &token_decimals);

    Some(Classification {
        decimals,
        asset,
        asset_decimals,
        tokens_in: quotable_in,
        tokens_out: quotable_out,
        deposit_steps,
        undetermined_in,
        undetermined_out,
        entry_list_unread,
        exit_list_unread,
        unprobed,
    })
}

/// The undetermined tokens of either direction whose decimals answered, each once.
fn unprobed(
    undetermined_in: &[Vec<u8>],
    undetermined_out: &[Vec<u8>],
    token_decimals: &[(Vec<u8>, u32)],
) -> Vec<Vec<u8>> {
    let mut unprobed: Vec<Vec<u8>> = Vec::new();
    for token in undetermined_in
        .iter()
        .chain(undetermined_out.iter())
    {
        let answered_decimals = token_decimals
            .iter()
            .any(|(t, _)| t == token);
        if answered_decimals && !unprobed.contains(token) {
            unprobed.push(token.clone());
        }
    }
    unprobed
}

fn component_tokens(
    sy_address: &[u8],
    tokens_in: &[(Vec<u8>, TokenClass)],
    tokens_out: &[(Vec<u8>, TokenClass)],
    unprobed: &[Vec<u8>],
) -> Vec<Vec<u8>> {
    let mut tokens = vec![sy_address.to_vec()];
    let classified = tokens_in
        .iter()
        .chain(tokens_out.iter())
        .map(|(token, _)| token);
    for token in classified.chain(unprobed.iter()) {
        if !tokens.contains(token) {
            tokens.push(token.clone());
        }
    }
    tokens
}

/// What an SY's creation reads found: the seeds its supply and balances are accumulated from, and
/// its pause flag, yield token and supply cap.
struct Seeds {
    total_supply: BigInt,
    holdings: Vec<(Vec<u8>, BigInt)>,
    paused: bool,
    yield_token: Option<Vec<u8>>,
    supply_cap: Option<BigInt>,
}

/// The cap an SY puts on its own supply; `None` when it has none.
///
/// Pendle's capped SYs revert a mint that takes `totalSupply()` past the cap, so the room left is
/// the cap less the supply this package already follows from mints and burns. Two shapes answer:
/// - `getAbsoluteSupplyCap()` with `getAbsoluteTotalSupply()`. The cap counts only when the
///   "absolute" supply it is checked against is the SY's own; some SYs cap a vault's supply
///   instead, which moves outside the SY and would need a read every block, so those are left
///   uncapped.
/// - A plain `supplyCap()`, on SYs without the absolute getters (`PendleERC20WithSupplyCapSY`,
///   `PendleZtakeUSDESY`), which check it against their own `totalSupply()`.
///
/// `type(uint256).max` is how an uncapped SY answers either.
pub fn own_supply_cap(
    cap: Option<BigInt>,
    absolute_supply: Option<BigInt>,
    plain_cap: Option<BigInt>,
    supply: &BigInt,
) -> Option<BigInt> {
    let cap = match (cap, absolute_supply) {
        (Some(cap), Some(absolute_supply)) => (absolute_supply == *supply).then_some(cap)?,
        (Some(_), None) => return None,
        (None, _) => plain_cap?,
    };
    (!is_uncapped(&cap)).then_some(cap)
}

/// `type(uint256).max`, which is how an uncapped SY answers.
pub fn uncapped() -> BigInt {
    BigInt::from(2).pow(256) - BigInt::from(1)
}

/// Whether a supply cap is `type(uint256).max`, which is how an uncapped SY answers.
pub fn is_uncapped(cap: &BigInt) -> bool {
    *cap == uncapped()
}

/// Adds the three cap getters [`own_supply_cap`] judges, in the order [`own_cap_from`] reads them.
fn add_cap_reads(batch: RpcBatch, sy_address: &[u8]) -> RpcBatch {
    batch
        .add(pendle_sy::functions::GetAbsoluteSupplyCap {}, sy_address.to_vec())
        .add(pendle_sy::functions::GetAbsoluteTotalSupply {}, sy_address.to_vec())
        .add(pendle_sy::functions::SupplyCap {}, sy_address.to_vec())
}

/// The cap on the SY's own supply, from the three responses [`add_cap_reads`] added.
fn own_cap_from(responses: &[RpcResponse], supply: &BigInt) -> Option<BigInt> {
    own_supply_cap(
        responses
            .first()
            .and_then(RpcBatch::decode::<_, pendle_sy::functions::GetAbsoluteSupplyCap>),
        responses
            .get(1)
            .and_then(RpcBatch::decode::<_, pendle_sy::functions::GetAbsoluteTotalSupply>),
        responses
            .get(2)
            .and_then(RpcBatch::decode::<_, pendle_sy::functions::SupplyCap>),
        supply,
    )
}

/// Reads the cap on the SY's own supply now, judged as at creation: `Some(None)` when the SY has
/// none, or caps another supply than its own. `None` when its `totalSupply()` does not answer, so
/// the cap cannot be judged.
pub(crate) fn read_own_supply_cap(sy_address: &[u8]) -> Option<Option<BigInt>> {
    let batch = RpcBatch::new().add(erc20::functions::TotalSupply {}, sy_address.to_vec());
    let responses = add_cap_reads(batch, sy_address)
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();
    let supply = RpcBatch::decode::<_, erc20::functions::TotalSupply>(responses.first()?)?;
    Some(own_cap_from(&responses[1..], &supply))
}

/// Reads whether the SY is paused, for a market whose SY did not profile. An SY without a
/// `paused()` cannot be paused, and one that does not answer is taken as not paused: its market
/// then quotes as it would have before this flag existed.
pub(crate) fn read_paused(sy_address: &[u8]) -> bool {
    RpcBatch::new()
        .add(pendle_sy::functions::Paused {}, sy_address.to_vec())
        .execute()
        .ok()
        .and_then(|r| {
            r.responses
                .first()
                .and_then(RpcBatch::decode::<_, pendle_sy::functions::Paused>)
        })
        .unwrap_or(false)
}

/// Reads the SY's supply, pause flag, yield token, supply cap and its holding of each token in one
/// batch, at the creation block.
///
/// These are the only reads its supply and balances ever need: afterwards both are accumulated
/// from events. `None` when the supply does not resolve, since a component without one could not
/// bound a redemption.
fn read_seeds(sy_address: &[u8], tokens: &[Vec<u8>]) -> Option<Seeds> {
    let batch = RpcBatch::new()
        .add(erc20::functions::TotalSupply {}, sy_address.to_vec())
        .add(pendle_sy::functions::Paused {}, sy_address.to_vec())
        .add(pendle_sy::functions::YieldToken {}, sy_address.to_vec());
    let mut batch = add_cap_reads(batch, sy_address);
    for token in tokens {
        batch =
            batch.add(erc20::functions::BalanceOf { owner: sy_address.to_vec() }, token.clone());
    }
    let responses = batch
        .execute()
        .map(|r| r.responses)
        .unwrap_or_default();

    let total_supply = RpcBatch::decode::<_, erc20::functions::TotalSupply>(responses.first()?)?;
    // Five SYs have no `paused()` at all; they cannot be paused, which is what `false` says.
    let paused = responses
        .get(1)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::Paused>)
        .unwrap_or(false);
    let yield_token = responses
        .get(2)
        .and_then(RpcBatch::decode::<_, pendle_sy::functions::YieldToken>)
        .filter(|token| token.iter().any(|byte| *byte != 0));
    let supply_cap = own_cap_from(responses.get(3..).unwrap_or_default(), &total_supply);
    let mut holdings = Vec::new();
    for (token, response) in tokens
        .iter()
        .zip(responses.iter().skip(6))
    {
        let Some(balance) = RpcBatch::decode::<_, erc20::functions::BalanceOf>(response) else {
            log::info!(
                "SY 0x{} did not answer balanceOf for 0x{}, its balance starts at zero",
                hex::encode(sy_address),
                hex::encode(token)
            );
            continue;
        };
        holdings.push((token.clone(), balance));
    }
    Some(Seeds { total_supply, holdings, paused, yield_token, supply_cap })
}

/// The most decimals a token can have and still be priced: `10^77` is the largest power of ten a
/// 256-bit integer holds.
const MAX_DECIMALS: u32 = 77;

/// A `decimals()` answer as a small integer, or `None` past [`MAX_DECIMALS`]. ABI decoding does not
/// range-check a `uint8`, and converting an out-of-range answer would panic and halt the stream, so
/// a token answering one is skipped instead.
pub fn bounded_decimals(value: &BigInt) -> Option<u32> {
    value
        .to_string()
        .parse::<u32>()
        .ok()
        .filter(|decimals| *decimals <= MAX_DECIMALS)
}

fn decode_decimals(response: &RpcResponse) -> Option<u32> {
    RpcBatch::decode::<_, pendle_sy::functions::Decimals>(response)
        .and_then(|decimals| bounded_decimals(&decimals))
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
        let Some(value) = RpcBatch::decode::<_, erc20::functions::Decimals>(response)
            .and_then(|decimals| bounded_decimals(&decimals))
        else {
            continue;
        };
        decimals.push((token, value));
    }
    decimals
}

/// Probes `previewDeposit` with one whole unit of each entry token.
///
/// Returns every entry token, with `None` where its decimals or its probe did not answer.
fn probe_deposits(
    sy_address: &[u8],
    tokens_in: &[Vec<u8>],
    token_decimals: &[(Vec<u8>, u32)],
) -> Vec<(Vec<u8>, Option<BigInt>)> {
    let mut results = Vec::new();
    let mut probes = Vec::new();
    for token in tokens_in {
        match token_decimals
            .iter()
            .find(|(t, _)| t == token)
        {
            Some((_, decimals)) => probes.push((token.clone(), *decimals)),
            None => results.push((token.clone(), None)),
        }
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
    let mut values = execute_probes::<pendle_sy::functions::PreviewDeposit>(batch).into_iter();
    for (token, _) in probes {
        results.push((token, values.next().flatten()));
    }
    results
}

/// Probes `previewRedeem` with one whole SY unit for each exit token.
///
/// Returns every exit token, with its own decimals and probe — the redeem conversion needs the
/// decimals to rescale the asset-unit result the SY reports — or `None` where either did not
/// answer.
fn probe_redeems(
    sy_address: &[u8],
    tokens_out: &[Vec<u8>],
    token_decimals: &[(Vec<u8>, u32)],
    sy_decimals: u32,
) -> Vec<(Vec<u8>, Option<RedeemProbe>)> {
    let mut results = Vec::new();
    let mut probes = Vec::new();
    for token in tokens_out {
        match token_decimals
            .iter()
            .find(|(t, _)| t == token)
        {
            Some((_, decimals)) => probes.push((token.clone(), *decimals)),
            None => results.push((token.clone(), None)),
        }
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

    let mut values = execute_probes::<pendle_sy::functions::PreviewRedeem>(batch).into_iter();
    for (token, decimals) in probes {
        results.push((
            token,
            values
                .next()
                .flatten()
                .map(|value| (decimals, value)),
        ));
    }
    results
}

/// An exit token's decimals and what `previewRedeem` returned for it.
type RedeemProbe = (u32, BigInt);

/// Executes a probe batch, yielding one entry per call in the order they were added.
///
/// A reverting probe — a token the view rejects, or a wrapped protocol that refuses it — yields
/// `None`. A failed batch yields nothing, so every probe in it reads as `None` to the caller.
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    /// USDC into SY-PPLUS: the SY computes in USDC's six decimals and scales up, so one USDC mints
    /// 978,906 × 10^12 SY wei and every mint is a multiple of 10^12. A mint that is not a whole
    /// multiple, or a token no coarser than the SY, has no step.
    #[test]
    fn a_mint_rounded_to_the_token_precision_has_a_step() {
        let pplus = BigInt::from(978_906u64) * pow10(12);
        assert_eq!(deposit_step(&pplus, 18, 6), pow10(12));
        assert_eq!(deposit_step(&(pplus + BigInt::one()), 18, 6), BigInt::one());
        assert_eq!(deposit_step(&pow10(18), 18, 18), BigInt::one());
    }

    /// The sUSDat vault took its 25 bp entry fee at block 26,100,000: one USDat minted
    /// 958,437,362,004,859,566 SY wei against 960,839,460,656,500,818 without it. An SY that pays
    /// the fee still classifies as `index_rate` from that probe; anything more than 1% below the
    /// closed form, or above it, does not.
    #[test]
    fn a_probe_taken_while_the_entry_fee_is_charged_still_classifies() {
        let without_fee = BigInt::from(960_839_460_656_500_818u64);
        let rate = pow10(6) * BigInt::from(RATE_ONE) / without_fee.clone();
        let index_rate = pow10(6) * BigInt::from(RATE_ONE) / rate.clone();
        let with_fee = BigInt::from(958_437_362_004_859_566u64);
        assert_eq!(classify_deposit(&with_fee, 18, 6, &rate), None);
        assert_eq!(
            classify_deposit_net_of_entry_fee(&with_fee, 6, &rate),
            Some(TokenClass::IndexRate)
        );
        let two_percent_below = index_rate.clone() * BigInt::from(98) / BigInt::from(100);
        assert_eq!(classify_deposit_net_of_entry_fee(&two_percent_below, 6, &rate), None);
        assert_eq!(
            classify_deposit_net_of_entry_fee(&(index_rate + BigInt::one()), 6, &rate),
            None
        );
    }

    /// Every fixture below is a value read from Ethereum mainnet, not one computed from this
    /// module's own constants. That matters: predictions derived from `RATE_ONE` would agree
    /// with a wrongly-scaled `RATE_ONE` and the tests would pass while every quote was wrong.
    ///
    /// Every fixture within one module is a single-block snapshot: the rate and the previews
    /// must come from the same block, or the rate accrues between reads and the prediction is
    /// compared against a probe taken under a different rate.
    ///
    /// SY-wstETH `0xcbc72d92...c0bc` at block 25803485 -- 18-decimal SY over 18-decimal stETH.
    mod wsteth_sy {
        pub const SY_DECIMALS: u32 = 18;
        pub const ASSET_DECIMALS: u32 = 18;
        pub const EXCHANGE_RATE: u64 = 1_242_190_142_783_145_750;
        /// `previewDeposit(wstETH, 1e18)` -- the SY's own wrapper token, taken at par.
        pub const DEPOSIT_WSTETH: u64 = 1_000_000_000_000_000_000;
        /// `previewDeposit(stETH, 1e18)` -- the accounting asset, converted at the rate.
        pub const DEPOSIT_STETH: u64 = 805_029_733_821_172_431;
        /// `previewRedeem(1e18)` to stETH.
        pub const REDEEM_STETH: u64 = 1_242_190_142_783_145_750;
        /// `previewRedeem(1e18)` to wstETH.
        pub const REDEEM_WSTETH: u64 = 1_000_000_000_000_000_000;
    }

    /// SY `0x457904b5...59d6` at block 25813586 -- 18-decimal SY over 6-decimal USDC. The
    /// decimal gap lives in the rate, which is why the rate is ~1.12e6 rather than ~1.12e18.
    mod usdc_sy {
        pub const SY_DECIMALS: u32 = 18;
        pub const ASSET_DECIMALS: u32 = 6;
        pub const USDC_DECIMALS: u32 = 6;
        pub const EXCHANGE_RATE: u64 = 1_121_519;
        /// `previewDeposit(USDC, 1e6)`.
        pub const DEPOSIT_USDC: u64 = 891_647_606_956_028_309;
        /// `previewRedeem(1e18)` to USDC. Below the rate: this SY takes a haircut on exit, so
        /// neither closed form explains it.
        pub const REDEEM_USDC: u64 = 1_115_911;
    }

    /// SY-siUSD `0x9f30507c...a920` at block 25810474 -- 18-decimal SY over an 18-decimal asset
    /// that also redeems to 6-decimal USDC. The only live shape where the exit token's decimals
    /// differ from the accounting asset's, so it is the only fixture that exercises the redeem
    /// rescale.
    mod cross_decimal_sy {
        pub const SY_DECIMALS: u32 = 18;
        pub const ASSET_DECIMALS: u32 = 18;
        pub const USDC_DECIMALS: u32 = 6;
        pub const EXCHANGE_RATE: u64 = 1_084_785_195_710_495_612;
        /// `previewRedeem(1e18)` to USDC.
        pub const REDEEM_USDC: u64 = 1_084_785;
    }

    /// A real wrapper taken at par, where the two predictions are far apart -- the only case in
    /// which `OneToOne` is provable.
    #[test]
    fn wsteth_deposit_of_wrapper_token_is_one_to_one() {
        assert_eq!(
            classify_deposit(
                &BigInt::from(wsteth_sy::DEPOSIT_WSTETH),
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::OneToOne)
        );
    }

    /// The same SY converts its accounting asset at the rate. The observed value sits one wei
    /// above the exact quotient, so this also pins the tolerance.
    #[test]
    fn wsteth_deposit_of_accounting_asset_is_index_rate() {
        assert_eq!(
            classify_deposit(
                &BigInt::from(wsteth_sy::DEPOSIT_STETH),
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::IndexRate)
        );
    }

    #[test]
    fn wsteth_redeem_splits_by_token() {
        assert_eq!(
            classify_redeem(
                &BigInt::from(wsteth_sy::REDEEM_STETH),
                wsteth_sy::ASSET_DECIMALS,
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::IndexRate)
        );
        assert_eq!(
            classify_redeem(
                &BigInt::from(wsteth_sy::REDEEM_WSTETH),
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::OneToOne)
        );
    }

    /// The decimal axis: an 18-decimal SY over a 6-decimal asset. Fails if the index is treated
    /// as `10^sy_decimals` instead of 1e18.
    #[test]
    fn deposit_across_a_decimal_gap_is_index_rate() {
        assert_eq!(
            classify_deposit(
                &BigInt::from(usdc_sy::DEPOSIT_USDC),
                usdc_sy::SY_DECIMALS,
                usdc_sy::ASSET_DECIMALS,
                &BigInt::from(usdc_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::IndexRate)
        );
    }

    /// The tolerance is the widest entry haircut a quotable SY may take, because the simulation
    /// quotes the closed form and so over-quotes such a deposit by the haircut. The widest the
    /// sweep found is 1e-6; this pins that a haircut at 1e-6 still classifies and anything wider,
    /// 5e-6 included, is left out rather than quoted five times worse.
    #[test]
    fn an_entry_haircut_classifies_up_to_one_in_a_million() {
        let one_to_one = BigInt::from(1_000_000_000_000_000_000u64);
        let classify_at = |observed: BigInt| {
            classify_deposit(
                &observed,
                wsteth_sy::SY_DECIMALS,
                wsteth_sy::ASSET_DECIMALS,
                &BigInt::from(wsteth_sy::EXCHANGE_RATE),
            )
        };
        let one_in_a_million = BigInt::from(1_000_000_000_000u64);

        assert_eq!(
            classify_at(one_to_one.clone() - one_in_a_million.clone()),
            Some(TokenClass::OneToOne)
        );
        assert_eq!(
            classify_at(one_to_one.clone() - one_in_a_million.clone() - BigInt::from(1)),
            None
        );
        assert_eq!(classify_at(one_to_one - one_in_a_million * BigInt::from(5)), None);
    }

    /// A real exit that neither closed form explains -- this SY redeems ~0.5% below the rate.
    /// It must be excluded, not rounded into the nearer class.
    #[test]
    fn redeem_with_a_haircut_is_rejected() {
        assert_eq!(
            classify_redeem(
                &BigInt::from(usdc_sy::REDEEM_USDC),
                usdc_sy::USDC_DECIMALS,
                usdc_sy::SY_DECIMALS,
                usdc_sy::ASSET_DECIMALS,
                &BigInt::from(usdc_sy::EXCHANGE_RATE),
            ),
            None
        );
    }

    /// Redeeming an 18-decimal SY into a 6-decimal token: the asset-unit result has to be
    /// rescaled to the exit token's decimals. Without that rescale the prediction is off by
    /// twelve orders of magnitude and the token would be dropped as unquotable.
    #[test]
    fn redeem_rescales_to_the_exit_token_decimals() {
        assert_eq!(
            classify_redeem(
                &BigInt::from(cross_decimal_sy::REDEEM_USDC),
                cross_decimal_sy::USDC_DECIMALS,
                cross_decimal_sy::SY_DECIMALS,
                cross_decimal_sy::ASSET_DECIMALS,
                &BigInt::from(cross_decimal_sy::EXCHANGE_RATE),
            ),
            Some(TokenClass::IndexRate)
        );
    }

    /// A rate of exactly one unit makes both predictions identical, so creation-time evidence
    /// cannot separate them and the tie must go to `IndexRate`.
    #[test]
    fn rate_of_one_ties_to_index_rate() {
        let observed = pow10(18);
        let rate = BigInt::from(RATE_ONE);
        assert_eq!(classify_deposit(&observed, 18, 18, &rate), Some(TokenClass::IndexRate));
        assert_eq!(classify_redeem(&observed, 18, 18, 18, &rate), Some(TokenClass::IndexRate));
    }

    /// A zero rate is a paused or uninitialised SY, not a 1:1 wrapper.
    #[test]
    fn zero_rate_is_rejected() {
        let observed = pow10(18);
        assert_eq!(classify_deposit(&observed, 18, 18, &BigInt::zero()), None);
        assert_eq!(classify_redeem(&observed, 18, 18, 18, &BigInt::zero()), None);
    }

    /// SY-sUSDe `0xbf98...5d02` on mainnet: a cap of 2.5e27 on its own supply, 2.26e25 minted.
    #[test]
    fn a_cap_on_the_sys_own_supply_is_kept() {
        let supply = BigInt::from_str("22606883884338964498272371").unwrap();
        let cap = BigInt::from_str("2500000000000000000000000000").unwrap();
        assert_eq!(
            own_supply_cap(Some(cap.clone()), Some(supply.clone()), None, &supply),
            Some(cap)
        );
    }

    /// An SY that caps a vault's supply rather than its own would need that supply read every
    /// block; an uncapped SY answers `type(uint256).max`; one without the getters has no cap.
    #[test]
    fn caps_on_another_supply_or_none_at_all_are_dropped() {
        let supply = BigInt::from(1_000);
        let cap = BigInt::from(5_000);
        assert_eq!(
            own_supply_cap(Some(cap.clone()), Some(BigInt::from(2_000)), None, &supply),
            None
        );
        let uncapped = BigInt::from(2).pow(256) - BigInt::from(1);
        assert!(is_uncapped(&uncapped));
        assert_eq!(
            own_supply_cap(Some(uncapped.clone()), Some(supply.clone()), None, &supply),
            None
        );
        assert_eq!(own_supply_cap(None, None, None, &supply), None);
        assert_eq!(own_supply_cap(Some(cap), None, None, &supply), None);
        assert_eq!(own_supply_cap(None, None, Some(uncapped), &supply), None);
    }

    /// `PendleERC20WithSupplyCapSY` has no absolute getters, only `supplyCap()`, which it checks
    /// against its own `totalSupply()`.
    #[test]
    fn a_plain_supply_cap_counts_when_the_absolute_getters_are_missing() {
        let supply = BigInt::from(1_000);
        let cap = BigInt::from(5_000);
        assert_eq!(own_supply_cap(None, None, Some(cap.clone()), &supply), Some(cap));
    }

    /// A token whose probe did not answer is listed once, whichever directions it failed in. One
    /// whose decimals did not answer either, such as native ETH as the zero address, is not.
    #[test]
    fn a_token_whose_probe_did_not_answer_is_listed_once() {
        let (sy, wsteth, steth, eth) =
            (vec![0xcb; 20], vec![0x7f; 20], vec![0xae; 20], vec![0; 20]);
        let unprobed = unprobed(
            &[steth.clone(), eth.clone()],
            std::slice::from_ref(&steth),
            &[(wsteth.clone(), 18), (steth.clone(), 18)],
        );
        assert_eq!(unprobed, vec![steth.clone()]);
        assert_eq!(
            component_tokens(&sy, &[(wsteth.clone(), TokenClass::OneToOne)], &[], &unprobed),
            vec![sy, wsteth, steth]
        );
    }

    /// An absurd `decimals()` answer would panic converting and halt the stream; it is skipped.
    #[test]
    fn decimals_past_what_can_be_priced_are_rejected() {
        assert_eq!(bounded_decimals(&BigInt::from(18)), Some(18));
        assert_eq!(bounded_decimals(&BigInt::from(77)), Some(77));
        assert_eq!(bounded_decimals(&BigInt::from(78)), None);
        assert_eq!(bounded_decimals(&(BigInt::from(2).pow(255))), None);
    }
}
