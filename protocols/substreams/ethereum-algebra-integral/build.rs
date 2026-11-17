use anyhow::{Ok, Result};
use substreams_ethereum::Abigen;

fn main() -> Result<(), anyhow::Error> {
    Abigen::new("Beacon", "abi/Beacon.json")?
        .generate()?
        .write_to_file("src/abi/beacon.rs")?;
    Ok(())
}
