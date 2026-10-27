use std::collections::HashMap;

use alloy::{
    primitives::{Address, U256},
    rpc::types::{TransactionInput, TransactionRequest},
    sol_types::SolCall,
};
use async_trait::async_trait;
use tycho_common::{models::blockchain::BlockTag, traits::TokenOwnerFinding, Bytes};

use super::{ethcall_detector::MAX_SELL_POOLS, map_block_tag};
use crate::{erc20::balanceOfCall, rpc::EthereumRpcClient, BytesCodec};

/// Finds token holders by asking the chain.
///
/// Holds, per token, the candidate holder addresses of each pool, pools in preference order.
/// Indexed data does not say which candidate holds a pool's tokens (a Curve pool lists its factory
/// too), so a pool's holder is its candidate with the largest `balanceOf` at the configured block.
/// A candidate whose `balanceOf` reverts or returns no data counts as holding nothing. Errors only
/// when the RPC call itself fails.
#[derive(Debug)]
pub struct OnChainTokenOwners {
    rpc: EthereumRpcClient,
    block: BlockTag,
    candidates: HashMap<Bytes, Vec<Vec<Bytes>>>,
}

impl OnChainTokenOwners {
    pub fn new(
        rpc: &EthereumRpcClient,
        block: BlockTag,
        candidates: HashMap<Bytes, Vec<Vec<Bytes>>>,
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

    /// Returns the candidate with the largest non-zero balance, or `None` when none holds any.
    /// On equal balances the earlier candidate wins.
    async fn pool_holder(
        &self,
        token: &Bytes,
        candidates: &[Bytes],
    ) -> Result<Option<(Bytes, U256)>, String> {
        let mut holder: Option<(Bytes, U256)> = None;
        for candidate in candidates {
            let Some(balance) = self
                .balance_of(token, candidate)
                .await?
            else {
                continue;
            };
            if !balance.is_zero() &&
                holder
                    .as_ref()
                    .is_none_or(|(_, largest)| balance > *largest)
            {
                holder = Some((candidate.clone(), balance));
            }
        }
        Ok(holder)
    }

    /// Returns each pool's holder with its on-chain balance, in pool order, at most
    /// `MAX_SELL_POOLS`. Pools whose candidates hold nothing are left out.
    async fn holders(&self, token: &Bytes) -> Result<Vec<(Bytes, U256)>, String> {
        let Some(pools) = self.candidates.get(token) else {
            return Ok(vec![]);
        };
        let mut holders: Vec<(Bytes, U256)> = Vec::new();
        for candidates in pools {
            if holders.len() == MAX_SELL_POOLS {
                break;
            }
            // Pools that share a holder (Uniswap V4 pools and their PoolManager) cost one call.
            if candidates
                .iter()
                .any(|c| holders.iter().any(|(h, _)| h == c))
            {
                continue;
            }
            if let Some(holder) = self
                .pool_holder(token, candidates)
                .await?
            {
                holders.push(holder);
            }
        }
        Ok(holders)
    }
}

#[async_trait]
impl TokenOwnerFinding for OnChainTokenOwners {
    /// Returns the first pool holder, in pool order, whose on-chain balance is at least
    /// `min_balance`. Only the pools that `find_pools` returns are checked.
    async fn find_owner(
        &self,
        token: Bytes,
        min_balance: Bytes,
    ) -> Result<Option<(Bytes, Bytes)>, String> {
        let min_balance = U256::try_from_be_slice(min_balance.as_ref())
            .ok_or_else(|| format!("min_balance {min_balance} does not fit in 256 bits"))?;
        Ok(self
            .holders(&token)
            .await?
            .into_iter()
            .find(|(_, balance)| *balance >= min_balance)
            .map(|(holder, balance)| (holder, balance.to_bytes())))
    }

    /// Returns each pool's holder with its on-chain balance, in pool order, at most
    /// `MAX_SELL_POOLS`.
    async fn find_pools(&self, token: Bytes) -> Result<Vec<(Bytes, Bytes)>, String> {
        Ok(self
            .holders(&token)
            .await?
            .into_iter()
            .map(|(holder, balance)| (holder, balance.to_bytes()))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    const TOKEN: &str = "0xc139190f447e929f090edeb554d95abb8b18ac1c";
    const FACTORY: &str = "0x6a8cbed756804b16e05e741edabd5cb544ae21bf";
    const POOL: &str = "0xc2921134073151490193ac7369313c8e0b08e1e7";
    const OTHER_POOL: &str = "0x7ec8e94a9b379f6b90ee5af7b9a78624280b50ea";
    const EMPTY_POOL: &str = "0x228c6fcd7376177ff0cff304043f461189752750";
    const POOL_MANAGER: &str = "0x000000000004444c5dc75cb358380d2e3de08a90";

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
            HashMap::from([(
                Bytes::from(TOKEN),
                vec![vec![Bytes::from(FACTORY), Bytes::from(POOL)]],
            )]),
        )
    }

    fn finder_with_pools(url: &str, pools: &[&[&str]]) -> OnChainTokenOwners {
        let rpc = EthereumRpcClient::new(url).expect("mock rpc client");
        let pools = pools
            .iter()
            .map(|candidates| {
                candidates
                    .iter()
                    .map(|c| Bytes::from(*c))
                    .collect()
            })
            .collect();
        OnChainTokenOwners::new(
            &rpc,
            BlockTag::Latest,
            HashMap::from([(Bytes::from(TOKEN), pools)]),
        )
    }

    fn holder(address: &str, balance: u64) -> (Bytes, Bytes) {
        (Bytes::from(address), U256::from(balance).to_bytes())
    }

    fn min_balance() -> Bytes {
        U256::from(100_000_u64).to_bytes()
    }

    #[tokio::test]
    async fn returns_funded_candidate_with_onchain_balance() {
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
    #[tokio::test]
    async fn pool_holder_is_the_candidate_with_the_largest_balance() {
        let mut server = mockito::Server::new_async().await;
        mock_holder(&mut server, FACTORY, &result_body(200_000)).await;
        mock_holder(&mut server, POOL, &result_body(5_000_000)).await;

        let res = finder(&server.url())
            .find_owner(Bytes::from(TOKEN), min_balance())
            .await
            .expect("find_owner failed");

        assert_eq!(res, Some(holder(POOL, 5_000_000)));
    }

    #[tokio::test]
    async fn find_owner_falls_through_to_the_next_pool() {
        let mut server = mockito::Server::new_async().await;
        mock_holder(&mut server, POOL, &result_body(99_999)).await;
        mock_holder(&mut server, OTHER_POOL, &result_body(300_000)).await;

        let res = finder_with_pools(&server.url(), &[&[POOL], &[OTHER_POOL]])
            .find_owner(Bytes::from(TOKEN), min_balance())
            .await
            .expect("find_owner failed");

        assert_eq!(res, Some(holder(OTHER_POOL, 300_000)));
    }

    #[tokio::test]
    async fn find_pools_returns_each_pool_holder_in_pool_order() {
        let mut server = mockito::Server::new_async().await;
        mock_holder(&mut server, FACTORY, &result_body(0)).await;
        mock_holder(&mut server, POOL, &result_body(5_000_000)).await;
        mock_holder(&mut server, EMPTY_POOL, &result_body(0)).await;
        mock_holder(&mut server, OTHER_POOL, &result_body(10)).await;

        let res =
            finder_with_pools(&server.url(), &[&[FACTORY, POOL], &[EMPTY_POOL], &[OTHER_POOL]])
                .find_pools(Bytes::from(TOKEN))
                .await
                .expect("find_pools failed");

        assert_eq!(res, vec![holder(POOL, 5_000_000), holder(OTHER_POOL, 10)]);
    }

    #[tokio::test]
    async fn pools_that_share_a_holder_cost_one_call() {
        let mut server = mockito::Server::new_async().await;
        let manager = mock_holder(&mut server, POOL_MANAGER, &result_body(7_000_000)).await;

        let res = finder_with_pools(&server.url(), &[&[POOL_MANAGER], &[POOL_MANAGER]])
            .find_pools(Bytes::from(TOKEN))
            .await
            .expect("find_pools failed");

        assert_eq!(res, vec![holder(POOL_MANAGER, 7_000_000)]);
        manager.assert_async().await;
    }

    #[tokio::test]
    async fn find_pools_stops_after_max_sell_pools_holders() {
        let mut server = mockito::Server::new_async().await;
        let pools: Vec<String> = (1..=MAX_SELL_POOLS + 1)
            .map(|i| format!("0x{i:040x}"))
            .collect();
        let mut mocks = Vec::new();
        for pool in &pools[..MAX_SELL_POOLS] {
            mocks.push(mock_holder(&mut server, pool, &result_body(1_000)).await);
        }
        let last = server
            .mock("POST", "/")
            .match_body(mockito::Matcher::Regex(pools[MAX_SELL_POOLS][2..].to_string()))
            .expect(0)
            .create_async()
            .await;
        let candidates: Vec<[&str; 1]> = pools
            .iter()
            .map(|p| [p.as_str()])
            .collect();
        let candidates: Vec<&[&str]> = candidates
            .iter()
            .map(|c| c.as_slice())
            .collect();

        let res = finder_with_pools(&server.url(), &candidates)
            .find_pools(Bytes::from(TOKEN))
            .await
            .expect("find_pools failed");

        assert_eq!(res.len(), MAX_SELL_POOLS);
        last.assert_async().await;
    }
}
