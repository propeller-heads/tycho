use anyhow::Result;
use substreams_ethereum::Abigen;

fn main() -> Result<()> {
    for (name, file) in [
        ("TychoRouterV2", "tycho_router_v2"),
        ("TychoRouterV3_0", "tycho_router_v3_0"),
        ("TychoRouterV3_1", "tycho_router_v3_1"),
        ("FeeCalculator", "fee_calculator"),
        ("FeeCalculatorV3_0", "fee_calculator_v3_0"),
    ] {
        Abigen::new(name, &format!("abi/{name}.json"))?
            .generate()?
            .write_to_file(format!("src/abi/{file}.rs"))?;
    }
    Ok(())
}
