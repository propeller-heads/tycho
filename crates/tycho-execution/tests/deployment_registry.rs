use std::{collections::BTreeSet, fs, path::PathBuf};

use chrono::{DateTime, Duration, FixedOffset};
use serde_json::{Map, Value};

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("tycho-execution must live under crates/")
        .to_path_buf()
}

fn read_json(path: &str) -> Value {
    let full_path = repository_root().join(path);
    let content = fs::read_to_string(&full_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", full_path.display()));
    serde_json::from_str(&content)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", full_path.display()))
}

fn object_at<'a>(value: &'a Value, key: &str) -> &'a Map<String, Value> {
    value[key]
        .as_object()
        .unwrap_or_else(|| panic!("{key} must be a JSON object"))
}

fn timestamp_at(value: &Value, key: &str, context: &str) -> Result<DateTime<FixedOffset>, String> {
    let timestamp = value[key]
        .as_str()
        .ok_or_else(|| format!("{context}.{key} must be an RFC 3339 string"))?;
    DateTime::parse_from_rfc3339(timestamp)
        .map_err(|error| format!("{context}.{key} is not RFC 3339: {error}"))
}

fn is_evm_address(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|address| address.starts_with("0x") && address.len() == 42)
}

fn validate_deployment_snapshot(deployment: &Value, context: &str) -> Result<(), String> {
    if !is_evm_address(&deployment["router"]["address"]) {
        return Err(format!("{context}.router.address must be an EVM address"))
    }
    if !is_evm_address(&deployment["fee_calculator"]["address"]) {
        return Err(format!("{context}.fee_calculator.address must be an EVM address"))
    }
    let executors = deployment["executors"]
        .as_object()
        .ok_or_else(|| format!("{context}.executors must be an object"))?;
    if executors.is_empty() ||
        executors
            .values()
            .any(|address| !is_evm_address(address))
    {
        return Err(format!("{context}.executors must contain EVM addresses"))
    }

    let dependencies = deployment["dependency_snapshot"]
        .as_object()
        .ok_or_else(|| format!("{context}.dependency_snapshot must be an object"))?;
    let source_commit = dependencies["source_commit"]
        .as_str()
        .ok_or_else(|| format!("{context}.dependency_snapshot.source_commit must be a string"))?;
    if source_commit.len() != 40 ||
        !source_commit
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(format!("{context}.dependency_snapshot.source_commit must be a commit SHA"))
    }
    for key in ["executor_deployments_path", "protocol_specific_addresses_path"] {
        dependencies[key]
            .as_str()
            .filter(|path| path.starts_with("crates/tycho-execution/config/"))
            .ok_or_else(|| format!("{context}.dependency_snapshot.{key} is invalid"))?;
    }
    Ok(())
}

fn validate_scheduled_successor(
    chain_name: &str,
    chain: &Value,
    standard_notice_days: i64,
) -> Result<(), String> {
    let Some(successor) = chain["scheduled_successor"].as_object() else { return Ok(()) };
    let successor = Value::Object(successor.clone());
    let context = format!("chains.{chain_name}.scheduled_successor");
    if successor["status"] != "scheduled" {
        return Err(format!("{context}.status must be scheduled"))
    }

    let notice = timestamp_at(&successor, "notice_published_at", &context)?;
    let effective = timestamp_at(&successor, "effective_at", &context)?;
    let deadline = timestamp_at(&successor, "migration_deadline", &context)?;
    if notice > effective {
        return Err(format!("{context}.notice_published_at must not follow effective_at"))
    }
    if deadline != effective {
        return Err(format!("{context}.migration_deadline must equal effective_at"))
    }
    validate_deployment_snapshot(&successor, &context)?;

    let notice_period = effective.signed_duration_since(notice);
    if notice_period >= Duration::days(standard_notice_days) {
        return Ok(())
    }

    let exception = successor["notice_exception"]
        .as_object()
        .ok_or_else(|| format!("{context} needs a material-security notice exception"))?;
    if exception["kind"] != "material_security_risk" {
        return Err(format!("{context}.notice_exception.kind is invalid"))
    }
    exception["reason"]
        .as_str()
        .filter(|reason| !reason.trim().is_empty())
        .ok_or_else(|| format!("{context}.notice_exception.reason must not be empty"))?;
    Ok(())
}

fn validate_registry_lifecycle(registry: &Value) -> Result<(), String> {
    timestamp_at(&registry["notice_policy"], "effective_at", "notice_policy")?;
    let standard_notice_days = registry["notice_policy"]["standard_notice_days"]
        .as_i64()
        .ok_or_else(|| "notice_policy.standard_notice_days must be an integer".to_string())?;
    if standard_notice_days != 30 {
        return Err("notice_policy.standard_notice_days must match Fynd License 1.0".to_string())
    }
    if registry["notice_policy"]["short_notice_exception"] != "material_security_risk" {
        return Err("notice_policy.short_notice_exception must match Fynd License 1.0".to_string())
    }

    for (chain_name, chain) in object_at(registry, "chains") {
        let context = format!("chains.{chain_name}");
        let notice = timestamp_at(chain, "notice_published_at", &context)?;
        let effective = timestamp_at(chain, "effective_at", &context)?;
        if notice > effective {
            return Err(format!("{context}.notice_published_at must not follow effective_at"))
        }
        if chain
            .get("scheduled_successor")
            .is_none()
        {
            return Err(format!("{context}.scheduled_successor is required"))
        }
        validate_deployment_snapshot(chain, &context)?;
        validate_scheduled_successor(chain_name, chain, standard_notice_days)?;
    }
    Ok(())
}

fn chain_heading(chain_name: &str) -> &str {
    match chain_name {
        "ethereum" => "Ethereum",
        "base" => "Base",
        "unichain" => "Unichain",
        "arbitrum" => "Arbitrum",
        "bsc" => "BSC",
        "polygon" => "Polygon",
        "plasma" => "Plasma",
        "robinhood" => "Robinhood",
        _ => panic!("missing documentation heading for {chain_name}"),
    }
}

fn markdown_section<'a>(docs: &'a str, heading: &str) -> &'a str {
    let marker = format!("## {heading}\n");
    docs.split_once(&marker)
        .unwrap_or_else(|| panic!("docs missing {marker}"))
        .1
        .split("\n## ")
        .next()
        .unwrap()
}

fn evm_addresses(text: &str) -> BTreeSet<String> {
    text.split(|character: char| {
        !(character.is_ascii_hexdigit() || character == 'x' || character == 'X')
    })
    .filter(|token| {
        token.len() == 42 &&
            token.starts_with("0x") &&
            token[2..]
                .chars()
                .all(|character| character.is_ascii_hexdigit())
    })
    .map(str::to_ascii_lowercase)
    .collect()
}

fn deployment_addresses(deployment: &Value) -> BTreeSet<String> {
    let mut addresses = BTreeSet::new();
    addresses.insert(
        deployment["router"]["address"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase(),
    );
    addresses.insert(
        deployment["fee_calculator"]["address"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase(),
    );
    for address in object_at(deployment, "executors").values() {
        addresses.insert(
            address
                .as_str()
                .unwrap()
                .to_ascii_lowercase(),
        );
    }
    addresses
}

#[test]
fn active_registry_matches_runtime_address_configs() {
    let registry = read_json("crates/tycho-execution/config/deployment_registry.json");
    let routers = read_json("crates/tycho-execution/config/router_addresses.json");
    let executors = read_json("crates/tycho-execution/config/executor_addresses.json");
    let chains = object_at(&registry, "chains");
    let routers = routers
        .as_object()
        .expect("router config must be a JSON object");

    assert_eq!(registry["schema_version"], 1);
    assert_eq!(registry["designation"], "Fynd License 1.0");
    assert_eq!(registry["legal_contact"], "legal@propellerheads.xyz");
    validate_registry_lifecycle(&registry).unwrap();
    assert_eq!(chains.len(), routers.len());

    for (chain_name, router_address) in routers {
        let chain = &chains[chain_name];
        assert_eq!(chain["status"], "active", "{chain_name} status");
        assert!(chain.get("effective_at").is_some(), "{chain_name} effective_at");
        assert!(
            chain
                .get("notice_published_at")
                .is_some(),
            "{chain_name} notice"
        );
        assert!(
            chain
                .get("migration_deadline")
                .is_some(),
            "{chain_name} deadline"
        );
        assert_eq!(chain["router"]["address"], *router_address, "{chain_name} router");
        assert_eq!(chain["executors"], executors[chain_name], "{chain_name} executors");
        assert_eq!(chain["superseded"][0]["status"], "superseded");

        let fee_calculator = chain["fee_calculator"]["address"]
            .as_str()
            .unwrap_or_else(|| panic!("{chain_name} fee calculator must be a string"));
        assert!(fee_calculator.starts_with("0x") && fee_calculator.len() == 42);
    }
}

#[test]
fn scheduled_successor_requires_standard_notice_or_security_exception() {
    let registry = read_json("crates/tycho-execution/config/deployment_registry.json");
    let mut chain = registry["chains"]["ethereum"].clone();
    let mut successor = chain.clone();
    successor
        .as_object_mut()
        .unwrap()
        .remove("scheduled_successor");
    successor["status"] = "scheduled".into();
    successor["notice_published_at"] = "2026-10-01T00:00:00Z".into();
    successor["effective_at"] = "2026-10-31T00:00:00Z".into();
    successor["migration_deadline"] = "2026-10-31T00:00:00Z".into();
    successor["notice_exception"] = Value::Null;
    chain["scheduled_successor"] = successor;
    validate_scheduled_successor("ethereum", &chain, 30).unwrap();

    chain["scheduled_successor"]["effective_at"] = "2026-10-30T23:59:59Z".into();
    chain["scheduled_successor"]["migration_deadline"] = "2026-10-30T23:59:59Z".into();
    let error = validate_scheduled_successor("ethereum", &chain, 30).unwrap_err();
    assert!(error.contains("material-security notice exception"));

    chain["scheduled_successor"]["effective_at"] = "2026-10-02T00:00:00Z".into();
    chain["scheduled_successor"]["migration_deadline"] = "2026-10-02T00:00:00Z".into();
    let error = validate_scheduled_successor("ethereum", &chain, 30).unwrap_err();
    assert!(error.contains("material-security notice exception"));

    chain["scheduled_successor"]["notice_exception"] = serde_json::json!({
        "kind": "maintenance",
        "reason": "Planned maintenance."
    });
    let error = validate_scheduled_successor("ethereum", &chain, 30).unwrap_err();
    assert!(error.contains("notice_exception.kind is invalid"));

    chain["scheduled_successor"]["notice_exception"] = serde_json::json!({
        "kind": "material_security_risk",
        "reason": ""
    });
    let error = validate_scheduled_successor("ethereum", &chain, 30).unwrap_err();
    assert!(error.contains("notice_exception.reason must not be empty"));

    chain["scheduled_successor"]["notice_exception"]["reason"] =
        "Immediate migration required to protect user funds.".into();
    validate_scheduled_successor("ethereum", &chain, 30).unwrap();
}

#[test]
fn scheduled_successor_rejects_invalid_dates_and_deadlines() {
    let registry = read_json("crates/tycho-execution/config/deployment_registry.json");
    let mut chain = registry["chains"]["ethereum"].clone();
    let mut successor = chain.clone();
    successor
        .as_object_mut()
        .unwrap()
        .remove("scheduled_successor");
    successor["status"] = "scheduled".into();
    successor["notice_published_at"] = "not-a-date".into();
    successor["effective_at"] = "2026-10-31T00:00:00Z".into();
    successor["migration_deadline"] = "2026-10-30T00:00:00Z".into();
    successor["notice_exception"] = Value::Null;
    chain["scheduled_successor"] = successor;

    let error = validate_scheduled_successor("ethereum", &chain, 30).unwrap_err();
    assert!(error.contains("not RFC 3339"));

    chain["scheduled_successor"]["notice_published_at"] = "2026-10-01T00:00:00Z".into();
    let error = validate_scheduled_successor("ethereum", &chain, 30).unwrap_err();
    assert!(error.contains("migration_deadline must equal effective_at"));

    chain["scheduled_successor"]["migration_deadline"] = "2026-10-31T00:00:00Z".into();
    chain["scheduled_successor"]["notice_published_at"] = "2026-11-01T00:00:00Z".into();
    let error = validate_scheduled_successor("ethereum", &chain, 30).unwrap_err();
    assert!(error.contains("notice_published_at must not follow effective_at"));
}

#[test]
fn registry_addresses_are_visible_in_contract_address_docs() {
    let registry = read_json("crates/tycho-execution/config/deployment_registry.json");
    let docs_path = repository_root().join("docs/for-solvers/execution/contract-addresses.md");
    let docs = fs::read_to_string(&docs_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", docs_path.display()));

    assert!(docs.contains("deployment_registry.json"));
    assert!(docs.contains("legal@propellerheads.xyz"));
    assert!(docs.contains("1 September 2026"));

    for (chain_name, chain) in object_at(&registry, "chains") {
        let section = markdown_section(&docs, chain_heading(chain_name));
        assert_eq!(
            evm_addresses(section),
            deployment_addresses(chain),
            "current docs and registry differ for {chain_name}"
        );
    }
}
