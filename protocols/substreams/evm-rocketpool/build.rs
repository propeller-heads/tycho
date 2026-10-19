use anyhow::Result;
use heck::ToSnakeCase;
use std::fs;
use substreams_ethereum::Abigen;

fn main() -> Result<()> {
    for entry in fs::read_dir("abi")? {
        let path = entry?.path();
        let name = path
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap();
        Abigen::new(name, path.to_str().unwrap())?
            .generate()?
            .write_to_file(format!("src/abi/{}.rs", name.to_snake_case()))?;
    }
    Ok(())
}
