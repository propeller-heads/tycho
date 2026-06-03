use std::{
    collections::{HashMap, HashSet},
    default::Default,
    env,
    str::FromStr,
};

use alloy::{
    eips::BlockNumberOrTag,
    network::{Ethereum, EthereumWallet},
    primitives::{Address, Bytes as AlloyBytes, Keccak256, Signature, TxKind, B256, U256},
    providers::{
        fillers::{FillProvider, JoinFill, WalletFiller},
        Identity, Provider, ProviderBuilder, RootProvider,
    },
    rpc::types::{
        simulate::{SimBlock, SimulatePayload},
        TransactionInput, TransactionRequest,
    },
    signers::{local::PrivateKeySigner, SignerSync},
    sol_types::{eip712_domain, SolStruct, SolValue},
};
use alloy_chains::NamedChain;
use clap::Parser;
use dialoguer::{theme::ColorfulTheme, Select};
use futures::StreamExt;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tracing_subscriber::EnvFilter;
use tycho_common::{models::token::Token, simulation::protocol_sim::GetAmountOutResult, Bytes};
use tycho_execution::encoding::{
    errors::EncodingError,
    evm::{
        approvals::permit2::{Permit2, PermitSingle},
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
        utils::{biguint_to_u256, convert_to_router_token},
        ROUTER_ETH_ADDRESS,
    },
    models,
    models::{ClientFeeParams, EncodedSolution, Solution, Swap, UserTransferType},
};
use tycho_simulation::{
    evm::{
        engine_db::tycho_db::PreCachedDB,
        protocol::{
            aerodrome_slipstreams::state::AerodromeSlipstreamsState,
            ekubo::state::EkuboState,
            ekubo_v3::{self, state::EkuboV3State},
            filters::balancer_v2_pool_filter,
            pancakeswap_v2::state::PancakeswapV2State,
            uniswap_v2::state::UniswapV2State,
            uniswap_v3::state::UniswapV3State,
            uniswap_v4::state::UniswapV4State,
            vm::state::EVMPoolState,
        },
        stream::ProtocolStreamBuilder,
    },
    protocol::models::{ProtocolComponent, Update},
    tycho_client::feed::component_tracker::ComponentFilter,
    tycho_common::{dto::TvlThresholdTier, models::Chain},
    utils::{get_default_url, load_all_tokens, load_blocklist},
};

/// Represents a transaction to be executed.
#[derive(Clone, Debug)]
pub struct Transaction {
    pub to: Bytes,
    pub value: BigUint,
    pub data: Vec<u8>,
}

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    sell_token: Option<String>,
    #[arg(long)]
    buy_token: Option<String>,
    #[arg(long, default_value_t = 10.0)]
    sell_amount: f64,
    /// The tvl threshold to filter the graph by. Defaults to a chain-appropriate
    /// value targeting ~$200K USD equivalent.
    #[arg(long)]
    tvl_threshold: Option<f64>,
    #[arg(long, default_value = "ethereum")]
    chain: Chain,
    /// Path to blocklist TOML config file
    #[arg(long)]
    blocklist_file: Option<std::path::PathBuf>,
}

impl Cli {
    fn with_defaults(mut self) -> Self {
        // By default, we swap a small amount of USDC to WETH on whatever chain we choose

        if self.buy_token.is_none() {
            self.buy_token = Some(match self.chain.to_string().as_str() {
                "ethereum" => "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
                "base" => "0x4200000000000000000000000000000000000006".to_string(),
                "unichain" => "0x4200000000000000000000000000000000000006".to_string(),
                "arbitrum" => "0x82aF49447D8a07e3bd95BD0d56f35241523fBab1".to_string(),
                _ => panic!("Execution does not yet support chain {chain}", chain = self.chain),
            });
        }

        if self.sell_token.is_none() {
            self.sell_token = Some(match self.chain.to_string().as_str() {
                "ethereum" => "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_string(),
                "base" => "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913".to_string(),
                "unichain" => "0x078d782b760474a361dda0af3839290b0ef57ad6".to_string(),
                "arbitrum" => "0xaf88d065e77c8cC2239327C5EDb3A432268e5831".to_string(),
                _ => panic!("Execution does not yet support chain {chain}", chain = self.chain),
            });
        }

        self
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse().with_defaults();

    let chain = cli.chain;

    let tycho_url = env::var("TYCHO_URL").unwrap_or_else(|_| {
        get_default_url(&chain)
            .unwrap_or_else(|| panic!("Unknown URL for chain {chain}", chain = cli.chain))
    });

    let tycho_api_key: String =
        env::var("TYCHO_API_KEY").unwrap_or_else(|_| "sampletoken".to_string());

    let tvl_threshold = cli
        .tvl_threshold
        .unwrap_or_else(|| chain.default_tvl_threshold(TvlThresholdTier::Medium));
    let tvl_filter = ComponentFilter::with_tvl_range(tvl_threshold, tvl_threshold);

    let swapper_pk = env::var("PRIVATE_KEY").ok();

    println!("Loading tokens from Tycho... {url}", url = tycho_url.as_str());
    let all_tokens = load_all_tokens(
        tycho_url.as_str(),
        false,
        Some(tycho_api_key.as_str()),
        true,
        chain,
        None,
        None,
    )
    .await
    .expect("Failed to load tokens");
    println!("Tokens loaded: {num}", num = all_tokens.len());

    let sell_token_address = Bytes::from_str(
        &cli.sell_token
            .expect("Sell token not provided"),
    )
    .expect("Invalid address for sell token");
    let buy_token_address = Bytes::from_str(
        &cli.buy_token
            .expect("NBuy token not provided"),
    )
    .expect("Invalid address for buy token");
    let sell_token = all_tokens
        .get(&sell_token_address)
        .expect("Sell token not found")
        .clone();
    let buy_token = all_tokens
        .get(&buy_token_address)
        .expect("Buy token not found")
        .clone();
    let amount_in =
        BigUint::from((cli.sell_amount * 10f64.powi(sell_token.decimals as i32)) as u128);

    println!(
        "Looking for pool with best price for {amount} {sell_symbol} -> {buy_symbol}",
        amount = cli.sell_amount,
        sell_symbol = sell_token.symbol,
        buy_symbol = buy_token.symbol
    );
    let mut pairs: HashMap<String, ProtocolComponent> = HashMap::new();
    let mut amounts_out: HashMap<String, GetAmountOutResult> = HashMap::new();

    let mut protocol_stream = ProtocolStreamBuilder::new(&tycho_url, chain);

    match chain {
        Chain::Ethereum => {
            protocol_stream = protocol_stream
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV2State>("sushiswap_v2", tvl_filter.clone(), None)
                .exchange::<PancakeswapV2State>("pancakeswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("pancakeswap_v3", tvl_filter.clone(), None)
                .exchange::<EVMPoolState<PreCachedDB>>(
                    "vm:balancer_v2",
                    tvl_filter.clone(),
                    Some(balancer_v2_pool_filter),
                )
                .exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4_hooks", tvl_filter.clone(), None)
                // Only uncomment if you have ANGSTROM_API_KEY set
                // .exchange::<UniswapV4State>("uniswap_v4_hooks", tvl_filter.clone(),
                // Some(uniswap_v4_angstrom_hook_pool_filter))
                .exchange::<EkuboState>("ekubo_v2", tvl_filter.clone(), None)
                .exchange::<EkuboV3State>("ekubo_v3", tvl_filter.clone(), Some(ekubo_v3::filter_fn))
                .exchange::<EVMPoolState<PreCachedDB>>("vm:curve", tvl_filter.clone(), None)
                .exchange::<EVMPoolState<PreCachedDB>>("vm:maverick_v2", tvl_filter.clone(), None)
        }
        Chain::Base => {
            protocol_stream = protocol_stream
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("pancakeswap_v3", tvl_filter.clone(), None)
                .exchange::<AerodromeSlipstreamsState>(
                    "aerodrome_slipstreams",
                    tvl_filter.clone(),
                    None,
                )
        }
        Chain::Bsc => {
            protocol_stream = protocol_stream
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None)
                .exchange::<PancakeswapV2State>("pancakeswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("pancakeswap_v3", tvl_filter.clone(), None)
        }
        Chain::Unichain => {
            protocol_stream = protocol_stream
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None)
        }
        Chain::Arbitrum => {
            protocol_stream = protocol_stream
                .exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV3State>("pancakeswap_v3", tvl_filter.clone(), None)
                .exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None)
        }
        _ => {}
    }

    let blocklist =
        load_blocklist(cli.blocklist_file.as_deref()).expect("Failed to load blocklist");
    protocol_stream = protocol_stream.blocklist_components(blocklist);

    let protocol_stream = protocol_stream
        .auth_key(Some(tycho_api_key.clone()))
        .skip_state_decode_failures(true)
        .set_tokens(all_tokens.clone())
        .await
        .build()
        .await
        .expect("Failed building protocol stream");
    tokio::pin!(protocol_stream);

    // Initialize the encoder
    let swap_encoder_registry = SwapEncoderRegistry::new(chain)
        .add_default_encoders(None)
        .expect("Failed to get default SwapEncoderRegistry");
    let encoder = TychoRouterEncoderBuilder::new()
        .chain(chain)
        .swap_encoder_registry(swap_encoder_registry)
        .build()
        .expect("Failed to build encoder");

    while let Some(message_result) = protocol_stream.next().await {
        let message = match message_result {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("Error receiving message: {e:?}. Continuing to next message...");
                continue;
            }
        };

        let best_swap = get_best_swap(
            message,
            &mut pairs,
            amount_in.clone(),
            sell_token.clone(),
            buy_token.clone(),
            &mut amounts_out,
        );

        if let Some((best_pool, amount_out_result)) = best_swap {
            let component = pairs
                .get(&best_pool)
                .expect("Best pool not found")
                .clone();

            let expected_amount = amount_out_result.amount.clone();
            let gas_usage = amount_out_result.gas.clone();
            // Clone expected_amount to avoid ownership issues
            let expected_amount_copy = expected_amount.clone();

            // Check if we have a private key first
            if swapper_pk.is_none() {
                println!(
                    "\nSigner private key was not provided. Skipping simulation/execution. Set PRIVATE_KEY env variable to perform simulation/execution.\n"
                );
                continue;
            }

            // Create signer and provider now that we know we have a private key
            let pk_str = swapper_pk.as_ref().unwrap();
            let pk = B256::from_str(pk_str).expect("Failed to convert swapper pk to B256");
            let signer =
                PrivateKeySigner::from_bytes(&pk).expect("Failed to create PrivateKeySigner");
            let tx_signer = EthereumWallet::from(signer.clone());
            let provider = ProviderBuilder::default()
                .with_chain(NamedChain::try_from(chain.id()).expect("Invalid chain"))
                .wallet(tx_signer)
                .connect(&env::var("RPC_URL").expect("RPC_URL env var not set"))
                .await
                .expect("Failed to connect provider");

            let solution = create_solution(
                component,
                sell_token.clone(),
                buy_token.clone(),
                amount_in.clone(),
                Bytes::from(signer.address().to_vec()),
                expected_amount,
                gas_usage,
            );

            // Encode the swaps of the solution
            let encoded_solution = encoder
                .encode_solutions(vec![solution.clone()])
                .expect("Failed to encode router calldata")[0]
                .clone();

            let tx = encode_tycho_router_call(
                chain.id(),
                encoded_solution.clone(),
                &solution,
                chain.native_token().address,
                signer.clone(),
            )
            .expect("Failed to encode router call");

            // Print token balances before showing the swap options
            match get_token_balance(
                &provider,
                Address::from_slice(&sell_token.address),
                signer.address(),
                Address::from_slice(&chain.native_token().address),
            )
            .await
            {
                Ok(balance) => {
                    let formatted_balance = format_token_amount(&balance, &sell_token);
                    println!(
                        "\nYour balance: {formatted_balance} {sell_symbol}",
                        sell_symbol = sell_token.symbol
                    );

                    if balance < amount_in {
                        let required = format_token_amount(&amount_in, &sell_token);
                        println!("⚠️ Warning: Insufficient balance for swap. You have {formatted_balance} {sell_symbol} but need {required} {sell_symbol}",
                                 formatted_balance = formatted_balance,
                                 sell_symbol = sell_token.symbol,
                        );
                        return;
                    }
                }
                Err(e) => eprintln!("Failed to get token balance: {e}"),
            }

            // Also show buy token balance
            match get_token_balance(
                &provider,
                Address::from_slice(&buy_token.address),
                signer.address(),
                Address::from_slice(&chain.native_token().address),
            )
            .await
            {
                Ok(balance) => {
                    let formatted_balance = format_token_amount(&balance, &buy_token);
                    println!(
                        "Your {buy_symbol} balance: {formatted_balance} {buy_symbol}",
                        buy_symbol = buy_token.symbol
                    );
                }
                Err(e) => eprintln!(
                    "Failed to get {buy_symbol} balance: {e}",
                    buy_symbol = buy_token.symbol
                ),
            }
            println!("Would you like to simulate or execute this swap?");
            println!("Please be aware that the market might move while you make your decision, which might lead to a revert if you've set a min amount out or slippage.");
            println!("Warning: slippage is set to 0.25% during execution by default.\n");
            let options = vec!["Simulate the swap", "Execute the swap", "Skip this swap"];
            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("What would you like to do?")
                .default(0)
                .items(&options)
                .interact()
                .unwrap_or(2); // Default to skip if error

            let choice = match selection {
                0 => "simulate",
                1 => "execute",
                _ => "skip",
            };

            match choice {
                "simulate" => {
                    println!("\nSimulating by performing an approval (for permit2) and a swap transaction...");

                    let (approval_request, swap_request) = get_tx_requests(
                        provider.clone(),
                        biguint_to_u256(&amount_in),
                        signer.address(),
                        Address::from_slice(&sell_token_address),
                        tx.clone(),
                        chain.id(),
                    )
                    .await;

                    let payload = SimulatePayload {
                        block_state_calls: vec![SimBlock {
                            block_overrides: None,
                            state_overrides: None,
                            calls: vec![approval_request, swap_request],
                        }],
                        trace_transfers: true,
                        validation: true,
                        return_full_transactions: true,
                    };

                    match provider.simulate(&payload).await {
                        Ok(output) => {
                            for block in output.iter() {
                                println!(
                                    "\nSimulated Block {block_num}:",
                                    block_num = block.inner.header.number
                                );
                                for (j, transaction) in block.calls.iter().enumerate() {
                                    println!(
                                        "  Transaction {transaction_num}: Status: {status:?}, Gas Used: {gas_used}",
                                        transaction_num = j + 1,
                                        status = transaction.status,
                                        gas_used = transaction.gas_used
                                    );
                                }
                            }
                            println!(); // Add empty line after simulation results
                            continue;
                        }
                        Err(e) => {
                            eprintln!("\nSimulation failed: {e:?}");
                            println!("Your RPC provider does not support transaction simulation.");
                            println!("Do you want to proceed with execution instead?\n");
                            let yes_no_options = vec!["Yes", "No"];
                            let yes_no_selection = Select::with_theme(&ColorfulTheme::default())
                                .with_prompt("Do you want to proceed with execution instead?")
                                .default(1) // Default to No
                                .items(&yes_no_options)
                                .interact()
                                .unwrap_or(1); // Default to No if error

                            if yes_no_selection == 0 {
                                match execute_swap_transaction(
                                    provider.clone(),
                                    &amount_in,
                                    signer.address(),
                                    &sell_token_address,
                                    tx.clone(),
                                    chain.id(),
                                )
                                .await
                                {
                                    Ok(_) => {
                                        println!("\n✅ Swap executed successfully! Exiting the session...\n");

                                        // Calculate the correct price ratio
                                        let (forward_price, _reverse_price) = format_price_ratios(
                                            &amount_in,
                                            &expected_amount_copy,
                                            &sell_token,
                                            &buy_token,
                                        );

                                        println!(
                                            "Summary: Swapped {formatted_in} {sell_symbol} → {formatted_out} {buy_symbol} at
                                            a price of {forward_price:.6} {buy_symbol} per {sell_symbol}",
                                            formatted_in = format_token_amount(&amount_in, &sell_token),
                                            sell_symbol = sell_token.symbol,
                                            formatted_out = format_token_amount(&expected_amount_copy, &buy_token),
                                            buy_symbol = buy_token.symbol,
                                        );
                                        return; // Exit the program after successful execution
                                    }
                                    Err(e) => {
                                        eprintln!("\nFailed to execute transaction: {e:?}\n");
                                        continue;
                                    }
                                }
                            } else {
                                println!("\nSkipping this swap...\n");
                                continue;
                            }
                        }
                    }
                }
                "execute" => {
                    match execute_swap_transaction(
                        provider.clone(),
                        &amount_in,
                        signer.address(),
                        &sell_token_address,
                        tx,
                        chain.id(),
                    )
                    .await
                    {
                        Ok(_) => {
                            println!("\n✅ Swap executed successfully! Exiting the session...\n");

                            // Calculate the correct price ratio
                            let (forward_price, _reverse_price) = format_price_ratios(
                                &amount_in,
                                &expected_amount_copy,
                                &sell_token,
                                &buy_token,
                            );

                            println!(
                                "Summary: Swapped {formatted_in} {sell_symbol} → {formatted_out} {buy_symbol} at
                                a price of {forward_price:.6} {buy_symbol} per {sell_symbol}",
                                formatted_in = format_token_amount(&amount_in, &sell_token),
                                sell_symbol = sell_token.symbol,
                                formatted_out = format_token_amount(&expected_amount_copy, &buy_token),
                                buy_symbol = buy_token.symbol,
                            );
                            return; // Exit the program after successful execution
                        }
                        Err(e) => {
                            eprintln!("\nFailed to execute transaction: {e:?}\n");
                            continue;
                        }
                    }
                }
                "skip" => {
                    println!("\nSkipping this swap...\n");
                    continue;
                }
                _ => {
                    println!("\nInvalid input. Please choose 'simulate', 'execute' or 'skip'.\n");
                    continue;
                }
            }
        }
    }
}

fn get_best_swap(
    message: Update,
    pairs: &mut HashMap<String, ProtocolComponent>,
    amount_in: BigUint,
    sell_token: Token,
    buy_token: Token,
    amounts_out: &mut HashMap<String, GetAmountOutResult>,
) -> Option<(String, GetAmountOutResult)> {
    println!(
        "\n==================== Received block {block:?} ====================",
        block = message.block_number_or_timestamp
    );
    for (id, comp) in message.new_pairs.iter() {
        pairs
            .entry(id.clone())
            .or_insert_with(|| comp.clone());
    }
    if message.states.is_empty() {
        println!("No pools of interest were updated this block. The best swap is the previous one");
        return None;
    }
    for (id, state) in message.states.iter() {
        if let Some(component) = pairs.get(id) {
            let tokens = &component.tokens;
            if HashSet::from([&sell_token, &buy_token])
                .is_subset(&HashSet::from_iter(tokens.iter()))
            {
                let amount_out_result = state
                    .get_amount_out(amount_in.clone(), &sell_token, &buy_token)
                    .map_err(|e| eprintln!("Error calculating amount out for Pool {id:?}: {e:?}"))
                    .ok();

                if let Some(amount_out) = amount_out_result {
                    amounts_out.insert(id.clone(), amount_out);
                }

                // If you would like to know how much of each token you are able to swap on the
                // pool, do
                // let limits = state
                //     .get_limits(sell_token.address.clone(), buy_token.address.clone())
                //     .unwrap();

                // If you would like to save spot prices instead of the amount out, do
                // let spot_price = state
                //     .spot_price(&tokens[0], &tokens[1])
                //     .ok();
            }
        }
    }
    if let Some((key, result)) = amounts_out
        .iter()
        .max_by_key(|(_, result)| result.amount.clone())
    {
        println!(
            "\nThe best swap (out of {amounts} possible pools) is:",
            amounts = amounts_out.len()
        );
        println!(
            "Protocol: {protocol}",
            protocol = pairs
                .get(key)
                .expect("Failed to get best pool")
                .protocol_system
        );
        println!("Pool address: {key:?}");
        let formatted_in = format_token_amount(&amount_in, &sell_token);
        let formatted_out = format_token_amount(&result.amount, &buy_token);
        let (forward_price, reverse_price) =
            format_price_ratios(&amount_in, &result.amount, &sell_token, &buy_token);

        println!(
            "Swap: {formatted_in} {sell_symbol} -> {formatted_out} {buy_symbol} \n
            Price: {forward_price:.6} {buy_symbol} per {sell_symbol},
            {reverse_price:.6} {sell_symbol} per {buy_symbol}",
            sell_symbol = sell_token.symbol,
            buy_symbol = buy_token.symbol,
        );
        Some((key.to_string(), result.clone()))
    } else {
        println!("\nThere aren't pools with the tokens we are looking for");
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn create_solution(
    component: ProtocolComponent,
    sell_token: Token,
    buy_token: Token,
    sell_amount: BigUint,
    user_address: Bytes,
    expected_amount: BigUint,
    gas_usage: BigUint,
) -> Solution {
    // Prepare data to encode. First we need to create a swap object
    let simple_swap = Swap::new(component, sell_token.clone(), buy_token.clone(), gas_usage);

    // Compute a minimum amount out
    //
    // # ⚠️ Important Responsibility Note
    // For maximum security, in production code, this minimum amount out should be computed
    // from a third-party source.
    let slippage = 0.0025; // 0.25% slippage
    let bps = BigUint::from(10_000u32);
    let slippage_percent = BigUint::from((slippage * 10000.0) as u32);
    let multiplier = &bps - slippage_percent;
    let min_amount_out = (expected_amount * &multiplier) / &bps;

    // For native ETH we use TransferFrom (payable singleSwap);
    // for ERC20s we use Permit2.
    let is_native = sell_token.address == *ROUTER_ETH_ADDRESS || sell_token.address.is_zero();
    let transfer_type = if is_native {
        UserTransferType::TransferFrom
    } else {
        UserTransferType::TransferFromPermit2
    };

    // Then we create a solution object with the previous swap
    Solution::new(
        user_address.clone(),
        user_address,
        sell_token.address,
        buy_token.address,
        sell_amount,
        min_amount_out,
        vec![simple_swap],
    )
    .with_user_transfer_type(transfer_type)
}

/// Encodes a transaction for the Tycho Router.
///
/// Uses `singleSwapPermit2` for ERC-20 inputs and `singleSwap` (payable)
/// for native ETH. Translates `Address::ZERO` to the router's
/// `ETH_ADDRESS` (`0xEeee…`).
///
/// # ⚠️ Important Responsibility Note
///
/// This function is intended as **an illustrative example only**.
/// **Users must implement their own encoding logic** to ensure:
/// - Full control of parameters passed to the router.
/// - Proper validation and setting of critical inputs such as `minAmountOut`.
fn encode_tycho_router_call(
    chain_id: u64,
    encoded_solution: EncodedSolution,
    solution: &Solution,
    native_address: Bytes,
    signer: PrivateKeySigner,
) -> Result<Transaction, EncodingError> {
    let given_amount = biguint_to_u256(solution.amount_in());
    let min_amount_out = biguint_to_u256(solution.min_amount_out());
    let given_token = convert_to_router_token(Address::from_slice(solution.token_in()));
    let checked_token = convert_to_router_token(Address::from_slice(solution.token_out()));
    let receiver = Address::from_slice(solution.receiver());

    let is_native_in =
        solution.token_in() == &native_address || *solution.token_in() == *ROUTER_ETH_ADDRESS;

    let client_fee_params = ClientFeeParams::default().into_abi_params();

    let method_calldata = if is_native_in {
        // singleSwap (payable, no permit2)
        (
            given_amount,
            given_token,
            checked_token,
            min_amount_out,
            receiver,
            client_fee_params,
            encoded_solution.swaps().to_vec(),
        )
            .abi_encode()
    } else {
        // singleSwapPermit2
        let permit2 = Permit2::new()?;
        let p = permit2.get_permit(
            encoded_solution.interacting_with(),
            solution.sender(),
            solution.token_in(),
            solution.amount_in(),
        )?;
        let permit = PermitSingle::try_from(&p)
            .map_err(|_| EncodingError::InvalidInput("Invalid permit".to_string()))?;
        let signature = sign_permit(chain_id, &p, signer)?;

        (
            given_amount,
            given_token,
            checked_token,
            min_amount_out,
            receiver,
            client_fee_params,
            permit,
            signature.as_bytes().to_vec(),
            encoded_solution.swaps().to_vec(),
        )
            .abi_encode()
    };

    let contract_interaction = encode_input(encoded_solution.function_signature(), method_calldata);
    let value = if is_native_in { solution.amount_in().clone() } else { BigUint::ZERO };
    Ok(Transaction {
        to: encoded_solution
            .interacting_with()
            .clone(),
        value,
        data: contract_interaction,
    })
}

/// Signs a Permit2 `PermitSingle` struct using the EIP-712 signing scheme.
///
/// This function constructs an EIP-712 domain specific to the Permit2 contract and computes the
/// hash of the provided `PermitSingle`. It then uses the given `PrivateKeySigner` to produce
/// a cryptographic signature of the permit.
///
/// # Warning
/// This is only an **example implementation** provided for reference purposes.
/// **Do not rely on this in production.** You should implement your own version.
fn sign_permit(
    chain_id: u64,
    permit_single: &models::PermitSingle,
    signer: PrivateKeySigner,
) -> Result<Signature, EncodingError> {
    let permit2_address = Address::from_str("0x000000000022D473030F116dDEE9F6B43aC78BA3")
        .map_err(|_| EncodingError::FatalError("Permit2 address not valid".to_string()))?;
    let domain = eip712_domain! {
        name: "Permit2",
        chain_id: chain_id,
        verifying_contract: permit2_address,
    };
    let permit_single: PermitSingle = PermitSingle::try_from(permit_single)?;
    let hash = permit_single.eip712_signing_hash(&domain);
    signer
        .sign_hash_sync(&hash)
        .map_err(|e| {
            EncodingError::FatalError(format!("Failed to sign permit2 approval with error: {e}"))
        })
}

/// Encodes the input data for a function call to the given function selector.
pub fn encode_input(selector: &str, mut encoded_args: Vec<u8>) -> Vec<u8> {
    let mut hasher = Keccak256::new();
    hasher.update(selector.as_bytes());
    let selector_bytes = &hasher.finalize()[..4];
    let mut call_data = selector_bytes.to_vec();
    // Remove extra prefix if present (32 bytes for dynamic data)
    // Alloy encoding is including a prefix for dynamic data indicating the offset or length
    // but at this point we don't want that
    if encoded_args.len() > 32 &&
        encoded_args[..32] ==
            [0u8; 31]
                .into_iter()
                .chain([32].to_vec())
                .collect::<Vec<u8>>()
    {
        encoded_args = encoded_args[32..].to_vec();
    }
    call_data.extend(encoded_args);
    call_data
}

async fn get_tx_requests(
    provider: FillProvider<
        JoinFill<Identity, WalletFiller<EthereumWallet>>,
        RootProvider<Ethereum>,
    >,
    amount_in: U256,
    user_address: Address,
    sell_token_address: Address,
    tx: Transaction,
    chain_id: u64,
) -> (TransactionRequest, TransactionRequest) {
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await
        .expect("Failed to fetch latest block")
        .expect("Block not found");

    let base_fee = block
        .header
        .base_fee_per_gas
        .expect("Base fee not available");
    let max_priority_fee_per_gas = 1_000_000_000u64;
    let max_fee_per_gas = base_fee + max_priority_fee_per_gas;

    let approve_function_signature = "approve(address,uint256)";
    let args = (
        Address::from_str("0x000000000022D473030F116dDEE9F6B43aC78BA3")
            .expect("Couldn't convert to address"),
        amount_in,
    );
    let data = encode_input(approve_function_signature, args.abi_encode());
    let nonce = provider
        .get_transaction_count(user_address)
        .await
        .expect("Failed to get nonce");

    let approval_request = TransactionRequest {
        to: Some(TxKind::Call(sell_token_address)),
        from: Some(user_address),
        value: None,
        input: TransactionInput { input: Some(AlloyBytes::from(data)), data: None },
        gas: Some(100_000u64),
        chain_id: Some(chain_id),
        max_fee_per_gas: Some(max_fee_per_gas.into()),
        max_priority_fee_per_gas: Some(max_priority_fee_per_gas.into()),
        nonce: Some(nonce),
        ..Default::default()
    };

    let swap_request = TransactionRequest {
        to: Some(TxKind::Call(Address::from_slice(&tx.to))),
        from: Some(user_address),
        value: Some(biguint_to_u256(&tx.value)),
        input: TransactionInput { input: Some(AlloyBytes::from(tx.data)), data: None },
        gas: Some(800_000u64),
        chain_id: Some(chain_id),
        max_fee_per_gas: Some(max_fee_per_gas.into()),
        max_priority_fee_per_gas: Some(max_priority_fee_per_gas.into()),
        nonce: Some(nonce + 1),
        ..Default::default()
    };
    (approval_request, swap_request)
}

// Format token amounts to human-readable values
fn format_token_amount(amount: &BigUint, token: &Token) -> String {
    let decimal_amount = amount.to_f64().unwrap_or(0.0) / 10f64.powi(token.decimals as i32);
    format!("{decimal_amount:.6}")
}

// Calculate price ratios in both directions
fn format_price_ratios(
    amount_in: &BigUint,
    amount_out: &BigUint,
    token_in: &Token,
    token_out: &Token,
) -> (f64, f64) {
    let decimal_in = amount_in.to_f64().unwrap_or(0.0) / 10f64.powi(token_in.decimals as i32);
    let decimal_out = amount_out.to_f64().unwrap_or(0.0) / 10f64.powi(token_out.decimals as i32);

    if decimal_in > 0.0 && decimal_out > 0.0 {
        let forward = decimal_out / decimal_in;
        let reverse = decimal_in / decimal_out;
        (forward, reverse)
    } else {
        (0.0, 0.0)
    }
}

async fn get_token_balance(
    provider: &FillProvider<
        JoinFill<Identity, WalletFiller<EthereumWallet>>,
        RootProvider<Ethereum>,
    >,
    token_address: Address,
    wallet_address: Address,
    native_token_address: Address,
) -> Result<BigUint, Box<dyn std::error::Error>> {
    let balance = if token_address == native_token_address {
        provider
            .get_balance(wallet_address)
            .await?
    } else {
        let balance_of_signature = "balanceOf(address)";
        let data = encode_input(balance_of_signature, (wallet_address,).abi_encode());

        let result = provider
            .call(TransactionRequest {
                to: Some(TxKind::Call(token_address)),
                input: TransactionInput { input: Some(AlloyBytes::from(data)), data: None },
                ..Default::default()
            })
            .await?;

        U256::from_be_bytes(
            result
                .to_vec()
                .try_into()
                .unwrap_or([0u8; 32]),
        )
    };
    // Convert the U256 to BigUint
    Ok(BigUint::from_bytes_be(&balance.to_be_bytes::<32>()))
}

async fn execute_swap_transaction(
    provider: FillProvider<
        JoinFill<Identity, WalletFiller<EthereumWallet>>,
        RootProvider<Ethereum>,
    >,
    amount_in: &BigUint,
    wallet_address: Address,
    sell_token_address: &Bytes,
    tx: Transaction,
    chain_id: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("\nExecuting by performing an approval (for permit2) and a swap transaction...");
    let (approval_request, swap_request) = get_tx_requests(
        provider.clone(),
        biguint_to_u256(amount_in),
        wallet_address,
        Address::from_slice(sell_token_address),
        tx.clone(),
        chain_id,
    )
    .await;

    let approval_receipt = provider
        .send_transaction(approval_request)
        .await?;

    let approval_result = approval_receipt.get_receipt().await?;
    println!(
        "\nApproval transaction sent with hash: {hash:?} and status: {status:?}",
        hash = approval_result.transaction_hash,
        status = approval_result.status()
    );

    let swap_receipt = provider
        .send_transaction(swap_request)
        .await?;

    let swap_result = swap_receipt.get_receipt().await?;
    println!(
        "\nSwap transaction sent with hash: {hash:?} and status: {status:?}\n",
        hash = swap_result.transaction_hash,
        status = swap_result.status()
    );

    if !swap_result.status() {
        return Err(format!(
            "Swap transaction with hash {hash:?} failed.",
            hash = swap_result.transaction_hash
        )
        .into());
    }

    Ok(())
}
