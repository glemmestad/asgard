//! Cost rollups over `usage_events`. The attribution dimensions are denormalized
//! onto each event at insert time, so every report is a single-table GROUP BY.
//! The `by` dimension is chosen from a closed enum (never interpolated from user
//! input) so the column name can't be injected.

use frontkeep_storage::Db;
use serde::Serialize;
use sqlx::Row;

use crate::RegistryError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostDim {
    Project,
    Owner,
    Manager,
    Group,
    Classification,
    Model,
    Provider,
}

impl CostDim {
    pub fn parse(s: &str) -> Option<CostDim> {
        Some(match s {
            "project" | "project_id" => CostDim::Project,
            "owner" => CostDim::Owner,
            "manager" => CostDim::Manager,
            "group" | "cost_group" => CostDim::Group,
            "classification" | "tier" => CostDim::Classification,
            "model" => CostDim::Model,
            "provider" => CostDim::Provider,
            _ => return None,
        })
    }

    /// The physical column. Fixed strings only — never user input.
    fn column(&self) -> &'static str {
        match self {
            CostDim::Project => "project_id",
            CostDim::Owner => "owner",
            CostDim::Manager => "manager",
            CostDim::Group => "cost_group",
            CostDim::Classification => "classification",
            CostDim::Model => "model",
            CostDim::Provider => "provider",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            CostDim::Project => "project",
            CostDim::Owner => "owner",
            CostDim::Manager => "manager",
            CostDim::Group => "group",
            CostDim::Classification => "classification",
            CostDim::Model => "model",
            CostDim::Provider => "provider",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CostRow {
    pub key: String,
    pub total_usd: f64,
    pub tokens: i64,
    pub events: i64,
    pub pct_of_total: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostReport {
    pub by: String,
    pub total_usd: f64,
    pub rows: Vec<CostRow>,
}

pub async fn report(
    db: &Db,
    by: CostDim,
    since: Option<&str>,
    until: Option<&str>,
    scope: Option<&str>,
) -> Result<CostReport, RegistryError> {
    let col = by.column();
    let key_expr = format!("COALESCE(NULLIF({col}, ''), 'unknown')");
    let mut sql = format!(
        "SELECT {key_expr} AS k, SUM(cost_usd) AS total, \
         SUM(prompt_tokens + completion_tokens) AS tokens, COUNT(*) AS events \
         FROM usage_events WHERE 1=1"
    );
    if since.is_some() {
        sql.push_str(" AND ts >= ?");
    }
    if until.is_some() {
        sql.push_str(" AND ts < ?");
    }
    // Relationship scope: a non-privileged caller sees only rows for projects they
    // own or manage (both denormalized onto every usage row at insert time).
    if scope.is_some() {
        sql.push_str(" AND (owner = ? OR manager = ?)");
    }
    sql.push_str(&format!(" GROUP BY {col} ORDER BY total DESC"));

    let sql = db.q(&sql);
    let mut q = sqlx::query(&sql);
    if let Some(s) = since {
        q = q.bind(s.to_string());
    }
    if let Some(u) = until {
        q = q.bind(u.to_string());
    }
    if let Some(email) = scope {
        q = q.bind(email.to_string()).bind(email.to_string());
    }
    let rows = q.fetch_all(db.pool()).await?;

    let mut out: Vec<CostRow> = rows
        .into_iter()
        .map(|r| CostRow {
            key: r.get::<String, _>("k"),
            total_usd: r.get::<Option<f64>, _>("total").unwrap_or(0.0),
            tokens: r.get::<Option<i64>, _>("tokens").unwrap_or(0),
            events: r.get::<Option<i64>, _>("events").unwrap_or(0),
            pct_of_total: 0.0,
        })
        .collect();

    let total: f64 = out.iter().map(|r| r.total_usd).sum();
    if total > 0.0 {
        for r in &mut out {
            r.pct_of_total = (r.total_usd / total * 1000.0).round() / 10.0;
        }
    }

    Ok(CostReport {
        by: by.as_str().to_string(),
        total_usd: total,
        rows: out,
    })
}
