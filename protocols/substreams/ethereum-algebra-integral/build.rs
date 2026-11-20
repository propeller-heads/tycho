use anyhow::{Ok, Result};
use substreams_ethereum::Abigen;

fn main() -> Result<(), anyhow::Error> {
    Abigen::new("Beacon", "abi/Beacon.json")?
        .generate()?
        .write_to_file("src/abi/beacon.rs")?;
    Abigen::new("Factory", "abi/Factory.json")?
        .generate()?
        .write_to_file("src/abi/factory.rs")?;
    Abigen::new("Pool", "abi/Pool.json")?
        .generate()?
        .write_to_file("src/abi/pool.rs")?;
    Abigen::new("Plugin", "abi/Plugin.json")?
        .generate()?
        .write_to_file("src/abi/plugin.rs")?;
    Ok(())
}
