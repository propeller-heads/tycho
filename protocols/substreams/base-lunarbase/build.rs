use std::{env, fs, path::Path};

use anyhow::{Ok, Result};
use substreams_ethereum::Abigen;

fn main() -> Result<(), anyhow::Error> {
    let artifact = fs::read_to_string("abi/Pool.json")?;
    let artifact: serde_json::Value = serde_json::from_str(&artifact)?;
    let abi = artifact.get("abi").unwrap_or(&artifact);
    let abi_path = Path::new(&env::var("OUT_DIR")?).join("Pool.abi.json");
    fs::write(&abi_path, serde_json::to_vec(abi)?)?;

    let abi_path = abi_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Pool ABI path is not valid UTF-8"))?
        .to_owned();

    Abigen::new("Pool", abi_path.as_str())?
        .generate()?
        .write_to_file("src/abi/pool.rs")?;
    Ok(())
}
