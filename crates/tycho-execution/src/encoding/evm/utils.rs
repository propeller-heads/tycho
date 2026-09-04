use std::{
    env, fs,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
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
use tokio::runtime::{Handle, Runtime};
use tycho_common::Bytes;

use crate::encoding::{errors::EncodingError, evm::constants::ROUTER_ETH_ADDRESS, models::Swap};

/// Converts `Address::ZERO` (protocol-native ETH marker) to the
/// `ETH_ADDRESS` marker (0xEeee…) used by the TychoRouterV3. Non-zero
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

/// Extracts the human-readable message from a panic payload, if it carries one.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "non-string panic payload"
    }
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
        s.spawn(f).join().map_err(|payload| {
            EncodingError::FatalError(format!(
                "blocking thread panicked: {}",
                panic_message(&*payload)
            ))
        })
    })
}

/// Upper bound on the OS threads one `map_on_threads` call runs at a time.
///
/// The threads block on RFQ network round trips, so the cap bounds peak memory (each thread
/// reserves stack space) while still overlapping the waits.
const MAX_ENCODING_THREADS: usize = 32;

/// Runs `f` on every item on up to [`MAX_ENCODING_THREADS`] OS threads, and returns the results
/// in input order.
///
/// An RFQ encoder blocks on a network round trip for its signed quote, so running the items at
/// the same time bounds the total wait by the slowest item instead of the sum of all items.
///
/// A single item runs on the calling thread. Any item's error is returned; earlier items win.
pub(crate) fn map_on_threads<T, R, F>(items: &[T], f: F) -> Result<Vec<R>, EncodingError>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> Result<R, EncodingError> + Sync,
{
    if let [item] = items {
        return Ok(vec![f(item)?]);
    }
    let next_index = AtomicUsize::new(0);
    let workers = items.len().min(MAX_ENCODING_THREADS);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(scope.spawn(|| {
                let mut worker_results = Vec::new();
                loop {
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        return worker_results;
                    };
                    worker_results.push((index, f(item)));
                }
            }));
        }
        let mut results: Vec<Option<R>> = Vec::new();
        results.resize_with(items.len(), || None);
        let mut first_error: Option<(usize, EncodingError)> = None;
        for handle in handles {
            let worker_results = handle.join().map_err(|payload| {
                EncodingError::FatalError(format!(
                    "encoding thread panicked: {}",
                    panic_message(&*payload)
                ))
            })?;
            for (index, result) in worker_results {
                match result {
                    Ok(value) => results[index] = Some(value),
                    Err(error) => {
                        if first_error
                            .as_ref()
                            .is_none_or(|(first_index, _)| index < *first_index)
                        {
                            first_error = Some((index, error));
                        }
                    }
                }
            }
        }
        if let Some((_, error)) = first_error {
            return Err(error);
        }
        let mut ordered = Vec::with_capacity(items.len());
        for result in results {
            ordered.push(result.ok_or_else(|| {
                EncodingError::FatalError("encoding thread dropped a result".to_string())
            })?);
        }
        Ok(ordered)
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

/// Directory holding the calldata a solidity integration test replays, one
/// `<test_identifier>.hex` file per test. One file per test keeps concurrent writes from the
/// separate `tests/*.rs` binaries independent, and keeps two branches that add different tests
/// from touching the same file.
const CALLDATA_DIR: &str = "contracts/test/assets/calldata";

// Function used in tests to write calldata to a file that then is used by the corresponding
// solidity tests.
pub fn write_calldata_to_file(test_identifier: &str, hex_calldata: &str) {
    assert!(
        test_identifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "Test identifier {test_identifier} is used as a file name, so it must consist of ASCII \
         alphanumerics and underscores only"
    );
    let path = Path::new(CALLDATA_DIR).join(format!("{test_identifier}.hex"));
    fs::write(&path, hex_calldata)
        .unwrap_or_else(|e| panic!("Failed to write calldata to {}: {e}", path.display()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_on_threads_keeps_input_order_above_the_thread_cap() {
        let items: Vec<usize> = (0..(MAX_ENCODING_THREADS * 3 + 1)).collect();

        let results = map_on_threads(&items, |item| Ok(*item * 2)).unwrap();

        let expected: Vec<usize> = items
            .iter()
            .map(|item| item * 2)
            .collect();
        assert_eq!(results, expected);
    }

    #[test]
    fn test_map_on_threads_returns_the_earliest_error() {
        let items: Vec<usize> = (0..(MAX_ENCODING_THREADS * 2)).collect();

        let result: Result<Vec<usize>, EncodingError> = map_on_threads(&items, |item| {
            if *item >= 3 {
                Err(EncodingError::InvalidInput(format!("item {item} is broken")))
            } else {
                Ok(*item)
            }
        });

        let Err(EncodingError::InvalidInput(message)) = result else {
            panic!("expected an InvalidInput error, got {result:?}");
        };
        assert_eq!(message, "item 3 is broken");
    }

    #[test]
    fn test_map_on_threads_reports_the_panic_message() {
        let items = vec![1, 2];

        let result: Result<Vec<i32>, EncodingError> = map_on_threads(&items, |item| {
            assert_ne!(*item, 2, "item 2 is broken");
            Ok(*item)
        });

        let Err(EncodingError::FatalError(message)) = result else {
            panic!("expected a FatalError, got {result:?}");
        };
        assert!(message.contains("item 2 is broken"), "{message}");
    }

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
