use std::collections::HashMap;

use alloy::{
    primitives::{Address, U256},
    rpc::types::{TransactionInput, TransactionRequest},
    sol_types::SolCall,
};
use async_trait::async_trait;
use tycho_common::{models::blockchain::BlockTag, traits::TokenOwnerFinding, Bytes};

use super::map_block_tag;
use crate::{erc20::balanceOfCall, rpc::EthereumRpcClient, BytesCodec};

/// Finds a token holder by asking the chain.
///
/// Holds, per token, candidate holder addresses in preference order. `find_owner` returns the
/// first candidate whose `balanceOf` at the configured block is at least `min_balance`, with that
/// on-chain balance. A candidate whose `balanceOf` reverts or returns no data counts as holding
/// nothing. Errors only when the RPC call itself fails.
#[derive(Debug)]
pub struct OnChainTokenOwners {
    rpc: EthereumRpcClient,
    block: BlockTag,
    candidates: HashMap<Bytes, Vec<Bytes>>,
}

impl OnChainTokenOwners {
    pub fn new(
        rpc: &EthereumRpcClient,
        block: BlockTag,
        candidates: HashMap<Bytes, Vec<Bytes>>,
    ) -> Self {
        Self { rpc: rpc.clone(), block, candidates }
    }
}

impl OnChainTokenOwners {
    /// Returns the holder's balance, or `None` when the call reverts or returns no decodable
    /// data. A reverting `balanceOf` is a property of the token, not an RPC fault, and must end
    /// in a Bad verdict instead of an error that the cron retries forever.
    async fn balance_of(&self, token: &Bytes, holder: &Bytes) -> Result<Option<U256>, String> {
        let calldata = balanceOfCall { _owner: Address::from_bytes(holder) }.abi_encode();
        let request = TransactionRequest::default()
            .to(Address::from_bytes(token))
            .input(TransactionInput::both(calldata.into()));
        match self
            .rpc
            .eth_call(request, map_block_tag(self.block))
            .await
        {
            Ok(raw) => Ok(balanceOfCall::abi_decode_returns(raw.as_ref()).ok()),
            Err(e) if e.is_execution_reverted() => Ok(None),
            Err(e) => Err(format!("balanceOf({holder}) on token {token} failed: {e}")),
        }
    }
}

#[async_trait]
impl TokenOwnerFinding for OnChainTokenOwners {
    async fn find_owner(
        &self,
        token: Bytes,
        min_balance: Bytes,
    ) -> Result<Option<(Bytes, Bytes)>, String> {
        let Some(candidates) = self.candidates.get(&token) else {
            return Ok(None);
        };
        let min_balance = U256::try_from_be_slice(min_balance.as_ref())
            .ok_or_else(|| format!("min_balance {min_balance} does not fit in 256 bits"))?;
        for holder in candidates {
            if let Some(balance) = self.balance_of(&token, holder).await? {
                if balance >= min_balance {
                    return Ok(Some((holder.clone(), balance.to_bytes())));
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    const TOKEN: &str = "0xc139190f447e929f090edeb554d95abb8b18ac1c";
    const FACTORY: &str = "0x6a8cbed756804b16e05e741edabd5cb544ae21bf";
    const POOL: &str = "0xc2921134073151490193ac7369313c8e0b08e1e7";

    fn result_body(balance: u64) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":0,"result":"0x{balance:064x}"}}"#)
    }

    const REVERT_BODY: &str =
        r#"{"jsonrpc":"2.0","id":0,"error":{"code":3,"message":"execution reverted","data":"0x"}}"#;
    const EMPTY_BODY: &str = r#"{"jsonrpc":"2.0","id":0,"result":"0x"}"#;
    const INVALID_PARAMS_BODY: &str =
        r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32602,"message":"invalid params"}}"#;

    /// Mocks one `eth_call` whose calldata names `holder` (balanceOf encodes the holder address).
    async fn mock_holder(
        server: &mut mockito::ServerGuard,
        holder: &str,
        body: &str,
    ) -> mockito::Mock {
        server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Regex(
                holder
                    .trim_start_matches("0x")
                    .to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .expect(1)
            .create_async()
            .await
    }

    fn finder(url: &str) -> OnChainTokenOwners {
        let rpc = EthereumRpcClient::new(url).expect("mock rpc client");
        OnChainTokenOwners::new(
            &rpc,
            BlockTag::Latest,
            HashMap::from([(Bytes::from(TOKEN), vec![Bytes::from(FACTORY), Bytes::from(POOL)])]),
        )
    }

    fn min_balance() -> Bytes {
        U256::from(100_000_u64).to_bytes()
    }

    #[tokio::test]
    async fn returns_first_funded_candidate_with_onchain_balance() {
        let mut server = mockito::Server::new_async().await;
        let factory = mock_holder(&mut server, FACTORY, &result_body(0)).await;
        let pool = mock_holder(&mut server, POOL, &result_body(5_000_000)).await;

        let res = finder(&server.url())
            .find_owner(Bytes::from(TOKEN), min_balance())
            .await
            .expect("find_owner failed");

        assert_eq!(res, Some((Bytes::from(POOL), U256::from(5_000_000_u64).to_bytes())));
        factory.assert_async().await;
        pool.assert_async().await;
    }

    #[tokio::test]
    async fn returns_none_when_no_candidate_holds_the_minimum() {
        let mut server = mockito::Server::new_async().await;
        mock_holder(&mut server, FACTORY, &result_body(0)).await;
        mock_holder(&mut server, POOL, &result_body(99_999)).await;

        let res = finder(&server.url())
            .find_owner(Bytes::from(TOKEN), min_balance())
            .await
            .expect("find_owner failed");

        assert_eq!(res, None);
    }

    #[tokio::test]
    async fn skips_candidate_whose_balance_of_reverts() {
        let mut server = mockito::Server::new_async().await;
        mock_holder(&mut server, FACTORY, REVERT_BODY).await;
        mock_holder(&mut server, POOL, &result_body(5_000_000)).await;

        let res = finder(&server.url())
            .find_owner(Bytes::from(TOKEN), min_balance())
            .await
            .expect("a reverting balanceOf must not be an error");

        assert_eq!(res, Some((Bytes::from(POOL), U256::from(5_000_000_u64).to_bytes())));
    }

    #[tokio::test]
    async fn skips_candidate_that_returns_no_data() {
        let mut server = mockito::Server::new_async().await;
        mock_holder(&mut server, FACTORY, EMPTY_BODY).await;
        mock_holder(&mut server, POOL, &result_body(5_000_000)).await;

        let res = finder(&server.url())
            .find_owner(Bytes::from(TOKEN), min_balance())
            .await
            .expect("empty return data must not be an error");

        assert_eq!(res, Some((Bytes::from(POOL), U256::from(5_000_000_u64).to_bytes())));
    }

    #[tokio::test]
    async fn rpc_failure_is_an_error() {
        let mut server = mockito::Server::new_async().await;
        mock_holder(&mut server, FACTORY, INVALID_PARAMS_BODY).await;

        let res = finder(&server.url())
            .find_owner(Bytes::from(TOKEN), min_balance())
            .await;

        assert!(res.is_err());
    }

    #[tokio::test]
    async fn token_without_candidates_makes_no_rpc_call() {
        // Port 1 refuses connections: any RPC call would turn into an Err.
        let res = finder("http://localhost:1")
            .find_owner(
                Bytes::from_str("0x0000000000000000000000000000000000000001").unwrap(),
                min_balance(),
            )
            .await
            .expect("no RPC call expected");

        assert_eq!(res, None);
    }
}
