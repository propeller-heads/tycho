use anyhow::{Context, Result};
use opentelemetry::global;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    runtime,
    trace::{self, BatchConfigBuilder},
    Resource,
};
use serde::Deserialize;
use tracing::{debug, error, Subscriber};
use tracing_subscriber::{
    layer::SubscriberExt, registry::LookupSpan, util::SubscriberInitExt, EnvFilter, Layer,
};

#[derive(Debug, Clone, Deserialize)]
pub struct TracingConfig {
    pub otlp_exporter_endpoint: String,
}

/// Initialize tracing: apply an `EnvFilter` using the `RUST_LOG` environment variable to define the
/// log levels, add a formatter layer logging trace events as JSON and on OpenTelemetry layer
/// exporting trace data.
///
/// The OTLP layer uses `RUST_LOG_OTLP` if set, otherwise falls back to `RUST_LOG`. This allows
/// exporting detailed spans to Tempo without flooding stdout logs:
///
/// ```sh
/// RUST_LOG=info RUST_LOG_OTLP=tycho_storage::postgres=debug
/// ```
pub fn init_tracing(config: TracingConfig) -> Result<()> {
    global::set_text_map_propagator(TraceContextPropagator::new());

    global::set_error_handler(|err| {
        let msg = format!("{err:#}");
        // Downgrade noisy shutdown/transport errors so they don't flood ERROR logs
        if msg.contains("channel is closed") || msg.contains("batch processor") {
            debug!(error = %msg, "otel error");
        } else {
            error!(error = %msg, "otel error");
        }
    })
    .context("set error handler")?;

    let format = tracing_subscriber::fmt::format()
        .with_level(true)
        .with_target(false)
        .compact();

    let fmt_filter = EnvFilter::from_default_env();
    let otlp_filter = otlp_env_filter();

    tracing_subscriber::registry()
        .with(otlp_layer(config)?.with_filter(otlp_filter))
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(format)
                .with_filter(fmt_filter),
        )
        .try_init()
        .context("initialize tracing subscriber")
}

/// Build the `EnvFilter` for the OTLP layer.
/// Uses `RUST_LOG_OTLP` if set, otherwise falls back to `RUST_LOG` / default.
fn otlp_env_filter() -> EnvFilter {
    if let Ok(val) = std::env::var("RUST_LOG_OTLP") {
        EnvFilter::new(val)
    } else {
        EnvFilter::from_default_env()
    }
}

/// Create an OTLP layer exporting tracing data.
fn otlp_layer<S>(config: TracingConfig) -> Result<impl Layer<S>>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    let exporter = opentelemetry_otlp::new_exporter()
        .tonic()
        .with_endpoint(config.otlp_exporter_endpoint);

    let trace_config = trace::config().with_resource(Resource::default());

    let batch_config = BatchConfigBuilder::default()
        .with_max_queue_size(20_480)
        .with_max_export_batch_size(2_560)
        .build();

    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(exporter)
        .with_trace_config(trace_config)
        .with_batch_config(batch_config)
        .install_batch(runtime::Tokio)
        .context("install tracer")?;

    // Source location and thread identity are constant per span name and repeat on every
    // exported span. Drop them; the span name and its parent chain already identify the code
    // path. Busy/idle timings stay on, they differ per span.
    Ok(tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_location(false)
        .with_threads(false))
}
