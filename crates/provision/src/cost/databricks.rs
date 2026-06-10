//! Databricks billing source: actual DBU spend for a project, read from the
//! `system.billing.usage` system table via the SQL Statement Execution API and
//! filtered to the `project=<id>` custom tag that the connector stamps on every
//! Databricks resource. This is how Databricks spend lands per-project in
//! Frontkeep's dashboard — Frontkeep fronts Databricks for cost too.
//!
//! Like the AWS source, real numbers need two operator prerequisites (both a
//! human's job): the workspace's system schemas must be enabled and readable by
//! the token's principal, and resource tags must propagate into the usage table's
//! `custom_tags`. Until then this reports `actual_usd: None` (the estimate stands
//! in) rather than inventing a figure.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::CostSource;
use crate::{CostWindow, ProvisionError, ServiceCost};

pub struct DatabricksCostSource {
    host: String,
    token: String,
    warehouse_id: String,
    client: reqwest::Client,
}

impl DatabricksCostSource {
    pub fn new(
        host: impl Into<String>,
        token: impl Into<String>,
        warehouse_id: impl Into<String>,
    ) -> Self {
        DatabricksCostSource {
            host: host.into(),
            token: token.into(),
            warehouse_id: warehouse_id.into(),
            client: reqwest::Client::new(),
        }
    }
}

/// The aggregation query: sum DBU usage × list price over the window for rows
/// tagged with this project. Named markers are bound via the API's `parameters`
/// array (no string interpolation into SQL).
const USAGE_SQL: &str = "\
SELECT COALESCE(SUM(u.usage_quantity * lp.pricing.default), 0) AS cost_usd \
FROM system.billing.usage u \
JOIN system.billing.list_prices lp \
  ON u.sku_name = lp.sku_name \
 AND u.usage_end_time >= lp.price_start_time \
 AND (lp.price_end_time IS NULL OR u.usage_end_time < lp.price_end_time) \
WHERE u.usage_date >= CAST(:start AS DATE) \
  AND u.usage_date < CAST(:end AS DATE) \
  AND u.custom_tags.project = :pid";

#[async_trait]
impl CostSource for DatabricksCostSource {
    fn name(&self) -> &str {
        "databricks-billing"
    }

    async fn cost(
        &self,
        project_id: &str,
        window: &CostWindow,
    ) -> Result<ServiceCost, ProvisionError> {
        let body = json!({
            "warehouse_id": self.warehouse_id,
            "statement": USAGE_SQL,
            "wait_timeout": "30s",
            "on_wait_timeout": "CANCEL",
            "parameters": [
                { "name": "start", "value": window.start },
                { "name": "end", "value": window.end },
                { "name": "pid", "value": project_id },
            ],
        });
        let url = format!("{}/api/2.0/sql/statements", self.host.trim_end_matches('/'));
        let resp = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&body)
            .send()
            .await;
        let unavailable = |why: String| ServiceCost {
            backend: "databricks".into(),
            actual_usd: None,
            source: format!("databricks-billing unavailable: {why}"),
        };
        let resp = match resp {
            Ok(r) => r,
            Err(e) => return Ok(unavailable(e.to_string())),
        };
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            return Ok(unavailable(format!("http {status}")));
        }
        let payload: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Ok(unavailable(e.to_string())),
        };
        match parse_cost(&payload) {
            Some(usd) => Ok(ServiceCost {
                backend: "databricks".into(),
                actual_usd: Some(usd),
                source: "databricks-billing".into(),
            }),
            // SUCCEEDED-but-empty is a real $0; anything else (PENDING after the
            // wait timeout, FAILED) is "not measured", not zero.
            None => Ok(unavailable(state_of(&payload))),
        }
    }
}

/// Extract the scalar cost from a Statement Execution API response. Returns
/// `Some` only when the statement SUCCEEDED and a numeric cell is present.
fn parse_cost(payload: &Value) -> Option<f64> {
    if payload.pointer("/status/state")?.as_str()? != "SUCCEEDED" {
        return None;
    }
    let cell = payload.pointer("/result/data_array/0/0")?;
    match cell {
        Value::String(s) => s.parse::<f64>().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn state_of(payload: &Value) -> String {
    payload
        .pointer("/status/state")
        .and_then(|s| s.as_str())
        .unwrap_or("no result")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_filters_by_project_tag_and_window() {
        assert!(USAGE_SQL.contains("system.billing.usage"));
        assert!(USAGE_SQL.contains("custom_tags.project = :pid"));
        assert!(USAGE_SQL.contains(":start") && USAGE_SQL.contains(":end"));
    }

    #[test]
    fn parses_succeeded_scalar() {
        let v = json!({
            "status": {"state": "SUCCEEDED"},
            "result": {"data_array": [["123.45"]]}
        });
        assert_eq!(parse_cost(&v), Some(123.45));
    }

    #[test]
    fn succeeded_zero_is_a_real_zero() {
        let v = json!({"status": {"state": "SUCCEEDED"}, "result": {"data_array": [["0"]]}});
        assert_eq!(parse_cost(&v), Some(0.0));
    }

    #[test]
    fn non_succeeded_is_not_measured() {
        let pending = json!({"status": {"state": "PENDING"}});
        assert_eq!(parse_cost(&pending), None);
        let failed = json!({"status": {"state": "FAILED"}, "result": {"data_array": [["9"]]}});
        assert_eq!(parse_cost(&failed), None);
    }
}
