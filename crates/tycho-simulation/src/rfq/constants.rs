use std::time::Duration;

/// Default deadline for binding quote requests, seeded into every client builder.
pub const DEFAULT_QUOTE_TIMEOUT: Duration = Duration::from_secs(5);
