use serde::Deserialize;
use tycho_simulation::tycho_common::{models::token::Token, simulation::protocol_sim::ProtocolSim};

#[derive(Debug, Deserialize)]
struct Pool {
    tokens: Vec<Token>,
    state: Box<dyn ProtocolSim>,
}

fn main() {
    let data_path = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/use_serialized_state/data.json");
    let json = std::fs::read_to_string(data_path).unwrap();
    let states: Vec<Pool> = serde_json::from_str(&json).unwrap();

    for (i, pool) in states.iter().enumerate() {
        let token_in = &pool.tokens[0];
        let token_out = &pool.tokens[1];
        let amount_in = token_in.one();

        println!("Pool #{}", i + 1);
        println!("  Token In:  {} ({:#x})", token_in.symbol, token_in.address);
        println!("  Token Out: {} ({:#x})", token_out.symbol, token_out.address);
        println!("  Amount In: {} (1 {})", amount_in, token_in.symbol);

        match pool
            .state
            .get_amount_out(amount_in, token_in, token_out)
        {
            Ok(result) => {
                let amount_out_human = result
                    .amount
                    .to_string()
                    .parse::<f64>()
                    .unwrap() /
                    10f64.powf(f64::from(token_out.decimals));
                println!(
                    "  Amount Out: {} ({:.3} {})",
                    result.amount, amount_out_human, token_out.symbol
                );
                println!("  Gas: {}", result.gas);
            }
            Err(e) => {
                println!("  Error: {}", e);
            }
        }
        println!();
    }
}
