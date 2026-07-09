use std::env;

use crate::rfq::errors::RFQError;

pub const DEFAULT_METRIC_API_URL: &str = "http://54.199.103.16:8080";

/// Hashflow authentication configuration
pub struct HashflowAuth {
    pub user: String,
    pub key: String,
}

/// Bebop authentication configuration
pub struct BebopAuth {
    pub user: String,
    pub key: String,
}

/// Metric API configuration
pub struct MetricConfig {
    pub base_url: String,
    pub secret_key: Option<String>,
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
/// Returns the BEBOP_USER and BEBOP_KEY environment variables
pub fn get_bebop_auth() -> Result<BebopAuth, RFQError> {
    let user = env::var("BEBOP_USER").map_err(|_| {
        RFQError::InvalidInput("BEBOP_USER environment variable is required".into())
    })?;

    let key = env::var("BEBOP_KEY")
        .map_err(|_| RFQError::InvalidInput("BEBOP_KEY environment variable is required".into()))?;

    Ok(BebopAuth { user, key })
}

/// Read Metric API configuration from environment variables.
/// METRIC_API_URL defaults to the public Metric endpoint; METRIC_SECRET_KEY is optional.
pub fn get_metric_config() -> MetricConfig {
    let base_url = env::var("METRIC_API_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_METRIC_API_URL.to_string());
    let secret_key = env::var("METRIC_SECRET_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty());

    MetricConfig { base_url, secret_key }
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
        env::set_var("BEBOP_USER", "test_user");
        env::set_var("BEBOP_KEY", "test_key");

        let auth = get_bebop_auth().unwrap();
        assert_eq!(auth.user, "test_user");
        assert_eq!(auth.key, "test_key");

        env::remove_var("BEBOP_USER");
        env::remove_var("BEBOP_KEY");
    }

    #[test]
    fn test_bebop_auth_missing_user() {
        env::remove_var("BEBOP_USER");
        env::set_var("BEBOP_KEY", "test_key");

        let result = get_bebop_auth();
        assert!(result.is_err());

        env::remove_var("BEBOP_KEY");
    }

    #[test]
    fn test_bebop_auth_missing_key() {
        env::set_var("BEBOP_USER", "test_user");
        env::remove_var("BEBOP_KEY");

        let result = get_bebop_auth();
        assert!(result.is_err());

        env::remove_var("BEBOP_USER");
    }

    #[test]
    fn test_metric_config_defaults_and_reads_env() {
        env::remove_var("METRIC_API_URL");
        env::remove_var("METRIC_SECRET_KEY");

        let config = get_metric_config();
        assert_eq!(config.base_url, DEFAULT_METRIC_API_URL);
        assert_eq!(config.secret_key, None);

        env::set_var("METRIC_API_URL", "https://metric.example");
        env::set_var("METRIC_SECRET_KEY", "secret");

        let config = get_metric_config();
        assert_eq!(config.base_url, "https://metric.example");
        assert_eq!(config.secret_key.as_deref(), Some("secret"));

        env::remove_var("METRIC_API_URL");
        env::remove_var("METRIC_SECRET_KEY");
    }
}
