use std::{collections::HashMap, str::FromStr};

use tycho_client::feed::{synchronizer::ComponentWithState, BlockHeader};
use tycho_common::{models::token::Token, Bytes};

use super::state::FxSaveState;
use crate::{
    evm::{
        engine_db::{create_engine, SHARED_TYCHO_DB},
        protocol::fxsave::vm,
    },
    protocol::{
        errors::InvalidSnapshotError,
        models::{DecoderContext, TryFromWithBlock},
    },
};

/// Static attribute naming fxSP, the pool fxSAVE deposits into.
const FX_BASE_POOL_ATTRIBUTE: &str = "fx_base_pool";

impl TryFromWithBlock<ComponentWithState, BlockHeader> for FxSaveState {
    type Error = InvalidSnapshotError;

    async fn try_from_with_header(
        snapshot: ComponentWithState,
        _block: BlockHeader,
        _account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
        decoder_context: &DecoderContext,
    ) -> Result<Self, Self::Error> {
        let vault = Bytes::from_str(&snapshot.component.id).map_err(|e| {
            InvalidSnapshotError::ValueError(format!(
                "Expected component id to be the fxSAVE address: {e}"
            ))
        })?;
        let base_pool = snapshot
            .component
            .static_attributes
            .get(FX_BASE_POOL_ATTRIBUTE)
            .ok_or_else(|| {
                InvalidSnapshotError::MissingAttribute(FX_BASE_POOL_ATTRIBUTE.to_string())
            })?
            .clone();
        let token = |address: &Bytes| {
            all_tokens
                .get(address)
                .cloned()
                .ok_or_else(|| {
                    InvalidSnapshotError::ValueError(format!(
                        "fxSAVE {vault}: token {address} is missing from the token map"
                    ))
                })
        };
        for address in &snapshot.component.tokens {
            token(address)?;
        }

        let engine = create_engine(
            SHARED_TYCHO_DB.clone(),
            decoder_context
                .vm_traces
                .unwrap_or_default(),
        )?;
        let readings = vm::read_vault(&engine, &vault, &base_pool)?;
        let (stable_token, yield_token) = vm::read_tokens(&engine, &base_pool)?;

        Ok(FxSaveState::new(
            token(&vault)?,
            base_pool,
            token(&stable_token)?,
            token(&yield_token)?,
            readings,
        ))
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_common::{
        models::protocol::{ProtocolComponent, ProtocolComponentState},
        simulation::errors::SimulationError,
    };

    use super::*;
    use crate::evm::protocol::fxsave::state::tests::{token, FXSAVE, FXUSD, FX_BASE_POOL, USDC};

    async fn decode(
        snapshot: ComponentWithState,
        all_tokens: &HashMap<Bytes, Token>,
    ) -> Result<FxSaveState, InvalidSnapshotError> {
        FxSaveState::try_from_with_header(
            snapshot,
            Default::default(),
            &HashMap::default(),
            all_tokens,
            &Default::default(),
        )
        .await
    }

    fn all_tokens() -> HashMap<Bytes, Token> {
        [token(USDC, "USDC", 6), token(FXUSD, "fxUSD", 18), token(FXSAVE, "fxSAVE", 18)]
            .into_iter()
            .map(|t| (t.address.clone(), t))
            .collect()
    }

    fn snapshot(id: &str, fx_base_pool: Option<Bytes>) -> ComponentWithState {
        ComponentWithState {
            state: ProtocolComponentState {
                component_id: id.to_string(),
                attributes: HashMap::new(),
                balances: HashMap::new(),
            },
            component: ProtocolComponent {
                id: id.to_string(),
                tokens: [USDC, FXUSD, FXSAVE]
                    .iter()
                    .map(|t| Bytes::from_str(t).unwrap())
                    .collect(),
                static_attributes: fx_base_pool
                    .map(|address| (FX_BASE_POOL_ATTRIBUTE.to_string(), address))
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
            component_tvl: None,
            entrypoints: Vec::new(),
        }
    }

    fn fx_base_pool() -> Bytes {
        Bytes::from_str(FX_BASE_POOL).unwrap()
    }

    #[tokio::test]
    async fn id_that_is_not_hex_errors() {
        let result = decode(snapshot("fxsave", Some(fx_base_pool())), &all_tokens()).await;

        assert!(matches!(result, Err(InvalidSnapshotError::ValueError(_))));
    }

    #[tokio::test]
    async fn missing_fx_base_pool_errors() {
        let result = decode(snapshot(FXSAVE, None), &all_tokens()).await;

        assert!(matches!(
            result,
            Err(InvalidSnapshotError::MissingAttribute(name)) if name == "fx_base_pool"
        ));
    }

    #[rstest]
    #[case::usdc(USDC)]
    #[case::fxusd(FXUSD)]
    #[case::fxsave(FXSAVE)]
    #[tokio::test]
    async fn missing_token_errors(#[case] missing: &str) {
        let mut tokens = all_tokens();
        tokens.remove(&Bytes::from_str(missing).unwrap());

        let result = decode(snapshot(FXSAVE, Some(fx_base_pool())), &tokens).await;

        assert!(matches!(
            result,
            Err(InvalidSnapshotError::ValueError(message)) if message.contains(missing)
        ));
    }

    #[rstest]
    #[case::short_id("0x7743e50f534a7f9f1791dde7dcd89f7783eefc", Some(fx_base_pool()))]
    #[case::short_fx_base_pool(FXSAVE, Some(Bytes::from(vec![0x65; 19])))]
    #[tokio::test]
    async fn address_that_is_not_20_bytes_errors(
        #[case] id: &str,
        #[case] fx_base_pool: Option<Bytes>,
    ) {
        let result = decode(snapshot(id, fx_base_pool), &all_tokens()).await;

        assert!(matches!(
            result,
            Err(InvalidSnapshotError::VMError(SimulationError::FatalError(message)))
                if message.contains("not 20 bytes")
        ));
    }
}
