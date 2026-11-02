mod common;

use std::{collections::HashMap, fs, str::FromStr};

use alloy::{
    hex::encode,
    primitives::B256,
    signers::{local::PrivateKeySigner, SignerSync},
};
use num_bigint::{BigInt, BigUint};
use tycho_common::{
    models::{protocol::ProtocolComponent, Chain},
    Bytes,
};
use tycho_execution::encoding::{
    evm::{
        encoder_builders::TychoRouterEncoderBuilder,
        swap_encoder::swap_encoder_registry::SwapEncoderRegistry, utils::write_calldata_to_file,
    },
    models::{default_token, Solution, SubsidyTerms, Swap, UserTransferType},
    tycho_encoder::TychoEncoder,
};

use crate::common::{
    alice_address, client_fee_receiver, dai, encoding::encode_tycho_router_call, eth, eth_chain,
    get_signer, router_address, usdc, wbtc, weth,
};

// Must match the constants in contracts/test/protocols/TychoSubsidizingEncoder.t.sol.
const SUBSIDIZING_EXECUTOR: &str = "0xE06C68f4c151619a8F32d475e91349B3D820343a";
const SUBSIDY_PAYER: &str = "0xB45394E79c96A1eF3DF90ebc93B31E87aeB1AAdA";
const SUBSIDY_SIGNER_PK: &str =
    "0xd05309bfe92ede9638264d4537fba0c605f619ff6923f5118f44c035b708163a";
const SUBSIDY_NONCE: u8 = 42;
const SUBSIDY_DEADLINE: u64 = 4_102_444_800;
const DAI_SUBSIDY: u128 = 5_000_000_000_000_000_000;
const USDC_SUBSIDY: u128 = 5_000_000;
const WETH_SUBSIDY: u128 = 2_000_000_000_000_000;
const ETH_SUBSIDY: u128 = 2_000_000_000_000_000;

fn subsidy_encoder() -> Box<dyn TychoEncoder> {
    let executors = fs::read_to_string("config/test_executor_addresses.json").unwrap();
    let registry = SwapEncoderRegistry::new(Chain::Ethereum)
        .add_default_encoders(Some(executors))
        .unwrap()
        .register_subsidy_contracts(
            Bytes::from_str(SUBSIDIZING_EXECUTOR).unwrap(),
            Bytes::from_str(SUBSIDY_PAYER).unwrap(),
        );
    TychoRouterEncoderBuilder::new()
        .chain(Chain::Ethereum)
        .swap_encoder_registry(registry)
        .router_address(router_address())
        .build()
        .unwrap()
}

fn terms(amount: u128, subsidized_swap_index: usize) -> SubsidyTerms {
    SubsidyTerms {
        amount: BigUint::from(amount),
        min_amount_in: BigUint::from(1u32),
        nonce: B256::with_last_byte(SUBSIDY_NONCE),
        deadline: SUBSIDY_DEADLINE,
        subsidized_swap_index,
    }
}

/// A solution from ALICE whose quote includes the subsidy and whose minimum excludes it.
fn subsidized_solution(
    token_in: Bytes,
    token_out: Bytes,
    amount_in: BigUint,
    swaps: Vec<Swap>,
    terms: SubsidyTerms,
) -> Solution {
    Solution::new(
        alice_address(),
        alice_address(),
        token_in,
        token_out,
        amount_in,
        &terms.amount + 1u32,
        BigUint::from(1u32),
        swaps,
    )
    .with_subsidy(terms)
}

/// Writes the unsigned calldata as `{test_identifier}_unsigned` and the signed calldata as
/// `test_identifier`.
fn write_signed_calldata(test_identifier: &str, solution: Solution, client_fee_bps: u32) {
    let mut encoded_solution = subsidy_encoder()
        .encode_solutions(vec![solution.clone()])
        .unwrap()
        .remove(0);
    let client_fee_receiver =
        if client_fee_bps == 0 { Bytes::zero(20) } else { client_fee_receiver() };
    let router_calldata = |encoded_solution| {
        encode_tycho_router_call(
            eth_chain().id(),
            encoded_solution,
            &solution,
            &eth(),
            Some(get_signer()),
            client_fee_bps,
            client_fee_receiver.clone(),
            BigUint::ZERO,
        )
        .unwrap()
        .data
    };
    let unsigned_calldata = router_calldata(encoded_solution.clone());
    write_calldata_to_file(&format!("{test_identifier}_unsigned"), &encode(&unsigned_calldata));

    let signing_hash = encoded_solution
        .subsidy_signing_hash()
        .unwrap();
    let signature = PrivateKeySigner::from_str(SUBSIDY_SIGNER_PK)
        .unwrap()
        .sign_hash_sync(&signing_hash)
        .unwrap()
        .as_bytes();
    encoded_solution
        .write_subsidy_signature(&signature)
        .unwrap();
    write_calldata_to_file(test_identifier, &encode(router_calldata(encoded_solution)));
}

fn swap(component: ProtocolComponent, token_in: &Bytes, token_out: &Bytes) -> Swap {
    Swap::new(
        component,
        default_token(token_in.clone()),
        default_token(token_out.clone()),
        BigUint::ZERO,
    )
}

fn usv2(pool: &str, token_in: &Bytes, token_out: &Bytes) -> Swap {
    swap(
        ProtocolComponent {
            id: pool.to_string(),
            protocol_system: "uniswap_v2".to_string(),
            ..Default::default()
        },
        token_in,
        token_out,
    )
}

fn usv3(pool: &str, fee: i32, token_in: &Bytes, token_out: &Bytes) -> Swap {
    swap(
        ProtocolComponent {
            id: pool.to_string(),
            protocol_system: "uniswap_v3".to_string(),
            static_attributes: HashMap::from([(
                "fee".to_string(),
                Bytes::from(BigInt::from(fee).to_signed_bytes_be()),
            )]),
            ..Default::default()
        },
        token_in,
        token_out,
    )
}

const DAI_WETH_USV2: &str = "0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11";
const DAI_USDC_USV2: &str = "0xAE461cA67B15dc8dc81CE7615e0320dA1A9aB8D5";
const WETH_WBTC_USV2: &str = "0xBb2b8038a1640196FbE3e38816F3e67Cba72D940";
const USDC_WBTC_USV2: &str = "0x004375Dff511095CC5A197A54140a24eFEF3A416";
const DAI_WETH_USV3: &str = "0xC2e9F25Be6257c210d7Adf0D4Cd6E3E881ba25f8";
const USDC_WETH_USV3: &str = "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640";

fn one_weth() -> BigUint {
    BigUint::from(1_000_000_000_000_000_000u64)
}

#[test]
fn test_subsidized_uniswap_v2_single_swap() {
    // WETH ──(USV2, subsidized)──> DAI
    let solution = subsidized_solution(
        weth(),
        dai(),
        one_weth(),
        vec![usv2(DAI_WETH_USV2, &weth(), &dai())],
        terms(DAI_SUBSIDY, 0),
    );

    write_signed_calldata("test_subsidized_uniswap_v2_single_swap", solution, 0);
}

#[test]
fn test_subsidized_sequential_swap_ending_in_uniswap_v3() {
    // DAI ──(USV2)──> WETH ──(USV3, subsidized)──> USDC
    let solution = subsidized_solution(
        dai(),
        usdc(),
        BigUint::from(3_000u32) * one_weth(),
        vec![usv2(DAI_WETH_USV2, &dai(), &weth()), usv3(USDC_WETH_USV3, 500, &weth(), &usdc())],
        terms(USDC_SUBSIDY, 1),
    );

    write_signed_calldata("test_subsidized_sequential_swap_ending_in_uniswap_v3", solution, 0);
}

#[test]
fn test_subsidized_split_swap_larger_share() {
    //       ┌──[70%, subsidized]── (USV2) ──┐
    // WETH ─┤                               ├─> DAI
    //       └──[rest]───────────── (USV3) ──┘
    let solution = subsidized_solution(
        weth(),
        dai(),
        one_weth(),
        vec![
            usv2(DAI_WETH_USV2, &weth(), &dai()).with_split(0.7),
            usv3(DAI_WETH_USV3, 3000, &weth(), &dai()),
        ],
        terms(DAI_SUBSIDY, 0),
    );

    write_signed_calldata("test_subsidized_split_swap_larger_share", solution, 0);
}

#[test]
fn test_subsidized_split_swap_after_intermediate() {
    //       ┌──[60%]── (USV2) ──> WBTC ──(USV2)──────────────┐
    // WETH ─┤                                                ├─> USDC
    //       └──[rest]─ (USV2) ──> DAI ───(USV2, subsidized)──┘
    let solution = subsidized_solution(
        weth(),
        usdc(),
        one_weth(),
        vec![
            usv2(WETH_WBTC_USV2, &weth(), &wbtc()).with_split(0.6),
            usv2(USDC_WBTC_USV2, &wbtc(), &usdc()),
            usv2(DAI_WETH_USV2, &weth(), &dai()),
            usv2(DAI_USDC_USV2, &dai(), &usdc()),
        ],
        terms(USDC_SUBSIDY, 3),
    );

    write_signed_calldata("test_subsidized_split_swap_after_intermediate", solution, 0);
}

fn usv4_usdc_eth() -> Swap {
    swap(
        ProtocolComponent {
            id: "0xdce6394339af00981949f5f3baf27e3610c76326a700af57e4b3e3ae4977f78d".to_string(),
            protocol_system: "uniswap_v4".to_string(),
            static_attributes: HashMap::from([
                ("key_lp_fee".to_string(), Bytes::from(BigInt::from(3000).to_signed_bytes_be())),
                ("tick_spacing".to_string(), Bytes::from(BigInt::from(60).to_signed_bytes_be())),
            ]),
            ..Default::default()
        },
        &usdc(),
        &eth(),
    )
}

fn native_wrap(token_in: &Bytes, token_out: &Bytes) -> Swap {
    swap(
        ProtocolComponent { protocol_system: "native_wrapper".to_string(), ..Default::default() },
        token_in,
        token_out,
    )
}

#[test]
fn test_subsidized_uniswap_v4_eth_payout() {
    // USDC ──(USV4, subsidized)──> ETH
    let solution = subsidized_solution(
        usdc(),
        eth(),
        BigUint::from(3_000_000_000u64),
        vec![usv4_usdc_eth()],
        terms(ETH_SUBSIDY, 0),
    );

    write_signed_calldata("test_subsidized_uniswap_v4_eth_payout", solution, 0);
}

#[test]
fn test_subsidized_pool_swap_before_unwrap() {
    // DAI ──(USV2, subsidized)──> WETH ──(unwrap)──> ETH
    let solution = subsidized_solution(
        dai(),
        eth(),
        BigUint::from(3_000u32) * one_weth(),
        vec![usv2(DAI_WETH_USV2, &dai(), &weth()), native_wrap(&weth(), &eth())],
        terms(WETH_SUBSIDY, 0),
    );

    write_signed_calldata("test_subsidized_pool_swap_before_unwrap", solution, 0);
}

#[test]
fn test_subsidized_pool_swap_before_wrap() {
    // USDC ──(USV4, subsidized)──> ETH ──(wrap)──> WETH
    let solution = subsidized_solution(
        usdc(),
        weth(),
        BigUint::from(3_000_000_000u64),
        vec![usv4_usdc_eth(), native_wrap(&eth(), &weth())],
        terms(ETH_SUBSIDY, 0),
    );

    write_signed_calldata("test_subsidized_pool_swap_before_wrap", solution, 0);
}

#[test]
fn test_subsidized_vault_credit_swap() {
    // WETH ──(USV2, subsidized)──> DAI, credited to ALICE's vault balance
    let solution = Solution::new(
        alice_address(),
        router_address(),
        weth(),
        dai(),
        one_weth(),
        BigUint::from(DAI_SUBSIDY) + 1u32,
        BigUint::from(1u32),
        vec![usv2(DAI_WETH_USV2, &weth(), &dai())],
    )
    .with_subsidy(terms(DAI_SUBSIDY, 0));

    write_signed_calldata("test_subsidized_vault_credit_swap", solution, 0);
}

#[test]
fn test_evm_subsidized_swap_with_client_fee_and_permit2() {
    // WETH ──(USV2, subsidized)──> DAI, paid with Permit2, with a 1% client fee
    let solution = subsidized_solution(
        weth(),
        dai(),
        one_weth(),
        vec![usv2(DAI_WETH_USV2, &weth(), &dai())],
        terms(DAI_SUBSIDY, 0),
    )
    .with_user_transfer_type(UserTransferType::TransferFromPermit2);

    write_signed_calldata("test_subsidized_swap_with_client_fee_and_permit2", solution, 1_000_000);
}
