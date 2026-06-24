use tycho_common::dto::{
    AccountOverrides, AccountUpdate, BlockParam, Chain, ChangeType, ComponentTvlRequestBody,
    ComponentTvlRequestResponse, ContractId, EntryPoint, EntryPointWithTracingParams, Health,
    PaginationParams, PaginationResponse, ProtocolComponent, ProtocolComponentRequestResponse,
    ProtocolComponentsRequestBody, ProtocolId, ProtocolStateDelta, ProtocolStateRequestBody,
    ProtocolStateRequestResponse, ProtocolSystemsRequestBody, ProtocolSystemsRequestResponse,
    RPCTracerParams, ResponseAccount, ResponseProtocolState, ResponseToken, StateRequestBody,
    StateRequestResponse, StorageOverride, TokensRequestBody, TokensRequestResponse,
    TracedEntryPointRequestBody, TracedEntryPointRequestResponse, TracingParams, TracingResult,
    VersionParam,
};
use utoipa::{
    openapi::security::{ApiKey, ApiKeyValue, SecurityScheme},
    Modify, OpenApi,
};

use super::rpc;

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.as_mut().unwrap();
        components.add_security_scheme(
            "apiKey",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "authorization",
                "Use 'sampletoken' as value for testing",
            ))),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Tycho-Indexer RPC",),
    paths(
        rpc::health,
        rpc::protocol_systems,
        rpc::tokens,
        rpc::protocol_components,
        rpc::traced_entry_points,
        rpc::protocol_state,
        rpc::contract_state,
        rpc::component_tvl,
    ),
    components(
        schemas(VersionParam),
        schemas(BlockParam),
        schemas(ContractId),
        schemas(StateRequestResponse),
        schemas(StateRequestBody),
        schemas(Chain),
        schemas(ResponseAccount),
        schemas(TokensRequestBody),
        schemas(TokensRequestResponse),
        schemas(PaginationParams),
        schemas(PaginationResponse),
        schemas(ResponseToken),
        schemas(ProtocolComponentsRequestBody),
        schemas(ProtocolComponentRequestResponse),
        schemas(ProtocolComponent),
        schemas(ProtocolStateRequestBody),
        schemas(TracedEntryPointRequestBody),
        schemas(TracedEntryPointRequestResponse),
        schemas(ProtocolStateRequestResponse),
        schemas(AccountUpdate),
        schemas(ProtocolId),
        schemas(ResponseProtocolState),
        schemas(ChangeType),
        schemas(ProtocolStateDelta),
        schemas(Health),
        schemas(ProtocolSystemsRequestBody),
        schemas(ProtocolSystemsRequestResponse),
        schemas(ComponentTvlRequestBody),
        schemas(ComponentTvlRequestResponse),
        schemas(EntryPoint),
        schemas(EntryPointWithTracingParams),
        schemas(TracingResult),
        schemas(TracingParams),
        schemas(RPCTracerParams),
        schemas(AccountOverrides),
        schemas(StorageOverride),
    ),
    modifiers(&SecurityAddon),
)]
pub(super) struct ApiDoc;

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use utoipa::OpenApi;

    use super::ApiDoc;

    fn collect_refs(node: &Value, root: &Value, missing: &mut Vec<String>) {
        match node {
            Value::Object(map) => {
                for (key, value) in map {
                    if key == "$ref" {
                        if let Some(reference) = value.as_str() {
                            if let Some(pointer) = reference.strip_prefix('#') {
                                if root.pointer(pointer).is_none() {
                                    missing.push(reference.to_string());
                                }
                            }
                        }
                    } else {
                        collect_refs(value, root, missing);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_refs(item, root, missing);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn test_openapi_has_no_unresolved_refs() {
        let doc = ApiDoc::openapi();
        let json =
            serde_json::to_value(&doc).expect("OpenAPI document should serialize to JSON value");
        let mut missing = Vec::new();
        collect_refs(&json, &json, &mut missing);
        assert!(
            missing.is_empty(),
            "unresolved $ref targets present in OpenAPI document: {:?}",
            missing
        );
    }
}
