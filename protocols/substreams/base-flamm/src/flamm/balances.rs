// Copyright (c) 2026 Everlong Labs Limited
//! Component balances: the pool's tradable inventory as the contracts themselves count it (design
//! 5.1, schema section 4), computed from the tracked words.
//!
//! * pool asset = `physicalPoolAsset + Σ_{venues not retired} min(position.collateral,
//!   venue.managedCollateral)` (`FLAMMGateLib.grossOf`, `FLAMMGateLib.sol:166-169` →
//!   `MMRouterLib.positions`, `MMRouterLib.sol:488-507` → `read`, `:556-563`);
//! * loan asset `i` = `loans[i].liquid + Σ_{venues of loan i, not retired} recognizedSupplied`,
//!   where `recognizedSupplied` values `min(position.supplyShares, venue.managedSupplyShares)` at
//!   the market's totals (`MorphoBlueAccount.tryPosition`, `MorphoBlueAccount.sol:294-309`, and
//!   `supplySharesToAssets`, `:329-333`: `mulDiv(shares, totalSupplyAssets + 1, totalSupplyShares +
//!   1e6)` rounding down).
//!
//! Two documented simplifications: the Morpho totals are the stored ones (the contract accrues them
//! to `block.timestamp` first; the decoder, which carries the IRM, does that), and every venue is
//! taken as readable (an IRM the account cannot read makes the contract count the venue as zero).
//! Neither touches a quote.
use ethabi::ethereum_types::{U256, U512};

use crate::flamm::{
    keys::{self, field, Address, Word},
    words::WordView,
    PoolConfig,
};

/// Morpho `SharesMathLib`: `VIRTUAL_SHARES = 1e6`, `VIRTUAL_ASSETS = 1`
/// (`MorphoBlueAccount.sol:26-27`).
const VIRTUAL_SHARES: u64 = 1_000_000;
const VIRTUAL_ASSETS: u64 = 1;

/// `MMRouterLib.Venue` packing (`MMRouterLib.sol:51-63`): word +2 holds `kind @0 | loanIndex @1 |
/// lltvWad @2 | borrowEnabled @10 | supplyEnabled @11 | retired @12 | debtCap @13`; +4
/// `managedCollateral`; +5 `managedSupplyShares`.
const VENUE_FLAGS_WORD: u64 = 2;
const VENUE_MANAGED_COLLATERAL_WORD: u64 = 4;
const VENUE_MANAGED_SUPPLY_SHARES_WORD: u64 = 5;
const VENUE_LOAN_INDEX_OFFSET: usize = 1;
const VENUE_RETIRED_OFFSET: usize = 12;

/// `FLAMMStore.S.physicalPoolAsset` is namespace word 12 (`FLAMMStore.sol:267`); `LoanCfg.liquid`
/// is loan word 5 (`:216`).
const POOL_PHYSICAL_WORD: u64 = 12;
const LOAN_LIQUID_WORD: u64 = 5;

fn u256(w: &Word) -> U256 {
    U256::from_big_endian(w)
}

/// `Math.mulDiv(a, b, c)` rounding down (OpenZeppelin), exact in 512 bits.
pub fn mul_div_down(a: U256, b: U256, c: U256) -> Option<U256> {
    if c.is_zero() {
        return None;
    }
    let q = a.full_mul(b) / U512::from(c);
    U256::try_from(q).ok()
}

/// `mulDiv(shares, totalSupplyAssets + VIRTUAL_ASSETS, totalSupplyShares + VIRTUAL_SHARES)`.
pub fn supply_shares_to_assets(
    shares: U256,
    total_supply_assets: U256,
    total_supply_shares: U256,
) -> Option<U256> {
    if shares.is_zero() {
        return Some(U256::zero());
    }
    mul_div_down(
        shares,
        total_supply_assets + U256::from(VIRTUAL_ASSETS),
        total_supply_shares + U256::from(VIRTUAL_SHARES),
    )
}

/// The inventory per token after transaction `tx_index`, or `None` when a Morpho total it needs is
/// not known yet. A FLAMM-owned word that was never written is zero (the pool and router are
/// tracked from their creation), and so is the Morpho position of the venue account (created with
/// the pool).
pub fn balances(
    cfg: &PoolConfig,
    view: &WordView<'_>,
    tx_index: u64,
) -> Option<Vec<(Address, U256)>> {
    inventory(cfg, |address, key| view.at(address, key, tx_index))
}

/// The inventory per token before transaction `tx_index` (after every earlier transaction of the
/// block, else as of the start of the block).
pub fn balances_before(
    cfg: &PoolConfig,
    view: &WordView<'_>,
    tx_index: u64,
) -> Option<Vec<(Address, U256)>> {
    inventory(cfg, |address, key| view.before_tx(address, key, tx_index))
}

fn inventory(
    cfg: &PoolConfig,
    word: impl Fn(&Address, &Word) -> Option<Word>,
) -> Option<Vec<(Address, U256)>> {
    let own = |address: &Address, key: &Word| -> Word { word(address, key).unwrap_or([0u8; 32]) };
    let physical = u256(&own(&cfg.pool, &keys::add(&keys::FLAMM_NS, POOL_PHYSICAL_WORD)));
    let mut pool_asset = physical;
    let mut loans: Vec<U256> = Vec::with_capacity(cfg.loan_assets.len());
    for i in 0..cfg.loan_assets.len() {
        loans.push(u256(&own(&cfg.pool, &keys::add(&keys::pool_loan(i as u64), LOAN_LIQUID_WORD))));
    }
    for (v, venue) in cfg.venues.iter().enumerate() {
        let head = keys::router_venue(&cfg.pool, v as u64);
        let flags = own(&cfg.router, &keys::add(&head, VENUE_FLAGS_WORD));
        if field(&flags, VENUE_RETIRED_OFFSET, 1) != 0 {
            continue;
        }
        let loan_index = field(&flags, VENUE_LOAN_INDEX_OFFSET, 1) as usize;
        let managed_collateral =
            u256(&own(&cfg.router, &keys::add(&head, VENUE_MANAGED_COLLATERAL_WORD)));
        let managed_shares =
            u256(&own(&cfg.router, &keys::add(&head, VENUE_MANAGED_SUPPLY_SHARES_WORD)));
        let [shares_key, collateral_key] =
            keys::morpho_position_keys(&venue.market_id, &venue.account);
        let shares = word(&venue.morpho, &shares_key)
            .map(|w| u256(&w))
            .unwrap_or_default();
        let collateral = word(&venue.morpho, &collateral_key)
            .map(|w| U256::from(field(&w, 16, 16)))
            .unwrap_or_default();
        pool_asset += collateral.min(managed_collateral);
        let managed = shares.min(managed_shares);
        if !managed.is_zero() {
            let [market0, ..] = keys::morpho_market_keys(&venue.market_id);
            let totals = word(&venue.morpho, &market0)?;
            let tsa = U256::from(field(&totals, 0, 16));
            let tss = U256::from(field(&totals, 16, 16));
            let recognized = supply_shares_to_assets(managed, tsa, tss)?;
            if let Some(loan) = loans.get_mut(loan_index) {
                *loan += recognized;
            }
        }
    }
    let mut out = vec![(cfg.pool_asset, pool_asset)];
    out.extend(
        cfg.loan_assets
            .iter()
            .copied()
            .zip(loans),
    );
    Some(out)
}

pub fn balance_bytes(value: &U256) -> Vec<u8> {
    let mut w = [0u8; 32];
    value.to_big_endian(&mut w);
    w.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_value_like_morpho() {
        // 1e6 virtual shares and 1 virtual asset: an empty market values shares at 1e-6 assets
        // each.
        assert_eq!(
            supply_shares_to_assets(U256::from(2_000_000u64), U256::zero(), U256::zero()),
            Some(U256::from(2u64))
        );
        assert_eq!(
            supply_shares_to_assets(U256::from(7u64), U256::from(10u64), U256::from(3u64)),
            Some(U256::from(7u64 * 11 / 1_000_003))
        );
        assert_eq!(
            supply_shares_to_assets(U256::zero(), U256::from(1u64), U256::zero()),
            Some(U256::zero())
        );
        let big = U256::MAX / U256::from(2u64);
        assert_eq!(mul_div_down(big, U256::from(4u64), U256::from(1u64)), None);
        assert_eq!(mul_div_down(big, U256::from(2u64), U256::from(2u64)), Some(big));
    }
}
