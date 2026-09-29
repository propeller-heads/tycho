use anyhow::Result;
use substreams_ethereum::Abigen;

fn main() -> Result<()> {
    Abigen::new("OrderBook", "abi/OrderBook.json")?
        .generate()?
        .write_to_file("src/abi/order_book.rs")?;
    Abigen::new("Router", "abi/Router.json")?
        .generate()?
        .write_to_file("src/abi/router.rs")?;
    Ok(())
}
