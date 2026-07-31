use alloy::{
    primitives::{Address, Bytes, TxKind, U256},
    providers::Provider,
    rpc::types::{TransactionInput, TransactionRequest},
    sol_types::SolValue,
};
use tokio::runtime::Handle;

use crate::encoding::{
    errors::EncodingError,
    evm::{
        encoding_utils::encode_input,
        utils::{
            create_encoding_runtime, get_client, on_blocking_thread, EVMProvider, SafeRuntime,
        },
    },
};

/// A manager for checking if an approval is needed for interacting with a certain spender.
pub struct ProtocolApprovalsManager {
    client: EVMProvider,
    runtime_handle: Handle,
    #[allow(dead_code)]
    runtime: SafeRuntime,
}
impl ProtocolApprovalsManager {
    pub fn new() -> Result<Self, EncodingError> {
        let (handle, runtime) = create_encoding_runtime()?;
        let client = on_blocking_thread(|| handle.block_on(get_client()))??;
        Ok(Self { client, runtime_handle: handle, runtime })
    }

    /// Checks the current allowance for the given token, owner, and spender, and returns true
    /// if the current allowance is zero.
    pub fn approval_needed(
        &self,
        token: Address,
        owner_address: Address,
        spender_address: Address,
    ) -> Result<bool, EncodingError> {
        let args = (owner_address, spender_address);
        let data = encode_input("allowance(address,address)", args.abi_encode());
        let tx = TransactionRequest {
            to: Some(TxKind::from(token)),
            input: TransactionInput { input: Some(Bytes::from(data)), data: None },
            ..Default::default()
        };

        let output = on_blocking_thread(|| {
            self.runtime_handle
                .block_on(async { self.client.call(tx).await })
        })?;
        match output {
            Ok(response) => {
                let allowance: U256 = U256::abi_decode(&response).map_err(|_| {
                    EncodingError::FatalError("Failed to decode response for allowance".to_string())
                })?;

                if allowance < U256::MAX / U256::from(2) {
                    return Ok(true);
                }

                Ok(false)
            }
            Err(err) => Err(EncodingError::RecoverableError(format!(
                "Allowance call failed with error: {err}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;

    use super::*;
    #[rstest]
    #[case::approval_not_needed(
        "0xba12222222228d8ba445958a75a0704d566bf2c8",
        "0x2c6a3cd97c6283b95ac8c5a4459ebb0d5fd404f4",
        false
    )]
    #[case::approval_needed(
        "0x2c6a3cd97c6283b95ac8c5a4459ebb0d5fd404f4",
        "0xba12222222228d8ba445958a75a0704d566bf2c8",
        true
    )]
    fn test_approval_needed(#[case] spender: &str, #[case] owner: &str, #[case] expected: bool) {
        let manager = ProtocolApprovalsManager::new().unwrap();

        let token = Address::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let spender = Address::from_str(spender).unwrap();
        let owner = Address::from_str(owner).unwrap();

        let result = manager
            .approval_needed(token, owner, spender)
            .unwrap();
        assert_eq!(result, expected);
    }
}
