use anyhow::{Ok, Result};
use substreams_ethereum::Abigen;

fn main() -> Result<(), anyhow::Error> {
    Abigen::new("Factory", "abi/PoolManager.json")?
        .generate()?
        .write_to_file("src/abi/pool_manager.rs")?;
    Ok(())
}
