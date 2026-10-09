use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet, HashMap},
};

use alloy::primitives::U256;
use num_bigint::BigUint;
use tycho_common::{
    dto::ProtocolStateDelta,
    models::token::Token,
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{
            Balances, BlockContext, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
        },
    },
    Bytes,
};

use super::{
    decoder::{apply_attributes, board_key, pair_key, AttributeChange},
    math::{
        after_protocol_fee, board_view, merge, Address, BlockEnv, Board, BoardInputs, BoardView,
        Plan, PricingError,
    },
};
use crate::evm::protocol::u256_num::{biguint_to_u256, u256_to_biguint};

/// Gas of a venue swap. The venue reads every registered maker's board (a live board loads its
/// levels and calls the provider's `available`), then fills each maker the order lands on.
/// Measured on Base Sepolia, block 47849900 (7 makers, 1 live): `quote` 285k to 292k, 38
/// router swaps with one fill 512k to 563k.
const BASE_GAS: u64 = 30_000;
const GAS_PER_DARK_BOARD: u64 = 20_000;
const GAS_PER_LIVE_BOARD: u64 = 120_000;
const GAS_PER_FILL: u64 = 200_000;

/// One Biconomy PropAMM venue: every registered maker's boards for every pair it serves, as the
/// executor stores them, plus the venue's fee and maker order and each provider's inventory.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BiconomyState {
    pub venue: Address,
    pub fee_bps: u16,
    /// Registered makers in registry order, which breaks price ties.
    pub makers: Vec<Address>,
    /// Raw storage words of each board, by `board_key(mm, tokenIn, tokenOut)` and slot.
    pub boards: BTreeMap<String, BTreeMap<u8, U256>>,
    /// Raw anchor word, by `board_key(mm, token0, token1)` for the sorted pair.
    pub anchors: BTreeMap<String, U256>,
    pub paused: BTreeSet<Address>,
    /// `provider.available(token)`, by `pair_key(provider, token)`.
    pub inventory: BTreeMap<String, U256>,
    /// The block a quote executes in.
    pub block: BlockEnvState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlockEnvState {
    pub number: u64,
    pub timestamp: u64,
}

impl BiconomyState {
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
        let mut boards = Vec::with_capacity(self.makers.len());
        let mut views = Vec::with_capacity(self.makers.len());
        for mm in &self.makers {
            let board = self.board(mm, token_in, token_out);
            let inputs = BoardInputs {
                board: &board,
                anchor: self.anchor(mm, token_in, token_out),
                paused: self.paused.contains(mm),
                available_out: self
                    .inventory
                    .get(&pair_key(&board.provider, token_out))
                    .copied(),
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
        let gas = gas(&views, &plan);
        Ok((after_protocol_fee(plan.out, self.fee_bps), plan, boards, gas))
    }

    /// The state after the venue fills `plan`: each touched board's meters advance as the
    /// executor's fill stores them, and each provider's output inventory shrinks by what it paid.
    fn after_fill(
        &self,
        token_in: &Address,
        token_out: &Address,
        plan: &Plan,
        boards: &[Board],
    ) -> Self {
        let mut next = self.clone();
        let number = self.block.number;
        for (i, mm) in self.makers.iter().enumerate() {
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

            if let Some(available) = next
                .inventory
                .get_mut(&pair_key(&board.provider, token_out))
            {
                if *available < U256::from(u128::MAX) {
                    *available = available.saturating_sub(plan.out_by_maker[i]);
                }
            }
        }
        next
    }
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
        let token_in = address(&base.address)?;
        let token_out = address(&quote.address)?;
        let (_, views) = self
            .board_views(&token_in, &token_out)
            .map_err(pricing_error)?;
        // The best level any live board would fill next.
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
        let decimals = f64::from(base.decimals as i32 - quote.decimals as i32);
        Ok(wad_to_f64(best) * 10f64.powf(decimals) * (1.0 - self.fee()))
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let tin = address(&token_in.address)?;
        let tout = address(&token_out.address)?;
        let (amount_out, plan, boards, gas) =
            self.quote(&tin, &tout, biguint_to_u256(&amount_in))?;
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
        let (_, views) = self
            .board_views(&tin, &tout)
            .map_err(pricing_error)?;
        let room = views
            .iter()
            .fold(U256::ZERO, |sum, view| sum.saturating_add(view.remaining));
        if room.is_zero() {
            return Ok((BigUint::ZERO, BigUint::ZERO));
        }
        let plan = merge(&views, room);
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

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        crate::evm::query_pool_swap::query_pool_swap(self, params)
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
