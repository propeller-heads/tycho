use anyhow::{Ok, Result};
use substreams_ethereum::Abigen;

// The ABI holds only the events the modules decode.
fn main() -> Result<(), anyhow::Error> {
    Abigen::new("CLPoolManager", "abi/CLPoolManager.json")?
        .generate()?
        .write_to_file("src/abi/cl_pool_manager.rs")?;
    Ok(())
}
