use anyhow::{Ok, Result};
use substreams_ethereum::Abigen;

fn main() -> Result<(), anyhow::Error> {
    Abigen::new("Factory", "abi/Factory.json")?
        .generate()?
        .write_to_file("src/abi/factory.rs")?;
    Abigen::new("Pool", "abi/Pool.json")?
        .generate()?
        .write_to_file("src/abi/pool.rs")?;
    Ok(())
}
