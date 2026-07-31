use std::{
    env,
    fs::OpenOptions,
    io::{BufRead, BufReader, Write},
    sync::{Arc, Mutex},
};

use alloy::{
    primitives::{aliases::U24, Address, U256, U8},
    providers::{
        fillers::{BlobGasFiller, ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller},
        ProviderBuilder, RootProvider,
    },
    sol_types::SolValue,
};
use num_bigint::BigUint;
use once_cell::sync::Lazy;
use tokio::runtime::{Handle, Runtime};
use tycho_common::Bytes;

use crate::encoding::{errors::EncodingError, evm::constants::ROUTER_ETH_ADDRESS, models::Swap};

/// Converts `Address::ZERO` (protocol-native ETH marker) to the
/// `ETH_ADDRESS` marker (0xEeee…) used by the TychoRouter. Non-zero
/// addresses pass through unchanged.
pub fn convert_to_router_token(addr: Address) -> Address {
    if addr == Address::ZERO {
        Address::from_slice(&ROUTER_ETH_ADDRESS)
    } else {
        addr
    }
}

/// Safely converts a `Bytes` object to an `Address` object.
///
/// Checks the length of the `Bytes` before attempting to convert, and returns an `EncodingError`
/// if not 20 bytes long.
pub fn bytes_to_address(address: &Bytes) -> Result<Address, EncodingError> {
    if address.len() == 20 {
        Ok(Address::from_slice(address))
    } else {
        Err(EncodingError::InvalidInput(format!("Invalid address: {address}",)))
    }
}

/// Converts a general `BigUint` to an EVM-specific `U256` value.
pub fn biguint_to_u256(value: &BigUint) -> U256 {
    let bytes = value.to_bytes_be();
    U256::from_be_slice(&bytes)
}

/// Converts a decimal to a `U24` value. The percentage is a `f64` value between 0 and 1.
/// MAX_UINT24 corresponds to 100%.
pub(crate) fn percentage_to_uint24(decimal: f64) -> U24 {
    const MAX_UINT24: u32 = 16_777_215; // 2^24 - 1

    let scaled = (decimal / 1.0) * (MAX_UINT24 as f64);
    U24::from(scaled.round())
}

/// Gets the position of a token in a list of tokens.
pub(crate) fn get_token_position(tokens: &Vec<&Bytes>, token: &Bytes) -> Result<U8, EncodingError> {
    let position = U8::from(
        tokens
            .iter()
            .position(|t| *t == token)
            .ok_or_else(|| {
                EncodingError::InvalidInput(format!("Token {token} not found in tokens array"))
            })?,
    );
    Ok(position)
}

/// Pads or truncates a byte slice to a fixed size array of N bytes.
/// If input is shorter than N, it pads with zeros at the start.
/// If input is longer than N, it truncates from the start (keeps last N bytes).
pub(crate) fn pad_or_truncate_to_size<const N: usize>(
    input: &[u8],
) -> Result<[u8; N], EncodingError> {
    let mut result = [0u8; N];

    if input.len() <= N {
        // Pad with zeros at the start
        let start = N - input.len();
        result[start..].copy_from_slice(input);
    } else {
        // Truncate from the start (take last N bytes)
        let start = input.len() - N;
        result.copy_from_slice(&input[start..]);
    }

    Ok(result)
}

/// Extracts a static attribute from a swap.
pub(crate) fn get_static_attribute(
    swap: &Swap,
    attribute_name: &str,
) -> Result<Vec<u8>, EncodingError> {
    Ok(swap
        .component()
        .static_attributes
        .get(attribute_name)
        .ok_or_else(|| EncodingError::FatalError(format!("Attribute {attribute_name} not found")))?
        .to_vec())
}

/// A tokio `Runtime` wrapped in `Arc` that safely drops from async contexts.
///
/// If dropped while a tokio runtime is active on the current thread, ensures
/// the actual runtime shutdown happens on a background OS thread, avoiding the
/// "cannot drop a runtime in a context where blocking is not allowed" panic.
#[derive(Clone)]
pub(crate) struct SafeRuntime(Option<Arc<Runtime>>);

impl Drop for SafeRuntime {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                std::thread::spawn(move || drop(rt));
            }
        }
    }
}

/// Creates a dedicated multi-thread tokio runtime for encoding operations.
///
/// Always creates a new runtime rather than reusing the caller's, so that I/O
/// futures are driven by dedicated worker threads regardless of the caller's
/// runtime flavor (including current-thread runtimes like actix-web workers).
///
/// Returns the runtime handle and a [`SafeRuntime`] that can be dropped safely
/// from any context.
pub(crate) fn create_encoding_runtime() -> Result<(Handle, SafeRuntime), EncodingError> {
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|_| {
                EncodingError::FatalError("Failed to create encoding runtime".to_string())
            })?,
    );
    let handle = rt.handle().clone();
    Ok((handle, SafeRuntime(Some(rt))))
}

/// Runs a closure on a fresh OS thread, blocking the caller until it completes.
///
/// Unlike `tokio::task::block_in_place`, this works on any runtime flavor
/// (including current-thread) because the spawned thread has no tokio context.
/// Typical usage: `on_blocking_thread(|| handle.block_on(some_future))`.
pub(crate) fn on_blocking_thread<F, T>(f: F) -> Result<T, EncodingError>
where
    F: FnOnce() -> T + Send,
    T: Send,
{
    std::thread::scope(|s| {
        s.spawn(f)
            .join()
            .map_err(|_| EncodingError::FatalError("blocking thread panicked".to_string()))
    })
}

pub(crate) type EVMProvider = Arc<
    FillProvider<
        JoinFill<
            alloy::providers::Identity,
            JoinFill<GasFiller, JoinFill<BlobGasFiller, JoinFill<NonceFiller, ChainIdFiller>>>,
        >,
        RootProvider,
    >,
>;

/// Gets the client used for interacting with the EVM-compatible network.
pub(crate) async fn get_client() -> Result<EVMProvider, EncodingError> {
    dotenvy::dotenv().ok();
    let eth_rpc_url = env::var("RPC_URL")
        .map_err(|_| EncodingError::FatalError("Missing RPC_URL in environment".to_string()))?;
    let client = ProviderBuilder::new()
        .connect(&eth_rpc_url)
        .await
        .map_err(|_| EncodingError::FatalError("Failed to build provider".to_string()))?;
    Ok(Arc::new(client))
}

/// Uses prefix-length encoding to efficient encode action data.
///
/// Prefix-length encoding is a data encoding method where the beginning of a data segment
/// (the "prefix") contains information about the length of the following data.
pub(crate) fn ple_encode(action_data_array: Vec<Vec<u8>>) -> Vec<u8> {
    let mut encoded_action_data: Vec<u8> = Vec::new();

    for action_data in action_data_array {
        let args = (encoded_action_data, action_data.len() as u16, action_data);
        encoded_action_data = args.abi_encode_packed();
    }

    encoded_action_data
}

static CALLDATA_WRITE_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
// Function used in tests to write calldata to a file that then is used by the corresponding
// solidity tests.
pub fn write_calldata_to_file(test_identifier: &str, hex_calldata: &str) {
    let _lock = CALLDATA_WRITE_MUTEX
        .lock()
        .expect("Couldn't acquire lock");

    let file_path = "contracts/test/assets/calldata.txt";
    let file = OpenOptions::new()
        .read(true)
        .open(file_path)
        .expect("Failed to open calldata file for reading");
    let reader = BufReader::new(file);

    let mut lines = Vec::new();
    let mut found = false;
    for line in reader.lines().map_while(Result::ok) {
        let mut parts = line.splitn(2, ':'); // split at the :
        let key = parts.next().unwrap_or("");
        if key == test_identifier {
            lines.push(format!("{test_identifier}:{hex_calldata}"));
            found = true;
        } else {
            lines.push(line);
        }
    }

    // If the test identifier wasn't found, append a new line
    if !found {
        lines.push(format!("{test_identifier}:{hex_calldata}"));
    }

    // Write the updated contents back to the file
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(file_path)
        .expect("Failed to open calldata file for writing");

    for line in lines {
        writeln!(file, "{line}").expect("Failed to write calldata");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pad_or_truncate_to_size() {
        // Test padding
        let input = hex::decode("0110").unwrap();
        let result = pad_or_truncate_to_size::<3>(&input).unwrap();
        assert_eq!(hex::encode(result), "000110");

        // Test truncation
        let input_long = hex::decode("00800000").unwrap();
        let result_truncated = pad_or_truncate_to_size::<3>(&input_long).unwrap();
        assert_eq!(hex::encode(result_truncated), "800000");
    }
}
