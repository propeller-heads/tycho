use substreams::scalar::BigInt;
use substreams_ethereum::pb::eth::v2::Call;

use super::*;
use crate::abi::{tycho_router_v2 as v2, tycho_router_v3_0 as v3_0, tycho_router_v3_1 as v3_1};

/// Calldata produced by the `tycho-execution` encoder tests (V3.1 router ABI).
const FIXTURES: &str = include_str!("../../../../contracts/test/assets/calldata.txt");

fn call_with_input(input: Vec<u8>) -> Call {
    Call { input, ..Default::default() }
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    FIXTURES
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(name, data)| (name.to_string(), hex::decode(data.trim()).expect("fixture hex")))
        .collect()
}

const SWAP_FUNCTIONS: [&str; 9] = [
    "singleSwap",
    "singleSwapPermit2",
    "singleSwapUsingVault",
    "sequentialSwap",
    "sequentialSwapPermit2",
    "sequentialSwapUsingVault",
    "splitSwap",
    "splitSwapPermit2",
    "splitSwapUsingVault",
];

fn contract(json: &str) -> ethabi::Contract {
    ethabi::Contract::load(json.as_bytes()).expect("valid router ABI")
}

fn swap_selectors(json: &str, names: &[&str]) -> std::collections::HashSet<[u8; 4]> {
    let contract = contract(json);
    names
        .iter()
        .map(|name| {
            contract
                .function(name)
                .unwrap()
                .short_signature()
        })
        .collect()
}

#[test]
fn decodes_every_encoder_fixture_as_v3_1() {
    let fixtures = fixtures();
    assert!(fixtures.len() > 50, "fixture file looks truncated");
    let selectors = swap_selectors(include_str!("../../abi/TychoRouterV3_1.json"), &SWAP_FUNCTIONS);
    let mut failures = Vec::new();
    let mut router_calls = 0;
    for (name, input) in fixtures {
        // Executor-level fixtures bypass the router and carry executor selectors.
        if input
            .get(..4)
            .is_none_or(|selector| !selectors.contains(selector))
        {
            continue;
        }
        router_calls += 1;
        if let Err(err) = check_fixture(&name, input) {
            failures.push(format!("{name}: {err}"));
        }
    }
    assert!(router_calls > 40, "only {router_calls} router fixtures decoded");
    // The substreams panic hook aborts before printing the panic payload, so print first.
    for failure in &failures {
        eprintln!("fixture failure: {failure}");
    }
    assert!(failures.is_empty());
}

fn token_for(kind: &ethabi::ParamType) -> ethabi::Token {
    use ethabi::{ParamType, Token};
    match kind {
        ParamType::Address => Token::Address(ethabi::Address::repeat_byte(0x11)),
        ParamType::Bytes => Token::Bytes(vec![0x22; 32]),
        ParamType::Int(_) => Token::Int(1.into()),
        ParamType::Uint(_) => Token::Uint(1.into()),
        ParamType::Bool => Token::Bool(true),
        ParamType::String => Token::String("value".to_string()),
        ParamType::Array(inner) => Token::Array(vec![token_for(inner)]),
        ParamType::FixedBytes(size) => Token::FixedBytes(vec![0x33; *size]),
        ParamType::FixedArray(inner, size) => Token::FixedArray(
            (0..*size)
                .map(|_| token_for(inner))
                .collect(),
        ),
        ParamType::Tuple(items) => Token::Tuple(items.iter().map(token_for).collect()),
    }
}

fn expected_shape(name: &str) -> (Method, Funding) {
    let method = if name.starts_with("single") {
        Method::Single
    } else if name.starts_with("sequential") {
        Method::Sequential
    } else {
        Method::Split
    };
    let funding = if name.ends_with("Permit2") {
        Funding::Permit2
    } else if name.ends_with("UsingVault") {
        Funding::Vault
    } else {
        Funding::TransferFrom
    };
    (method, funding)
}

fn assert_entry_points_decode(version: RouterVersion, json: &str, names: &[&str]) {
    let contract = contract(json);
    for name in names {
        let function = contract.function(name).unwrap();
        let tokens = function
            .inputs
            .iter()
            .map(|input| token_for(&input.kind))
            .collect::<Vec<_>>();
        let input = function.encode_input(&tokens).unwrap();
        let swap = decode_swap_call(version, &call_with_input(input))
            .unwrap_or_else(|| panic!("{name} selector was not recognised"))
            .unwrap_or_else(|error| panic!("{name} failed to decode: {error}"));
        assert_eq!((swap.method, swap.funding), expected_shape(name), "{name}");
    }
}

#[test]
fn decodes_every_v2_entry_point() {
    let names = SWAP_FUNCTIONS
        .iter()
        .copied()
        .filter(|name| !name.ends_with("UsingVault"))
        .collect::<Vec<_>>();
    assert_entry_points_decode(
        RouterVersion::V2,
        include_str!("../../abi/TychoRouterV2.json"),
        &names,
    );
}

#[test]
fn decodes_every_v3_0_entry_point() {
    assert_entry_points_decode(
        RouterVersion::V3_0,
        include_str!("../../abi/TychoRouterV3_0.json"),
        &SWAP_FUNCTIONS,
    );
}

fn check_fixture(name: &str, input: Vec<u8>) -> Result<(), String> {
    let call = call_with_input(input);
    let swap = decode_swap_call(RouterVersion::V3_1, &call)
        .ok_or_else(|| "selector not recognised".to_string())??;
    if swap.token_in.len() != 20 || swap.token_out.len() != 20 {
        return Err("token address is not 20 bytes".into());
    }
    if swap.amount_in <= BigInt::zero() {
        return Err("zero amountIn".into());
    }
    if !swap.watermark.is_empty() {
        return Err(format!("unexpected watermark 0x{}", hex::encode(&swap.watermark)));
    }
    if swap.client_fee.is_none() {
        return Err("v3 must carry client fee params".into());
    }
    let hops = swaps::decode_hops(swap.method, &swap.swaps)?;
    if hops.is_empty() {
        return Err("no hops".into());
    }
    let expected_method = if name.starts_with("test_split") {
        Some(Method::Split)
    } else if name.starts_with("test_sequential") {
        Some(Method::Sequential)
    } else if name.starts_with("test_single") {
        Some(Method::Single)
    } else {
        None
    };
    if expected_method.is_some_and(|m| m != swap.method) {
        return Err(format!("method {:?} but name implies {:?}", swap.method, expected_method));
    }
    match swap.method {
        Method::Single if hops.len() != 1 => Err(format!("single swap with {} hops", hops.len())),
        Method::Split if swap.n_tokens.unwrap_or_default() < 2 => {
            Err("split swap with < 2 tokens".into())
        }
        Method::Single | Method::Sequential | Method::Split => Ok(()),
    }
}

#[test]
fn fixtures_are_not_decodable_as_other_versions() {
    let (name, input) = fixtures()
        .into_iter()
        .find(|(n, _)| n == "test_single_swap_strategy_encoder")
        .unwrap();
    let call = call_with_input(input);
    assert!(decode_swap_call(RouterVersion::V2, &call).is_none(), "{name} matched v2");
    assert!(decode_swap_call(RouterVersion::V3_0, &call).is_none(), "{name} matched v3_0");
}

#[test]
fn split_fixture_exposes_indices_and_splits() {
    let (_, input) = fixtures()
        .into_iter()
        .find(|(n, _)| n == "test_split_swap_strategy_encoder")
        .unwrap();
    let swap = decode_swap_call(RouterVersion::V3_1, &call_with_input(input))
        .unwrap()
        .unwrap();
    assert_eq!(swap.method, Method::Split);
    let hops = swaps::decode_hops(Method::Split, &swap.swaps).unwrap();
    assert!(hops.len() >= 2);
    assert!(
        hops.iter()
            .any(|h| h.split.unwrap() > 0),
        "at least one hop must carry a split share"
    );
    assert!(hops
        .iter()
        .all(|h| (h.token_in_index.unwrap() as u32) < swap.n_tokens.unwrap()));
}

#[test]
fn fee_fixtures_carry_client_fee_params() {
    let (_, input) = fixtures()
        .into_iter()
        .find(|(n, _)| n == "test_single_swap_with_client_fees")
        .unwrap();
    let swap = decode_swap_call(RouterVersion::V3_1, &call_with_input(input))
        .unwrap()
        .unwrap();
    let fee = swap.client_fee.unwrap();
    assert!(fee.fee_bps > BigInt::zero());
    assert_ne!(fee.receiver, vec![0u8; 20]);
    assert!(!fee.signature.is_empty());
    assert!(swap.expected_amount_out.is_some());
}

#[test]
fn watermark_is_trailing_calldata() {
    let (_, input) = fixtures()
        .into_iter()
        .find(|(n, _)| n == "test_single_swap_strategy_encoder")
        .unwrap();
    let mut with_mark = input.clone();
    with_mark.extend_from_slice(b"tycho-watermark");
    let plain = decode_swap_call(RouterVersion::V3_1, &call_with_input(input))
        .unwrap()
        .unwrap();
    let marked = decode_swap_call(RouterVersion::V3_1, &call_with_input(with_mark))
        .unwrap()
        .unwrap();
    assert_eq!(marked.watermark, b"tycho-watermark".to_vec());
    assert_eq!(marked.swaps, plain.swaps);
    assert_eq!(marked.amount_in, plain.amount_in);
}

#[test]
fn non_swap_selector_is_ignored_and_garbage_args_error() {
    let call = call_with_input(vec![0x12, 0x34, 0x56, 0x78, 0, 0]);
    assert!(decode_swap_call(RouterVersion::V3_1, &call).is_none());
    let mut truncated = fixtures()
        .into_iter()
        .find(|(n, _)| n == "test_single_swap_strategy_encoder")
        .unwrap()
        .1;
    truncated.truncate(40);
    assert!(decode_swap_call(RouterVersion::V3_1, &call_with_input(truncated))
        .unwrap()
        .is_err());
}

fn v2_split_call(transfer_from: bool) -> Call {
    let swaps = [vec![0, 1, 0, 0, 0], vec![0xab; 20], vec![1, 2, 3]].concat();
    let mut ple = (swaps.len() as u16)
        .to_be_bytes()
        .to_vec();
    ple.extend(swaps);
    let encoded = v2::functions::SplitSwap {
        amount_in: BigInt::from(1_000),
        token_in: vec![0x11; 20],
        token_out: vec![0x22; 20],
        min_amount_out: BigInt::from(900),
        wrap_eth: true,
        unwrap_eth: false,
        n_tokens: BigInt::from(2),
        receiver: vec![0x33; 20],
        is_transfer_from_allowed: transfer_from,
        swaps: ple,
    }
    .encode();
    call_with_input(encoded)
}

#[test]
fn decodes_v2_split_swap() {
    let swap = decode_swap_call(RouterVersion::V2, &v2_split_call(true))
        .unwrap()
        .unwrap();
    assert_eq!(swap.method, Method::Split);
    assert_eq!(swap.funding, Funding::TransferFrom);
    assert!(swap.wrap_eth);
    assert!(!swap.unwrap_eth);
    assert_eq!(swap.n_tokens, Some(2));
    assert_eq!(swap.expected_amount_out, None);
    assert!(swap.client_fee.is_none());
    let hops = swaps::decode_hops(Method::Split, &swap.swaps).unwrap();
    assert_eq!(hops[0].executor, vec![0xab; 20]);
    assert_eq!(hops[0].token_out_index, Some(1));

    let no_transfer = decode_swap_call(RouterVersion::V2, &v2_split_call(false))
        .unwrap()
        .unwrap();
    assert_eq!(no_transfer.funding, Funding::None);
    assert!(decode_swap_call(RouterVersion::V3_1, &v2_split_call(true)).is_none());
}

#[test]
fn decodes_v3_0_vault_swap_without_expected_amount() {
    let encoded = v3_0::functions::SequentialSwapUsingVault {
        amount_in: BigInt::from(5),
        token_in: vec![0x11; 20],
        token_out: vec![0x22; 20],
        min_amount_out: BigInt::from(4),
        receiver: vec![0x33; 20],
        client_fee_params: (
            BigInt::from(30),
            vec![0x44; 20],
            BigInt::from(7),
            BigInt::from(99),
            vec![1; 65],
        ),
        swaps: Vec::new(),
    }
    .encode();
    let swap = decode_swap_call(RouterVersion::V3_0, &call_with_input(encoded))
        .unwrap()
        .unwrap();
    assert_eq!(swap.method, Method::Sequential);
    assert_eq!(swap.funding, Funding::Vault);
    assert_eq!(swap.expected_amount_out, None);
    let fee = swap.client_fee.unwrap();
    assert_eq!(fee.fee_bps, BigInt::from(30));
    assert_eq!(fee.receiver, vec![0x44; 20]);
    assert_eq!(fee.deadline, BigInt::from(99));
    assert_eq!(fee.signature.len(), 65);
}

#[test]
fn huge_n_tokens_and_deadline_do_not_panic() {
    let max = BigInt::from_unsigned_bytes_be(&[0xff; 32]);
    let encoded = v3_1::functions::SplitSwap {
        amount_in: BigInt::from(5),
        token_in: vec![0x11; 20],
        token_out: vec![0x22; 20],
        expected_amount_out: BigInt::from(5),
        min_amount_out: BigInt::from(4),
        n_tokens: max.clone(),
        receiver: vec![0x33; 20],
        client_fee_params: (max.clone(), vec![0x44; 20], max.clone(), max.clone(), Vec::new()),
        swaps: Vec::new(),
    }
    .encode();
    let swap = decode_swap_call(RouterVersion::V3_1, &call_with_input(encoded))
        .unwrap()
        .unwrap();
    assert_eq!(swap.n_tokens, Some(u32::MAX));
    assert_eq!(swap.client_fee.unwrap().deadline, max);
}

#[test]
fn decodes_amount_out_return_value() {
    let data = ethabi::encode(&[ethabi::Token::Uint(123_456u64.into())]);
    assert_eq!(decode_amount_out(&data).unwrap(), BigInt::from(123_456));
    assert!(decode_amount_out(&[]).is_err());
}
