mod config;
mod decoder;
mod etherscan;
mod renderer;
mod signatures;

// Private imports for internal use only
use config::TraceConfig;
use decoder::CallTraceDecoder;
use renderer::render_trace_arena;
// Re-export only what's needed by simulation.rs
use revm_inspectors::tracing::CallTraceArena;
pub use signatures::SignaturesIdentifier;

/// Type alias for the traces collection (internal use only)
type Traces = Vec<CallTraceArena>;

/// A slimmed down return from the executor used for returning minimal trace + gas metering info
#[derive(Debug, Clone)]
pub(crate) struct TraceResult {
    pub success: bool,
    pub traces: Option<Traces>,
    pub gas_used: u64,
}

/// Handle traces with Etherscan & 4Byte identification and pretty printing
/// This is the main public function used by SimulationEngine
pub(crate) async fn handle_traces(
    mut result: TraceResult,
    etherscan_api_key: Option<String>,
    chain: tycho_common::models::Chain,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let offline = etherscan_api_key.is_none();
    let trace_config = TraceConfig::new(chain)
        .with_etherscan_api_key(etherscan_api_key.unwrap_or_default())
        .with_offline(offline);

    let mut decoder = CallTraceDecoder::with_config(&trace_config).await?;

    // Decode traces
    if let Some(traces) = result.traces.as_mut() {
        for arena in traces {
            decoder.decode_arena(arena).await?;
        }
    }

    print_traces(&result, &decoder).await?;

    Ok(())
}

/// Print decoded traces to console (internal use only)
async fn print_traces(
    result: &TraceResult,
    _decoder: &CallTraceDecoder, // Decoder already applied in handle_traces
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let traces = result
        .traces
        .as_ref()
        .ok_or_else(|| std::io::Error::other("No traces found"))?;

    println!("Traces:");
    for arena in traces {
        let rendered = render_trace_arena(arena);
        println!("{}", rendered);
    }
    println!();

    if result.success {
        println!("Transaction successfully executed.");
    } else {
        println!("Transaction failed.");
    }

    println!("Gas used: {gas}", gas = result.gas_used);
    Ok(())
}
