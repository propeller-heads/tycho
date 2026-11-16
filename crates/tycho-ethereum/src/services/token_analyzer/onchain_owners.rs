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
/// Holds, per token, candidate holder addresses in preference order. `find_owner` reads the
/// `balanceOf` of every candidate at the configured block in one batch request and returns the
/// candidate with the largest balance that is at least `min_balance`, with that on-chain balance.
/// On equal balances the earlier candidate wins. A candidate whose `balanceOf` reverts or returns
/// no data counts as holding nothing. Errors only when the RPC call itself fails.
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
    /// Returns each holder's balance from one batch request, in holder order. A balance is `None`
    /// when its call reverts or returns no decodable data. A reverting `balanceOf` is a property
    /// of the token, not an RPC fault, and must end in a Bad verdict instead of an error that the
    /// cron retries forever.
    async fn balances_of(
        &self,
        token: &Bytes,
        holders: &[Bytes],
    ) -> Result<Vec<Option<U256>>, String> {
        let requests = holders
            .iter()
            .map(|holder| {
                let calldata = balanceOfCall { _owner: Address::from_bytes(holder) }.abi_encode();
                TransactionRequest::default()
                    .to(Address::from_bytes(token))
                    .input(TransactionInput::both(calldata.into()))
            })
            .collect::<Vec<_>>();
        let results = self
            .rpc
            .batch_eth_call(&requests, map_block_tag(self.block))
            .await
            .map_err(|e| format!("balanceOf batch on token {token} failed: {e}"))?;
        holders
            .iter()
            .zip(results)
            .map(|(holder, result)| match result {
                Ok(raw) => Ok(balanceOfCall::abi_decode_returns(raw.as_ref()).ok()),
                Err(e) if e.is_execution_reverted() => Ok(None),
                Err(e) => Err(format!("balanceOf({holder}) on token {token} failed: {e}")),
            })
            .collect()
    }
}

#[async_trait]
impl TokenOwnerFinding for OnChainTokenOwners {
    async fn find_owner(
        &self,
        token: Bytes,
        min_balance: Bytes,
    ) -> Result<Option<(Bytes, Bytes)>, String> {
        let Some(candidates) = self
            .candidates
            .get(&token)
            .filter(|candidates| !candidates.is_empty())
        else {
            return Ok(None);
        };
        let min_balance = U256::try_from_be_slice(min_balance.as_ref())
            .ok_or_else(|| format!("min_balance {min_balance} does not fit in 256 bits"))?;
        let balances = self
            .balances_of(&token, candidates)
            .await?;
        let mut owner: Option<(&Bytes, U256)> = None;
        for (holder, balance) in candidates.iter().zip(balances) {
            let Some(balance) = balance else {
                continue;
            };
            if balance >= min_balance && owner.is_none_or(|(_, largest)| balance > largest) {
                owner = Some((holder, balance));
            }
        }
        Ok(owner.map(|(holder, balance)| (holder.clone(), balance.to_bytes())))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;

    use super::*;

    const TOKEN: &str = "0xc139190f447e929f090edeb554d95abb8b18ac1c";
    const FACTORY: &str = "0x6a8cbed756804b16e05e741edabd5cb544ae21bf";
    const POOL: &str = "0xc2921134073151490193ac7369313c8e0b08e1e7";

    // Each test uses a new client, so the batch request ids are 0 for FACTORY and 1 for POOL.
    fn balance_response(id: usize, balance: u64) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"result":"0x{balance:064x}"}}"#)
    }

    fn revert_response(id: usize) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":3,"message":"execution reverted","data":"0x"}}}}"#
        )
    }

    fn empty_response(id: usize) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"result":"0x"}}"#)
    }

    fn invalid_params_response(id: usize) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32602,"message":"invalid params"}}}}"#
        )
    }

    /// Mocks one batch request that asks for the balances of FACTORY and POOL.
    async fn mock_batch(
        server: &mut mockito::ServerGuard,
        responses: [String; 2],
    ) -> mockito::Mock {
        server
            .mock("POST", "/")
            .match_body(mockito::Matcher::AllOf(
                [FACTORY, POOL]
                    .iter()
                    .map(|holder| {
                        mockito::Matcher::Regex(
                            holder
                                .trim_start_matches("0x")
                                .to_string(),
                        )
                    })
                    .collect(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(format!("[{}]", responses.join(",")))
            .expect(1)
            .create_async()
            .await
    }

    fn finder(url: &str) -> OnChainTokenOwners {
        let rpc = EthereumRpcClient::new(url).expect("mock rpc client");
        OnChainTokenOwners::new(
            &rpc,
            BlockTag::Latest,
            HashMap::from([
                (Bytes::from(TOKEN), vec![Bytes::from(FACTORY), Bytes::from(POOL)]),
                (Bytes::from_str("0x0000000000000000000000000000000000000002").unwrap(), vec![]),
            ]),
        )
    }

    fn min_balance() -> Bytes {
        U256::from(100_000_u64).to_bytes()
    }

    #[tokio::test]
    async fn picks_largest_balance_at_least_min() {
        let mut server = mockito::Server::new_async().await;
        let batch =
            mock_batch(&mut server, [balance_response(0, 200_000), balance_response(1, 5_000_000)])
                .await;

        let res = finder(&server.url())
            .find_owner(Bytes::from(TOKEN), min_balance())
            .await
            .expect("find_owner failed");

        assert_eq!(res, Some((Bytes::from(POOL), U256::from(5_000_000_u64).to_bytes())));
        batch.assert_async().await;
    }

    #[tokio::test]
    async fn equal_balances_keep_the_earlier_candidate() {
        let mut server = mockito::Server::new_async().await;
        mock_batch(&mut server, [balance_response(0, 5_000_000), balance_response(1, 5_000_000)])
            .await;

        let res = finder(&server.url())
            .find_owner(Bytes::from(TOKEN), min_balance())
            .await
            .expect("find_owner failed");

        assert_eq!(res, Some((Bytes::from(FACTORY), U256::from(5_000_000_u64).to_bytes())));
    }

    #[tokio::test]
    async fn returns_none_when_no_candidate_holds_the_minimum() {
        let mut server = mockito::Server::new_async().await;
        mock_batch(&mut server, [balance_response(0, 0), balance_response(1, 99_999)]).await;

        let res = finder(&server.url())
            .find_owner(Bytes::from(TOKEN), min_balance())
            .await
            .expect("find_owner failed");

        assert_eq!(res, None);
    }

    #[tokio::test]
    async fn skips_candidate_whose_balance_of_reverts() {
        let mut server = mockito::Server::new_async().await;
        mock_batch(&mut server, [revert_response(0), balance_response(1, 5_000_000)]).await;

        let res = finder(&server.url())
            .find_owner(Bytes::from(TOKEN), min_balance())
            .await
            .expect("a reverting balanceOf must not be an error");

        assert_eq!(res, Some((Bytes::from(POOL), U256::from(5_000_000_u64).to_bytes())));
    }

    #[tokio::test]
    async fn skips_candidate_that_returns_no_data() {
        let mut server = mockito::Server::new_async().await;
        mock_batch(&mut server, [empty_response(0), balance_response(1, 5_000_000)]).await;

        let res = finder(&server.url())
            .find_owner(Bytes::from(TOKEN), min_balance())
            .await
            .expect("empty return data must not be an error");

        assert_eq!(res, Some((Bytes::from(POOL), U256::from(5_000_000_u64).to_bytes())));
    }

    #[tokio::test]
    async fn rpc_failure_is_an_error() {
        let mut server = mockito::Server::new_async().await;
        mock_batch(&mut server, [invalid_params_response(0), balance_response(1, 5_000_000)]).await;

        let res = finder(&server.url())
            .find_owner(Bytes::from(TOKEN), min_balance())
            .await;

        assert!(res.is_err());
    }

    #[rstest]
    #[case::unknown_token("0x0000000000000000000000000000000000000001")]
    #[case::empty_candidate_list("0x0000000000000000000000000000000000000002")]
    #[tokio::test]
    async fn token_without_candidates_makes_no_rpc_call(#[case] token: &str) {
        // Port 1 refuses connections: any RPC call would turn into an Err.
        let res = finder("http://localhost:1")
            .find_owner(Bytes::from_str(token).unwrap(), min_balance())
            .await
            .expect("no RPC call expected");

        assert_eq!(res, None);
    }
}
