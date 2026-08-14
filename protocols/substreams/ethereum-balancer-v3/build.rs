#![allow(clippy::all)]

use anyhow::{Ok, Result};
use regex::Regex;
use std::fs;
use substreams_ethereum::Abigen;

fn main() -> Result<(), anyhow::Error> {
    let file_names = [
        "abi/vault_contract.abi.json",
        "abi/stable_pool_factory_contract.abi.json",
        "abi/weighted_pool_factory_contract.abi.json",
        "abi/reclamm_pool_factory_contract.abi.json",
        "abi/quantamm_weighted_pool_factory_contract.abi.json",
        "abi/stable_pool_contract.abi.json",
        "abi/weighted_pool_contract.abi.json",
    ];
    let file_output_names = [
        "src/abi/vault_contract.rs",
        "src/abi/stable_pool_factory_contract.rs",
        "src/abi/weighted_pool_factory_contract.rs",
        "src/abi/reclamm_pool_factory_contract.rs",
        "src/abi/quantamm_weighted_pool_factory_contract.rs",
        "src/abi/stable_pool_contract.rs",
        "src/abi/weighted_pool_contract.rs",
    ];

    let mut i = 0;
    for f in file_names {
        let contents = fs::read_to_string(f).expect("Should have been able to read the file");

        // sanitize fields and attributes starting with an underscore
        let regex = Regex::new(r#"("\w+"\s?:\s?")_(\w+")"#).unwrap();
        let sanitized_abi_file = regex.replace_all(contents.as_str(), "${1}u_${2}");

        // sanitize fields and attributes with multiple consecutive underscores
        let re = Regex::new(r"_+").unwrap();

        let re_sanitized_abi_file =
            re.replace_all(&sanitized_abi_file, |caps: &regex::Captures| {
                let count = caps[0].len();
                let replacement = format!("{}_", "_u".repeat(count - 1));
                replacement
            });

        Abigen::from_bytes("Contract", re_sanitized_abi_file.as_bytes())?
            .generate()?
            .write_to_file(file_output_names[i])?;

        // The QuantAMM factory takes its creation parameters as one deeply nested struct, which
        // abigen renders as a single tuple field of seventeen elements. Rust only implements
        // `Debug` and `PartialEq` for tuples up to twelve, so the generated derives do not
        // compile; the bindings are only ever read field by field, so dropping them is enough.
        if file_output_names[i].contains("quantamm_weighted_pool_factory_contract") {
            let generated = fs::read_to_string(file_output_names[i])?;
            let without_unsupported_derives =
                generated.replace("#[derive(Debug, Clone, PartialEq)]", "#[derive(Clone)]");
            fs::write(file_output_names[i], without_unsupported_derives)?;
        }

        i = i + 1;
    }

    Ok(())
}
