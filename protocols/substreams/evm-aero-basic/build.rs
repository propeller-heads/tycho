use anyhow::Result;
use substreams_ethereum::Abigen;

fn main() -> Result<()> {
    Abigen::new("PoolFactory", "abi/PoolFactory.json")?
        .generate()?
        .write_to_file("src/abi/pool_factory.rs")?;
    Abigen::new("Pool", "abi/Pool.json")?
        .generate()?
        .write_to_file("src/abi/pool.rs")?;
    Abigen::new("CustomFeeModule", "abi/CustomFeeModule.json")?
        .generate()?
        .write_to_file("src/abi/custom_fee_module.rs")?;
    Ok(())
}
