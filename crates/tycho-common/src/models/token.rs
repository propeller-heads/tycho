use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::Arc,
};

use deepsize::DeepSizeOf;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};

use super::{Address, Balance};
use crate::{dto::ResponseToken, models::Chain, traits::TokenOwnerFinding, Bytes};

/// Cost related to a token transfer, for example amount of gas in evm chains.
pub type TransferCost = u64;

/// Tax related to a token transfer. Should be given in Basis Points (1/100th of a percent)
pub type TransferTax = u64;

#[derive(Debug, Clone, Deserialize, Serialize, Eq, DeepSizeOf)]
pub struct Token {
    pub address: Bytes,
    pub symbol: String,
    pub decimals: u32,
    pub tax: TransferTax,
    pub gas: Vec<Option<TransferCost>>,
    pub chain: Chain,
    /// Quality is between 0-100, where:
    ///  - 100: Normal token
    ///  - 75: Rebase token
    ///  - 50: Fee token
    ///  - 10: Blocked, temporarily blocked or without liquidity, or the analysis failed
    ///  - 9-5: Still blocked or without liquidity on later analyses
    ///  - 1: Must never be quoted (a honeypot or a watch-listed call)
    ///  - 0: Failed to extract decimals onchain
    pub quality: u32,
}

impl Token {
    pub fn new(
        address: &Bytes,
        symbol: &str,
        decimals: u32,
        tax: u64,
        gas: &[Option<u64>],
        chain: Chain,
        quality: u32,
    ) -> Self {
        Self {
            address: address.clone(),
            symbol: symbol.to_string(),
            decimals,
            tax,
            gas: gas.to_owned(),
            chain,
            quality,
        }
    }

    /// Sets quality, tax and gas from `analysis`.
    pub fn apply_analysis(&mut self, analysis: &TokenAnalysis) {
        self.quality = analysis.verdict.quality();
        self.tax = analysis.fees.max_tax();
        self.gas = analysis
            .gas
            .map(|g| vec![Some(g)])
            .unwrap_or_default();
    }

    /// One
    /// Get one token in BigUint format
    ///
    /// ## Return
    /// Returns one token as BigUint
    pub fn one(&self) -> BigUint {
        BigUint::from((1.0 * 10f64.powi(self.decimals as i32)) as u128)
    }

    pub fn gas_usage(&self) -> BigUint {
        BigUint::from(
            self.gas
                .clone()
                .into_iter()
                .flatten()
                .collect::<Vec<u64>>()
                .iter()
                .min()
                .copied()
                .unwrap_or(0u64),
        )
    }
}

impl PartialOrd for Token {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.address.partial_cmp(&other.address)
    }
}

impl PartialEq for Token {
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address
    }
}

impl Hash for Token {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.address.hash(state);
    }
}

impl From<Arc<Token>> for Address {
    fn from(val: Arc<Token>) -> Self {
        val.address.clone()
    }
}

impl From<ResponseToken> for Token {
    fn from(value: ResponseToken) -> Self {
        Self {
            chain: value.chain.into(),
            address: value.address,
            symbol: value.symbol,
            decimals: value.decimals,
            tax: value.tax,
            gas: value.gas,
            quality: value.quality,
        }
    }
}

/// Represents the quality of a token.
///
/// * `Good`: Indicates that the token has successfully passed the analysis process.
/// * `Bad`: Indicates that the token has failed the analysis process. In this case, a detailed
///   reason for the failure is provided.
///
/// Note: Transfer taxes do not impact the token's quality.
/// Even if a token has transfer taxes, as long as it successfully passes the analysis,
/// it will still be marked as `Good`.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TokenQuality {
    Good,
    Bad { reason: String },
}

impl TokenQuality {
    pub fn is_good(&self) -> bool {
        matches!(self, Self::Good { .. })
    }

    pub fn bad(reason: impl ToString) -> Self {
        Self::Bad { reason: reason.to_string() }
    }
}

/// Whether a token can be quoted, for a router that buys, forwards the output to the receiver and
/// lets the receiver sell, all within one transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, DeepSizeOf)]
#[serde(rename_all = "snake_case")]
pub enum TokenVerdict {
    /// Every leg succeeds without a fee.
    Ok,
    /// Every leg succeeds, with a fee on at least one leg.
    Fee,
    /// A leg fails, and a simulated owner action or the passage of time makes all legs succeed.
    TemporarilyBlocked,
    /// A leg fails and nothing tried lifts it.
    Blocked,
    /// Must never be quoted: buys and transfers work but sells fail or deliver nothing to the
    /// pool (a honeypot), or a transfer calls a watch-listed contract.
    NoQuote,
    /// The pool holds too little of the token to simulate a trade; analyse again later.
    NoLiquidity,
}

impl TokenVerdict {
    /// The `Token::quality` this verdict maps to.
    pub fn quality(self) -> u32 {
        match self {
            TokenVerdict::Ok => 100,
            TokenVerdict::Fee => 50,
            TokenVerdict::TemporarilyBlocked |
            TokenVerdict::Blocked |
            TokenVerdict::NoLiquidity => 10,
            TokenVerdict::NoQuote => 1,
        }
    }
}

/// Fee per leg in basis points of the amount sent; `None` when the leg did not run or reverted.
/// Negative when more arrived than was sent (rebasing or reflection).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, DeepSizeOf)]
pub struct LegFees {
    /// Pool to trader.
    pub buy: Option<i64>,
    /// Trader to an unrelated wallet.
    pub transfer: Option<i64>,
    /// Trader to pool.
    pub sell: Option<i64>,
}

impl LegFees {
    /// The highest fee of any leg, clamped at zero, as stored in `Token::tax`.
    pub fn max_tax(&self) -> TransferTax {
        let mut max = 0;
        for fee in [self.buy, self.transfer, self.sell]
            .into_iter()
            .flatten()
        {
            max = max.max(fee);
        }
        TransferTax::try_from(max).unwrap_or(0)
    }
}

/// Behaviour observed during the analysis that does not decide the verdict on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, DeepSizeOf)]
#[serde(rename_all = "snake_case")]
pub enum TokenFlag {
    /// A transfer called a contract other than the token and the pool.
    CallsExternalContract,
    /// A function other than `transfer` and `transferFrom` writes the balance mapping.
    BalanceWriter,
    /// The fee differs a day or a month later without any owner action.
    TimeDependentFee,
    /// Buy, transfer and sell fees differ.
    DirectionDependentFee,
    /// A transfer made the token sell accumulated fees.
    SwapsOnTransfer,
    /// A holder's balance changed while it took no part in a transfer.
    BalanceDrift,
    /// The large trade fails while the small one succeeds: a max transaction or wallet limit.
    SizeLimited,
    /// The legs fail in one block but succeed when the transfer and sell run a block later.
    PerBlockLimit,
    /// The token is an EIP-1967 or EIP-1167 proxy.
    Proxy,
    /// `owner()` returns the zero address.
    OwnershipRenounced,
    /// The legs succeed or charge fees differently when the transaction looks simulated (zero
    /// gas price and base fee) than in a realistic environment.
    EnvironmentSensitive,
    /// The pool is not a Uniswap V2 style pair against WETH, so the legs were not real swaps:
    /// the pool paid out the buy itself, which a token can tell apart from a swap.
    DirectLegs,
    /// The sell logged no standard `Transfer` event from the seller, so what reached the pool
    /// is measured from the pool's balance alone.
    UnloggedTransfer,
}

/// A call the token made during a simulated leg.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, DeepSizeOf)]
pub struct ExternalCall {
    /// `buy`, `transfer` or `sell`.
    pub leg: String,
    pub target: Bytes,
    /// First four bytes of the calldata; `None` for a plain ETH send.
    pub selector: Option<Bytes>,
    pub reverted: bool,
}

/// What a control slot governs, as proven by a simulated owner call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, DeepSizeOf)]
#[serde(rename_all = "snake_case")]
pub enum SlotRole {
    /// Calling a writer opened trading.
    Gate,
    /// Calling a writer with a large value lifted a size limit.
    Limit,
    /// Calling a writer with zero changed the measured fee.
    Fee,
    /// No simulated call proved what it governs.
    Unknown,
}

/// A token storage field the transfer path reads and an owner-callable function writes. A write
/// to it can change the token's verdict or fees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, DeepSizeOf)]
pub struct ControlSlot {
    pub slot: Bytes,
    /// Byte offset of the field within the slot, for packed variables.
    pub offset: u8,
    /// Selectors of the functions that write the field.
    pub writers: Vec<Bytes>,
    pub role: SlotRole,
}

/// The outcome of analysing a token at one block against one pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, DeepSizeOf)]
pub struct TokenAnalysis {
    pub verdict: TokenVerdict,
    /// Why the verdict is not `Ok`, e.g. the token's revert reason.
    pub reason: Option<String>,
    pub fees: LegFees,
    /// Average gas of a transfer.
    pub gas: Option<TransferCost>,
    /// The block analysed; `None` when the analyzer ran at `latest` without resolving it.
    pub block: Option<u64>,
    /// The address the buy came from and the sell went to; `None` when no holder was found.
    pub pool: Option<Bytes>,
    /// Identifies the analyzer and its version, e.g. `simulation/1`.
    pub analyzer: String,
    pub flags: Vec<TokenFlag>,
    pub external_calls: Vec<ExternalCall>,
    pub control_slots: Vec<ControlSlot>,
}

impl TokenAnalysis {
    /// An analysis with only a verdict and reason, for analyzers that observe nothing else.
    pub fn verdict_only(
        verdict: TokenVerdict,
        reason: Option<String>,
        block: Option<u64>,
        analyzer: &str,
    ) -> Self {
        Self {
            verdict,
            reason,
            fees: LegFees::default(),
            gas: None,
            block,
            pool: None,
            analyzer: analyzer.to_string(),
            flags: Vec::new(),
            external_calls: Vec::new(),
            control_slots: Vec::new(),
        }
    }
}

/// A token with the analysis its quality, tax and gas were derived from; `None` when the token
/// was not analysed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzedToken {
    pub token: Token,
    pub analysis: Option<TokenAnalysis>,
}

/// A store for tracking token owners and their balances.
///
/// The `TokenOwnerStore` maintains a mapping between token addresses and their respective
/// owner's address and balance. It can be used to quickly retrieve token owner information
/// without needing to query external sources.
#[derive(Debug)]
pub struct TokenOwnerStore {
    /// A `HashMap` where the key is the token address and the value is a tuple containing
    /// the owner address and the balance of the owner for the token.
    values: HashMap<Address, (Address, Balance)>,
}

impl TokenOwnerStore {
    pub fn new(values: HashMap<Address, (Address, Balance)>) -> Self {
        TokenOwnerStore { values }
    }
}

#[async_trait::async_trait]
impl TokenOwnerFinding for TokenOwnerStore {
    async fn find_owner(
        &self,
        token: Address,
        min_balance: Balance,
    ) -> Result<Option<(Address, Balance)>, String> {
        Ok(self
            .values
            .get(&token)
            .filter(|(_, balance)| {
                BigUint::from_bytes_be(balance.as_ref()) >=
                    BigUint::from_bytes_be(min_balance.as_ref())
            })
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn test_constructor() {
        let token = Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC",
            6,
            1000,
            &[Some(1000u64)],
            Chain::Ethereum,
            100,
        );

        assert_eq!(token.symbol, "USDC");
        assert_eq!(token.decimals, 6);
        assert_eq!(
            format!("{token_address:#x}", token_address = token.address),
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
    }

    #[test]
    fn test_cmp() {
        let usdc = Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC",
            6,
            1000,
            &[Some(1000u64)],
            Chain::Ethereum,
            100,
        );
        let usdc2 = Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC2",
            6,
            1000,
            &[Some(1000u64)],
            Chain::Ethereum,
            100,
        );
        let weth = Token::new(
            &Bytes::from_str("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2").unwrap(),
            "WETH",
            18,
            1000,
            &[Some(1000u64)],
            Chain::Ethereum,
            100,
        );

        assert!(usdc < weth);
        assert_eq!(usdc, usdc2);
    }

    #[test]
    fn test_one() {
        let usdc = Token::new(
            &Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "USDC",
            6,
            1000,
            &[Some(1000u64)],
            Chain::Ethereum,
            100,
        );

        assert_eq!(usdc.one(), BigUint::from(1000000u64));
    }

    #[tokio::test]
    async fn test_find_owner_respects_min_balance() {
        let token = Bytes::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap();
        let owner = Bytes::from_str("0x1111111111111111111111111111111111111111").unwrap();
        // Owner holds 1000 (unsigned big-endian).
        let store = TokenOwnerStore::new(HashMap::from([(
            token.clone(),
            (owner.clone(), Bytes::from(1_000u64)),
        )]));

        // Below the requested minimum -> no adequate owner.
        assert_eq!(
            store
                .find_owner(token.clone(), Bytes::from(2_000u64))
                .await
                .unwrap(),
            None
        );
        // Exactly at the minimum -> returned.
        assert_eq!(
            store
                .find_owner(token.clone(), Bytes::from(1_000u64))
                .await
                .unwrap(),
            Some((owner.clone(), Bytes::from(1_000u64)))
        );
        // Above the minimum -> returned.
        assert_eq!(
            store
                .find_owner(token.clone(), Bytes::from(500u64))
                .await
                .unwrap(),
            Some((owner, Bytes::from(1_000u64)))
        );
        // Unknown token -> None.
        let other = Bytes::from_str("0x2222222222222222222222222222222222222222").unwrap();
        assert_eq!(
            store
                .find_owner(other, Bytes::from(1u64))
                .await
                .unwrap(),
            None
        );
    }
}
