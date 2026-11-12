use anyhow::{Ok, Result};
use substreams_ethereum::Abigen;

// Same pattern as the CL package. The ABI holds only the events the modules decode.
fn main() -> Result<(), anyhow::Error> {
    Abigen::new("BinPoolManager", "abi/BinPoolManager.json")?
        .generate()?
        .write_to_file("src/abi/bin_pool_manager.rs")?;
    Ok(())
}
