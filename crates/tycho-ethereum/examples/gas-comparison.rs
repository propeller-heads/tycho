/// Compare EthCallDetector gas estimates against actual on-chain transfer gas costs.
///
/// The Analyzer contract measures `gasIn` and `gasOut` as `gasleft()` deltas around the
/// ERC-20 calls — these capture only EVM execution cost, with no base tx overhead or calldata.
/// On-chain transactions include those additional fixed costs:
///   - Base transaction: 21,000 gas
///   - Calldata for transfer(address,uint256): ~700 gas
///
/// This script fetches recent Transfer logs for each token, collects per-tx gasUsed from
/// receipts, subtracts the fixed overhead, and prints a comparison table.
///
/// Run with:
///   RPC_URL=<url> cargo run --example gas-comparison
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::Arc,
};

use alloy::{primitives::Address, rpc::client::ClientBuilder};
use serde_json::{json, Value};
use tycho_common::{
    models::{blockchain::BlockTag, token::TokenOwnerStore},
    Bytes,
};
use tycho_ethereum::{rpc::EthereumRpcClient, services::token_analyzer::EthCallDetector};

// ERC-20 Transfer(address,address,uint256) event topic
const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

// Fixed overhead subtracted from on-chain gasUsed before comparison.
// Base tx: 21000. Calldata for transfer(address,uint256): selector (4 non-zero bytes @ 16) +
// padded address (12 zero @ 4, 20 non-zero @ 16) + padded uint256 (~16 zero @ 4, 16 non-zero
// @ 16) = 64 + 48 + 320 + 64 + 256 = ~752. Round to 700.
const FIXED_OVERHEAD: u64 = 21_700;

const COWSWAP_SETTLEMENT: &str = "0xc9f2e6ea1637E499406986ac50ddC92401ce1f58";

struct TokenConfig {
    symbol: &'static str,
    address: &'static str,
    holder: &'static str,
    balance_hex: &'static str,
}

const TOKENS: &[TokenConfig] = &[
    TokenConfig {
        symbol: "USDC",
        address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
        holder: "0x000000000004444c5dc75cB358380D2e3dE08A90", // Uniswap V4 pool manager
        balance_hex: "0x43f6e8f16703",
    },
    TokenConfig {
        symbol: "WETH",
        address: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        holder: "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc", // Uniswap V2 WETH/USDC pair
        balance_hex: "0x2386f26fc10000",
    },
    TokenConfig {
        symbol: "USDT",
        address: "0xdAC17F958D2ee523a2206206994597C13D831ec7",
        holder: "0x5754284f345afc66a98fbB0a0Afe71e0F007B949", // Tether treasury
        balance_hex: "0x5af3107a4000",
    },
    TokenConfig {
        symbol: "DAI",
        address: "0x6B175474E89094C44Da98b954EedeAC495271d0F",
        holder: "0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7", // Curve 3pool
        balance_hex: "0x52b7d2dcc80cd2e4000000",
    },
    TokenConfig {
        symbol: "stETH",
        address: "0xae7ab96520de3a18e5e111b5eaab095312d7fe84",
        holder: "0xDC24316b9AE028F1497c275EB9192a3Ea0f67022", // Curve stETH pool
        balance_hex: "0x52b7d2dcc80cd2e4000000",
    },
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");
    let rpc = EthereumRpcClient::new(&rpc_url).expect("failed to create RPC client");
    // Raw alloy client for eth_getLogs / eth_getTransactionReceipt (not wrapped in
    // EthereumRpcClient)
    let raw_client = ClientBuilder::default().http(rpc_url.parse()?);

    let settlement: Address = COWSWAP_SETTLEMENT.parse()?;
    let tip: u64 = rpc.get_block_number().await?;
    let from_block = tip.saturating_sub(500);

    println!(
        "\nComparing EthCallDetector gas estimates vs on-chain transfer gas (blocks {from_block}..{tip})\n"
    );
    println!(
        "{:<8} {:>12} {:>14} {:>14} {:>9} {:>7}",
        "Token", "est_avg", "onchain_p50", "onchain_adj", "delta%", "n_txs"
    );
    println!("{}", "-".repeat(70));

    for t in TOKENS {
        let token_addr: Address = t.address.parse()?;

        // --- EthCallDetector estimate ---
        let holders: HashMap<Bytes, (Bytes, Bytes)> = HashMap::from([(
            Bytes::from_str(t.address)?,
            (Bytes::from_str(t.holder)?, Bytes::from_str(t.balance_hex)?),
        )]);
        let finder = TokenOwnerStore::new(holders);
        let detector = EthCallDetector::new(&rpc, Arc::new(finder), settlement);

        let est_avg: u64 = match detector
            .detect_impl(token_addr, BlockTag::Latest)
            .await
        {
            Ok((_, Some(avg), _)) => avg.try_into().unwrap_or(u64::MAX),
            Ok((quality, None, _)) => {
                println!("{:<8} detection returned no gas ({quality:?})", t.symbol);
                continue;
            }
            Err(e) => {
                println!("{:<8} detection failed: {e}", t.symbol);
                continue;
            }
        };

        // --- On-chain Transfer logs ---
        let logs: Vec<Value> = match raw_client
            .request(
                "eth_getLogs",
                (json!({
                    "fromBlock": format!("{:#x}", from_block),
                    "toBlock":   format!("{:#x}", tip),
                    "address":   format!("{token_addr:#x}"),
                    "topics":    [TRANSFER_TOPIC],
                }),),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                println!("{:<8} eth_getLogs failed: {e}", t.symbol);
                continue;
            }
        };

        // Unique tx hashes, capped to avoid spending too long on receipts
        let tx_hashes: Vec<String> = {
            let mut seen = HashSet::new();
            logs.iter()
                .filter_map(|l| {
                    l["transactionHash"]
                        .as_str()
                        .map(str::to_string)
                })
                .filter(|h| seen.insert(h.clone()))
                .take(200)
                .collect()
        };

        let mut on_chain_gas: Vec<u64> = Vec::new();
        for hash in &tx_hashes {
            let receipt: Value = match raw_client
                .request("eth_getTransactionReceipt", (hash,))
                .await
            {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Only simple single-transfer transactions (to avoid multi-hop swaps distorting data)
            let log_count = receipt["logs"]
                .as_array()
                .map(|v| v.len())
                .unwrap_or(0);
            if log_count != 1 {
                continue;
            }

            let Some(gas_used) = receipt["gasUsed"]
                .as_str()
                .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
            else {
                continue
            };

            on_chain_gas.push(gas_used);
        }

        let (p50_raw, p50_adj) = if on_chain_gas.is_empty() {
            (None, None)
        } else {
            on_chain_gas.sort_unstable();
            let p = on_chain_gas[on_chain_gas.len() / 2];
            (Some(p), Some(p.saturating_sub(FIXED_OVERHEAD)))
        };

        let delta_pct = p50_adj.map(|adj| {
            if adj == 0 {
                return 0i128;
            }
            (i128::from(est_avg) - i128::from(adj)) * 100 / i128::from(adj)
        });

        println!(
            "{:<8} {:>12} {:>14} {:>14} {:>9} {:>7}",
            t.symbol,
            est_avg,
            p50_raw
                .map(|p| p.to_string())
                .unwrap_or_else(|| "n/a".to_string()),
            p50_adj
                .map(|p| p.to_string())
                .unwrap_or_else(|| "n/a".to_string()),
            delta_pct
                .map(|d| format!("{d:+}%"))
                .unwrap_or_else(|| "n/a".to_string()),
            on_chain_gas.len(),
        );
    }

    println!(
        "\nest_avg   = (gasIn + gasOut) / 2 from Analyzer contract (no base tx / calldata overhead)"
    );
    println!("onchain_p50 = median gasUsed of single-transfer txs in the sample window");
    println!("onchain_adj = onchain_p50 - {FIXED_OVERHEAD} (21000 base + 700 calldata)");
    println!("delta%    = (est_avg - onchain_adj) / onchain_adj * 100");

    Ok(())
}
