use std::{
    collections::{HashMap, HashSet},
    fmt, fs,
    path::Path,
};

use chrono::{TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use tracing::{info, trace};
use tycho_common::dto;

use crate::services::rpc::RpcError;

/// Comparison operator for numerical restrictions.
///
/// Config example:
/// ```yaml
/// plans:
///   default:
///     component_tvl:
///       op: gte
///       value: 10000.0
///     token_quality:
///       op: gte
///       value: 50
///     traded_n_days_ago:
///       op: lte
///       value: 30
///   strict:
///     allowed_protocol_systems:
///       - uniswap_v3
///     token_quality:
///       op: eq
///       value: 100
///     traded_n_days_ago:
///       op: eq
///       value: 7
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Operator {
    Gte,
    Lte,
    Eq,
}

impl fmt::Display for Operator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operator::Gte => write!(f, ">="),
            Operator::Lte => write!(f, "<="),
            Operator::Eq => write!(f, "=="),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct NumericRestriction {
    pub op: Operator,
    pub value: f64,
}

impl NumericRestriction {
    fn check(&self, actual: f64) -> bool {
        match self.op {
            Operator::Gte => actual >= self.value,
            Operator::Lte => actual <= self.value,
            Operator::Eq => (actual - self.value).abs() < f64::EPSILON,
        }
    }

    fn violation_message(&self, param_name: &str, actual: f64) -> String {
        format!(
            "{param_name} must be {op} {expected} (got {actual})",
            op = self.op,
            expected = self.value,
        )
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct PlanRestrictions {
    #[serde(default)]
    pub allowed_protocol_systems: Option<HashSet<String>>,
    #[serde(default)]
    pub component_tvl: Option<NumericRestriction>,
    #[serde(default)]
    pub token_quality: Option<NumericRestriction>,
    #[serde(default)]
    pub traded_n_days_ago: Option<NumericRestriction>,
    /// Maximum age in minutes for version timestamps. When set, requests with a
    /// version older than this threshold are rejected. `None` means unrestricted.
    #[serde(default)]
    pub max_version_age_minutes: Option<u64>,
}

impl PlanRestrictions {
    pub fn check_protocol_system(&self, protocol_system: &str) -> Result<(), RpcError> {
        if let Some(allowed) = &self.allowed_protocol_systems {
            if !allowed.contains(protocol_system) {
                return Err(RpcError::PlanRestrictionViolation(format!(
                    "protocol_system '{protocol_system}' is not available on this plan \
                     (allowed: {})",
                    allowed
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                )));
            }
        }
        Ok(())
    }

    /// Rejects requests whose version timestamp exceeds `max_version_age_minutes`.
    ///
    /// When the restriction is `None`, all versions are allowed. When set, the
    /// version must have a timestamp within the threshold — requests without a
    /// timestamp are rejected since their age cannot be verified.
    pub fn check_version_recency(&self, version: &dto::VersionParam) -> Result<(), RpcError> {
        let max_minutes = match self.max_version_age_minutes {
            Some(m) => m,
            None => return Ok(()),
        };
        let max_age = TimeDelta::minutes(max_minutes as i64);
        match version.timestamp {
            Some(ts) if Utc::now().naive_utc() - ts <= max_age => Ok(()),
            _ => Err(RpcError::PlanRestrictionViolation(format!(
                "version is older than the {max_minutes} minute limit on this plan"
            ))),
        }
    }

    pub fn check_numeric(
        &self,
        param_name: &str,
        restriction: &Option<NumericRestriction>,
        actual: Option<f64>,
    ) -> Result<(), RpcError> {
        if let Some(restriction) = restriction {
            match actual {
                // Plan requires this param but caller didn't provide it
                None => {
                    return Err(RpcError::PlanRestrictionViolation(format!(
                        "{param_name} parameter is required on this plan"
                    )));
                }
                // Param provided but doesn't satisfy the restriction
                Some(actual) if !restriction.check(actual) => {
                    return Err(RpcError::PlanRestrictionViolation(
                        restriction.violation_message(param_name, actual),
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PlansConfig {
    #[serde(default)]
    plans: HashMap<String, PlanRestrictions>,
}

impl PlansConfig {
    /// Resolves plan restrictions for the given plan name.
    ///
    /// Falls back to the "default" plan if the name isn't found. Returns `None`
    /// (unrestricted) if neither the requested plan nor a "default" plan exists.
    pub fn resolve(&self, plan_name: &str) -> Option<&PlanRestrictions> {
        self.plans.get(plan_name).or_else(|| {
            let default = self.plans.get("default");
            if default.is_some() {
                trace!("unknown plan '{plan_name}', falling back to 'default' plan");
            } else {
                trace!(
                    "unknown plan '{plan_name}' and no 'default' plan configured, \
                     defaulting to unrestricted"
                );
            }
            default
        })
    }

    pub fn is_empty(&self) -> bool {
        self.plans.is_empty()
    }

    /// Loads the plans config from a YAML file.
    ///
    /// If the file does not exist, returns a default instance (no restrictions).
    /// If the file exists but cannot be read or parsed, returns an error.
    pub fn from_yaml(path: &str) -> Result<Self, String> {
        let path = Path::new(path);
        if !path.exists() {
            info!("No plans config found at {}, running without plan restrictions", path.display());
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read plans config at {}: {e}", path.display()))?;
        serde_yaml::from_str(&contents)
            .map_err(|e| format!("Failed to parse plans config at {}: {e}", path.display()))
    }
}

/// Trait for request types that can be validated against plan restrictions.
///
/// The `is_targeted` method indicates whether the request specifies explicit IDs
/// (e.g. component_ids, token_addresses), in which case numerical restrictions are skipped.
pub trait ValidateRestrictions {
    fn validate_restrictions(&self, restrictions: &PlanRestrictions) -> Result<(), RpcError>;
}

impl ValidateRestrictions for dto::ProtocolComponentsRequestBody {
    fn validate_restrictions(&self, restrictions: &PlanRestrictions) -> Result<(), RpcError> {
        restrictions.check_protocol_system(&self.protocol_system)?;
        // Skip numeric checks when targeting specific components by ID
        if self.component_ids.is_none() {
            restrictions.check_numeric("tvl_gt", &restrictions.component_tvl, self.tvl_gt)?;
        }
        Ok(())
    }
}

impl ValidateRestrictions for dto::TokensRequestBody {
    fn validate_restrictions(&self, restrictions: &PlanRestrictions) -> Result<(), RpcError> {
        // Skip numeric checks when targeting specific token addresses
        if self.token_addresses.is_none() {
            restrictions.check_numeric(
                "min_quality",
                &restrictions.token_quality,
                self.min_quality.map(f64::from),
            )?;
            restrictions.check_numeric(
                "traded_n_days_ago",
                &restrictions.traded_n_days_ago,
                self.traded_n_days_ago.map(|v| v as f64),
            )?;
        }
        Ok(())
    }
}

impl ValidateRestrictions for dto::ProtocolStateRequestBody {
    fn validate_restrictions(&self, restrictions: &PlanRestrictions) -> Result<(), RpcError> {
        restrictions.check_protocol_system(&self.protocol_system)?;
        restrictions.check_version_recency(&self.version)
    }
}

impl ValidateRestrictions for dto::StateRequestBody {
    fn validate_restrictions(&self, restrictions: &PlanRestrictions) -> Result<(), RpcError> {
        // protocol_system defaults to "" via serde; skip check when not provided
        if !self.protocol_system.is_empty() {
            restrictions.check_protocol_system(&self.protocol_system)?;
        }
        restrictions.check_version_recency(&self.version)
    }
}

impl ValidateRestrictions for dto::ComponentTvlRequestBody {
    fn validate_restrictions(&self, restrictions: &PlanRestrictions) -> Result<(), RpcError> {
        // protocol_system is optional on this endpoint; skip check when absent
        if let Some(protocol_system) = &self.protocol_system {
            restrictions.check_protocol_system(protocol_system)?;
        }
        Ok(())
    }
}

impl ValidateRestrictions for dto::TracedEntryPointRequestBody {
    fn validate_restrictions(&self, restrictions: &PlanRestrictions) -> Result<(), RpcError> {
        restrictions.check_protocol_system(&self.protocol_system)
    }
}

impl ValidateRestrictions for dto::DeltaCatchUpRequestBody {
    fn validate_restrictions(&self, restrictions: &PlanRestrictions) -> Result<(), RpcError> {
        // The other limits bound component, token and version filters; a catch-up takes none.
        restrictions.check_protocol_system(&self.protocol_system)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use tycho_common::Bytes;

    use super::*;

    fn restricted_plan() -> PlanRestrictions {
        PlanRestrictions {
            allowed_protocol_systems: Some(
                ["uniswap_v2", "uniswap_v3"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<HashSet<_>>(),
            ),
            component_tvl: Some(NumericRestriction { op: Operator::Gte, value: 1000.0 }),
            token_quality: Some(NumericRestriction { op: Operator::Gte, value: 50.0 }),
            traded_n_days_ago: Some(NumericRestriction { op: Operator::Lte, value: 30.0 }),
            ..Default::default()
        }
    }

    #[test]
    fn test_yaml_deserialization() {
        let yaml = r#"
plans:
  default:
    component_tvl:
      op: gte
      value: 10000.0
    token_quality:
      op: gte
      value: 50
    traded_n_days_ago:
      op: lte
      value: 30
  strict:
    allowed_protocol_systems:
      - uniswap_v3
    token_quality:
      op: eq
      value: 100
    traded_n_days_ago:
      op: eq
      value: 7
"#;
        let config: PlansConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.plans.len(), 2);

        let default = config.resolve("default").unwrap();
        assert!(default
            .allowed_protocol_systems
            .is_none());
        assert_eq!(
            default
                .component_tvl
                .as_ref()
                .unwrap()
                .op,
            Operator::Gte
        );
        assert_eq!(
            default
                .component_tvl
                .as_ref()
                .unwrap()
                .value,
            10000.0
        );

        let strict = config.resolve("strict").unwrap();
        assert!(strict
            .allowed_protocol_systems
            .as_ref()
            .unwrap()
            .contains("uniswap_v3"));
        assert_eq!(
            strict
                .token_quality
                .as_ref()
                .unwrap()
                .op,
            Operator::Eq
        );
        assert_eq!(
            strict
                .traded_n_days_ago
                .as_ref()
                .unwrap()
                .value,
            7.0
        );
    }

    #[test]
    fn test_resolve_unknown_plan_falls_back_to_default_plan() {
        let default_restrictions = PlanRestrictions {
            component_tvl: Some(NumericRestriction { op: Operator::Gte, value: 500.0 }),
            ..Default::default()
        };
        let config = PlansConfig {
            plans: HashMap::from([("default".to_string(), default_restrictions.clone())]),
        };
        let result = config.resolve("nonexistent").unwrap();
        assert_eq!(*result, default_restrictions);
    }

    #[test]
    fn test_resolve_unknown_plan_unrestricted_without_default() {
        let config = PlansConfig::default();
        assert!(config.resolve("nonexistent").is_none());
    }

    #[test]
    fn test_protocol_system_allowed() {
        let restrictions = PlanRestrictions {
            allowed_protocol_systems: Some(
                ["uniswap_v2", "uniswap_v3"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            ..Default::default()
        };
        assert!(restrictions
            .check_protocol_system("uniswap_v2")
            .is_ok());
        assert!(restrictions
            .check_protocol_system("sushiswap")
            .is_err());
    }

    #[test]
    fn test_protocol_system_unrestricted() {
        let restrictions = PlanRestrictions::default();
        assert!(restrictions
            .check_protocol_system("anything")
            .is_ok());
    }

    #[rstest]
    #[case::gte_passes(Operator::Gte, 100.0, Some(100.0), true)]
    #[case::gte_above(Operator::Gte, 100.0, Some(200.0), true)]
    #[case::gte_below(Operator::Gte, 100.0, Some(50.0), false)]
    #[case::lte_passes(Operator::Lte, 30.0, Some(30.0), true)]
    #[case::lte_below(Operator::Lte, 30.0, Some(15.0), true)]
    #[case::lte_above(Operator::Lte, 30.0, Some(60.0), false)]
    #[case::eq_passes(Operator::Eq, 100.0, Some(100.0), true)]
    #[case::eq_fails(Operator::Eq, 100.0, Some(99.0), false)]
    #[case::missing_param(Operator::Gte, 100.0, None, false)]
    fn test_numeric_restriction(
        #[case] op: Operator,
        #[case] threshold: f64,
        #[case] actual: Option<f64>,
        #[case] should_pass: bool,
    ) {
        let restrictions = PlanRestrictions {
            component_tvl: Some(NumericRestriction { op, value: threshold }),
            ..Default::default()
        };
        let result =
            restrictions.check_numeric("component_tvl", &restrictions.component_tvl, actual);
        assert_eq!(result.is_ok(), should_pass);
    }

    #[test]
    fn test_missing_file_returns_empty() {
        let config = PlansConfig::from_yaml("/nonexistent/plans.yaml").unwrap();
        assert!(config.is_empty());
    }

    #[test]
    fn test_invalid_yaml_file_returns_error() {
        let dir = std::env::temp_dir();
        let path = dir.join("bad_plans.yaml");
        std::fs::write(&path, "not: [valid: yaml: plans").unwrap();
        let result = PlansConfig::from_yaml(path.to_str().unwrap());
        assert!(result.is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_check_numeric_no_restriction() {
        let restrictions = PlanRestrictions::default();
        assert!(restrictions
            .check_numeric("component_tvl", &restrictions.component_tvl, None)
            .is_ok());
        assert!(restrictions
            .check_numeric("component_tvl", &restrictions.component_tvl, Some(1.0))
            .is_ok());
    }

    #[test]
    fn test_violation_message_is_descriptive() {
        let restrictions = PlanRestrictions {
            component_tvl: Some(NumericRestriction { op: Operator::Gte, value: 10000.0 }),
            ..Default::default()
        };
        let result =
            restrictions.check_numeric("component_tvl", &restrictions.component_tvl, Some(500.0));
        let err = result.unwrap_err().to_string();
        assert!(err.contains("component_tvl"));
        assert!(err.contains(">="));
        assert!(err.contains("10000"));
        assert!(err.contains("500"));
    }

    #[rstest]
    #[case::rejects_missing_tvl(None, None, true)]
    #[case::rejects_below_threshold(Some(500.0), None, true)]
    #[case::accepts_at_threshold(Some(1000.0), None, false)]
    #[case::accepts_above_threshold(Some(5000.0), None, false)]
    #[case::skips_with_component_ids(None, Some(vec!["c1".to_string()]), false)]
    #[case::skips_with_low_tvl_and_ids(Some(1.0), Some(vec!["c1".to_string()]), false)]
    #[tokio::test]
    async fn test_component_tvl_restriction(
        #[case] tvl_gt: Option<f64>,
        #[case] component_ids: Option<Vec<String>>,
        #[case] should_fail: bool,
    ) {
        let plan = restricted_plan();
        let request = dto::ProtocolComponentsRequestBody {
            protocol_system: "uniswap_v2".to_string(),
            component_ids,
            tvl_gt,
            chain: dto::Chain::Ethereum,
            pagination: dto::PaginationParams::new(0, 10),
        };
        let result = request.validate_restrictions(&plan);
        assert_eq!(result.is_err(), should_fail);
    }

    #[rstest]
    #[case::allowed_system("uniswap_v2", false)]
    #[case::blocked_system("sushiswap", true)]
    #[tokio::test]
    async fn test_component_protocol_system_restriction(
        #[case] protocol_system: &str,
        #[case] should_fail: bool,
    ) {
        let plan = restricted_plan();
        let request = dto::ProtocolComponentsRequestBody {
            protocol_system: protocol_system.to_string(),
            component_ids: Some(vec!["c1".to_string()]),
            tvl_gt: None,
            chain: dto::Chain::Ethereum,
            pagination: dto::PaginationParams::new(0, 10),
        };
        let result = request.validate_restrictions(&plan);
        assert_eq!(result.is_err(), should_fail);
    }

    #[rstest]
    #[case::rejects_missing_quality(None, Some(15), None, true)]
    #[case::rejects_below_quality(Some(30), Some(15), None, true)]
    #[case::accepts_both_valid(Some(50), Some(15), None, false)]
    #[case::rejects_missing_traded_days(Some(80), None, None, true)]
    #[case::rejects_above_traded_days(Some(80), Some(60), None, true)]
    #[case::accepts_within_traded_days(Some(80), Some(15), None, false)]
    #[case::skips_with_addresses(None, None, Some(vec![Bytes::from("0x01")]), false)]
    #[tokio::test]
    async fn test_token_restrictions(
        #[case] min_quality: Option<i32>,
        #[case] traded_n_days_ago: Option<u64>,
        #[case] token_addresses: Option<Vec<Bytes>>,
        #[case] should_fail: bool,
    ) {
        let plan = restricted_plan();
        let request = dto::TokensRequestBody {
            chain: dto::Chain::Ethereum,
            token_addresses,
            min_quality,
            traded_n_days_ago,
            pagination: dto::PaginationParams::new(0, 10),
        };
        let result = request.validate_restrictions(&plan);
        assert_eq!(result.is_err(), should_fail);
    }

    #[rstest]
    #[case::allowed_system("uniswap_v2", false)]
    #[case::blocked_system("sushiswap", true)]
    #[tokio::test]
    async fn test_protocol_state_restriction(
        #[case] protocol_system: &str,
        #[case] should_fail: bool,
    ) {
        let plan = restricted_plan();
        let request = dto::ProtocolStateRequestBody {
            protocol_ids: None,
            protocol_system: protocol_system.to_string(),
            include_balances: true,
            version: Default::default(),
            chain: dto::Chain::Ethereum,
            pagination: dto::PaginationParams::new(0, 10),
        };
        let result = request.validate_restrictions(&plan);
        assert_eq!(result.is_err(), should_fail);
    }

    #[rstest]
    #[case::allowed_system("uniswap_v2", false)]
    #[case::blocked_system("sushiswap", true)]
    #[case::empty_system_skips("", false)]
    #[tokio::test]
    async fn test_contract_state_restriction(
        #[case] protocol_system: &str,
        #[case] should_fail: bool,
    ) {
        let plan = restricted_plan();
        let request = dto::StateRequestBody::new(
            None,
            protocol_system.to_string(),
            Default::default(),
            dto::Chain::Ethereum,
            dto::PaginationParams::new(0, 10),
        );
        let result = request.validate_restrictions(&plan);
        assert_eq!(result.is_err(), should_fail);
    }

    #[rstest]
    #[case::allowed_system(Some("uniswap_v2".to_string()), false)]
    #[case::blocked_system(Some("sushiswap".to_string()), true)]
    #[case::none_skips(None, false)]
    #[tokio::test]
    async fn test_component_tvl_protocol_restriction(
        #[case] protocol_system: Option<String>,
        #[case] should_fail: bool,
    ) {
        let plan = restricted_plan();
        let request = dto::ComponentTvlRequestBody {
            chain: dto::Chain::Ethereum,
            protocol_system,
            component_ids: None,
            pagination: dto::PaginationParams::new(0, 10),
        };
        let result = request.validate_restrictions(&plan);
        assert_eq!(result.is_err(), should_fail);
    }

    #[rstest]
    #[case::allowed_system("uniswap_v2", false)]
    #[case::blocked_system("sushiswap", true)]
    #[tokio::test]
    async fn test_traced_entry_point_restriction(
        #[case] protocol_system: &str,
        #[case] should_fail: bool,
    ) {
        let plan = restricted_plan();
        let request = dto::TracedEntryPointRequestBody {
            chain: dto::Chain::Ethereum,
            protocol_system: protocol_system.to_string(),
            component_ids: None,
            pagination: dto::PaginationParams::new(0, 10),
        };
        let result = request.validate_restrictions(&plan);
        assert_eq!(result.is_err(), should_fail);
    }

    #[tokio::test]
    async fn test_no_restrictions_passes_everything() {
        let plan = PlanRestrictions::default();
        let request = dto::ProtocolComponentsRequestBody {
            protocol_system: "anything".to_string(),
            component_ids: None,
            tvl_gt: None,
            chain: dto::Chain::Ethereum,
            pagination: dto::PaginationParams::new(0, 10),
        };
        assert!(request
            .validate_restrictions(&plan)
            .is_ok());
    }

    #[rstest]
    #[case::recent_allowed(Some(5), TimeDelta::minutes(0), false)]
    #[case::old_rejected(Some(5), TimeDelta::minutes(10), true)]
    #[case::boundary_allowed(Some(5), TimeDelta::minutes(4), false)]
    #[case::no_restriction_old_allowed(None, TimeDelta::minutes(10), false)]
    #[case::custom_threshold(Some(30), TimeDelta::minutes(20), false)]
    #[case::custom_threshold_exceeded(Some(30), TimeDelta::minutes(31), true)]
    fn test_check_version_recency(
        #[case] max_version_age_minutes: Option<u64>,
        #[case] age: TimeDelta,
        #[case] should_fail: bool,
    ) {
        let restrictions = PlanRestrictions { max_version_age_minutes, ..Default::default() };
        let ts = Utc::now().naive_utc() - age;
        let version = dto::VersionParam { timestamp: Some(ts), block: None };
        let result = restrictions.check_version_recency(&version);
        assert_eq!(result.is_err(), should_fail);
    }

    #[test]
    fn test_check_version_recency_no_timestamp_rejected() {
        let restrictions =
            PlanRestrictions { max_version_age_minutes: Some(5), ..Default::default() };
        let version = dto::VersionParam { timestamp: None, block: None };
        assert!(restrictions
            .check_version_recency(&version)
            .is_err());
    }

    #[test]
    fn test_yaml_max_version_age() {
        let yaml = r#"
plans:
  free:
    max_version_age_minutes: 5
  pro: {}
  custom:
    max_version_age_minutes: 30
"#;
        let config: PlansConfig = serde_yaml::from_str(yaml).unwrap();

        let free = config.resolve("free").unwrap();
        assert_eq!(free.max_version_age_minutes, Some(5));

        let pro = config.resolve("pro").unwrap();
        assert_eq!(pro.max_version_age_minutes, None);

        let custom = config.resolve("custom").unwrap();
        assert_eq!(custom.max_version_age_minutes, Some(30));
    }
}
