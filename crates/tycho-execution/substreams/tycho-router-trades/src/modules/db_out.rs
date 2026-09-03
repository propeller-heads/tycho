//! Conversion of trades and fee events into `DatabaseChanges` rows for substreams-sink-sql.
use anyhow::{bail, Result};
use substreams::scalar::{BigDecimal, BigInt};
use substreams_database_change::{
    pb::sf::substreams::sink::database::v1::DatabaseChanges, tables::Tables,
};

use super::hex_addr;
use crate::pb::tycho::router::v1::{FeeConfigEvents, Trade, Trades};

#[substreams::handlers::map]
pub fn db_out(trades: Trades, fee_events: FeeConfigEvents) -> Result<DatabaseChanges> {
    let mut tables = Tables::new();
    for trade in &trades.trades {
        insert_trade(&mut tables, trade)?;
    }
    for error in &trades.errors {
        let id = format!("{}:{}:{}", error.chain, hex_addr(&error.tx_hash), error.call_index);
        tables
            .create_row("router_call_errors", id)
            .set("chain", &error.chain)
            .set("block_number", error.block_number)
            .set("block_time", rfc3339(error.block_timestamp))
            .set("tx_hash", hex_addr(&error.tx_hash))
            .set("tx_index", error.tx_index)
            .set("call_index", error.call_index)
            .set("router", hex_addr(&error.router))
            .set("router_version", &error.router_version)
            .set("stage", &error.stage)
            .set("error", &error.error)
            .set("tx_success", error.tx_success)
            .set("call_success", error.call_success);
    }
    for ev in &fee_events.events {
        let id = format!("{}:{}:{}", ev.chain, hex_addr(&ev.tx_hash), ev.log_index);
        let row = tables.create_row("fee_config_events", id);
        row.set("chain", &ev.chain)
            .set("block_number", ev.block_number)
            .set("block_time", rfc3339(ev.block_timestamp))
            .set("tx_hash", hex_addr(&ev.tx_hash))
            .set("log_index", ev.log_index)
            .set("emitter", hex_addr(&ev.emitter))
            .set("event", &ev.event);
        if !ev.client.is_empty() {
            row.set("client", hex_addr(&ev.client));
        }
        if !ev.old_value.is_empty() {
            row.set("old_value", &ev.old_value);
        }
        if !ev.new_value.is_empty() {
            row.set("new_value", &ev.new_value);
        }
    }
    Ok(tables.to_database_changes())
}

fn insert_trade(tables: &mut Tables, t: &Trade) -> Result<()> {
    let trade_id = format!("{}:{}:{}", t.chain, hex_addr(&t.tx_hash), t.call_index);
    let executors: Vec<String> = t
        .hops
        .iter()
        .map(|h| hex_addr(&h.executor))
        .collect();
    let protocol_systems: Vec<String> = t
        .hops
        .iter()
        .flat_map(|hop| hop.protocol_systems.iter().cloned())
        .collect();
    let fee_split = split_fees(t)?;

    let row = tables.create_row("trades", trade_id.clone());
    row.set("chain", &t.chain)
        .set("block_number", t.block_number)
        .set("block_time", rfc3339(t.block_timestamp))
        .set("tx_hash", hex_addr(&t.tx_hash))
        .set("tx_index", t.tx_index)
        .set("call_index", t.call_index)
        .set("tx_success", t.tx_success)
        .set("call_success", t.call_success)
        .set("router", hex_addr(&t.router))
        .set("router_version", &t.router_version)
        .set("strategy", &t.strategy)
        .set("funding", &t.funding)
        .set("eoa", hex_addr(&t.eoa))
        .set("msg_sender", hex_addr(&t.msg_sender))
        .set("receiver", hex_addr(&t.receiver))
        .set("token_in", hex_addr(&t.token_in))
        .set("token_out", hex_addr(&t.token_out))
        .set("amount_in", &t.amount_in)
        .set("min_amount_out", &t.min_amount_out)
        .set("native_value", &t.native_value)
        .set("gas_used", t.gas_used)
        .set("n_tokens", t.n_tokens)
        .set("n_hops", t.hops.len() as u32)
        .set("wrap_eth", t.wrap_eth)
        .set("unwrap_eth", t.unwrap_eth);
    row.set("executors", psql_text_array(&executors));
    row.set("protocol_systems", psql_text_array(&protocol_systems));
    if !t.expected_amount_out.is_empty() {
        row.set("expected_amount_out", &t.expected_amount_out);
        if let Some(bps) = slippage_tolerance_bps(&t.expected_amount_out, &t.min_amount_out) {
            row.set("slippage_tolerance_bps", bps);
        }
    }
    if !t.amount_out.is_empty() {
        row.set("amount_out", &t.amount_out);
        let total_fees = &fee_split.router + &fee_split.client;
        let gross = parse_bigint(&t.amount_out) + total_fees;
        row.set("gross_amount_out", gross.to_string());
        if !t.expected_amount_out.is_empty() {
            let surplus = gross - parse_bigint(&t.expected_amount_out);
            let surplus = if surplus < BigInt::zero() { BigInt::zero() } else { surplus };
            row.set("positive_slippage", surplus.to_string());
        }
    }
    if !t.revert_selector.is_empty() {
        row.set("revert_selector", &t.revert_selector);
    }
    if !t.revert_reason.is_empty() {
        row.set("revert_reason", &t.revert_reason);
    }
    if !t.watermark.is_empty() {
        row.set("watermark", hex_addr(&t.watermark));
    }
    if let Some(c) = &t.client_fee {
        row.set("client_fee_bps", c.fee_bps)
            .set("client_fee_receiver", hex_addr(&c.receiver))
            .set("max_client_contribution", &c.max_client_contribution)
            .set("client_fee_deadline", &c.deadline)
            .set("has_client_signature", c.has_signature);
    }
    if let Some(f) = &t.router_fee_config {
        row.set("fee_calculator", hex_addr(&f.fee_calculator))
            .set("router_fee_on_output_bps", f.fee_on_output_bps)
            .set("router_fee_on_client_fee_bps", f.fee_on_client_fee_bps)
            .set("custom_fee_on_output", f.custom_fee_on_output)
            .set("custom_fee_on_client_fee", f.custom_fee_on_client_fee)
            .set("positive_slippage_enabled", f.positive_slippage_enabled)
            .set("fee_bps_scale", f.bps_scale);
    }
    if !t.fees_taken.is_empty() {
        row.set("router_fee_amount", fee_split.router.to_string())
            .set("client_fee_amount", fee_split.client.to_string());
    }

    for hop in &t.hops {
        let row = tables.create_row("trade_hops", format!("{trade_id}:{}", hop.index));
        row.set("trade_id", &trade_id)
            .set("chain", &t.chain)
            .set("block_number", t.block_number)
            .set("hop_index", hop.index)
            .set("executor", hex_addr(&hop.executor))
            .set("protocol_data", hex_addr(&hop.protocol_data));
        row.set("protocol_systems", psql_text_array(&hop.protocol_systems));
        if t.strategy == "split" {
            row.set("token_in_index", hop.token_in_index)
                .set("token_out_index", hop.token_out_index)
                .set("split", hop.split);
        }
    }

    for (i, fee) in t.fees_taken.iter().enumerate() {
        let row = tables.create_row("fees_taken", format!("{trade_id}:{i}"));
        row.set("trade_id", &trade_id)
            .set("chain", &t.chain)
            .set("block_number", t.block_number)
            .set("token", hex_addr(&t.token_out))
            .set("recipient", hex_addr(&fee.recipient))
            .set("amount", &fee.amount)
            .set("role", &fee.role);
    }
    Ok(())
}

#[derive(Debug)]
struct FeeSplit {
    router: BigInt,
    client: BigInt,
}

/// Attributes fees from the contract's ordered `FeesTaken` array.
fn split_fees(t: &Trade) -> Result<FeeSplit> {
    let mut split = FeeSplit { router: BigInt::zero(), client: BigInt::zero() };
    for fee in &t.fees_taken {
        let amount = parse_bigint(&fee.amount);
        match fee.role.as_str() {
            "router" => split.router += amount,
            "client" => split.client += amount,
            role => bail!("unknown fee role {role:?} for trade {}", hex_addr(&t.tx_hash)),
        }
    }
    Ok(split)
}

fn slippage_tolerance_bps(expected: &str, min: &str) -> Option<String> {
    let expected = parse_bigint(expected);
    if expected <= BigInt::zero() {
        return None;
    }
    let diff = BigDecimal::from(expected.clone() - parse_bigint(min)) * BigDecimal::from(10_000);
    Some(trim_decimal(
        (diff / BigDecimal::from(expected))
            .with_prec(24)
            .to_string(),
    ))
}

/// Strips insignificant trailing zeros and a dangling decimal point.
fn trim_decimal(s: String) -> String {
    if !s.contains('.') {
        return s;
    }
    let trimmed = s
        .trim_end_matches('0')
        .trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Formats a quoted SQL literal for a Postgres `TEXT[]` column. The sink passes values for
/// array columns through verbatim (its own `set_psql_array` leaves elements unquoted, which
/// breaks on empty strings and commas), so this emits `'{"a","b"}'` with full escaping.
fn psql_text_array(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|s| {
            format!(
                "\"{}\"",
                s.replace('\\', "\\\\")
                    .replace('"', "\\\"")
            )
        })
        .collect();
    format!("'{{{}}}'", quoted.join(",").replace('\'', "''"))
}

fn parse_bigint(s: &str) -> BigInt {
    s.parse::<BigInt>()
        .unwrap_or_else(|_| BigInt::zero())
}

fn rfc3339(unix_seconds: u64) -> String {
    let days = unix_seconds / 86_400;
    let secs = unix_seconds % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Days since 1970-01-01 to (year, month, day); Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_timestamps() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn slippage_bps() {
        assert_eq!(
            slippage_tolerance_bps("1000", "990")
                .unwrap()
                .to_string(),
            "100"
        );
        assert!(slippage_tolerance_bps("3", "2")
            .unwrap()
            .to_string()
            .starts_with("3333.3333"));
        assert!(slippage_tolerance_bps("0", "0").is_none());
        assert!(slippage_tolerance_bps("", "1").is_none());
    }

    #[test]
    fn text_array_literal_quotes_elements() {
        assert_eq!(psql_text_array(&[]), "'{}'");
        assert_eq!(psql_text_array(&["a".into(), "".into()]), r#"'{"a",""}'"#);
        assert_eq!(psql_text_array(&["x,y".into(), "q\"".into()]), r#"'{"x,y","q\""}'"#);
        assert_eq!(psql_text_array(&["it's".into()]), r#"'{"it''s"}'"#);
    }

    #[test]
    fn trims_decimals() {
        assert_eq!(trim_decimal("100.000".into()), "100");
        assert_eq!(trim_decimal("0.500".into()), "0.5");
        assert_eq!(trim_decimal("0.000".into()), "0");
        assert_eq!(trim_decimal("42".into()), "42");
        assert_eq!(trim_decimal("1000".into()), "1000");
    }

    #[test]
    fn splits_fees_between_router_and_client() {
        use crate::pb::tycho::router::v1::FeeTaken;
        let t = Trade {
            fees_taken: vec![
                FeeTaken { recipient: vec![0xaa; 20], amount: "10".into(), role: "router".into() },
                FeeTaken { recipient: vec![0xcc; 20], amount: "5".into(), role: "client".into() },
            ],
            ..Default::default()
        };
        let s = split_fees(&t).unwrap();
        assert_eq!(s.router.to_string(), "10");
        assert_eq!(s.client.to_string(), "5");
    }

    #[test]
    fn same_recipient_keeps_contract_fee_roles() {
        use crate::pb::tycho::router::v1::FeeTaken;
        let t = Trade {
            fees_taken: vec![
                FeeTaken { recipient: vec![0xee; 20], amount: "7".into(), role: "router".into() },
                FeeTaken { recipient: vec![0xee; 20], amount: "3".into(), role: "client".into() },
            ],
            ..Default::default()
        };
        let split = split_fees(&t).unwrap();
        assert_eq!(split.router.to_string(), "7");
        assert_eq!(split.client.to_string(), "3");
    }

    #[test]
    fn rejects_unknown_fee_role() {
        use crate::pb::tycho::router::v1::FeeTaken;
        let t = Trade {
            fees_taken: vec![FeeTaken {
                recipient: vec![0xee; 20],
                amount: "7".into(),
                role: "unknown".into(),
            }],
            ..Default::default()
        };
        assert!(split_fees(&t)
            .unwrap_err()
            .to_string()
            .contains("unknown fee role"));
    }
}
