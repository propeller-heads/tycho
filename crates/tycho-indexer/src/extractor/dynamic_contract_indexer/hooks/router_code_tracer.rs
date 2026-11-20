use async_trait::async_trait;
use tycho_common::{
    models::{
        blockchain::{EntryPointWithTracingParams, TracedEntryPoint, TracingParams},
        Address, BlockHash,
    },
    traits::EntryPointTracer,
    Bytes,
};

/// Traces hook entrypoints with the current mini router code.
///
/// Each stored hook tracing param embeds the router code it was generated with, and retries and
/// retriggers replay stored params unchanged. Params generated with an older router (such as the
/// non-payable one, whose token->ETH swaps all reverted) would fail on every retry, so this
/// tracer swaps in the current code before tracing. Results keep the stored params, because the
/// DCI cache and the database identify a result by them.
pub struct RouterCodeTracer<T> {
    inner: T,
    router_address: Address,
    router_code: Bytes,
}

impl<T> RouterCodeTracer<T> {
    pub(super) fn new(inner: T, router_address: Address, router_code: Bytes) -> Self {
        Self { inner, router_address, router_code }
    }

    fn with_current_router_code(
        &self,
        mut entry_point: EntryPointWithTracingParams,
    ) -> EntryPointWithTracingParams {
        let TracingParams::RPCTracer(params) = &mut entry_point.params;
        if let Some(code) = params
            .state_overrides
            .as_mut()
            .and_then(|overrides| overrides.get_mut(&self.router_address))
            .and_then(|router| router.code.as_mut())
        {
            *code = self.router_code.clone();
        }
        entry_point
    }
}

#[async_trait]
impl<T: EntryPointTracer + Send + Sync> EntryPointTracer for RouterCodeTracer<T> {
    type Error = T::Error;

    async fn trace(
        &self,
        block_hash: BlockHash,
        entry_points: Vec<EntryPointWithTracingParams>,
    ) -> Vec<Result<TracedEntryPoint, Self::Error>> {
        let current = entry_points
            .iter()
            .cloned()
            .map(|entry_point| self.with_current_router_code(entry_point))
            .collect();
        // The inner tracer returns one result per entrypoint, in input order.
        let mut results = self
            .inner
            .trace(block_hash, current)
            .await;
        for (result, stored) in results.iter_mut().zip(entry_points) {
            if let Ok(traced) = result {
                traced.entry_point_with_params = stored;
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tycho_common::{
        models::{
            blockchain::{
                AccountOverrides, EntryPoint, RPCTracerParams, StorageOverride, TracingResult,
            },
            Chain,
        },
        traits::MockEntryPointTracer,
    };
    use tycho_ethereum::{
        rpc::EthereumRpcClient, services::entrypoint_tracer::tracer::EVMEntrypointService,
    };

    use super::*;
    use crate::extractor::dynamic_contract_indexer::hooks::entrypoint_generator;

    fn router() -> Address {
        Address::from([0x2e; 20])
    }

    fn entry_point(router_code: &str, other_code: &str) -> EntryPointWithTracingParams {
        let state_overrides = BTreeMap::from([
            (
                router(),
                AccountOverrides {
                    slots: None,
                    native_balance: None,
                    code: Some(Bytes::from(router_code)),
                },
            ),
            (
                Address::from([0x11; 20]),
                AccountOverrides {
                    slots: None,
                    native_balance: None,
                    code: Some(Bytes::from(other_code)),
                },
            ),
        ]);
        EntryPointWithTracingParams::new(
            EntryPoint::new(
                "hook:execute(bytes)".to_string(),
                router(),
                "execute(bytes)".to_string(),
            ),
            TracingParams::RPCTracer(
                RPCTracerParams::new(None, Bytes::from("0x09c5eabe"))
                    .with_state_overrides(state_overrides),
            ),
        )
    }

    #[tokio::test]
    async fn test_traces_with_current_router_code_and_keeps_stored_params() {
        let succeeding = entry_point("0xaa", "0xbb");
        let failing = entry_point("0xcc", "0xdd");
        let mut inner = MockEntryPointTracer::new();
        inner
            .expect_trace()
            .withf(|_, entry_points| {
                *entry_points == vec![entry_point("0x60", "0xbb"), entry_point("0x60", "0xdd")]
            })
            .return_once(|block_hash, entry_points| {
                vec![
                    Ok(TracedEntryPoint::new(
                        entry_points[0].clone(),
                        block_hash,
                        TracingResult::default(),
                    )),
                    Err("reverted".to_string()),
                ]
            });
        let tracer = RouterCodeTracer::new(inner, router(), Bytes::from("0x60"));

        let results = tracer
            .trace(BlockHash::from([1u8; 32]), vec![succeeding.clone(), failing])
            .await;

        assert_eq!(results.len(), 2);
        let traced = results[0]
            .as_ref()
            .expect("first entrypoint traces");
        assert_eq!(traced.entry_point_with_params, succeeding);
        assert_eq!(results[1].as_ref().err(), Some(&"reverted".to_string()));
    }

    #[tokio::test]
    async fn test_leaves_params_without_router_code_unchanged() {
        let mut without_router = entry_point("0xaa", "0xbb");
        let TracingParams::RPCTracer(params) = &mut without_router.params;
        params.state_overrides = None;
        let expected = without_router.clone();
        let mut inner = MockEntryPointTracer::new();
        inner
            .expect_trace()
            .withf(move |_, entry_points| *entry_points == vec![expected.clone()])
            .return_once(|_, _| vec![]);
        let tracer = RouterCodeTracer::new(inner, router(), Bytes::from("0x60"));

        tracer
            .trace(BlockHash::from([1u8; 32]), vec![without_router])
            .await;
    }

    /// Replays a VIBE->ETH sample swap whose stored params failed with the non-payable router
    /// (params 79802 in the dev DB, minus the stored router code). The stored code is replaced by
    /// INVALID, so the trace only succeeds with the current router swapped in. It traces at the
    /// chain tip, like the DCI, because nodes serve state by block hash only for recent blocks.
    #[tokio::test]
    #[ignore = "Requires RPC_URL pointing to an Ethereum node"]
    async fn test_traces_stored_token_to_eth_swap_with_current_router() {
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");
        let rpc = EthereumRpcClient::new(&rpc_url).expect("valid RPC_URL");
        let router = Address::from("0x2e234DAe75C793f67A35089C9d99245E1C58470b");
        let pool_manager = Address::from("0x000000000004444c5dc75cb358380d2e3de08a90");
        let vibe = Address::from("0xd34a99bc0f67ae1bbd63c660e6d0b0dd03e263b7");
        let slot_override = |slot: &str, value: &str| AccountOverrides {
            slots: Some(StorageOverride::Diff(BTreeMap::from([(
                Bytes::from(slot),
                Bytes::from(value),
            )]))),
            native_balance: None,
            code: None,
        };
        let state_overrides = BTreeMap::from([
            (
                Address::zero(20),
                AccountOverrides {
                    slots: None,
                    native_balance: Some(Bytes::from("0x8ac7230489e80000")),
                    code: None,
                },
            ),
            (
                pool_manager.clone(),
                slot_override(
                    "0x917ea27924cbfe58bb6fd69ccb53b65a1e5a06e38836d133174d60344c25bca4",
                    "0x000000000000000000000000000000000000000000000005b623a14e0c37c108",
                ),
            ),
            (
                router.clone(),
                AccountOverrides {
                    slots: None,
                    native_balance: None,
                    code: Some(Bytes::from("0xfe")),
                },
            ),
            (
                vibe,
                slot_override(
                    "0x44eb83fe218cddcf2da3422795d427fe76c5b3ee7b70d6d5e794446e27767fba",
                    "0x00000000000000000000000000000000000000000000000b6c47429c186f8210",
                ),
            ),
        ]);
        let calldata = Bytes::from(concat!(
            "0x09c5eabe",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000340",
            "0000000000000000000000000000000000000000000000000000000000000040",
            "0000000000000000000000000000000000000000000000000000000000000080",
            "0000000000000000000000000000000000000000000000000000000000000003",
            "060c0f0000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000003",
            "0000000000000000000000000000000000000000000000000000000000000060",
            "00000000000000000000000000000000000000000000000000000000000001e0",
            "0000000000000000000000000000000000000000000000000000000000000240",
            "0000000000000000000000000000000000000000000000000000000000000160",
            "0000000000000000000000000000000000000000000000000000000000000020",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "000000000000000000000000d34a99bc0f67ae1bbd63c660e6d0b0dd03e263b7",
            "0000000000000000000000000000000000000000000000000000000000002710",
            "000000000000000000000000000000000000000000000000000000000000003c",
            "000000000000000000000000c6c965bd164c483e87d0b550671798e9a3602840",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "000000000000000000000000000000000000000000000005b623a14e0c37c108",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000120",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000040",
            "000000000000000000000000d34a99bc0f67ae1bbd63c660e6d0b0dd03e263b7",
            "000000000000000000000000000000000000000000000005b623a14e0c37c108",
            "0000000000000000000000000000000000000000000000000000000000000040",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ));
        let stored = EntryPointWithTracingParams::new(
            EntryPoint::new(
                "0xc6c965bd164c483e87d0b550671798e9a3602840:execute(bytes)".to_string(),
                router.clone(),
                "execute(bytes)".to_string(),
            ),
            TracingParams::RPCTracer(
                RPCTracerParams::new(None, calldata)
                    .with_state_overrides(state_overrides)
                    .with_prune_addresses(vec![router.clone()]),
            ),
        );
        let tracer = RouterCodeTracer::new(
            EVMEntrypointService::new(&rpc),
            router,
            Bytes::from(entrypoint_generator::V4_MINI_ROUTER_BYTECODE),
        );
        let tip = rpc
            .get_block_number()
            .await
            .expect("block number");
        let block = rpc
            .get_block(tip, Chain::Ethereum)
            .await
            .expect("tip block");

        let mut results = tracer
            .trace(block.hash, vec![stored.clone()])
            .await;

        let traced = results
            .pop()
            .expect("one result")
            .expect("token->ETH swap traces");
        assert_eq!(traced.entry_point_with_params, stored);
        // PoolManager ERC6909 balanceOf[hook][VIBE]: only the VIBE->ETH direction touches it.
        let hook_vibe_claims =
            Bytes::from("0x13c37c52f9ded03ecb10e248d91546a6445f00a7cf52daf9e68f77ce851b5529");
        assert!(traced.tracing_result.accessed_slots[&pool_manager].contains(&hook_vibe_claims));
    }
}
