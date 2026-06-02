use std::{collections::HashSet, env, str::FromStr, sync::Arc, time::Duration};

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
use dotenv::dotenv;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;
use tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim, Bytes};
use tycho_execution::encoding::{
    errors::EncodingError,
    evm::{
        approvals::permit2::{Permit2, PermitSingle},
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry,
        utils::biguint_to_u256,
    },
    models,
    models::{EncodedSolution, Solution, Swap, UserTransferType},
};
use tycho_simulation::{
    protocol::models::{ProtocolComponent, Update},
    rfq::{
        protocols::{
            bebop::{client_builder::BebopClientBuilder, state::BebopState},
            hashflow::{client_builder::HashflowClientBuilder, state::HashflowState},
            liquorice::{client_builder::LiquoriceClientBuilder, state::LiquoriceState},
            metric::{client_builder::MetricClientBuilder, state::MetricState},
        },
        stream::RFQStreamBuilder,
    },
    tycho_common::models::Chain,
    utils::{get_default_url, load_all_tokens},
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
    /// The minimum TVL threshold for RFQ quotes in USD
    #[arg(long, default_value_t = 1000.0)]
    tvl_threshold: f64,
    #[arg(long, default_value = "ethereum")]
    chain: Chain,
    /// Run PAMM RFQ protocols.
    #[arg(long, default_value_t = false)]
    run_pamm_protocols: bool,
}

impl Cli {
    fn with_defaults(mut self) -> Self {
        // By default, we request quotes for USDC to WETH on whatever chain we choose

        if self.buy_token.is_none() {
            self.buy_token = Some(match self.chain {
                Chain::Ethereum => "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2".to_string(),
                Chain::Base => "0x4200000000000000000000000000000000000006".to_string(),
                _ => {
                    panic!("RFQ quickstart does not yet support chain {chain}", chain = self.chain)
                }
            });
        }

        if self.sell_token.is_none() {
            self.sell_token = Some(match self.chain {
                Chain::Ethereum => "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_string(),
                Chain::Base => "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913".to_string(),
                _ => {
                    panic!("RFQ quickstart does not yet support chain {chain}", chain = self.chain)
                }
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

    dotenv().expect("Missing .env file");
    let tycho_url = env::var("TYCHO_URL").unwrap_or_else(|_| {
        get_default_url(&chain)
            .unwrap_or_else(|| panic!("Unknown URL for chain {chain}", chain = cli.chain))
    });

    let tycho_api_key: String =
        env::var("TYCHO_API_KEY").unwrap_or_else(|_| "sampletoken".to_string());

    // Get credentials for any RFQ(s) we are using
    let (bebop_user, bebop_key) = (env::var("BEBOP_USER").ok(), env::var("BEBOP_KEY").ok());
    let (hashflow_user, hashflow_key) =
        (env::var("HASHFLOW_USER").ok(), env::var("HASHFLOW_KEY").ok());
    let (liquorice_user, liquorice_key) =
        (env::var("LIQUORICE_USER").ok(), env::var("LIQUORICE_KEY").ok());
    if (bebop_user.is_none() || bebop_key.is_none()) &&
        (hashflow_user.is_none() || hashflow_key.is_none()) &&
        (liquorice_user.is_none() || liquorice_key.is_none())
    {
        if cli.run_pamm_protocols {
            println!("No authenticated RFQ credentials found. Continuing with PAMM RFQ protocols only.\n");
        } else {
            panic!("No RFQ credentials found. Please set BEBOP_USER and BEBOP_KEY, HASHFLOW_USER and HASHFLOW_KEY, or LIQUORICE_USER and LIQUORICE_KEY environment variables. To run PAMM RFQ protocols, pass --run-pamm-protocols.");
        }
    }

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
            .expect("Buy token not provided"),
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
        "Looking for RFQ quotes for {amount} {sell_symbol} -> {buy_symbol} on {chain:?}",
        amount = cli.sell_amount,
        sell_symbol = sell_token.symbol,
        buy_symbol = buy_token.symbol
    );

    let swapper_pk = env::var("PRIVATE_KEY").ok();
    // Initialize the encoder
    let swap_encoder_registry = SwapEncoderRegistry::new(chain)
        .add_default_encoders(None)
        .expect("Failed to get default SwapEncoderRegistry");
    let encoder = TychoRouterEncoderBuilder::new()
        .chain(chain)
        .swap_encoder_registry(swap_encoder_registry)
        .build()
        .expect("Failed to build encoder");

    // Set up RFQ client using the builder pattern
    let mut rfq_tokens = HashSet::new();
    rfq_tokens.insert(sell_token_address.clone());
    rfq_tokens.insert(buy_token_address.clone());

    let mut rfq_stream_builder = RFQStreamBuilder::new()
        .set_tokens(all_tokens.clone())
        .await;
    if let (Some(user), Some(key)) = (bebop_user, bebop_key) {
        println!("Setting up Bebop RFQ client...\n");
        let bebop_client = BebopClientBuilder::new(chain, user, key)
            .tokens(rfq_tokens.clone())
            .tvl_threshold(cli.tvl_threshold)
            .build()
            .expect("Failed to create Bebop RFQ client");
        rfq_stream_builder =
            rfq_stream_builder.add_client::<BebopState>("bebop", Box::new(bebop_client))
    }
    if let (Some(user), Some(key)) = (hashflow_user, hashflow_key) {
        println!("Setting up Hashflow RFQ client...\n");
        let hashflow_client = HashflowClientBuilder::new(chain, user, key)
            .tokens(rfq_tokens.clone())
            .tvl_threshold(cli.tvl_threshold)
            .poll_time(Duration::from_secs(5))
            .build()
            .expect("Failed to create Hashflow RFQ client");
        rfq_stream_builder =
            rfq_stream_builder.add_client::<HashflowState>("hashflow", Box::new(hashflow_client))
    }
    if let (Some(user), Some(key)) = (liquorice_user, liquorice_key) {
        println!("Setting up Liquorice RFQ client...\n");
        let liquorice_client = LiquoriceClientBuilder::new(chain, user, key)
            .tokens(rfq_tokens.clone())
            .tvl_threshold(cli.tvl_threshold)
            .build()
            .expect("Failed to create Liquorice RFQ client");
        rfq_stream_builder =
            rfq_stream_builder.add_client::<LiquoriceState>("liquorice", Box::new(liquorice_client))
    }
    if cli.run_pamm_protocols {
        println!("Setting up Metric RFQ client...\n");
        match MetricClientBuilder::new(chain)
            .tokens(rfq_tokens.clone())
            .token_metadata(all_tokens.clone())
            .tvl_threshold(cli.tvl_threshold)
            .build()
        {
            Ok(metric_client) => {
                rfq_stream_builder =
                    rfq_stream_builder.add_client::<MetricState>("metric", Box::new(metric_client));
            }
            Err(e) => eprintln!("Skipping Metric RFQ client: {e}"),
        }
    }

    // Start the RFQ stream in a background task
    let (tx, mut rx) = mpsc::channel::<Update>(100);
    tokio::spawn(rfq_stream_builder.build(tx));
    println!("Connected to RFQs! Streaming live price levels...\n");

    // Stream quotes from RFQ stream
    while let Some(update) = rx.recv().await {
        // Drain any additional buffered messages to get the most recent one
        //
        // ⚠️Warning: This works fine only if you assume that this message is entirely
        // representative of the current state, as done in this quickstart.
        // You should comment out this code portion if you would like to manually track removed
        // components.
        let mut latest_update = update;
        let mut drained_count = 0;
        while let Ok(newer_update) =
            tokio::time::timeout(std::time::Duration::from_millis(10), rx.recv()).await
        {
            if let Some(newer_update) = newer_update {
                latest_update = newer_update;
                drained_count += 1;
            } else {
                break;
            }
        }
        if drained_count > 0 {
            println!(
                "Fast-forwarded through {drained_count} older RFQ updates to get latest prices"
            );
        }
        let update = latest_update;

        println!(
            "Received RFQ price levels with {} new pairs for block/timestamp {}",
            update.states.len(),
            update.block_number_or_timestamp
        );

        // Process state updates
        for (comp_id, state) in &update.states {
            if let Some(component) = update.new_pairs.get(comp_id) {
                let tokens = &component.tokens;

                // Check if this component trades our desired pair
                if HashSet::from([&sell_token, &buy_token])
                    .is_subset(&HashSet::from_iter(tokens.iter()))
                {
                    // Try to calculate amount out using the state
                    if let Ok(amount_out_result) =
                        state.get_amount_out(amount_in.clone(), &sell_token, &buy_token)
                    {
                        let amount_out = amount_out_result.amount;

                        println!(
                            "Best indicative price for swap {}: {} {} -> {} {}",
                            component.protocol_system,
                            format_token_amount(&amount_in, &sell_token),
                            sell_token.symbol,
                            format_token_amount(&amount_out, &buy_token),
                            buy_token.symbol
                        );

                        // Clone expected_amount to avoid ownership issues
                        let expected_amount_copy = amount_out.clone();

                        // Check if we have a private key first
                        if swapper_pk.is_none() {
                            println!(
                                "\nSigner private key was not provided. Skipping simulation/execution. Set PRIVATE_KEY env variable to perform simulation/execution.\n"
                            );
                            continue;
                        }

                        // Create signer and provider now that we know we have a private key
                        let pk_str = swapper_pk.as_ref().unwrap();
                        let pk =
                            B256::from_str(pk_str).expect("Failed to convert swapper pk to B256");
                        let signer = PrivateKeySigner::from_bytes(&pk)
                            .expect("Failed to create PrivateKeySigner");
                        let tx_signer = EthereumWallet::from(signer.clone());
                        let provider = ProviderBuilder::default()
                            .with_chain(NamedChain::try_from(chain.id()).expect("Invalid chain"))
                            .wallet(tx_signer)
                            .connect(&env::var("RPC_URL").expect("RPC_URL env var not set"))
                            .await
                            .expect("Failed to connect provider");

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
                                    continue;
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
                        println!(
                            "Warning: slippage is set to 0.25% during execution by default.\n"
                        );
                        let options =
                            vec!["Simulate the swap", "Execute the swap", "Skip this swap"];
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
                                println!("\nSimulating RFQ swap...");
                                println!("Step 1: Encoding the permit2 transaction...");
                                let approve_function_signature = "approve(address,uint256)";
                                let args = (
                                    Address::from_str("0x000000000022D473030F116dDEE9F6B43aC78BA3")
                                        .expect("Couldn't convert to address"),
                                    biguint_to_u256(&amount_in),
                                );
                                let approval_data =
                                    encode_input(approve_function_signature, args.abi_encode());
                                let nonce = provider
                                    .get_transaction_count(signer.address())
                                    .await
                                    .expect("Failed to get nonce");
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

                                let approval_request = TransactionRequest {
                                    to: Some(TxKind::Call(Address::from_slice(
                                        &sell_token_address,
                                    ))),
                                    from: Some(signer.address()),
                                    value: None,
                                    input: TransactionInput {
                                        input: Some(AlloyBytes::from(approval_data)),
                                        data: None,
                                    },
                                    gas: Some(100_000u64),
                                    chain_id: Some(chain.id()),
                                    max_fee_per_gas: Some(max_fee_per_gas.into()),
                                    max_priority_fee_per_gas: Some(max_priority_fee_per_gas.into()),
                                    nonce: Some(nonce),
                                    ..Default::default()
                                };

                                println!("Step 2: Encoding the solution transaction...");

                                let solution = create_solution(
                                    component.clone(),
                                    Arc::from(state.clone_box()),
                                    sell_token.clone(),
                                    buy_token.clone(),
                                    amount_in.clone(),
                                    Bytes::from(signer.address().to_vec()),
                                    amount_out.clone(),
                                    amount_out_result.gas,
                                );

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

                                let swap_request = TransactionRequest {
                                    to: Some(TxKind::Call(Address::from_slice(&tx.to))),
                                    from: Some(signer.address()),
                                    value: Some(biguint_to_u256(&tx.value)),
                                    input: TransactionInput {
                                        input: Some(AlloyBytes::from(tx.data)),
                                        data: None,
                                    },
                                    gas: Some(1_000_000u64),
                                    chain_id: Some(chain.id()),
                                    max_fee_per_gas: Some(max_fee_per_gas.into()),
                                    max_priority_fee_per_gas: Some(max_priority_fee_per_gas.into()),
                                    nonce: Some(nonce + 1),
                                    ..Default::default()
                                };

                                println!("Step 3: Simulating approval and solution transactions together...");
                                let approval_payload = SimulatePayload {
                                    block_state_calls: vec![SimBlock {
                                        block_overrides: None,
                                        state_overrides: None,
                                        calls: vec![approval_request.clone(), swap_request],
                                    }],
                                    trace_transfers: true,
                                    validation: true,
                                    return_full_transactions: true,
                                };

                                match provider
                                    .simulate(&approval_payload)
                                    .await
                                {
                                    Ok(output) => {
                                        let mut all_successful = true;
                                        for block in output.iter() {
                                            println!(
                                                "\nSimulated Block {block_num}:",
                                                block_num = block.inner.header.number
                                            );
                                            for transaction in block.calls.iter() {
                                                println!(
                                                    "  RFQ Swap: Status: {status:?}, Gas Used: {gas_used}",
                                                    status = transaction.status,
                                                    gas_used = transaction.gas_used
                                                );
                                                if !transaction.status {
                                                    all_successful = false;
                                                }
                                            }
                                        }

                                        if all_successful {
                                            println!("\n✅ Simulation successful!");
                                        } else {
                                            println!("\n❌ Simulation failed! One or more transactions reverted.");
                                            println!("Consider adjusting parameters and re-simulating before execution.");
                                        }
                                        println!();
                                        continue;
                                    }
                                    Err(e) => {
                                        eprintln!("\n❌ Simulation failed: {e:?}");
                                        println!("Your RPC provider does not support transaction simulation. Consider proceeding with execution instead or switching RPC provider.");
                                    }
                                }
                            }
                            "execute" => {
                                println!("\nExecuting RFQ swap...");

                                // Step 1: Send permit2 approval first
                                println!("Step 1: Sending permit2 approval...");
                                let approve_function_signature = "approve(address,uint256)";
                                let args = (
                                    Address::from_str("0x000000000022D473030F116dDEE9F6B43aC78BA3")
                                        .expect("Couldn't convert to address"),
                                    biguint_to_u256(&amount_in),
                                );
                                let approval_data =
                                    encode_input(approve_function_signature, args.abi_encode());
                                let nonce = provider
                                    .get_transaction_count(signer.address())
                                    .await
                                    .expect("Failed to get nonce");

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

                                let approval_request = TransactionRequest {
                                    to: Some(TxKind::Call(Address::from_slice(
                                        &sell_token_address,
                                    ))),
                                    from: Some(signer.address()),
                                    value: None,
                                    input: TransactionInput {
                                        input: Some(AlloyBytes::from(approval_data)),
                                        data: None,
                                    },
                                    gas: Some(100_000u64),
                                    chain_id: Some(chain.id()),
                                    max_fee_per_gas: Some(max_fee_per_gas.into()),
                                    max_priority_fee_per_gas: Some(max_priority_fee_per_gas.into()),
                                    nonce: Some(nonce),
                                    ..Default::default()
                                };

                                let approval_receipt = match provider
                                    .send_transaction(approval_request)
                                    .await
                                {
                                    Ok(receipt) => receipt,
                                    Err(e) => {
                                        eprintln!("\nFailed to send approval transaction: {e:?}\n");
                                        continue;
                                    }
                                };

                                let approval_result = match approval_receipt.get_receipt().await {
                                    Ok(result) => result,
                                    Err(e) => {
                                        eprintln!("\nFailed to get approval receipt: {e:?}\n");
                                        continue;
                                    }
                                };

                                println!(
                                    "Approval transaction sent with hash: {hash:?} and status: {status:?}",
                                    hash = approval_result.transaction_hash,
                                    status = approval_result.status()
                                );

                                if !approval_result.status() {
                                    eprintln!("\nApproval transaction failed! Cannot proceed with swap.\n");
                                    continue;
                                }

                                println!("Step 2: Encoding solution transaction...");

                                let solution = create_solution(
                                    component.clone(),
                                    Arc::from(state.clone_box()),
                                    sell_token.clone(),
                                    buy_token.clone(),
                                    amount_in.clone(),
                                    Bytes::from(signer.address().to_vec()),
                                    amount_out.clone(),
                                    amount_out_result.gas,
                                );

                                // Encode the swaps of the solution
                                let encoded_solution = encoder
                                    .encode_solutions(vec![solution.clone()])
                                    .expect("Failed to encode router calldata")[0]
                                    .clone();

                                let swap_tx = encode_tycho_router_call(
                                    chain.id(),
                                    encoded_solution.clone(),
                                    &solution,
                                    chain.native_token().address,
                                    signer.clone(),
                                )
                                .expect("Failed to encode router call");

                                let swap_request = TransactionRequest {
                                    to: Some(TxKind::Call(Address::from_slice(&swap_tx.to))),
                                    from: Some(signer.address()),
                                    value: Some(biguint_to_u256(&swap_tx.value)),
                                    input: TransactionInput {
                                        input: Some(AlloyBytes::from(swap_tx.data)),
                                        data: None,
                                    },
                                    gas: Some(1_000_000u64),
                                    chain_id: Some(chain.id()),
                                    max_fee_per_gas: Some(max_fee_per_gas.into()),
                                    max_priority_fee_per_gas: Some(max_priority_fee_per_gas.into()),
                                    nonce: Some(nonce + 1),
                                    ..Default::default()
                                };

                                let swap_receipt = match provider
                                    .send_transaction(swap_request)
                                    .await
                                {
                                    Ok(receipt) => receipt,
                                    Err(e) => {
                                        eprintln!("\nFailed to send swap transaction: {e:?}\n");
                                        continue;
                                    }
                                };

                                let swap_result = match swap_receipt.get_receipt().await {
                                    Ok(result) => result,
                                    Err(e) => {
                                        eprintln!("\nFailed to get swap receipt: {e:?}\n");
                                        continue;
                                    }
                                };

                                println!(
                                    "Swap transaction sent with hash: {hash:?} and status: {status:?}",
                                    hash = swap_result.transaction_hash,
                                    status = swap_result.status()
                                );

                                if swap_result.status() {
                                    println!(
                                        "\n✅ Swap executed successfully! Exiting the session...\n"
                                    );

                                    // Calculate the correct price ratio
                                    let (forward_price, _reverse_price) = format_price_ratios(
                                        &amount_in,
                                        &expected_amount_copy,
                                        &sell_token,
                                        &buy_token,
                                    );

                                    println!(
                                        "Summary: Swapped {formatted_in} {sell_symbol} → {formatted_out} {buy_symbol} at a price of {forward_price:.6} {buy_symbol} per {sell_symbol}",
                                        formatted_in = format_token_amount(&amount_in, &sell_token),
                                        sell_symbol = sell_token.symbol,
                                        formatted_out = format_token_amount(&expected_amount_copy, &buy_token),
                                        buy_symbol = buy_token.symbol,
                                    );
                                    return; // Exit the program after successful execution
                                } else {
                                    eprintln!("\nSwap transaction failed!\n");
                                    continue;
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
            } else {
                println!("No matching pair found in update.");
            }
        }

        println!("\nWaiting for more price levels... (Press Ctrl+C to exit)");
    }
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

#[allow(clippy::too_many_arguments)]
fn create_solution(
    component: ProtocolComponent,
    state: Arc<dyn ProtocolSim>,
    sell_token: Token,
    buy_token: Token,
    sell_amount: BigUint,
    user_address: Bytes,
    expected_amount: BigUint,
    gas_usage: BigUint,
) -> Solution {
    // Prepare data to encode. First we need to create a swap object
    let simple_swap = Swap::new(component, sell_token.clone(), buy_token.clone(), gas_usage)
        .with_protocol_state(state)
        .with_estimated_amount_in(sell_amount.clone());

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
    .with_user_transfer_type(UserTransferType::TransferFromPermit2)
}

/// Encodes a transaction for the Tycho Router using the `singleSwapPermit2` method.
///
/// # ⚠️ Important Responsibility Note
///
/// This function is intended as **an illustrative example only** and supports only the method of
/// interest of this quickstart. **Users must implement their own encoding logic** to ensure:
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
    let given_token = Address::from_slice(solution.token_in());
    let checked_token = Address::from_slice(solution.token_out());
    let receiver = Address::from_slice(solution.receiver());

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

    let method_calldata = (
        given_amount,
        given_token,
        checked_token,
        min_amount_out,
        receiver,
        permit,
        signature.as_bytes().to_vec(),
        encoded_solution.swaps().to_vec(),
    )
        .abi_encode();

    let contract_interaction = encode_input(encoded_solution.function_signature(), method_calldata);
    let value = if solution.token_in() == &native_address {
        solution.amount_in().clone()
    } else {
        BigUint::ZERO
    };
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
