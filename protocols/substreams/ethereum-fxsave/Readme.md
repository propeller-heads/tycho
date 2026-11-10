# fxSAVE deposits

Indexes the deposit route into f(x) Protocol's fxSAVE on Ethereum: USDC or fxUSD → fxSP (stability
pool share, `FxUSDBasePool`) → fxSAVE (`SavingFxUSD`, ERC4626 over fxSP), as one `fxsave`
component.

## Component

| Field | Value |
|---|---|
| id | fxSAVE address |
| tokens | USDC, fxUSD, fxSAVE |
| `swap_directions` (static) | USDC → fxSAVE, fxUSD → fxSAVE, in the layout `tycho_simulation::book::decode_swap_directions` reads |
| `fx_base_pool` (static) | fxSP address |
| balances | fxSP's fxUSD (`totalYieldToken`, slot 50) and USDC (`totalStableToken`, slot 51): read by `eth_call` in the creation block, then the value each later transaction leaves in those slots |
| entrypoints | `fxSP.previewDeposit(USDC, 1e6)`, `fxSP.previewRedeem(1e18)`, `fxSP.stableDepegPrice()`, `fxSAVE.previewDeposit(1e18)`, `fxSAVE.getThreshold()` |

The component is created in the transaction set in the params (`creation_block`, `creation_tx`),
a deposit into fxSP and then fxSAVE chosen as the start (both contracts were deployed long
before); DCI traces the entrypoints there. The balances overstate fxSAVE's share of fxSP by the
part other holders own (about 3%); they are a TVL signal, not inventory.

Simulation is `tycho_simulation::evm::protocol::fxsave::state::FxSaveState`: it reads fxSP and
fxSAVE through the VM at every update and prices deposits natively; its module holds the pricing
formula and the gas it charges. Execution is `FxSaveExecutor`.

Out of scope: exits (fxSAVE → fxSP → fxUSD + USDC need a 1 h cooldown or pay 1%, and return two
tokens), depositing fxSP into fxSAVE alone, and fxSAVE's `depositGauge`.

Facts below are from block 26,353,817 (2026-11-06) unless stated.

## Contracts

| Role | Address | Notes |
|---|---|---|
| fxSAVE (vault + share) | `0x7743e50f534a7f9f1791dde7dcd89f7783eefc39` | Transparent proxy, impl `0xe4031e27…10e8a6` (`SavingFxUSD`) |
| fxSP / fxBASE | `0x65c9a641afceb9c0e6034e558a319488fa0fa3be` | Transparent proxy, impl `0x9cfefd90…7897f9` (`FxUSDBasePool`) |
| fxUSD | `0x085780639cc2cacd35e474e71f4d000e2405d8f6` | `yieldToken` of fxSP, counted at $1 |
| USDC | `0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48` | `stableToken` of fxSP, counted at the Chainlink price |
| fxUSD price oracle | `0x3ec677ba393f2257ed0e1d37aa8442eb7f0953fa` | Proxy, impl `0x12be1e1e…dbc8aa` (`FxUSDPriceOracle`); `getUSDCPrice()` reads Chainlink |
| Chainlink USDC/USD | `0x8fffffd4afb6115b954bd326cbe7b4ba576818f6` | Encoded in the oracle's immutable `Chainlink_USDC_USD_Spot`: scale 1e10, heartbeat 129,600 s (36 h) |
| fxSP USDC strategy | `0xd023aac0e2d46c93d4c6e8e2a449bf2d4687804f` | `allocations[USDC]`, `AaveV3Strategy`, capacity 1e14; `totalSupply()` = aUSDC balance, 0 today |
| fxSP gauge | `0xed92dde3214c24ae04f5f96927e3be8f8dbc3289` | Proxy, impl `SharedLiquidityGauge`; fxSAVE stakes into it through its Convex vault |

## Deposit path

1. `fxSP.deposit(receiver, tokenIn, amount, minSharesOut)`, `tokenIn` ∈ {USDC, fxUSD}.
   - `sync` first sets `totalStableToken = USDC.balanceOf(fxSP) + strategy.totalSupply()`.
   - Reverts `ErrorStableTokenDepeg` when `getUSDCPrice() < stableDepegPrice` (0.995e18 today),
     for **both** tokens. `getUSDCPrice()` reverts `"expired"` when Chainlink is older than its
     heartbeat, also for both tokens.
   - No pause, no cap.
2. `fxSAVE.deposit(assets, receiver)`: OpenZeppelin ERC4626, `maxDeposit` = `type(uint256).max`.
   - `_deposit` then stakes all of fxSAVE's idle fxSP into the gauge once
     `fxSP.balanceOf(fxSAVE) >= getThreshold()` (`_miscData`, slot 2; 100e18 today). This changes
     gas, not the shares minted; the gauge has no pause on deposit.

## State the quote reads

DCI traces the entrypoints so it indexes the state the simulation reads. The simulation's own
calls are fxSP's and fxSAVE's getters, `previewRedeem` and `stableDepegPrice` among them, not
`previewDeposit`.

- `fxSP.previewDeposit(USDC)`: fxSP's supply and holdings, the oracle and Chainlink feed, the USDC
  strategy, the Aave V3 pool and aUSDC (read even at a zero strategy balance). Depositing fxUSD
  reads the same state.
- `fxSP.previewRedeem`: the same fxSP state without the oracle, so fxSP stays traced while the
  oracle reverts.
- `fxSP.stableDepegPrice()`: slot 52.
- `fxSAVE.previewDeposit`: fxSAVE's supply, its idle fxSP (`fxSP.balanceOf(fxSAVE)`) and its stake
  (`gauge.balanceOf(vault)`).
- `fxSAVE.getThreshold()`: slot 2, which no other entrypoint reads.

Together (`prestateTracer`, block 26,383,653) they touch 17 accounts: fxSP, fxSAVE, the oracle,
USDC, the gauge, the Aave V3 pool and aUSDC with their implementations, the Chainlink proxy and
aggregator, and the strategy.

Time dependence: `strategy.totalSupply()` (aUSDC accrues per second), zero today; Chainlink
staleness gate (`block.timestamp - updatedAt > heartbeat`).

## Upgrade boundary

fxSP and fxSAVE are upgradeable. fxSP's implementation changed three times since block 22,000,000
(`0x1d100d…` → `0x0fd265…` → `0x9579c3…` → `0x9cfefd…`, the last by 24,000,000); fxSAVE's has been
`0xe4031e…` since at least 22,500,000. An upgrade writes the proxy's implementation slot, which
every traced call reads, so DCI re-traces the entrypoints and the VM reads follow the new code.
What does not follow it:

| Depends on the current implementation | Where | Re-check after an upgrade |
|---|---|---|
| fxSP `totalYieldToken` at slot 50, `totalStableToken` at slot 51 | `map_protocol_changes.rs` (balances) | The new storage layout keeps both slots |
| Entrypoint signatures and what they read | `add_entrypoints` | Each function still exists and still reaches the state above (re-run the prestate trace) |
| fxSP share math and its depeg/oracle gates; fxSAVE's ERC4626 rounding and gauge-stake threshold | `FxSaveState` in tycho-simulation | Port the new `previewDeposit`, `deposit` and `_deposit` and match on-chain previews to the wei |
| Both implementation addresses | tycho-simulation's implementation check | It refuses quotes while either proxy points elsewhere; update it once the rows above are re-checked |
| `fxSP.deposit(address,address,uint256,uint256)`, `fxSAVE.deposit(uint256,address)` | `FxSaveExecutor` | Signatures and return values are unchanged |
