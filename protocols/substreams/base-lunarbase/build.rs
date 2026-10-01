use std::{env, fs, path::Path};

use anyhow::{Ok, Result};
use substreams_ethereum::Abigen;

fn main() -> Result<(), anyhow::Error> {
    let artifact = fs::read_to_string("abi/Pool.json")?;
    let artifact: serde_json::Value = serde_json::from_str(&artifact)?;
    let abi = artifact
        .get("abi")
        .unwrap_or(&artifact)
        .as_array()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Pool ABI must be a JSON array or an artifact containing an `abi` array"
            )
        })?;
    // Keep the complete runtime ABI as the source; this indexer consumes only these events.
    let event_names = [
        "StateUpdated",
        "Sync",
        "BlockDelaySet",
        "MaxPunishmentX24Set",
        "PunishmentApplied",
        "BlacklistFeeMultiplierSet",
        "WhitelistSet",
        "Paused",
        "Unpaused",
    ];
    let events = abi
        .iter()
        .filter(|entry| {
            entry
                .get("type")
                .and_then(serde_json::Value::as_str) ==
                Some("event") &&
                entry
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|name| event_names.contains(&name))
        })
        .collect::<Vec<_>>();
    let abi_path = Path::new(&env::var("OUT_DIR")?).join("Pool.abi.json");
    fs::write(&abi_path, serde_json::to_vec(&events)?)?;

    let abi_path = abi_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Pool ABI path is not valid UTF-8"))?
        .to_owned();

    Abigen::new("Pool", abi_path.as_str())?
        .generate()?
        .write_to_file("src/abi/pool.rs")?;
    Ok(())
}
