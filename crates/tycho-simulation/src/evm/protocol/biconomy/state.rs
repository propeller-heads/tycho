use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet, HashMap},
};

use alloy::primitives::U256;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, BlockContext, GetAmountOutResult, PoolSwap, Price, ProtocolSim,
            QueryPoolSwapParams, SwapConstraint,
        },
    },
    Bytes,
};

use super::{
    decoder::{allowance_key, apply_attributes, board_key, hex_address, pair_key, AttributeChange},
    math::{
        after_protocol_fee, board_view, merge, Address, BlockEnv, Board, BoardInputs, BoardView,
        Plan, PricingError,
    },
};
use crate::evm::protocol::u256_num::{biguint_to_u256, u256_to_biguint};

/// Gas of a venue swap. The venue reads every registered maker's board (a live board loads its
/// levels and calls the provider's `signer` and `available`), then fills each maker the order
/// lands on. Measured on Base at block 52549180 (5 makers, 1 live): `quote` 166k to 177k,
/// `board()` 34k dark and 79k live, all including the 21k transaction base; 7 swaps with one
/// fill each used 328k to 355k.
const BASE_GAS: u64 = 40_000;
const GAS_PER_DARK_BOARD: u64 = 15_000;
const GAS_PER_LIVE_BOARD: u64 = 60_000;
const GAS_PER_FILL: u64 = 130_000;

/// How a provider's `available(token)` is computed, from its indexed config.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProviderKind {
    /// The provider's own token balance.
    Balance,
    /// The smaller of the vault's balance and its allowance to the provider.
    Vault(Address),
    /// No inventory cap.
    Unbounded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
}

/// One Biconomy PropAMM venue: every registered maker's boards for every pair it serves, as the
/// executor stores them, plus the venue's fee and maker list and the token balances and
/// allowances behind each configured provider's inventory.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BiconomyState {
    pub venue: Address,
    /// The executor the venue fills through. A provider must approve it.
    pub executor: Address,
    pub fee_bps: u16,
    /// `makerList.length`; entries in `maker_slots` beyond it are ignored.
    pub maker_count: u64,
    /// `makerList` entries by index.
    pub maker_slots: BTreeMap<u8, Address>,
    /// Raw storage words of each board, by `board_key(mm, tokenIn, tokenOut)` and slot.
    pub boards: BTreeMap<String, BTreeMap<u8, U256>>,
    /// Raw anchor word, by `board_key(mm, token0, token1)` for the sorted pair.
    pub anchors: BTreeMap<String, U256>,
    pub paused: BTreeSet<Address>,
    /// Configured providers, by address. A provider without a config has no inventory.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Each provider's `signer()`, by provider. A board is dark unless it is the maker.
    pub signers: BTreeMap<String, Address>,
    /// Each provider's `approvedExecutor`, by provider. `board()` does not check it, so the venue
    /// still routes to the maker; a quote that allocates to it fails, as the swap would revert.
    pub approved_executors: BTreeMap<String, Address>,
    /// Each vault's router array length, by vault. `router()` is zero while it is empty.
    pub router_counts: BTreeMap<String, U256>,
    /// Each vault's first router, by vault. A vault provider has no inventory unless the
    /// vault's `router()` is the provider.
    pub routers: BTreeMap<String, Address>,
    /// Raw `balanceOf` words, by `pair_key(token, holder)`.
    pub balances: BTreeMap<String, U256>,
    /// `allowance` values, by `allowance_key(token, owner, spender)`.
    pub allowances: BTreeMap<String, U256>,
    /// The block a quote executes in.
    pub block: BlockEnvState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlockEnvState {
    pub number: u64,
    pub timestamp: u64,
}

/// Balance words can carry flags above bit 255 (USDC keeps a blacklist bit there).
fn balance_value(word: U256) -> U256 {
    word & (U256::MAX >> 1)
}

impl BiconomyState {
    /// Registered makers in registry order, which breaks price ties.
    pub fn makers(&self) -> Vec<Address> {
        (0..self.maker_count.min(u64::from(u8::MAX)))
            .map(|i| {
                self.maker_slots
                    .get(&(i as u8))
                    .copied()
                    .unwrap_or_default()
            })
            .collect()
    }

    fn balance(&self, token: &Address, holder: &Address) -> U256 {
        self.balances
            .get(&pair_key(token, holder))
            .copied()
            .map(balance_value)
            .unwrap_or_default()
    }

    fn allowance(&self, token: &Address, owner: &Address, spender: &Address) -> U256 {
        self.allowances
            .get(&allowance_key(token, owner, spender))
            .copied()
            .unwrap_or_default()
    }

    /// `provider.available(token)` as the provider computes it, or `None` for a provider
    /// without a config.
    fn available(&self, provider: &Address, token: &Address) -> Option<U256> {
        let config = self
            .providers
            .get(&hex_address(provider))?;
        Some(match config.kind {
            ProviderKind::Balance => self.balance(token, provider),
            ProviderKind::Vault(vault) if self.router(&vault) == Some(*provider) => self
                .balance(token, &vault)
                .min(self.allowance(token, &vault, provider)),
            ProviderKind::Vault(_) => U256::ZERO,
            ProviderKind::Unbounded => U256::MAX,
        })
    }

    /// The vault's `router()`: the first element of its router array, `None` while empty.
    fn router(&self, vault: &Address) -> Option<Address> {
        let key = hex_address(vault);
        if self
            .router_counts
            .get(&key)
            .is_none_or(|count| count.is_zero())
        {
            return None;
        }
        self.routers.get(&key).copied()
    }

    /// Whether `provider` names `mm` as its signer, the only provider check `board()` makes.
    fn signer_matches(&self, provider: &Address, mm: &Address) -> bool {
        let key = hex_address(provider);
        self.providers.contains_key(&key) && self.signers.get(&key) == Some(mm)
    }

    /// Whether `provider` lets this venue's executor fill through it. `board()` does not check
    /// it, so the venue still routes to the maker, and the fill then reverts.
    fn approves_executor(&self, provider: &Address) -> bool {
        self.approved_executors
            .get(&hex_address(provider)) ==
            Some(&self.executor)
    }

    fn env(&self) -> BlockEnv {
        BlockEnv { number: self.block.number, timestamp: self.block.timestamp }
    }

    fn board(&self, mm: &Address, token_in: &Address, token_out: &Address) -> Board {
        self.boards
            .get(&board_key(mm, token_in, token_out))
            .map(Board::decode)
            .unwrap_or_else(|| Board::decode(&BTreeMap::new()))
    }

    fn anchor(&self, mm: &Address, token_in: &Address, token_out: &Address) -> U256 {
        let (t0, t1) =
            if token_in < token_out { (token_in, token_out) } else { (token_out, token_in) };
        self.anchors
            .get(&board_key(mm, t0, t1))
            .copied()
            .unwrap_or_default()
    }

    /// Every maker's board for a direction, as the venue reads them.
    fn board_views(
        &self,
        token_in: &Address,
        token_out: &Address,
    ) -> Result<(Vec<Board>, Vec<BoardView>), PricingError> {
        let env = self.env();
        let makers = self.makers();
        let mut boards = Vec::with_capacity(makers.len());
        let mut views = Vec::with_capacity(makers.len());
        for mm in &makers {
            let board = self.board(mm, token_in, token_out);
            let inputs = BoardInputs {
                board: &board,
                anchor: self.anchor(mm, token_in, token_out),
                paused: self.paused.contains(mm),
                signer_matches: self.signer_matches(&board.provider, mm),
                available_out: self.available(&board.provider, token_out),
            };
            views.push(board_view(&inputs, token_in, token_out, &env)?);
            boards.push(board);
        }
        Ok((boards, views))
    }

    /// `PropAMMVenue.quote`, plus the plan behind it.
    fn quote(
        &self,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
    ) -> Result<(U256, Plan, Vec<Board>, BigUint), SimulationError> {
        if amount_in.is_zero() {
            return Err(inactive());
        }
        let (boards, views) = self
            .board_views(token_in, token_out)
            .map_err(pricing_error)?;
        let plan = merge(&views, amount_in);
        if plan.covered < amount_in || plan.out.is_zero() {
            return Err(inactive());
        }
        self.check_fillable(&boards, &plan)?;
        let gas = gas(&views, &plan);
        Ok((after_protocol_fee(plan.out, self.fee_bps), plan, boards, gas))
    }

    /// Whether the plan allocates to a maker whose provider no longer approves the executor.
    fn routes_to_stuck_maker(&self, boards: &[Board], plan: &Plan) -> bool {
        plan.alloc
            .iter()
            .zip(boards)
            .any(|(alloc, board)| !alloc.is_zero() && !self.approves_executor(&board.provider))
    }

    /// The venue routes to a maker whose provider no longer approves the executor, and the swap
    /// then reverts with `FillLegFailed`. A plan that allocates to one cannot be executed.
    fn check_fillable(&self, boards: &[Board], plan: &Plan) -> Result<(), SimulationError> {
        if self.routes_to_stuck_maker(boards, plan) {
            return Err(SimulationError::RecoverableError(
                "Biconomy venue would route to a maker whose provider does not approve the \
                 executor, so the swap reverts (FillLegFailed)"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    /// The state after the venue fills `plan`: each touched board's meters advance as the
    /// executor's fill stores them, and the tokens move as the provider moves them (input in,
    /// output out of the provider or its vault, spending the vault's allowance).
    fn after_fill(
        &self,
        token_in: &Address,
        token_out: &Address,
        plan: &Plan,
        boards: &[Board],
    ) -> Self {
        let mut next = self.clone();
        let number = self.block.number;
        for (i, mm) in self.makers().iter().enumerate() {
            let alloc = plan.alloc[i];
            if alloc.is_zero() {
                continue;
            }
            let board = &boards[i];
            let in_block =
                if board.last_fill_block == number { board.filled_in_block + alloc } else { alloc };
            let words = next
                .boards
                .entry(board_key(mm, token_in, token_out))
                .or_default();
            Board::record_fill(words, board.filled + alloc, number, in_block);

            next.move_inventory(&board.provider, token_in, token_out, alloc, plan.out_by_maker[i]);
        }
        next
    }
}

impl BiconomyState {
    /// The largest amount up to `capacity` the venue fills in full without allocating to a maker
    /// whose provider dropped the executor. The merge fills the best price first, so each maker's
    /// allocation only grows with the amount, and a binary search finds where the merge first
    /// reaches such a maker.
    fn executable_capacity(&self, boards: &[Board], views: &[BoardView], capacity: U256) -> U256 {
        let executable = |amount: U256| {
            let plan = merge(views, amount);
            plan.covered == amount && !self.routes_to_stuck_maker(boards, &plan)
        };
        if executable(capacity) {
            return capacity;
        }
        largest_passing(capacity, executable)
    }

    /// `TradeLimitPrice`: the largest amount whose whole-trade price, net of the fee, is at least
    /// `limit`. The merge fills the best price first, so that price only falls as the amount
    /// grows. Compared in raw units, without f64 or decimal loss.
    fn trade_limit_amount(&self, views: &[BoardView], limit: &Price, capacity: U256) -> U256 {
        let meets = |amount: U256| {
            let out = after_protocol_fee(merge(views, amount).out, self.fee_bps);
            !out.is_zero() &&
                u256_to_biguint(out) * &limit.denominator >=
                    u256_to_biguint(amount) * &limit.numerator
        };
        if meets(capacity) {
            return capacity;
        }
        largest_passing(capacity, meets)
    }

    /// `PoolTargetPrice`: the amount that moves the venue's marginal price for selling `token_in`
    /// (the next level the forward merge would fill, net of the fee) down to `target`, within
    /// `tolerance` above it. Levels are discrete, so when no level lands in that band the result
    /// stops on the last level above it, the closest price not below `target`. When every amount
    /// up to `capacity` stays above the band, the capped fill is returned, as LunarBase does.
    fn target_price_amount(
        &self,
        views: &[BoardView],
        target: &Price,
        tolerance: f64,
        capacity: U256,
    ) -> Result<U256, SimulationError> {
        // The marginal price net of the fee, over `target`: (price * (BPS - fee) / (WAD * BPS))
        // / (numerator / denominator), as a numerator and denominator in raw units.
        let fee_factor = BigUint::from(10_000u32 - u32::from(self.fee_bps));
        let scale = BigUint::from(10u8).pow(18) * BigUint::from(10_000u32);
        let versus_target = |price: U256| {
            (u256_to_biguint(price) * &fee_factor * &target.denominator, &target.numerator * &scale)
        };
        let at_least_target = |price: U256| {
            let (marginal, target) = versus_target(price);
            marginal >= target
        };
        let within_band = |price: U256| {
            let (marginal, target) = versus_target(price);
            let ratio = marginal.to_f64().unwrap_or(f64::MAX) / target.to_f64().unwrap_or(f64::MAX);
            ratio <= 1.0 + tolerance
        };

        let spot = marginal_after(views, U256::ZERO).ok_or_else(inactive)?;
        if !at_least_target(spot) {
            return Err(SimulationError::InvalidInput(
                "Target is above the Biconomy venue's marginal price".to_owned(),
                None,
            ));
        }
        if within_band(spot) {
            return Ok(U256::ZERO);
        }
        let reached = |amount: U256| marginal_after(views, amount).is_none_or(within_band);
        if !reached(capacity) {
            return Ok(capacity);
        }
        // The smallest amount that brings the marginal price into the band (or empties the book).
        let amount = largest_passing(capacity, |amount| !reached(amount)) + U256::from(1u8);
        Ok(match marginal_after(views, amount) {
            Some(price) if !at_least_target(price) => amount - U256::from(1u8),
            _ => amount,
        })
    }

    fn move_inventory(
        &mut self,
        provider: &Address,
        token_in: &Address,
        token_out: &Address,
        amount_in: U256,
        amount_out: U256,
    ) {
        let Some(config) = self
            .providers
            .get(&hex_address(provider))
            .copied()
        else {
            return;
        };
        let holder = match config.kind {
            ProviderKind::Balance => *provider,
            ProviderKind::Vault(vault) => {
                let key = allowance_key(token_out, &vault, provider);
                if let Some(allowance) = self.allowances.get_mut(&key) {
                    if *allowance != U256::MAX {
                        *allowance = allowance.saturating_sub(amount_out);
                    }
                }
                vault
            }
            ProviderKind::Unbounded => return,
        };
        for (token, delta_in, delta_out) in
            [(token_in, amount_in, U256::ZERO), (token_out, U256::ZERO, amount_out)]
        {
            let entry = self
                .balances
                .entry(pair_key(token, &holder))
                .or_default();
            let value_mask: U256 = U256::MAX >> 1;
            let flags = *entry & !value_mask;
            let value = balance_value(*entry)
                .saturating_add(delta_in)
                .saturating_sub(delta_out);
            *entry = flags | value;
        }
    }
}

/// The largest amount in `[0, high)` for which `passes` holds, given that it holds for every amount
/// below a boundary and fails from there on (`passes(0)` is taken as true, `passes(high)` as
/// false).
fn largest_passing(high: U256, passes: impl Fn(U256) -> bool) -> U256 {
    let (mut low, mut high) = (U256::ZERO, high);
    while high - low > U256::from(1u8) {
        let mid = low + (high - low) / U256::from(2u8);
        if passes(mid) {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

/// The price (raw out per raw in, 1e18-scaled) of the next unit the venue would fill after
/// `amount`: the best level any board still has room for. `None` once the book is used up.
fn marginal_after(views: &[BoardView], amount: U256) -> Option<U256> {
    let plan = merge(views, amount);
    views
        .iter()
        .zip(&plan.alloc)
        .filter(|(view, alloc)| view.remaining > **alloc)
        .filter_map(|(view, alloc)| {
            let cursor = view.filled + *alloc;
            view.levels
                .iter()
                .find(|level| level.size > cursor)
                .map(|level| level.price)
        })
        .max()
}

/// A swap amount as the venue takes it; Tycho amounts can exceed uint256.
fn amount_u256(amount: &BigUint) -> Result<U256, SimulationError> {
    if amount.bits() > 256 {
        return Err(SimulationError::InvalidInput("amount_in exceeds uint256".to_owned(), None));
    }
    Ok(biguint_to_u256(amount))
}

fn gas(views: &[BoardView], plan: &Plan) -> BigUint {
    let live = views
        .iter()
        .filter(|view| !view.remaining.is_zero())
        .count() as u64;
    let dark = views.len() as u64 - live;
    let fills = plan
        .alloc
        .iter()
        .filter(|alloc| !alloc.is_zero())
        .count() as u64;
    BigUint::from(
        BASE_GAS + GAS_PER_DARK_BOARD * dark + GAS_PER_LIVE_BOARD * live + GAS_PER_FILL * fills,
    )
}

fn inactive() -> SimulationError {
    SimulationError::RecoverableError("Biconomy venue cannot cover the size (Inactive)".to_owned())
}

fn pricing_error(err: PricingError) -> SimulationError {
    SimulationError::FatalError(format!("Biconomy venue pricing would revert: {err:?}"))
}

fn address(token: &Bytes) -> Result<Address, SimulationError> {
    token
        .as_ref()
        .try_into()
        .map_err(|_| SimulationError::InvalidInput(format!("invalid token address {token}"), None))
}

fn wad_to_f64(value: U256) -> f64 {
    value
        .to_string()
        .parse::<f64>()
        .unwrap_or(0.0) /
        1e18
}

#[typetag::serde]
impl ProtocolSim for BiconomyState {
    fn fee(&self) -> f64 {
        f64::from(self.fee_bps) / 10_000.0
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        // Buying `base` sells `quote` into the venue, so price the quote -> base boards.
        let token_in = address(&quote.address)?;
        let token_out = address(&base.address)?;
        let (_, views) = self
            .board_views(&token_in, &token_out)
            .map_err(pricing_error)?;
        // The best level any live board would fill next: the most base per quote.
        let best = views
            .iter()
            .filter(|view| !view.remaining.is_zero())
            .filter_map(|view| {
                view.levels
                    .iter()
                    .find(|level| level.size > view.filled)
            })
            .map(|level| level.price)
            .max()
            .ok_or_else(inactive)?;
        // `best` is raw base per raw quote (1e18-scaled); invert to quote per base in whole
        // tokens, then add the fee as a markup.
        let base_per_quote =
            wad_to_f64(best) * 10f64.powf(f64::from(quote.decimals as i32 - base.decimals as i32));
        if base_per_quote <= 0.0 {
            return Err(inactive());
        }
        Ok(1.0 / base_per_quote / (1.0 - self.fee()))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let tin = address(&token_in.address)?;
        let tout = address(&token_out.address)?;
        let (amount_out, plan, boards, gas) = self.quote(&tin, &tout, amount_u256(&amount_in)?)?;
        let next = self.after_fill(&tin, &tout, &plan, &boards);
        Ok(GetAmountOutResult::new(u256_to_biguint(amount_out), gas, Box::new(next)))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let tin = address(&sell_token)?;
        let tout = address(&buy_token)?;
        let (boards, views) = self
            .board_views(&tin, &tout)
            .map_err(pricing_error)?;
        let none = (BigUint::ZERO, BigUint::ZERO);
        let room = views
            .iter()
            .fold(U256::ZERO, |sum, view| sum.saturating_add(view.remaining));
        if room.is_zero() {
            return Ok(none);
        }
        // `PropAMMVenue.quoteFillable`: when the merge covers less than asked, price exactly the
        // covered amount, and report nothing if that second plan does not cover it.
        let mut plan = merge(&views, room);
        if plan.covered < room {
            let capacity = plan.covered;
            if capacity.is_zero() {
                return Ok(none);
            }
            plan = merge(&views, capacity);
            if plan.covered != capacity {
                return Ok(none);
            }
        }
        if plan.out.is_zero() {
            return Ok(none);
        }
        // The venue still quotes a plan that routes to a provider that dropped the executor, but
        // the swap reverts there: the limit stops where the merge would first reach one.
        if self
            .check_fillable(&boards, &plan)
            .is_err()
        {
            let executable = self.executable_capacity(&boards, &views, plan.covered);
            if executable.is_zero() {
                return Ok(none);
            }
            plan = merge(&views, executable);
            if plan.out.is_zero() {
                return Ok(none);
            }
        }
        Ok((
            u256_to_biguint(plan.covered),
            u256_to_biguint(after_protocol_fee(plan.out, self.fee_bps)),
        ))
    }

    fn delta_transition(
        &mut self,
        delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        let changes = delta
            .updated_attributes
            .iter()
            .filter(|(name, _)| *name != "block_number" && *name != "block_timestamp")
            .map(|(name, value)| AttributeChange::Set(name, value))
            .chain(
                delta
                    .deleted_attributes
                    .iter()
                    .map(AttributeChange::Delete),
            );
        apply_attributes(self, changes).map_err(|err| TransitionError::DecodeError(err.to_string()))
    }

    /// Walks the forward merge (the `token_in -> token_out` boards a fill moves) instead of the
    /// generic search, which reads `spot_price` as the price of selling `token_in`; here that
    /// comes from the reverse boards.
    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        let (price, tolerance, min_amount_in, max_amount_in, trade_limit) = match params
            .swap_constraint()
        {
            SwapConstraint::TradeLimitPrice { limit, tolerance, min_amount_in, max_amount_in } => {
                (limit, *tolerance, min_amount_in, max_amount_in, true)
            }
            SwapConstraint::PoolTargetPrice { target, tolerance, min_amount_in, max_amount_in } => {
                (target, *tolerance, min_amount_in, max_amount_in, false)
            }
        };
        if price.numerator == BigUint::ZERO ||
            price.denominator == BigUint::ZERO ||
            !tolerance.is_finite() ||
            tolerance < 0.0 ||
            matches!((min_amount_in, max_amount_in), (Some(min), Some(max)) if min > max)
        {
            return Err(SimulationError::InvalidInput(
                "invalid Biconomy price query bounds".to_owned(),
                None,
            ));
        }
        let token_in = params.token_in();
        let token_out = params.token_out();
        let zero = || PoolSwap::new(BigUint::ZERO, BigUint::ZERO, self.clone_box(), None);

        let (max_in, _) = self.get_limits(token_in.address.clone(), token_out.address.clone())?;
        let mut capacity = biguint_to_u256(&max_in);
        if let Some(max) = max_amount_in {
            if max.bits() <= 256 {
                capacity = capacity.min(biguint_to_u256(max));
            }
        }
        if capacity.is_zero() {
            return Ok(zero());
        }
        let (_, views) = self
            .board_views(&address(&token_in.address)?, &address(&token_out.address)?)
            .map_err(pricing_error)?;
        let amount = if trade_limit {
            self.trade_limit_amount(&views, price, capacity)
        } else {
            self.target_price_amount(&views, price, tolerance, capacity)?
        };
        let amount = u256_to_biguint(amount);
        if amount == BigUint::ZERO ||
            min_amount_in
                .as_ref()
                .is_some_and(|min| amount < *min)
        {
            return Ok(zero());
        }
        let result = self.get_amount_out(amount.clone(), token_in, token_out)?;
        Ok(PoolSwap::new(amount, result.amount, result.new_state, None))
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }

    /// Prices move with the execution block's timestamp (anchor drift, widening, expiry) and
    /// number (premium window, block cap), so every new block can change quotes.
    fn apply_block(&mut self, block: &BlockContext) -> bool {
        let next = BlockEnvState { number: block.number(), timestamp: block.timestamp() };
        if next == self.block {
            return false;
        }
        self.block = next;
        !self.boards.is_empty()
    }
}
