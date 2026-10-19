# ABIs

`get_abis.py` is a simple python script using the etherscan API (free plan) to gather ABIs for all of the contracts we are tracking!

We then can define all of the abis via `substreams_ethereum::Abigen::new` in our `build.rs`.

## Recommendation

It would be apt to convert (maybe through copilot) the python code into the `build.rs` file and then automate the `Abigen` functionality.

## Usage

Requires `python 3.8+`,

```bash
cd abi
python get_abis.py
```

This will populate the files in the `abi` folder.

When the `build.rs` file runs (when `rust-analyzer` activates or `cargo build` is manually ran), Abigen will generate new rust src files from the abis in the `src/abi` folder.
