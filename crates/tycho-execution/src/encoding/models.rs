use std::sync::Arc;

#[cfg(feature = "evm")]
use alloy::primitives::{Address, U256};
use clap::ValueEnum;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_common::{
    models::{protocol::ProtocolComponent, token::Token},
    simulation::protocol_sim::ProtocolSim,
    Bytes,
};

use crate::encoding::serde_primitives::biguint_string;

/// Specifies the method for transferring user funds into Tycho execution.
///
/// Options:
///
/// - `TransferFromPermit2`: Use Permit2 for token transfer.
///     - You must manually approve the Permit2 contract and sign the permit object externally
///       (outside `tycho-execution`).
///
/// - `TransferFrom`: Use standard ERC-20 approval and `transferFrom`.
///     - You must approve the Tycho Router contract to spend your tokens via standard `approve()`
///       calls.
///
/// - `UseVaultsFunds`: No transfer will be performed and the Vault's funds will be used
///     - Assumes the tokens are already present in the Tycho Router.
///     - The tokens must be deposited into the TychoRouter before performing the swap
#[derive(Clone, Debug, PartialEq, ValueEnum, Serialize, Deserialize, Default)]
pub enum UserTransferType {
    TransferFromPermit2,
    #[default]
    TransferFrom,
    UseVaultsFunds,
}

/// Client fee parameters passed to the router, matching the Solidity `ClientFeeParams` struct.
///
/// The default value (all zeros) represents no fee. Clients are responsible for constructing
/// and signing this struct; `tycho-execution` does not use it internally.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ClientFeeParams {
    /// Fee in basis points charged by the client (0–10000).
    client_fee_bps: u16,
    /// Address that identifies the client and receives any client fee.
    client_fee_receiver: Bytes,
    /// Maximum amount the client will contribute from their vault if slippage reduces the output
    /// below `min_amount_out`.
    #[serde(with = "biguint_string")]
    max_client_contribution: BigUint,
    /// Deadline for the fee signature as a unix timestamp.
    #[serde(with = "biguint_string")]
    deadline: BigUint,
    /// EIP-712 signature over the fee parameters and swap intent.
    client_signature: Bytes,
}

impl ClientFeeParams {
    /// Creates params that identify the client and charge a fee in basis points.
    pub fn new(
        client_fee_receiver: Bytes,
        client_signature: Bytes,
        deadline: BigUint,
        client_fee_bps: u16,
    ) -> Self {
        Self {
            client_fee_bps,
            client_fee_receiver,
            max_client_contribution: BigUint::ZERO,
            deadline,
            client_signature,
        }
    }

    /// Creates params that identify the client for router fee discounts without charging a fee.
    pub fn new_without_fee(
        client_fee_receiver: Bytes,
        client_signature: Bytes,
        deadline: BigUint,
    ) -> Self {
        Self {
            client_fee_bps: 0,
            client_fee_receiver,
            max_client_contribution: BigUint::ZERO,
            deadline,
            client_signature,
        }
    }

    pub fn with_max_client_contribution(mut self, max_client_contribution: BigUint) -> Self {
        self.max_client_contribution = max_client_contribution;
        self
    }
}

#[cfg(feature = "evm")]
impl ClientFeeParams {
    /// Converts into the ABI-encodable tuple matching the Solidity `ClientFeeParams` struct.
    pub fn into_abi_params(self) -> (u16, Address, U256, U256, Vec<u8>) {
        let receiver = if self.client_fee_receiver.is_empty() {
            Address::ZERO
        } else {
            Address::from_slice(&self.client_fee_receiver)
        };
        (
            self.client_fee_bps,
            receiver,
            U256::from_be_slice(
                &self
                    .max_client_contribution
                    .to_bytes_be(),
            ),
            U256::from_be_slice(&self.deadline.to_bytes_be()),
            self.client_signature.to_vec(),
        )
    }
}

/// Represents a solution containing details describing an order, and instructions for filling
/// the order.
#[derive(Clone, Default, Debug, Deserialize, Serialize)]
pub struct Solution {
    /// Address of the sender.
    sender: Bytes,
    /// Address of the receiver.
    receiver: Bytes,
    /// The token being sold
    token_in: Bytes,
    /// Amount of the token in.
    #[serde(with = "biguint_string")]
    amount_in: BigUint,
    /// The token being bought
    token_out: Bytes,
    /// Minimum amount that the receiver must receive at the end of the transaction.
    #[serde(with = "biguint_string")]
    min_amount_out: BigUint,
    /// List of swaps to fulfill the solution.
    swaps: Vec<Swap>,
    /// The transfer type to be used in this swap for user's funds (token in)
    user_transfer_type: UserTransferType,
}

impl Solution {
    pub fn new(
        sender: Bytes,
        receiver: Bytes,
        token_in: Bytes,
        token_out: Bytes,
        amount_in: BigUint,
        min_amount_out: BigUint,
        swaps: Vec<Swap>,
    ) -> Self {
        Self {
            sender,
            receiver,
            token_in,
            token_out,
            amount_in,
            min_amount_out,
            swaps,
            user_transfer_type: UserTransferType::TransferFrom,
        }
    }
    pub fn sender(&self) -> &Bytes {
        &self.sender
    }
    pub fn receiver(&self) -> &Bytes {
        &self.receiver
    }

    pub fn token_in(&self) -> &Bytes {
        &self.token_in
    }

    pub fn amount_in(&self) -> &BigUint {
        &self.amount_in
    }

    pub fn token_out(&self) -> &Bytes {
        &self.token_out
    }

    pub fn min_amount_out(&self) -> &BigUint {
        &self.min_amount_out
    }

    pub fn swaps(&self) -> &[Swap] {
        &self.swaps
    }

    pub fn user_transfer_type(&self) -> &UserTransferType {
        &self.user_transfer_type
    }

    pub fn with_sender(mut self, sender: Bytes) -> Self {
        self.sender = sender;
        self
    }

    pub fn with_receiver(mut self, receiver: Bytes) -> Self {
        self.receiver = receiver;
        self
    }

    pub fn with_token_in(mut self, token_in: Bytes) -> Self {
        self.token_in = token_in;
        self
    }

    pub fn with_amount_in(mut self, amount_in: BigUint) -> Self {
        self.amount_in = amount_in;
        self
    }

    pub fn with_token_out(mut self, token_out: Bytes) -> Self {
        self.token_out = token_out;
        self
    }

    pub fn with_min_amount_out(mut self, min_amount_out: BigUint) -> Self {
        self.min_amount_out = min_amount_out;
        self
    }

    pub fn with_swaps(mut self, swaps: Vec<Swap>) -> Self {
        self.swaps = swaps;
        self
    }

    pub fn with_user_transfer_type(mut self, user_transfer_type: UserTransferType) -> Self {
        self.user_transfer_type = user_transfer_type;
        self
    }
}

/// Represents a swap operation to be performed on a pool.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Swap {
    /// Protocol component from tycho indexer
    component: ProtocolComponent,
    /// Token being input into the pool.
    token_in: Token,
    /// Token being output from the pool.
    token_out: Token,
    /// Decimal of the amount to be swapped in this operation (for example, 0.5 means 50%)
    #[serde(default)]
    split: f64,
    /// Optional user data to be passed to encoding.
    user_data: Option<Bytes>,
    /// Optional protocol state used to perform the swap.
    #[serde(skip)]
    protocol_state: Option<Arc<dyn ProtocolSim>>,
    /// Optional estimated amount in for this Swap. This is necessary for RFQ protocols. This value
    /// is used to request the quote
    estimated_amount_in: Option<BigUint>,
    /// Estimated gas usage for this swap by simulation
    estimated_gas: BigUint,
}

impl Swap {
    pub fn new<T: Into<ProtocolComponent>>(
        component: T,
        token_in: Token,
        token_out: Token,
        estimated_gas: BigUint,
    ) -> Self {
        Self {
            component: component.into(),
            token_in,
            token_out,
            split: 0.0,
            user_data: None,
            protocol_state: None,
            estimated_amount_in: None,
            estimated_gas,
        }
    }

    /// Sets the split value (percentage of the amount to be swapped)
    pub fn with_split(mut self, split: f64) -> Self {
        self.split = split;
        self
    }

    /// Sets the user data to be passed to encoding
    pub fn with_user_data(mut self, user_data: Bytes) -> Self {
        self.user_data = Some(user_data);
        self
    }

    /// Sets the protocol state used to perform the swap
    pub fn with_protocol_state(mut self, protocol_state: Arc<dyn ProtocolSim>) -> Self {
        self.protocol_state = Some(protocol_state);
        self
    }

    /// Sets the estimated amount in for RFQ protocols
    pub fn with_estimated_amount_in(mut self, estimated_amount_in: BigUint) -> Self {
        self.estimated_amount_in = Some(estimated_amount_in);
        self
    }

    pub fn component(&self) -> &ProtocolComponent {
        &self.component
    }

    pub fn token_in(&self) -> &Token {
        &self.token_in
    }

    pub fn token_out(&self) -> &Token {
        &self.token_out
    }

    pub fn split(&self) -> f64 {
        self.split
    }

    pub fn user_data(&self) -> &Option<Bytes> {
        &self.user_data
    }

    pub fn protocol_state(&self) -> &Option<Arc<dyn ProtocolSim>> {
        &self.protocol_state
    }

    pub fn estimated_amount_in(&self) -> &Option<BigUint> {
        &self.estimated_amount_in
    }

    pub fn estimated_gas(&self) -> &BigUint {
        &self.estimated_gas
    }
}

impl PartialEq for Swap {
    fn eq(&self, other: &Self) -> bool {
        self.component() == other.component() &&
            self.token_in().address == other.token_in().address &&
            self.token_out().address == other.token_out().address &&
            self.split() == other.split() &&
            self.user_data() == other.user_data() &&
            self.estimated_amount_in() == other.estimated_amount_in() &&
            self.estimated_gas() == other.estimated_gas()
    }
}

/// Represents a solution that has been encoded for execution.
///
/// # Fields
/// * `swaps`: Encoded swaps to be executed.
/// * `interacting_with`: Address of the contract to be called.
/// * `function_signature`: The signature of the function to be called.
/// * `n_tokens`: Number of tokens in the swap.
/// * `estimated_gas`: Estimated gas usage for the encoded solution
#[derive(Clone, Debug)]
pub struct EncodedSolution {
    /// Encoded swaps to be executed.
    swaps: Vec<u8>,
    /// Address of the contract to be called.
    interacting_with: Bytes,
    /// The signature of the function to be called.
    function_signature: String,
    /// Number of tokens in the swap.
    n_tokens: usize,
    /// Estimated gas usage for this solution
    estimated_gas: BigUint,
}

impl EncodedSolution {
    pub(crate) fn new(
        swaps: Vec<u8>,
        interacting_with: Bytes,
        function_signature: String,
        n_tokens: usize,
        estimated_gas: BigUint,
    ) -> Self {
        Self { swaps, interacting_with, function_signature, n_tokens, estimated_gas }
    }

    pub fn swaps(&self) -> &[u8] {
        &self.swaps
    }

    pub fn interacting_with(&self) -> &Bytes {
        &self.interacting_with
    }

    pub fn function_signature(&self) -> &str {
        &self.function_signature
    }

    pub fn n_tokens(&self) -> usize {
        self.n_tokens
    }

    pub fn estimated_gas(&self) -> &BigUint {
        &self.estimated_gas
    }

    /// Byte offset within TychoRouter calldata where the client fee signature starts.
    pub fn client_fee_signature_offset(&self) -> usize {
        let name = self
            .function_signature
            .split('(')
            .next()
            .unwrap_or("");
        let head_params = match name {
            "singleSwap" |
            "singleSwapUsingVault" |
            "sequentialSwap" |
            "sequentialSwapUsingVault" => 7,
            "splitSwap" | "splitSwapUsingVault" => 8,
            "singleSwapPermit2" | "sequentialSwapPermit2" => 14,
            "splitSwapPermit2" => 15,
            _ => 0,
        };
        // selector (4) + ABI head + offset to signature data within ClientFeeParams tuple
        4 + head_params * 32 + 192
    }
}

/// Represents a single permit for permit2.
///
/// # Fields
/// * `details`: The details of the permit, such as token, amount, expiration, and nonce.
/// * `spender`: The address authorized to spend the tokens.
/// * `sig_deadline`: The deadline (as a timestamp) for the permit signature
#[derive(Debug, Clone)]
pub struct PermitSingle {
    details: PermitDetails,
    spender: Bytes,
    sig_deadline: BigUint,
}

impl PermitSingle {
    pub fn new(details: PermitDetails, spender: Bytes, sig_deadline: BigUint) -> Self {
        Self { details, spender, sig_deadline }
    }

    pub fn details(&self) -> &PermitDetails {
        &self.details
    }

    pub fn spender(&self) -> &Bytes {
        &self.spender
    }

    pub fn sig_deadline(&self) -> &BigUint {
        &self.sig_deadline
    }
}

/// Details of a permit.
///
/// # Fields
/// * `token`: The token address for which the permit is granted.
/// * `amount`: The amount of tokens approved for spending.
/// * `expiration`: The expiration time (as a timestamp) for the permit.
/// * `nonce`: The unique nonce to prevent replay attacks.
#[derive(Debug, Clone)]
pub struct PermitDetails {
    token: Bytes,
    amount: BigUint,
    expiration: BigUint,
    nonce: BigUint,
}

impl PermitDetails {
    pub fn new(token: Bytes, amount: BigUint, expiration: BigUint, nonce: BigUint) -> Self {
        Self { token, amount, expiration, nonce }
    }

    pub fn token(&self) -> &Bytes {
        &self.token
    }

    pub fn amount(&self) -> &BigUint {
        &self.amount
    }

    pub fn expiration(&self) -> &BigUint {
        &self.expiration
    }

    pub fn nonce(&self) -> &BigUint {
        &self.nonce
    }
}

impl PartialEq for PermitSingle {
    fn eq(&self, other: &Self) -> bool {
        self.details == other.details && self.spender == other.spender
        // sig_deadline is intentionally ignored
    }
}

impl PartialEq for PermitDetails {
    fn eq(&self, other: &Self) -> bool {
        self.token == other.token && self.amount == other.amount && self.nonce == other.nonce
        // expiration is intentionally ignored
    }
}

/// Necessary context for encoding a swap within a strategy.
///
/// # Fields
///
/// * `router_address`: Address of the router contract to be used for the swaps. Zero address if
///   solution does not require router address.
/// * `group_token_in`: Token to be used as the input for the group swap.
/// * `group_token_out`: Token to be used as the output for the group swap.
#[derive(Clone, Debug)]
pub struct EncodingContext {
    pub router_address: Option<Bytes>,
    pub group_token_in: Bytes,
    pub group_token_out: Bytes,
}

#[derive(PartialEq)]
pub enum Strategy {
    Single,
    Sequential,
    Split,
}

/// Creates a minimal `Token` from just an address, with zero-value defaults for other fields.
/// Only available in tests and when the `test-utils` feature is enabled.
#[cfg(any(test, feature = "test-utils"))]
pub fn default_token(address: Bytes) -> Token {
    Token::new(&address, "", 0, 0, &[Some(60_000u64)], Default::default(), 100)
}

mod tests {
    use super::*;

    struct MockProtocolComponent {
        id: String,
        protocol_system: String,
    }

    impl From<MockProtocolComponent> for ProtocolComponent {
        fn from(component: MockProtocolComponent) -> Self {
            ProtocolComponent {
                id: component.id,
                protocol_system: component.protocol_system,
                tokens: vec![],
                protocol_type_name: "".to_string(),
                chain: Default::default(),
                contract_addresses: vec![],
                static_attributes: Default::default(),
                change: Default::default(),
                creation_tx: Default::default(),
                created_at: Default::default(),
            }
        }
    }

    #[test]
    fn test_swap_new() {
        let component = MockProtocolComponent {
            id: "i-am-an-id".to_string(),
            protocol_system: "uniswap_v2".to_string(),
        };
        let user_data = Bytes::from("0x1234");
        let swap = Swap::new(
            component,
            default_token(Bytes::from("0x12")),
            default_token(Bytes::from("0x34")),
            BigUint::ZERO,
        )
        .with_split(0.5)
        .with_user_data(user_data.clone());

        assert_eq!(swap.token_in().address, Bytes::from("0x12"));
        assert_eq!(swap.token_out().address, Bytes::from("0x34"));
        assert_eq!(swap.component().protocol_system, "uniswap_v2");
        assert_eq!(swap.component().id, "i-am-an-id");
        assert_eq!(swap.split(), 0.5);
        assert_eq!(swap.user_data(), &Some(user_data));
    }
}
