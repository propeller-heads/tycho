use anyhow::Result;
use std::{fs, io::Write};
use substreams_ethereum::Abigen;

fn module_name(contract_name: &str) -> String {
    contract_name
        .chars()
        .enumerate()
        .fold(String::new(), |mut acc, (idx, ch)| {
            if ch.is_uppercase() && idx > 0 {
                acc.push('_');
            }
            acc.push(ch.to_ascii_lowercase());
            acc
        })
}

fn main() -> Result<()> {
    let abi_folder = "abi";
    let output_folder = "src/abi";
    fs::create_dir_all(output_folder)?;

    let abis = fs::read_dir(abi_folder)?;

    let mut files = abis.collect::<Result<Vec<_>, _>>()?;

    // Sort the files by their name
    files.sort_by_key(|a| a.file_name());

    let mut mod_rs_content = String::new();
    mod_rs_content.push_str("#![allow(clippy::all)]\n");

    for file in files {
        let file_name = file.file_name();
        let file_name = file_name.to_string_lossy();

        if !file_name.ends_with(".json") {
            continue;
        }

        let contract_name = file_name.split('.').next().unwrap();
        let module_name = module_name(contract_name);

        let input_path = format!("{abi_folder}/{file_name}");
        let output_path = format!("{output_folder}/{module_name}.rs");
        let generated_input_path = format!("{output_folder}/{module_name}.events.json");

        mod_rs_content.push_str(&format!("pub mod {module_name};\n"));

        let abi: serde_json::Value = serde_json::from_str(&fs::read_to_string(&input_path)?)?;
        let events = abi
            .as_array()
            .expect("contract ABI must be a JSON array")
            .iter()
            .filter(|entry| {
                entry
                    .get("type")
                    .and_then(|entry_type| entry_type.as_str())
                    == Some("event")
            })
            .cloned()
            .collect::<Vec<_>>();

        fs::write(&generated_input_path, serde_json::to_string_pretty(&events)?)?;

        Abigen::new(contract_name, &generated_input_path)?
            .generate()?
            .write_to_file(&output_path)?;

        fs::remove_file(generated_input_path)?;
    }

    let mod_rs_path = format!("{output_folder}/mod.rs");
    let mut mod_rs_file = fs::File::create(mod_rs_path)?;

    mod_rs_file.write_all(mod_rs_content.as_bytes())?;

    Ok(())
}
