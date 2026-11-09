# fxSAVE deposits

Indexes the deposit route into f(x) Protocol's fxSAVE on Ethereum: USDC or fxUSD → fxSP (stability
pool share, `FxUSDBasePool`) → fxSAVE (`SavingFxUSD`, ERC4626 over fxSP), as one `fxsave`
component.

## Component

| Field | Value |
|---|---|
| id | fxSAVE address |
| tokens | USDC, fxUSD, fxSAVE |
| `swap_directions` (static) | USDC → fxSAVE, fxUSD → fxSAVE (40 bytes each: sold ‖ bought) |
| `fx_base_pool` (static) | fxSP address |
| balances | fxSP's fxUSD (`totalYieldToken`, slot 50) and USDC (`totalStableToken`, slot 51), read by `eth_call` at creation and from storage writes after |
| entrypoints | `fxSP.previewDeposit(USDC, 1e6)`, `fxSP.previewDeposit(fxUSD, 1e18)`, `fxSP.previewRedeem(1e18)`, `fxSP.stableDepegPrice()`, `fxSAVE.previewDeposit(1e18)` |

The component is created at the transaction set in the params (`creation_block`,
`creation_tx`); DCI traces the entrypoints there. The balances overstate fxSAVE's share of fxSP by
the part other holders own (about 3%); they are a TVL signal, not inventory.

Simulation is `tycho_simulation::evm::protocol::fxsave::state::FxSaveState`: it reads the inputs
below through the VM at every update and prices deposits natively. Execution is
`FxSaveExecutor`.

Facts below are from block 26,353,817 (2026-11-06).

## Contracts

| Role | Address | Notes |
|---|---|---|
| fxSAVE (vault + share) | `0x7743e50f534a7f9f1791dde7dcd89f7783eefc39` | Transparent proxy, impl `0xe4031e27…10e8a6` (`SavingFxUSD`), unchanged since ≤ block 22,500,000 |
| fxSP / fxBASE | `0x65c9a641afceb9c0e6034e558a319488fa0fa3be` | Transparent proxy, impl `0x9cfefd90…7897f9` (`FxUSDBasePool`); upgraded twice between 22.5M and 24.5M, unchanged since |
| fxUSD | `0x085780639cc2cacd35e474e71f4d000e2405d8f6` | `yieldToken` of fxSP, counted at $1 |
| USDC | `0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48` | `stableToken` of fxSP, counted at the Chainlink price |
| fxUSD price oracle | `0x3ec677ba393f2257ed0e1d37aa8442eb7f0953fa` | Proxy, impl `0x12be1e1e…dbc8aa` (`FxUSDPriceOracle`); `getUSDCPrice()` reads Chainlink |
| Chainlink USDC/USD | `0x8fffffd4afb6115b954bd326cbe7b4ba576818f6` | Encoded in the oracle's immutable `Chainlink_USDC_USD_Spot`: scale 1e10, heartbeat 129,600 s (36 h) |
| fxSP USDC strategy | `0xd023aac0e2d46c93d4c6e8e2a449bf2d4687804f` | `allocations[USDC]`, `AaveV3Strategy`, capacity 1e14; `totalSupply()` = aUSDC balance, 0 today |
| fxSP gauge | `0xed92dde3214c24ae04f5f96927e3be8f8dbc3289` | Proxy, impl `SharedLiquidityGauge`; fxSAVE stakes into it through its Convex vault |

## Deposit path

1. `fxSP.deposit(receiver, tokenIn, amount, minSharesOut)` — `tokenIn` ∈ {USDC, fxUSD}.
   - `sync` first sets `totalStableToken = USDC.balanceOf(fxSP) + strategy.totalSupply()`.
   - Reverts `ErrorStableTokenDepeg` when `getUSDCPrice() < stableDepegPrice` (0.995e18 today),
     for **both** tokens. `getUSDCPrice()` reverts `"expired"` when Chainlink is older than its
     heartbeat, also for both tokens.
   - No pause, no cap.
2. `fxSAVE.deposit(assets, receiver)` — OpenZeppelin ERC4626, `maxDeposit` = `type(uint256).max`.
   - `_deposit` also stakes fxSAVE's idle fxSP into the gauge once it reaches `getThreshold()`
     (100e18 today). Gas only; the gauge has no pause on deposit.

Exit (fxSAVE → fxSP → fxUSD + USDC) needs a 1 h cooldown or pays 1% and returns two tokens, so
it is out of scope.

## Pricing (exact, verified)

With `p = getStableTokenPriceWithScale()` (USDC price × 1e12), `Y = totalYieldToken`,
`St = previewRedeem(totalSupply).amountStableOut` (= the synced `totalStableToken`),
`S = fxSP.totalSupply()`, `A = fxSAVE.totalAssets()`, `S2 = fxSAVE.totalSupply()`:

```
usd       = tokenIn == USDC ? amount * p / 1e18 : amount
fxSP out  = usd * S / (Y + St * p / 1e18)          (S > 0)
fxSAVE out = fxSP_out * (S2 + 1) / (A + 1)          (OZ ERC4626, decimals offset 0)
```

Matched `fxSP.previewDeposit` and `fxSAVE.previewDeposit` to the wei for 1,000 USDC and
1,000 fxUSD at block 26,353,817. No size dependence: both steps are linear.

## State the quote reads

`debug_traceCall` (prestateTracer) of `fxSP.previewDeposit(USDC|fxUSD, x)`,
`fxSP.stableDepegPrice()` and `fxSAVE.previewDeposit(x)` at head touches 17 accounts: fxSP and
impl, oracle and impl, Chainlink proxy and aggregator, USDC and impl, the strategy, Aave V3 Pool
and aUSDC (read even at a zero strategy balance), fxSAVE and impl, the gauge and impl, and the
Convex vault.

Time dependence: `strategy.totalSupply()` (aUSDC accrues per second) — zero today; Chainlink
staleness gate (`block.timestamp - updatedAt > heartbeat`).

## Gas and evidence

- A USDC deposit through the router costs about 495k gas on a fork at 26,353,817, 246k of it the
  gauge stake.
- Real deposit txs seen (block 26,349,181–211, router `0xf82ef408…4263`) are Morpho loop
  positions, 0.6–1.5M gas; not representative of a plain deposit.
