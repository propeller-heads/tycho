use std::collections::{HashMap, HashSet};

use alloy::{
    primitives::{Address as AlloyAddress, B256},
    rpc::types::{BlockId, BlockNumberOrTag},
};
use async_trait::async_trait;
use chrono::DateTime;
use futures::future::try_join_all;
use tracing::{debug, info};
use tycho_common::{
    models::{blockchain::Block, contract::AccountDelta, Chain, ChangeType},
    traits::{AccountExtractor, StorageSnapshotRequest},
    Bytes,
};

use crate::{
    rpc::{
        errors::{RPCError, RequestError},
        EthereumRpcClient,
    },
    BytesCodec,
};

/// `EVMAccountExtractor` is a struct that implements the `AccountExtractor` trait for Ethereum
/// accounts.
/// TODO: once the `chain` attribute is deprecated from AccountDelta,
/// We can get rid of this struct and use the EthereumRpcClient directly
/// to implement the `AccountExtractor` trait.
pub struct EVMAccountExtractor {
    rpc: EthereumRpcClient,
    chain: Chain,
}

impl EVMAccountExtractor {
    pub fn new(client: &EthereumRpcClient, chain: Chain) -> Self {
        // As the client is a thin wrapper around an Arc, cloning is inexpensive.
        Self { rpc: client.clone(), chain }
    }

    pub async fn get_block_data(&self, block_id: u64) -> Result<Block, RPCError> {
        let block_id = BlockId::from(block_id);

        let block = self
            .rpc
            .eth_get_block_by_number(block_id)
            .await?;

        Ok(Block {
            number: block.header.number,
            hash: block.header.hash.to_bytes(),
            parent_hash: block.header.parent_hash.to_bytes(),
            chain: self.chain,
            ts: i64::try_from(block.header.timestamp)
                .ok()
                .and_then(|timestamp| DateTime::from_timestamp(timestamp, 0))
                .ok_or_else(|| {
                    RPCError::RequestError(RequestError::Other("Invalid timestamp in block".into()))
                })?
                .naive_utc(),
        })
    }
}

#[async_trait]
impl AccountExtractor for EVMAccountExtractor {
    type Error = RPCError;

    async fn get_accounts_at_block(
        &self,
        block: &Block,
        requests: &[StorageSnapshotRequest],
    ) -> Result<HashMap<Bytes, AccountDelta>, Self::Error> {
        let block_id = BlockNumberOrTag::Number(block.number);
        let block_hash = B256::from_slice(&block.hash);

        let mut updates = HashMap::new();

        // Remove duplicates to avoid making more requests than necessary.
        let unique_requests: Vec<StorageSnapshotRequest> = requests
            .iter()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        // Convert addresses to AlloyAddress for easier handling
        let alloy_addresses: Vec<AlloyAddress> = unique_requests
            .iter()
            .map(|request| AlloyAddress::from_bytes(&request.address))
            .collect();

        // Create a future for code and balance retrieval
        let codes_and_balances_fut = self
            .rpc
            .fetch_accounts_code_and_balance(block_id, &alloy_addresses);

        // Create futures for storage retrieval
        let storage_futs = unique_requests
            .iter()
            .map(|req| {
                let address = AlloyAddress::from_bytes(&req.address);

                let fut = async move {
                    if let Some(slots) = &req.slots {
                        let slots = slots
                            .iter()
                            .map(B256::from_bytes)
                            .collect::<Vec<_>>();

                        self.rpc
                            .get_selected_storage(block_id, address, &slots)
                            .await
                    } else {
                        self.rpc
                            .get_storage_range(address, block_hash)
                            .await
                            // Wrap the resulting hashmap values in Some to match the expected type
                            .map(|result| {
                                result
                                    .into_iter()
                                    .map(|(k, v)| (k, Some(v)))
                                    .collect()
                            })
                    }
                };

                fut
            })
            .collect::<Vec<_>>();

        let codes_and_balances = codes_and_balances_fut.await?;
        debug!(block_number = block.number, "Successfully retrieved account code and balance data");

        let storage_results = try_join_all(storage_futs).await?;
        debug!(block_number = block.number, "Successfully retrieved account storage");

        for (address, storage_result) in alloy_addresses
            .iter()
            .zip(storage_results)
        {
            let (code, balance) = codes_and_balances[address].clone();

            // Convert the storage result from Alloy to Tycho types
            let storage = storage_result
                .into_iter()
                .map(|(k, v)| (k.to_bytes(), v.map(|v| v.to_bytes())))
                .collect::<HashMap<_, _>>();

            let account_delta = AccountDelta::new(
                self.chain,
                address.to_bytes(),
                storage,
                Some(balance.to_bytes()),
                Some(code),
                ChangeType::Creation,
            );

            updates.insert(address.to_bytes(), account_delta);
        }

        info!(
            total_accounts_processed = updates.len(),
            block_number = block.number,
            "Completed batch account extraction successfully"
        );

        Ok(updates)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use rstest::rstest;
    use tracing::warn;
    use tracing_test::traced_test;
    use tycho_common::models::{Address, Chain};

    use super::*;
    use crate::test_fixtures::{
        TestFixture, BALANCER_VAULT_EXPECTED_SLOTS, BALANCER_VAULT_STR, STETH_EXPECTED_SLOTS,
        STETH_STR, TEST_SLOTS, TOKEN_ADDRESSES,
    };

    fn parse_address(address_str: &str) -> Address {
        Address::from_str(address_str).expect("failed to parse address")
    }

    fn create_storage_request(
        address_str: &str,
        slots: Option<Vec<Bytes>>,
    ) -> StorageSnapshotRequest {
        StorageSnapshotRequest { address: parse_address(address_str), slots }
    }

    impl TestFixture {
        fn create_evm_extractor(&self, batching: bool) -> EVMAccountExtractor {
            let rpc_client = self.create_rpc_client(batching);

            EVMAccountExtractor { rpc: rpc_client, chain: Chain::Ethereum }
        }
    }

    /// Test the account extractor with various contracts and their storage slots.
    ///
    /// Note: The STETH test case processes a large number of storage slots (789,526 slots,
    /// stETH is the 9th largest token by number of holders). This test takes around 2 minutes
    /// to run and retrieves around 50MB of data.
    #[rstest]
    #[case(BALANCER_VAULT_STR, BALANCER_VAULT_EXPECTED_SLOTS)]
    #[case(STETH_STR, STETH_EXPECTED_SLOTS)] // Large contract - takes ~2 mins, retrieves ~50MB
    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_account_extractor(
        #[case] address_str: &str,
        #[case] expected_slot_count: usize,
        #[values(false, true)] batching: bool,
    ) -> Result<(), RPCError> {
        let fixture = TestFixture::new();
        let extractor = fixture.create_evm_extractor(batching);

        // Warn about large contracts (STETH has 789k+ slots, takes ~2 mins, ~50MB data)
        if expected_slot_count > 100_000 {
            warn!(
                "Testing large contract {} with {} storage slots - this will take ~2 minutes and retrieve ~50MB of data",
                address_str, expected_slot_count
            );
        }

        let requests = vec![create_storage_request(address_str, None)];

        let updates = extractor
            .get_accounts_at_block(&fixture.block, &requests)
            .await?;

        assert_eq!(updates.len(), 1, "Expected exactly 1 account update");

        let update = updates
            .get(&Bytes::from_str(address_str).expect("valid address"))
            .expect("update exists");

        assert_eq!(
            update.slots.len(),
            expected_slot_count,
            "{} storage slot count mismatch. Expected: {}, Got: {}",
            address_str,
            expected_slot_count,
            update.slots.len()
        );

        Ok(())
    }

    #[rstest]
    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_get_storage_snapshots_plain(
        #[values(false, true)] batching: bool,
    ) -> Result<(), RPCError> {
        let fixture = TestFixture::new();

        let extractor = fixture.create_evm_extractor(batching);

        let requests = vec![
            create_storage_request(BALANCER_VAULT_STR, Some(vec![])),
            create_storage_request(STETH_STR, Some(vec![])),
        ];

        let start_time = std::time::Instant::now();
        let result = extractor
            .get_accounts_at_block(&fixture.block, &requests)
            .await?;
        let duration = start_time.elapsed();
        println!("Time taken to get storage snapshots: {duration:?}");

        assert_eq!(result.len(), 2);

        // First account check
        let first_address = parse_address(BALANCER_VAULT_STR);
        let first_delta = result
            .get(&first_address)
            .expect("first address should exist");
        assert_eq!(first_delta.address, first_address);
        assert_eq!(first_delta.chain, Chain::Ethereum);
        assert!(first_delta.code().is_some());
        assert!(first_delta.balance.is_some());
        println!("Balance: {:?}", first_delta.balance);

        // Second account check
        let second_address = parse_address(STETH_STR);
        let second_delta: &AccountDelta = result
            .get(&second_address)
            .expect("second address should exist");
        assert_eq!(second_delta.address, second_address);
        assert_eq!(second_delta.chain, Chain::Ethereum);
        assert!(second_delta.code().is_some());
        assert!(second_delta.balance.is_some());
        println!("Balance: {:?}", second_delta.balance);

        Ok(())
    }

    #[rstest]
    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_get_storage_snapshots_with_specific_slots(
        #[values(false, true)] batching: bool,
    ) -> Result<(), RPCError> {
        let fixture = TestFixture::new();
        let extractor = fixture.create_evm_extractor(batching);

        // Create request with specific slots
        let slots = &*TEST_SLOTS;
        let slots_request = slots
            .keys()
            .map(|k| k.to_bytes())
            .collect();

        let requests = vec![create_storage_request(BALANCER_VAULT_STR, Some(slots_request))];

        let result = extractor
            .get_accounts_at_block(&fixture.block, &requests)
            .await?;

        assert_eq!(result.len(), 1);

        // Check the account delta
        let address = parse_address(BALANCER_VAULT_STR);
        let delta = result
            .get(&address)
            .expect("address should exist");

        assert_eq!(delta.address, address);
        assert_eq!(delta.chain, Chain::Ethereum);
        assert!(delta.code().is_some());
        assert!(delta.balance.is_some());

        // Check that storage slots match what we requested
        assert_eq!(delta.slots.len(), 3);
        for (key, value) in slots.iter() {
            assert!(delta
                .slots
                .contains_key(&key.to_bytes()));
            assert_eq!(
                delta
                    .slots
                    .get(&key.to_bytes())
                    .and_then(|v| v.as_ref()),
                Some(&value.to_bytes())
            );
        }

        Ok(())
    }

    #[rstest]
    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_get_storage_snapshots_with_empty_slot(
        #[values(false, true)] batching: bool,
    ) -> Result<(), RPCError> {
        let fixture = TestFixture::new();
        let extractor = fixture.create_evm_extractor(batching);

        // Try to get a slot that was not initialized / is empty
        let slots_request: Vec<Bytes> = vec![Bytes::from_str(
            "0000000000000000000000000000000000000000000000000000000000000002",
        )
        .unwrap()];

        let requests =
            vec![create_storage_request(BALANCER_VAULT_STR, Some(slots_request.clone()))];

        let result = extractor
            .get_accounts_at_block(&fixture.block, &requests)
            .await?;

        assert_eq!(result.len(), 1);

        // Check the account delta
        let address = parse_address(BALANCER_VAULT_STR);
        let delta = result
            .get(&address)
            .expect("address should exist");

        assert_eq!(delta.address, address);
        assert_eq!(delta.chain, Chain::Ethereum);
        assert!(delta.code().is_some());
        assert!(delta.balance.is_some());

        // Check that storage slots match what we requested
        assert_eq!(delta.slots.len(), 1);
        assert_eq!(
            delta
                .slots
                .get(&slots_request[0])
                .unwrap(),
            &None
        );

        Ok(())
    }

    #[rstest]
    #[traced_test]
    #[tokio::test]
    #[ignore = "require RPC connection"]
    async fn test_get_storage_snapshots_multiple_accounts(
        #[values(false, true)] batching: bool,
    ) -> Result<(), RPCError> {
        let fixture = TestFixture::new();
        let extractor = fixture.create_evm_extractor(batching);

        // Create multiple requests with different token addresses
        let requests: Vec<_> = TOKEN_ADDRESSES
            .iter()
            .map(|&addr| {
                create_storage_request(
                    addr,
                    Some(vec![Bytes::from_str(
                        "0000000000000000000000000000000000000000000000000000000000000000",
                    )
                    .unwrap()]),
                )
            })
            .collect();

        let start_time = std::time::Instant::now();
        let result = extractor
            .get_accounts_at_block(&fixture.block, &requests)
            .await?;
        let duration = start_time.elapsed();
        println!(
            "Time taken to get storage snapshots for {} accounts: {:?}",
            requests.len(),
            duration
        );

        assert_eq!(result.len(), TOKEN_ADDRESSES.len());

        // Check each account has the required data
        for addr_str in TOKEN_ADDRESSES.iter() {
            let address = parse_address(addr_str);
            let delta = result
                .get(&address)
                .expect("address should exist");

            assert_eq!(delta.address, address);
            assert_eq!(delta.chain, Chain::Ethereum);
            assert!(delta.code().is_some());
            assert!(delta.balance.is_some());
            assert_eq!(delta.slots.len(), 1);

            println!(
                "Address: {}, Code size: {}, Has balance: {}",
                addr_str,
                delta.code().as_ref().unwrap().len(),
                delta.balance.is_some()
            );
        }

        Ok(())
    }
}
