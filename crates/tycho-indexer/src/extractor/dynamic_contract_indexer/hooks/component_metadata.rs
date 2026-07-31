#![allow(dead_code)] // TODO: Remove when implementation is complete

use std::{collections::HashMap, fmt::Debug, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tycho_common::{
    models::{
        blockchain::{Block, EntryPointWithTracingParams},
        protocol::ProtocolComponent,
        Address, ComponentId, TxHash,
    },
    Bytes,
};

// Core Types

// Core Metadata structure. Each Component that has a hook is expected to have Some (or None in case
// of error) of those fields by the end of the extraction process.

pub(super) type Balances = HashMap<Address, Bytes>;
// Here we link the limits to the entrypoint that triggered the limit fetching. This is necessary
// so that Tycho Simulation can use this information to calculate the limits for the component on
// every block.
// Represented by (tokenIn, tokenOut), (maxAmountIn, maxAmountOut, Entrypoint)
type Limits = Vec<((Address, Address), (Bytes, Bytes, Option<EntryPointWithTracingParams>))>;
type Tvl = f64;
pub(super) type DeduplicationId = String;

type RoutingKey = String;

#[cfg_attr(test, derive(Debug))]
#[derive(Clone)]
pub(super) struct ComponentTracingMetadata {
    // Here we need to link the each metadata field with the transaction hash that triggered the
    // fetching so we can modify the Block data with the new balances - and link the generated
    // Entrypoints to the correct transaction hash.
    tx_hash: TxHash,
    balances: Option<Result<Balances, MetadataError>>,
    limits: Option<Result<Limits, MetadataError>>,
    tvl: Option<Result<Tvl, MetadataError>>,
}

impl ComponentTracingMetadata {
    pub(super) fn new(tx_hash: TxHash) -> Self {
        Self { tx_hash, balances: None, limits: None, tvl: None }
    }

    pub(super) fn with_balances(self, balances: Result<Balances, MetadataError>) -> Self {
        Self { balances: Some(balances), ..self }
    }

    pub(super) fn with_limits(self, limits: Result<Limits, MetadataError>) -> Self {
        Self { limits: Some(limits), ..self }
    }

    pub(super) fn with_tvl(self, tvl: Result<Tvl, MetadataError>) -> Self {
        Self { tvl: Some(tvl), ..self }
    }

    pub(super) fn add_result(&mut self, result: MetadataResult) {
        match result.result {
            Ok(MetadataValue::Balances(balances)) => {
                self.balances = Some(Ok(balances));
            }
            Ok(MetadataValue::Limits(new_limits)) => {
                match &mut self.limits {
                    Some(Ok(existing_limits)) => {
                        // Merge new limits with existing limits
                        existing_limits.extend(new_limits);
                    }
                    _ => {
                        // No existing limits or existing was an error, set new limits
                        self.limits = Some(Ok(new_limits));
                    }
                }
            }
            Ok(MetadataValue::Tvl(tvl)) => {
                self.tvl = Some(Ok(tvl));
            }
            Err(e) => match result.request_type {
                MetadataRequestType::ComponentBalance { .. } => {
                    self.balances = Some(Err(e));
                }
                MetadataRequestType::Limits { .. } => {
                    self.limits = Some(Err(e));
                }
                MetadataRequestType::Tvl => {
                    self.tvl = Some(Err(e));
                }
            },
        }
    }

    pub(super) fn balances(&self) -> Option<&Result<Balances, MetadataError>> {
        self.balances.as_ref()
    }

    pub(super) fn limits(&self) -> Option<&Result<Limits, MetadataError>> {
        self.limits.as_ref()
    }

    pub(super) fn tvl(&self) -> Option<&Result<Tvl, MetadataError>> {
        self.tvl.as_ref()
    }

    pub(super) fn tx_hash(&self) -> &TxHash {
        &self.tx_hash
    }
}

// Request Generation Types

type RequestId = String;
// Represents a request to a provider.
pub(super) struct MetadataRequest {
    generator_name: String,
    request_id: RequestId,
    component_id: ComponentId,
    request_type: MetadataRequestType,
    transport: Box<dyn RequestTransport>,
}

impl Clone for MetadataRequest {
    fn clone(&self) -> Self {
        Self {
            generator_name: self.generator_name.clone(),
            request_id: self.request_id.clone(),
            component_id: self.component_id.clone(),
            request_type: self.request_type.clone(),
            transport: self.transport.clone_box(),
        }
    }
}

impl MetadataRequest {
    pub(super) fn new(
        generator_name: String,
        request_id: RequestId,
        component_id: ComponentId,
        request_type: MetadataRequestType,
        transport: Box<dyn RequestTransport>,
    ) -> Self {
        Self { generator_name, request_id, component_id, request_type, transport }
    }

    pub(super) fn get_generator_name(&self) -> &str {
        &self.generator_name
    }

    pub(super) fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    pub(super) fn component_id(&self) -> &ComponentId {
        &self.component_id
    }

    pub(super) fn request_type(&self) -> &MetadataRequestType {
        &self.request_type
    }

    pub(super) fn transport(&self) -> &dyn RequestTransport {
        self.transport.as_ref()
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub(super) enum MetadataRequestType {
    ComponentBalance { token_addresses: Vec<Address> },
    Tvl,
    // Every request should cover only one token pair - but we should leave the interface
    // open to allow for future extensions or requests that cover multiple token pairs.
    Limits { token_pair: Vec<(Address, Address)> },
}

/// Specifies a transport mechanism for sending metadata requests to external data providers.
///
/// This trait abstracts the transport layer, allowing different implementations for various
/// protocols (HTTP, RPC, WebSocket, GraphQL etc.) and chains. Implementations handle the low-level
/// details of how requests are formatted and transmitted.
///
/// # Examples
///
/// Common implementations include:
/// - `DefiLLamaHttpTransport`: For REST API calls to Defillama API (e.g., DeFiLlama TVL data)
/// - `RpcTransport`: For JSON-RPC calls to blockchain nodes
pub(super) trait RequestTransport: Send + Sync + Debug {
    /// Returns a routing key that identifies which provider should handle this request.
    ///
    /// The routing key groups requests by their destination provider, enabling efficient
    /// batching and connection pooling. For example, all RPC requests might share a routing key
    /// like "rpc_mainnet". All API requests to the same provider, like Defillama should share
    /// a routing key like defillama_api_v1
    ///
    /// # Returns
    ///
    /// A string identifier for routing requests to the appropriate provider.
    fn routing_key(&self) -> String;

    /// Sets a custom routing key for this transport.
    ///
    /// # Arguments
    ///
    /// * `key` - The new routing key to use for this transport
    fn set_routing_key(&mut self, key: String);

    /// Returns a unique identifier for request deduplication.
    ///
    /// This ID helps prevent duplicate requests within a batch. Transports should generate
    /// IDs based on the request content, so identical requests produce the same ID.
    ///
    /// # Returns
    ///
    /// A string that uniquely identifies this request's content for deduplication purposes.
    ///
    /// # Implementation Notes
    ///
    /// - Should be deterministic: same request parameters always produce the same ID
    /// - Should include all relevant request data (method, parameters, etc.)
    /// - Consider using a hash function for complex request data
    fn deduplication_id(&self) -> String;

    /// Creates a boxed clone of this transport.
    ///
    /// Required for cloning trait objects, as Rust's standard `Clone` trait cannot be
    /// object-safe. This enables storing and passing transports as trait objects.
    ///
    /// # Returns
    ///
    /// A new boxed instance with the same state as this transport.
    /// TODO: Might be removed if not necessary on the final implementations
    fn clone_box(&self) -> Box<dyn RequestTransport>;

    /// Returns a reference to the concrete type as `Any`.
    ///
    /// This enables downcasting to the specific transport implementation when needed,
    /// allowing providers to access transport-specific data and methods.
    ///
    /// # Returns
    ///
    /// A reference to this transport as a trait object that can be downcast.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Generates metadata requests for protocol components.
///
/// This trait is responsible for analyzing a protocol component and determining what metadata
/// needs to be fetched. It creates the appropriate requests but does not execute them.
/// Different implementations can specialize in different types of protocols or metadata.
///
/// # Design Philosophy
///
/// Generators are protocol-aware but transport-agnostic. They understand what data is needed
/// based on the component type and current blockchain state, but delegate the actual fetching
/// to providers through transport objects.
///
/// # Implementation Guidelines
///
/// - Generators should be stateless when possible
/// - Each generator typically specializes in one protocol or hook type
/// - Generators can create requests with different transport types as needed. Example: using RPC
///   transport for Balances and API calls for TVL
#[cfg_attr(test, mockall::automock)]
pub(super) trait MetadataRequestGenerator: Send + Sync {
    /// Generates all metadata requests needed for a component.
    ///
    /// Analyzes the component's state and the current block to determine what metadata
    /// should be fetched. This includes balances, limits, TVL, and any other relevant data.
    ///
    /// # Arguments
    ///
    /// * `component` - The protocol component to generate requests for
    /// * `block` - The current block context, used to determine state changes
    ///
    /// # Returns
    ///
    /// A vector of metadata requests with appropriate transport configurations, or an error
    /// if request generation fails.
    ///
    /// # Example Requests Generated
    ///
    /// - Token balance from onchain calls via RPC
    /// - TVL data from external APIs
    /// - Trading limits from onchain calls via RPC
    fn generate_requests(
        &self,
        component: &ProtocolComponent,
        block: &Block,
    ) -> Result<Vec<MetadataRequest>, MetadataError>;

    /// Generates only balance-related metadata requests for a component.
    ///
    /// This is a specialized method for components that only need balance updates,
    /// providing an optimized path for lightweight metadata collection.
    ///
    /// # Arguments
    ///
    /// * `component` - The protocol component to generate balance requests for
    /// * `block` - The current block context
    ///
    /// # Returns
    ///
    /// A vector of balance-specific metadata requests, or an error if generation fails.
    ///
    /// # Use Cases
    ///
    /// - Components marked for balance-only updates
    /// - Periodic balance reconciliation
    /// - Lightweight monitoring scenarios
    fn generate_balance_only_requests(
        &self,
        component: &ProtocolComponent,
        block: &Block,
    ) -> Result<Vec<MetadataRequest>, MetadataError>;

    /// Returns the types of metadata this generator can produce.
    ///
    /// Used by the orchestrator to understand generator capabilities and ensure
    /// all required metadata types are covered by available generators.
    ///
    /// # Returns
    ///
    /// A vector of metadata request types this generator supports.
    ///
    /// # Example
    ///
    /// A DEX generator might return:
    /// ```ignore
    /// vec![
    ///     MetadataRequestType::ComponentBalance { token_addresses: vec![] },
    ///     MetadataRequestType::Limits { token_pair: vec![] },
    ///     MetadataRequestType::Tvl,
    /// ]
    /// ```
    fn supported_metadata_types(&self) -> Vec<MetadataRequestType>;
}

#[cfg_attr(test, mockall::automock)]
pub(super) trait MetadataResponseParser: Send + Sync {
    /// Parses the response from the provider and returns the metadata value.
    ///
    /// # Arguments
    ///
    /// * `component` - The protocol component that the request is for
    /// * `request_type` - The type of request that was made
    /// * `response` - The response from the provider
    ///
    /// # Returns
    ///
    /// A `MetadataValue` that represents the parsed metadata.
    ///
    /// # Errors
    ///
    /// Returns a `MetadataError` if the response cannot be parsed.
    fn parse_response(
        &self,
        component: &ProtocolComponent,
        request: &MetadataRequest,
        response: &Value,
    ) -> Result<MetadataValue, MetadataError>;
}

pub(super) struct MetadataResponseParserRegistry {
    // Map of protocol name to responseparser
    parsers: HashMap<String, Box<dyn MetadataResponseParser>>,
}

impl Default for MetadataResponseParserRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataResponseParserRegistry {
    pub(super) fn new() -> Self {
        Self { parsers: HashMap::new() }
    }

    pub(super) fn register_parser(
        &mut self,
        protocol_name: String,
        parser: Box<dyn MetadataResponseParser>,
    ) {
        self.parsers
            .insert(protocol_name, parser);
    }

    pub(super) fn get_parser(&self, generator_name: &str) -> Option<&dyn MetadataResponseParser> {
        self.parsers
            .get(generator_name)
            .map(|boxed_parser| boxed_parser.as_ref())
    }
}
/// Registry that manages the mapping between protocol components and their metadata generators.
///
/// This registry is the central configuration point for determining which generator should
/// handle metadata requests for a given component. It supports both specific hook-based
/// mappings and a default fallback generator.
///
/// # Architecture
///
/// The registry uses a two-tier lookup system:
/// 1. **Hook-specific generators**: Mapped by the hook address from the component's static
///    attributes
/// 2. **Default generator**: Fallback for components without specific mappings
///
/// # Usage
///
/// The registry is typically configured during application initialization with generators
/// for each supported protocol or hook type. During runtime, the orchestrator queries
/// this registry to find the appropriate generator for each component.
///
/// # Example Configuration
///
/// ```ignore
/// let mut registry = MetadataGeneratorRegistry::new();
///
/// // Register specific generators for known hooks
/// registry.register_hook_generator(bunni_hook, BunniGenerator::new());
/// registry.register_hook_generator(euler_hook, EulerGenerator::new());
///
/// // Set a default generator for unknown hooks
/// registry.set_default_generator(GenericDexGenerator::new());
/// ```
pub(super) struct MetadataGeneratorRegistry {
    /// Maps hook addresses to their specific generator instances.
    ///
    /// Each hook address corresponds to a protocol-specific generator that understands
    /// how to create metadata requests for components using that hook.
    hook_generators: HashMap<Address, Box<dyn MetadataRequestGenerator>>,

    /// Maps hook identifiers to their specific generator instances.
    ///
    /// Each hook identifier corresponds to a protocol-specific generator that understands
    /// how to create metadata requests for components using that hook identifier.
    hook_identifiers: HashMap<String, Box<dyn MetadataRequestGenerator>>,

    /// Fallback generator for components without specific hook mappings.
    ///
    /// This ensures that all components can have metadata generated, even if they
    /// use an unknown or new hook type. The default generator should implement
    /// conservative, generic metadata collection strategies.
    default_generator: Option<Box<dyn MetadataRequestGenerator>>,
}

impl Default for MetadataGeneratorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataGeneratorRegistry {
    pub(super) fn new() -> Self {
        Self {
            hook_generators: HashMap::new(),
            hook_identifiers: HashMap::new(),
            default_generator: None,
        }
    }

    pub(super) fn register_hook_generator(
        &mut self,
        hook_address: Address,
        generator: Box<dyn MetadataRequestGenerator>,
    ) {
        self.hook_generators
            .insert(hook_address, generator);
    }

    pub(super) fn register_hook_identifier(
        &mut self,
        hook_identifier: String,
        generator: Box<dyn MetadataRequestGenerator>,
    ) {
        self.hook_identifiers
            .insert(hook_identifier, generator);
    }

    pub(super) fn set_default_generator(
        &mut self,
        generator: Option<Box<dyn MetadataRequestGenerator>>,
    ) {
        self.default_generator = generator;
    }

    /// Retrieves the appropriate metadata generator for a given component.
    ///
    /// This method implements the lookup logic, first checking for a hook-specific
    /// generator, then falling back to the hook identifier generator, then falling back to the
    /// default generator if available.
    ///
    /// # Arguments
    ///
    /// * `component` - The protocol component to find a generator for
    ///
    /// # Returns
    ///
    /// - `Some(&dyn MetadataRequestGenerator)` if a suitable generator is found
    /// - `None` if no generator is registered for the component's hook and no default is set
    /// - `Err(MetadataError::MissingData(field, component_id))` if the component does not have a
    ///   hook
    ///
    /// # Lookup Process
    ///
    /// 1. Extract the hook address from the component's static attributes
    /// 2. Look for a hook-specific generator in the registry
    /// 3. If not found, use the default generator if configured
    /// 4. Return None if no suitable generator exists
    ///
    /// # Example
    ///
    /// ```ignore
    /// if let Some(generator) = registry.get_generator_for_component(&component) {
    ///     let requests = generator.generate_requests(&component, &block)?;
    ///     // Process requests...
    /// }
    /// ```
    pub(super) fn get_generator_for_component(
        &self,
        component: &ProtocolComponent,
    ) -> Result<Option<&dyn MetadataRequestGenerator>, MetadataError> {
        let hook_address = component
            .static_attributes
            .get("hooks")
            .ok_or_else(|| MetadataError::MissingData("hooks".to_string(), component.id.clone()))?;

        // Priority: address > identifier > default
        match self.hook_generators.get(hook_address) {
            // Found generator by address (highest priority)
            Some(generator) => Ok(Some(generator.as_ref())),
            // No address match, try identifier if available
            None => match component
                .static_attributes
                .get("hook_identifier")
            {
                Some(hook_identifier_bytes) => {
                    let identifier =
                        String::from_utf8(hook_identifier_bytes.to_vec()).map_err(|_| {
                            MetadataError::UnknownError("Invalid hook identifier".to_string())
                        })?;
                    match self.hook_identifiers.get(&identifier) {
                        Some(generator) => Ok(Some(generator.as_ref())),
                        None => Ok(self.default_generator.as_deref()),
                    }
                }
                // No identifier, use default
                None => Ok(self.default_generator.as_deref()),
            },
        }
    }
}

// Request Execution Types

/// Executes metadata requests by interacting with external data sources.
///
/// Providers handle the actual communication with external systems (RPC nodes, APIs, etc.)
/// and implement optimizations like batching, deduplication, and connection pooling.
/// Each provider typically specializes in one transport type and protocol.
///
/// # Architecture
///
/// The provider layer sits between the high-level request generation and the low-level
/// transport execution. It adds intelligence for:
///
/// - Request batching (e.g., multicall for RPC)
/// - Deduplication of identical requests
/// - Connection management and pooling
/// - Error handling and retries
/// - Response parsing and normalization
///
/// # Implementation Requirements
///
/// - Each provider handles one specific `RequestTransport` type
/// - Providers must be thread-safe for concurrent execution
/// - Batch operations should be atomic when possible
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub(super) trait RequestProvider: Send + Sync {
    /// Executes a batch of requests and returns their results.
    ///
    /// This is the main entry point for request execution. Providers should implement
    /// optimizations like deduplication and batching internally. Results are returned
    /// in the same order as requests, with errors for failed requests.
    ///
    /// # Arguments
    ///
    /// * `requests` - Slice of boxed transport objects to execute
    ///
    /// # Returns
    ///
    /// A vector of results corresponding to each request. Failed requests return
    /// `MetadataError` in their result field.
    ///
    /// # Implementation Notes
    ///
    /// - Providers should downcast transports to their expected type
    /// - Batch size limits should be respected (e.g., RPC batch limits)
    /// - Connection pooling should be used for network efficiency
    /// - Duplicate requests within the batch should be deduplicated
    ///
    /// # Example
    ///
    /// An RPC provider might:
    /// 1. Group requests by endpoint
    /// 2. Deduplicate identical calls
    /// 3. Batch into multicall or JSON-RPC batch requests
    /// 4. Execute with connection pooling
    /// 5. Map results back to original requests
    async fn execute_batch(
        &self,
        requests: &[Box<dyn RequestTransport>],
    ) -> Vec<(DeduplicationId, Result<Value, MetadataError>)>;

    /// Groups requests into optimal batches for execution.
    ///
    /// Providers can override this to implement smart batching strategies.
    /// The default implementation treats each request independently.
    ///
    /// # Arguments
    ///
    /// * `requests` - Slice of requests to potentially group
    ///
    /// # Returns
    ///
    /// A vector of request groups, where each group can be executed together.
    ///
    /// # Batching Strategies
    ///
    /// - **RPC Providers**: Always group eth_calls for multicall
    /// - **API Providers**: Group by API endpoint and rate limits
    /// - **Default**: Each request in its own group (no batching)
    /// ```
    fn group_requests(
        &self,
        requests: &[Box<dyn RequestTransport>],
        _batch_size_limit: usize,
    ) -> Vec<Vec<Box<dyn RequestTransport>>> {
        // Default: no grouping, one request per call
        requests
            .iter()
            .map(|r| vec![r.clone_box()])
            .collect()
    }
}

pub(super) struct MetadataResult {
    request_id: RequestId,
    component_id: ComponentId,
    request_type: MetadataRequestType,
    result: Result<MetadataValue, MetadataError>,
}

impl MetadataResult {
    pub(super) fn new(
        request_id: RequestId,
        component_id: ComponentId,
        request_type: MetadataRequestType,
        result: Result<MetadataValue, MetadataError>,
    ) -> Self {
        Self { request_id, component_id, request_type, result }
    }

    pub(super) fn component_id(&self) -> &ComponentId {
        &self.component_id
    }

    pub(super) fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    pub(super) fn request_type(&self) -> &MetadataRequestType {
        &self.request_type
    }

    pub(super) fn result(&self) -> &Result<MetadataValue, MetadataError> {
        &self.result
    }
}

// Simple enum for actual metadata values
#[derive(Debug, Clone, PartialEq)]
pub(super) enum MetadataValue {
    Balances(HashMap<Address, Bytes>),
    Limits(Limits),
    Tvl(f64),
}

// Provider registry with configurable routing keys
pub(super) struct ProviderRegistry {
    providers: HashMap<RoutingKey, Arc<dyn RequestProvider>>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    pub(super) fn new() -> Self {
        Self { providers: HashMap::new() }
    }

    pub(super) fn register_provider(
        &mut self,
        routing_key: String,
        provider: Arc<dyn RequestProvider>,
    ) {
        self.providers
            .insert(routing_key, provider);
    }

    pub(super) fn get_provider(
        &self,
        transport: &dyn RequestTransport,
    ) -> Option<Arc<dyn RequestProvider>> {
        self.providers
            .get(&transport.routing_key())
            .cloned()
    }

    pub(super) fn get_provider_by_routing_key(
        &self,
        routing_key: &str,
    ) -> Option<Arc<dyn RequestProvider>> {
        self.providers.get(routing_key).cloned()
    }
}

// Error types
#[derive(Error, Debug, Clone, PartialEq)]
pub(super) enum MetadataError {
    #[error("Metadata generation failed: {0}")]
    GenerationFailed(String),
    #[error("Provider failed: {0}")]
    ProviderFailed(String),
    #[error("Request failed: {0}")]
    RequestFailed(String),
    #[error("Unknown error: {0}")]
    UnknownError(String),
    #[error("Missing {0} for {1}")]
    MissingData(String, String),
}

// Example Implementations

// HTTP Transport Example Implementation
#[derive(Clone, Debug, Serialize, Deserialize)]
struct HttpTransport {
    url: String,
    method: HttpMethod,
    headers: HashMap<String, String>,
    body: Option<serde_json::Value>,
    routing_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum HttpMethod {
    Get,
    Post,
}

impl HttpTransport {
    fn new(url: String, method: HttpMethod) -> Self {
        let routing_key = format!(
            "http_{}",
            url.split('/')
                .nth(2)
                .unwrap_or("default")
        );
        Self { url, method, headers: HashMap::new(), body: None, routing_key }
    }

    fn with_header(mut self, key: String, value: String) -> Self {
        self.headers.insert(key, value);
        self
    }

    fn with_body(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }
}

impl RequestTransport for HttpTransport {
    fn routing_key(&self) -> String {
        self.routing_key.clone()
    }

    fn set_routing_key(&mut self, key: String) {
        self.routing_key = key;
    }

    fn deduplication_id(&self) -> String {
        // Create unique ID based on URL and body. Please use a hash function.
        let body_str = self
            .body
            .as_ref()
            .map(|b| b.to_string())
            .unwrap_or_default();
        format!("{}_{}_{}", self.method.as_str(), self.url, body_str)
    }

    fn clone_box(&self) -> Box<dyn RequestTransport> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl HttpMethod {
    fn as_str(&self) -> &str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
        }
    }
}

// RPC Transport Implementation
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct RpcTransport {
    endpoint: String,
    method: String,
    params: Vec<serde_json::Value>,
    id: u64,
    routing_key: String,
}

impl RpcTransport {
    pub(super) fn new(endpoint: String, method: String, params: Vec<serde_json::Value>) -> Self {
        // Extract chain identifier from endpoint for routing. Since there is no expectation of
        // separation between RPC requests and providers, we will use a default routing key.
        let routing_key = "rpc_default".to_string();

        Self { endpoint, method, params, id: rand::random::<u64>() % 10000, routing_key }
        //TODO: use a better id that ensure no collisions at all
    }

    pub(super) fn eth_call(
        endpoint: String,
        contract: Address,
        data: Bytes,
        block: Option<String>,
    ) -> Self {
        let call_object = serde_json::json!({
            "to": format!("0x{}", hex::encode(contract.as_ref())),
            "data": format!("0x{}", hex::encode(data.as_ref()))
        });

        let params = if let Some(block_tag) = block {
            vec![call_object, serde_json::Value::String(block_tag)]
        } else {
            vec![call_object, serde_json::Value::String("latest".to_string())]
        };

        Self::new(endpoint, "eth_call".to_string(), params)
    }

    pub(super) fn multicall(
        endpoint: String,
        calls: Vec<(Address, Bytes)>,
        block: Option<String>,
    ) -> Self {
        // Create multicall3 aggregate call data
        // Function selector for aggregate3((address,bool,bytes)[])
        let selector = hex::decode("82ad56cb").unwrap();
        let call_data = selector;

        // Encode array of calls
        // This is simplified - in production you'd use proper ABI encoding
        let _encoded_calls = calls
            .iter()
            .map(|(target, data)| {
                serde_json::json!({
                    "target": format!("0x{}", hex::encode(target.as_ref())),
                    "allowFailure": true,
                    "callData": format!("0x{}", hex::encode(data.as_ref()))
                })
            })
            .collect::<Vec<_>>();

        // Multicall3 contract address (same on most chains)
        let multicall_address =
            Address::from(hex::decode("cA11bde05977b3631167028862bE2a173976CA11").unwrap());

        Self::eth_call(endpoint, multicall_address, Bytes::from(call_data), block)
    }

    pub(super) fn params(&self) -> &Vec<serde_json::Value> {
        &self.params
    }

    pub(super) fn id(&self) -> u64 {
        self.id
    }

    pub(super) fn endpoint(&self) -> &String {
        &self.endpoint
    }

    pub(super) fn method(&self) -> &String {
        &self.method
    }
}

impl RequestTransport for RpcTransport {
    fn routing_key(&self) -> String {
        self.routing_key.clone()
    }

    fn set_routing_key(&mut self, key: String) {
        self.routing_key = key;
    }

    fn deduplication_id(&self) -> String {
        // Create unique ID based on method and params
        let params_str = serde_json::to_string(&self.params).unwrap_or_default();
        format!("{}_{}", self.method, params_str)
    }

    fn clone_box(&self) -> Box<dyn RequestTransport> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use rstest::*;

    use super::*;

    fn create_test_component(id: &str, hook_address: Option<Address>) -> ProtocolComponent {
        let mut static_attributes = HashMap::new();
        if let Some(hook) = hook_address {
            static_attributes.insert("hooks".to_string(), hook);
        }

        ProtocolComponent { id: id.to_string(), static_attributes, ..Default::default() }
    }

    #[rstest]
    #[case::hook_with_registered_generator(true, true, true)] // Should return hook generator
    #[case::hook_without_registered_generator(true, false, true)] // Should return default generator
    #[case::hook_without_any_generator(true, false, false)] // Should return None
    #[case::no_hook_with_default_generator(false, false, true)] // Should return error (no hook)
    #[case::no_hook_without_default_generator(false, false, false)] // Should return error (no hook)
    fn test_get_generator_for_component(
        #[case] has_hook: bool,
        #[case] has_hook_generator: bool,
        #[case] has_default_generator: bool,
    ) {
        let mut registry = MetadataGeneratorRegistry::new();
        let hook_address = Address::from(Bytes::from(vec![1u8; 20]));

        if has_hook_generator {
            registry.register_hook_generator(
                hook_address.clone(),
                Box::new(MockMetadataRequestGenerator::new()),
            );
        }

        if has_default_generator {
            registry.set_default_generator(Some(Box::new(MockMetadataRequestGenerator::new())));
        }

        let component = create_test_component(
            "test_component",
            if has_hook { Some(hook_address) } else { None },
        );

        let generator_result = registry.get_generator_for_component(&component);

        if has_hook {
            // Component has a hook, so we expect Ok(Some(...)) or Ok(None)
            let generator =
                generator_result.expect("Should not return error for component with hook");
            let should_have_generator = has_hook_generator || has_default_generator;
            assert_eq!(generator.is_some(), should_have_generator);
        } else {
            // Component has no hook, so we expect an error
            assert!(generator_result.is_err());
            if let Err(MetadataError::MissingData(field, component_id)) = generator_result {
                assert_eq!(field, "hooks");
                assert_eq!(component_id, "test_component");
            } else {
                panic!("Expected MissingData error for component without hook");
            }
        }
    }

    #[test]
    fn test_provider_registry() {
        let mut registry = ProviderRegistry::new();
        assert!(registry.providers.is_empty());

        // Create a mock provider
        let mock_provider = Arc::new(MockRequestProvider::new());

        // Register provider
        let routing_key = "test_provider".to_string();
        registry.register_provider(routing_key.clone(), mock_provider);

        assert_eq!(registry.providers.len(), 1);
        assert!(registry
            .providers
            .contains_key(&routing_key));

        let mut transport = HttpTransport::new("http://example.com".to_string(), HttpMethod::Get);
        let provider = registry.get_provider(&transport);
        assert!(provider.is_none()); // Should be none since transport has different routing key

        // Set the routing key to the correct one
        transport.set_routing_key(routing_key.clone());
        let provider = registry.get_provider(&transport);
        assert!(provider.is_some());
    }
}
