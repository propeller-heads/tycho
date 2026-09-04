use std::{env, str::FromStr};

use tycho_common::Bytes;

use crate::rfq::errors::RFQError;

pub const DEFAULT_METRIC_API_URL: &str = "https://api.metric.xyz";

/// Hashflow authentication configuration
pub struct HashflowAuth {
    pub user: String,
    pub key: String,
}

/// Bebop authentication configuration
pub struct BebopAuth {
    pub key: String,
}

/// Metric API configuration
pub struct MetricConfig {
    pub base_url: String,
    pub api_key: Option<String>,
}

/// Read Hashflow authentication from environment variables
/// Returns the HASHFLOW_USER and HASHFLOW_KEY environment variables
pub fn get_hashflow_auth() -> Result<HashflowAuth, RFQError> {
    let user = env::var("HASHFLOW_USER").map_err(|_| {
        RFQError::InvalidInput("HASHFLOW_USER environment variable is required".into())
    })?;

    let key = env::var("HASHFLOW_KEY").map_err(|_| {
        RFQError::InvalidInput("HASHFLOW_KEY environment variable is required".into())
    })?;

    Ok(HashflowAuth { user, key })
}

/// Liquorice authentication configuration
pub struct LiquoriceAuth {
    pub solver: String,
    pub key: String,
}

/// Read Liquorice authentication from environment variables
/// Returns the LIQUORICE_USER and LIQUORICE_KEY environment variables
pub fn get_liquorice_auth() -> Result<LiquoriceAuth, RFQError> {
    let solver = env::var("LIQUORICE_USER").map_err(|_| {
        RFQError::InvalidInput("LIQUORICE_USER environment variable is required".into())
    })?;

    let key = env::var("LIQUORICE_KEY").map_err(|_| {
        RFQError::InvalidInput("LIQUORICE_KEY environment variable is required".into())
    })?;

    Ok(LiquoriceAuth { solver, key })
}

/// Read Bebop authentication from environment variables
/// Returns the BEBOP_KEY environment variable
pub fn get_bebop_auth() -> Result<BebopAuth, RFQError> {
    let key = env::var("BEBOP_KEY")
        .map_err(|_| RFQError::InvalidInput("BEBOP_KEY environment variable is required".into()))?;

    Ok(BebopAuth { key })
}

/// Bebop origin identification, sent with binding quote requests. Bebop can configure API
/// accounts to require these fields. See the `BebopClientBuilder` docs for their meaning.
#[derive(Debug, Default)]
pub struct BebopOrigins {
    pub address: Option<Bytes>,
    pub target: Option<Bytes>,
    pub source: Option<String>,
}

/// Read optional Bebop origin identification from the BEBOP_ORIGIN_ADDRESS,
/// BEBOP_ORIGIN_TARGET and BEBOP_ORIGIN_SOURCE environment variables.
///
/// Unset variables yield `None`; a set but unparseable address is an error.
pub fn get_bebop_origins() -> Result<BebopOrigins, RFQError> {
    let parse_address = |var: &str| -> Result<Option<Bytes>, RFQError> {
        match env::var(var) {
            Ok(value) => Bytes::from_str(&value)
                .map(Some)
                .map_err(|e| RFQError::InvalidInput(format!("Invalid {var}: {e}"))),
            Err(_) => Ok(None),
        }
    };
    Ok(BebopOrigins {
        address: parse_address("BEBOP_ORIGIN_ADDRESS")?,
        target: parse_address("BEBOP_ORIGIN_TARGET")?,
        source: env::var("BEBOP_ORIGIN_SOURCE").ok(),
    })
}

/// Read Metric API configuration from environment variables.
/// METRIC_API_URL defaults to the public Metric endpoint; METRIC_API_KEY is the Bearer trading key
/// required by the authenticated endpoints (`bid_ask`).
pub fn get_metric_config() -> MetricConfig {
    let base_url = env::var("METRIC_API_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_METRIC_API_URL.to_string());
    let api_key = env::var("METRIC_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty());

    MetricConfig { base_url, api_key }
}

#[cfg(test)]
mod tests {
    use std::env;

    use super::*;

    #[test]
    fn test_hashflow_auth_success() {
        env::set_var("HASHFLOW_USER", "test_user");
        env::set_var("HASHFLOW_KEY", "test_key");

        let auth = get_hashflow_auth().unwrap();
        assert_eq!(auth.user, "test_user");
        assert_eq!(auth.key, "test_key");

        env::remove_var("HASHFLOW_USER");
        env::remove_var("HASHFLOW_KEY");
    }

    #[test]
    fn test_hashflow_auth_missing_user() {
        env::remove_var("HASHFLOW_USER");
        env::set_var("HASHFLOW_KEY", "test_key");

        let result = get_hashflow_auth();
        assert!(result.is_err());

        env::remove_var("HASHFLOW_KEY");
    }

    #[test]
    fn test_hashflow_auth_missing_key() {
        env::set_var("HASHFLOW_USER", "test_user");
        env::remove_var("HASHFLOW_KEY");

        let result = get_hashflow_auth();
        assert!(result.is_err());

        env::remove_var("HASHFLOW_USER");
    }

    #[test]
    fn test_bebop_auth_success() {
        env::set_var("BEBOP_KEY", "test_key");

        let auth = get_bebop_auth().unwrap();
        assert_eq!(auth.key, "test_key");

        env::remove_var("BEBOP_KEY");
    }

    #[test]
    fn test_bebop_auth_missing_key() {
        env::remove_var("BEBOP_KEY");

        let result = get_bebop_auth();
        assert!(result.is_err());
    }

    #[test]
    fn test_metric_config_defaults_and_reads_env() {
        env::remove_var("METRIC_API_URL");
        env::remove_var("METRIC_API_KEY");

        let config = get_metric_config();
        assert_eq!(config.base_url, DEFAULT_METRIC_API_URL);
        assert_eq!(config.api_key, None);

        env::set_var("METRIC_API_URL", "https://metric.example");
        env::set_var("METRIC_API_KEY", "secret");

        let config = get_metric_config();
        assert_eq!(config.base_url, "https://metric.example");
        assert_eq!(config.api_key.as_deref(), Some("secret"));

        env::remove_var("METRIC_API_URL");
        env::remove_var("METRIC_API_KEY");
    }
}
