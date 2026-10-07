use alloy::{
    primitives::{B256, U256},
    sol_types::{eip712_domain, SolStruct},
};
use tycho_common::Bytes;

use crate::encoding::{
    errors::EncodingError,
    evm::{constants::UNISWAP_V2_FORKS, group_swaps::SwapGroup, utils::bytes_to_address},
    models::{SignedSubsidy, Subsidy},
};

mod typed {
    alloy::sol! {
        struct Subsidy {
            address executor;
            address tokenIn;
            uint256 subsidy;
            uint256 nonce;
            uint256 deadline;
        }
    }
}

/// Protocols whose executors take the subsidy: the router pays them before the swap, or they
/// pull from the router, and they price `amountIn` on chain.
const SUBSIDIZABLE_PROTOCOLS: &[&str] =
    &["aerodrome_v1", "vm:maverick_v2", "vm:liquidityparty", "vm:balancer_v2", "vm:curve"];

const MAX_DEADLINE: u64 = (1 << 48) - 1;
const SIGNATURE_LENGTH: usize = 65;

impl Subsidy {
    /// The EIP-712 hash the subsidy signer signs. `router` is the verifying contract.
    pub fn signing_hash(&self, chain_id: u64, router: &Bytes) -> Result<B256, EncodingError> {
        let domain = eip712_domain! {
            name: "TychoSubsidizingExecutor",
            version: "1",
            chain_id: chain_id,
            verifying_contract: bytes_to_address(router)?,
        };
        let typed = typed::Subsidy {
            executor: bytes_to_address(&self.executor)?,
            tokenIn: bytes_to_address(&self.token_in)?,
            subsidy: U256::from(self.amount),
            nonce: self.nonce_word()?,
            deadline: U256::from(self.deadline),
        };
        Ok(typed.eip712_signing_hash(&domain))
    }

    fn nonce_word(&self) -> Result<U256, EncodingError> {
        U256::try_from_be_slice(&self.nonce).ok_or_else(|| {
            EncodingError::InvalidInput(format!(
                "Subsidy nonce is {} bytes, at most 32 are allowed",
                self.nonce.len()
            ))
        })
    }
}

/// Wraps one swap group for the subsidizing executor. Returns the subsidizing executor and the
/// swap data it takes: the signed subsidy, the inner executor, then the inner swap data.
pub(crate) fn subsidize_swap_group(
    signed: &SignedSubsidy,
    group: &SwapGroup,
    inner_executor: &Bytes,
    inner_data: &[u8],
) -> Result<(Bytes, Vec<u8>), EncodingError> {
    let SignedSubsidy { subsidy, signature } = signed;
    let protocol = group.protocol_system.as_str();
    if !UNISWAP_V2_FORKS.contains(&protocol) && !SUBSIDIZABLE_PROTOCOLS.contains(&protocol) {
        return Err(EncodingError::InvalidInput(format!(
            "The subsidizing executor cannot add a subsidy to {protocol}"
        )));
    }
    if subsidy.token_in != group.token_in {
        return Err(EncodingError::InvalidInput(format!(
            "Subsidy signed for token {}, but the first swap group sells {}",
            subsidy.token_in, group.token_in
        )));
    }
    if subsidy.deadline > MAX_DEADLINE {
        return Err(EncodingError::InvalidInput(format!(
            "Subsidy deadline {} does not fit in 48 bits",
            subsidy.deadline
        )));
    }
    if signature.len() != SIGNATURE_LENGTH {
        return Err(EncodingError::InvalidInput(format!(
            "Subsidy signature is {} bytes, expected {SIGNATURE_LENGTH}",
            signature.len()
        )));
    }

    let mut data = Vec::new();
    data.extend_from_slice(&subsidy.amount.to_be_bytes());
    data.extend_from_slice(
        &subsidy
            .nonce_word()?
            .to_be_bytes::<32>(),
    );
    data.extend_from_slice(&subsidy.deadline.to_be_bytes()[2..]);
    data.extend_from_slice(signature);
    data.extend_from_slice(bytes_to_address(inner_executor)?.as_slice());
    data.extend_from_slice(inner_data);
    Ok((subsidy.executor.clone(), data))
}

#[cfg(test)]
mod tests {
    use alloy::hex::encode;
    use num_bigint::BigUint;
    use rstest::rstest;

    use super::*;

    const EXECUTOR: &str = "0x1111111111111111111111111111111111111111";
    const ROUTER: &str = "0x3333333333333333333333333333333333333333";
    const INNER_EXECUTOR: &str = "0x4444444444444444444444444444444444444444";
    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const DAI: &str = "0x6b175474e89094c44da98b954eedeac495271d0f";

    fn signed_subsidy() -> SignedSubsidy {
        SignedSubsidy {
            subsidy: Subsidy {
                executor: Bytes::from(EXECUTOR),
                token_in: Bytes::from(WETH),
                amount: 1_000_000_000_000_000_000,
                nonce: Bytes::from("0x07"),
                deadline: 1_700_000_000,
            },
            signature: Bytes::from(vec![0xab; 65]),
        }
    }

    fn group(protocol_system: &str) -> SwapGroup {
        SwapGroup {
            token_in: Bytes::from(WETH),
            token_out: Bytes::from(DAI),
            protocol_system: protocol_system.to_string(),
            swaps: vec![],
            split: 0.0,
            estimated_gas: BigUint::ZERO,
        }
    }

    fn subsidize(
        signed: &SignedSubsidy,
        protocol_system: &str,
    ) -> Result<(Bytes, Vec<u8>), EncodingError> {
        subsidize_swap_group(
            signed,
            &group(protocol_system),
            &Bytes::from(INNER_EXECUTOR),
            &[0xcd; 3],
        )
    }

    #[test]
    fn test_subsidy_eip712_type_matches_contract() {
        assert_eq!(
            typed::Subsidy::eip712_encode_type(),
            "Subsidy(address executor,address tokenIn,uint256 subsidy,uint256 nonce,\
             uint256 deadline)"
        );
    }

    /// Expected hash computed with `cast` from the contract's domain and struct hash formula.
    #[test]
    fn test_subsidy_signing_hash() {
        let hash = signed_subsidy()
            .subsidy
            .signing_hash(1, &Bytes::from(ROUTER))
            .unwrap();

        assert_eq!(
            encode(hash),
            "2150916b86fec801cdded9feb75e07bfaa9bc8fc6eafdb513b60c19dfbbc2195"
        );
    }

    #[test]
    fn test_subsidize_swap_group() {
        let (executor, data) = subsidize(&signed_subsidy(), "uniswap_v2").unwrap();

        assert_eq!(executor, Bytes::from(EXECUTOR));
        assert_eq!(
            encode(data),
            [
                "00000000000000000de0b6b3a7640000",
                "0000000000000000000000000000000000000000000000000000000000000007",
                "00006553f100",
                &"ab".repeat(65),
                "4444444444444444444444444444444444444444",
                "cdcdcd",
            ]
            .concat()
        );
    }

    #[rstest]
    #[case::callback_protocol(|_: &mut SignedSubsidy| {}, "uniswap_v3", "cannot add a subsidy to")]
    #[case::other_token(|s: &mut SignedSubsidy| s.subsidy.token_in = Bytes::from(DAI), "uniswap_v2", "signed for token")]
    #[case::deadline_over_48_bits(|s: &mut SignedSubsidy| s.subsidy.deadline = 1 << 48, "uniswap_v2", "48 bits")]
    #[case::short_signature(|s: &mut SignedSubsidy| s.signature = Bytes::from(vec![0xab; 64]), "uniswap_v2", "expected 65")]
    #[case::nonce_over_32_bytes(|s: &mut SignedSubsidy| s.subsidy.nonce = Bytes::from(vec![1; 33]), "uniswap_v2", "at most 32")]
    fn test_subsidize_rejects_invalid_subsidy(
        #[case] change: fn(&mut SignedSubsidy),
        #[case] protocol_system: &str,
        #[case] message: &str,
    ) {
        let mut signed = signed_subsidy();
        change(&mut signed);

        let Err(EncodingError::InvalidInput(error)) = subsidize(&signed, protocol_system) else {
            panic!("expected InvalidInput");
        };
        assert!(error.contains(message), "{error}");
    }
}
