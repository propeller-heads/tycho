//! Read-only orderbook demonstration. It does not sign or broadcast transactions.
use std::{str::FromStr, time::Duration};

use anyhow::{bail, Context, Result};
use clap::Parser;
use futures::StreamExt;
use num_bigint::BigUint;
use tycho_common::{
    models::{protocol::GetAmountOutParams, Chain},
    Bytes,
};
use tycho_simulation::{
    book::{BookFeedEvent, BookFeedOutcome, BookFeedStream},
    pamm::protocols::elfomofi::{ElfomofiBlock, ElfomofiFeedBuilder, ElfomofiOptions},
};

#[derive(Parser)]
struct Cli {
    /// Deployment: base, bsc or robinhood. RPC endpoints must serve this chain.
    #[arg(long, default_value = "base")]
    chain: String,
    /// Raw input in each book's quote token; defaults to one whole quote token.
    #[arg(long)]
    amount_in: Option<String>,
    /// Optional deployed Tycho Router address for a fresh contract quote in its caller context.
    #[arg(long)]
    tycho_router: Option<String>,
    /// Explicit HTTP polling fallback for a provider without WebSocket subscriptions.
    #[arg(long)]
    poll_ms: Option<u64>,
    #[arg(long, default_value_t = 10)]
    max_age_seconds: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let chain = Chain::from_str(&cli.chain)?;
    let amount = cli
        .amount_in
        .map(|value| value.parse::<BigUint>())
        .transpose()
        .context("invalid raw amount")?;
    let feed = ElfomofiFeedBuilder::new(chain)
        .options(ElfomofiOptions {
            poll_time: cli.poll_ms.map(Duration::from_millis),
            max_age: Duration::from_secs(cli.max_age_seconds),
            ..Default::default()
        })
        .build()?;
    let mut books = BookFeedStream::<ElfomofiBlock>::spawn("elfomofi", feed);
    let sender = cli
        .tycho_router
        .map(|address| Bytes::from_str(&address))
        .transpose()?;
    loop {
        tokio::select! {
            result = tokio::signal::ctrl_c() => { result?; break },
            event = books.next() => {
                let snapshot = match event {
                    Some(BookFeedEvent::Published(snapshot)) => snapshot,
                    Some(BookFeedEvent::Withdrawn) => {
                        println!("Orderbooks withdrawn; waiting for fresh state");
                        continue;
                    }
                    Some(BookFeedEvent::Ended(BookFeedOutcome::Failed(error))) => bail!(error),
                    Some(BookFeedEvent::Ended(BookFeedOutcome::Panicked(error))) => bail!(error),
                    Some(BookFeedEvent::Ended(BookFeedOutcome::RanOut)) | None => break,
                };
                println!("Block {}: {} orderbooks", snapshot.anchor.number, snapshot.books.len());
                for (id, book) in snapshot.books.iter() {
                    let component = &book.component;
                    let state = &book.state;
                    let [buy, sell] = component.tokens.as_slice() else { continue };
                    let amount = amount.clone().unwrap_or_else(|| BigUint::from(10u8).pow(sell.decimals));
                    match state.get_amount_out(amount.clone(),sell,buy) {
                        Ok(quote) => println!("{} on {}: {} raw {} -> {} raw {} (indicative)",
                            component.protocol_system,chain,amount,sell.symbol,quote.amount,buy.symbol),
                        Err(error) => { println!("{id}: no quote: {error}"); continue }
                    }
                    if let Some(sender) = &sender {
                        let quote = state.as_indicatively_priced()?.request_signed_quote(
                            GetAmountOutParams { amount_in:amount.clone(),
                                token_in:sell.address.clone(),token_out:buy.address.clone(),
                                sender:sender.clone(),receiver:sender.clone(),origin:None }).await;
                        match quote {
                            Ok(quote) => println!("Fresh unsigned contract quote: {} raw {}; attributes {:?}",
                                quote.amount_out, buy.symbol, quote.quote_attributes),
                            Err(error) => println!("{id}: fresh quote unavailable: {error}"),
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
