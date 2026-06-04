//! Persistence for the daily cost rollup, end-of-month forecast, and anomaly
//! flags (the data layer the cost dashboard reads). Org dimensions are
//! denormalized onto every row at insert time, mirroring `usage_events`, so
//! reports are single-table `GROUP BY`s and never join `projects_runtime` at read
//! time.

use asgard_storage::Db;
use serde::Serialize;
use sqlx::Row;

use crate::ProvisionError;

/// One day's cost for a `(project, service, source)`. `actual_usd` is the day's
/// delta (NULL = unmeasured); `cumulative_usd` is the month-to-date figure the
/// source reported, kept alongside so deltas are debuggable.
#[derive(Debug, Clone, Serialize)]
pub struct RollupRow {
    pub project_id: String,
    pub day: String,
    pub service: String,
    pub source: String,
    pub estimated_usd: f64,
    pub actual_usd: Option<f64>,
    pub cumulative_usd: Option<f64>,
    pub owner: String,
    pub manager: String,
    pub cost_group: String,
    pub cost_center: String,
    pub classification: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForecastRow {
    pub project_id: String,
    pub as_of_day: String,
    pub method: String,
    pub eom_usd: f64,
    pub low_usd: f64,
    pub high_usd: f64,
    pub r2: Option<f64>,
    pub n_days: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnomalyRow {
    pub project_id: String,
    pub day: String,
    pub service: String,
    pub expected_usd: f64,
    pub actual_usd: f64,
    pub z_score: f64,
    pub severity: String,
}

/// A dimension to roll spend up by. A closed enum mapped to fixed column names —
/// never interpolated from user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollupDim {
    Project,
    Owner,
    Manager,
    Group,
    CostCenter,
    Classification,
    Service,
}

impl RollupDim {
    pub fn parse(s: &str) -> Option<RollupDim> {
        Some(match s {
            "project" | "project_id" => RollupDim::Project,
            "owner" => RollupDim::Owner,
            "manager" => RollupDim::Manager,
            "group" | "cost_group" => RollupDim::Group,
            "cost_center" | "cost-center" => RollupDim::CostCenter,
            "classification" | "tier" => RollupDim::Classification,
            "service" => RollupDim::Service,
            _ => return None,
        })
    }

    fn column(&self) -> &'static str {
        match self {
            RollupDim::Project => "project_id",
            RollupDim::Owner => "owner",
            RollupDim::Manager => "manager",
            RollupDim::Group => "cost_group",
            RollupDim::CostCenter => "cost_center",
            RollupDim::Classification => "classification",
            RollupDim::Service => "service",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            RollupDim::Project => "project",
            RollupDim::Owner => "owner",
            RollupDim::Manager => "manager",
            RollupDim::Group => "group",
            RollupDim::CostCenter => "cost_center",
            RollupDim::Classification => "classification",
            RollupDim::Service => "service",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DimRow {
    pub key: String,
    pub actual_usd: f64,
    pub estimated_usd: f64,
}

/// A project's spend over a window with its denormalized org dimensions — the
/// raw material the dashboard's org tree and top-movers are assembled from.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectFact {
    pub project_id: String,
    pub owner: String,
    pub manager: String,
    pub cost_group: String,
    pub cost_center: String,
    pub classification: String,
    pub mtd_usd: f64,
    pub estimated_usd: f64,
}

#[derive(Clone)]
pub struct CostRollupRepo {
    db: Db,
    /// When set, every read is restricted to projects this email owns or manages
    /// (relationship scope). `None` = unrestricted (Admin/Finance).
    scope_email: Option<String>,
}

impl CostRollupRepo {
    pub fn new(db: Db) -> Self {
        CostRollupRepo {
            db,
            scope_email: None,
        }
    }

    /// A view of this repo restricted to one email's owned/managed projects.
    /// `None` returns an unrestricted view. The relationship columns (owner,
    /// manager) are denormalized onto every rollup row, so the filter is a plain
    /// predicate — no join.
    pub fn scoped(&self, email: Option<String>) -> Self {
        CostRollupRepo {
            db: self.db.clone(),
            scope_email: email,
        }
    }

    /// Idempotent upsert keyed on `(project, day, service, source)`: re-running the
    /// rollup for the same day overwrites the row rather than duplicating it.
    pub async fn upsert_daily(&self, r: &RollupRow) -> Result<(), ProvisionError> {
        sqlx::query(&self.db.q("INSERT INTO cost_rollup \
             (id, project_id, day, service, source, estimated_usd, actual_usd, cumulative_usd, \
              owner, manager, cost_group, cost_center, classification, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(project_id, day, service, source) DO UPDATE SET \
              estimated_usd = excluded.estimated_usd, actual_usd = excluded.actual_usd, \
              cumulative_usd = excluded.cumulative_usd, owner = excluded.owner, \
              manager = excluded.manager, cost_group = excluded.cost_group, \
              cost_center = excluded.cost_center, classification = excluded.classification"))
        .bind(asgard_storage::new_uid())
        .bind(&r.project_id)
        .bind(&r.day)
        .bind(&r.service)
        .bind(&r.source)
        .bind(r.estimated_usd)
        .bind(r.actual_usd)
        .bind(r.cumulative_usd)
        .bind(&r.owner)
        .bind(&r.manager)
        .bind(&r.cost_group)
        .bind(&r.cost_center)
        .bind(&r.classification)
        .bind(asgard_storage::now())
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// The most recent stored cumulative MTD for a `(project, service, source)`
    /// strictly before `day` — the baseline a cloud source's MTD figure is
    /// differenced against to recover the day's actual.
    pub async fn cumulative_for(
        &self,
        project_id: &str,
        service: &str,
        source: &str,
        day: &str,
    ) -> Result<Option<f64>, ProvisionError> {
        let v: Option<Option<f64>> =
            sqlx::query_scalar(&self.db.q("SELECT cumulative_usd FROM cost_rollup \
             WHERE project_id = ? AND service = ? AND source = ? AND day < ? \
             ORDER BY day DESC LIMIT 1"))
            .bind(project_id)
            .bind(service)
            .bind(source)
            .bind(day)
            .fetch_optional(self.db.pool())
            .await?;
        Ok(v.flatten())
    }

    pub async fn series(
        &self,
        project_id: &str,
        from_day: &str,
        to_day: &str,
    ) -> Result<Vec<RollupRow>, ProvisionError> {
        let scope_sql = if self.scope_email.is_some() {
            " AND (owner = ? OR manager = ?)"
        } else {
            ""
        };
        let sql = format!(
            "SELECT project_id, day, service, source, estimated_usd, actual_usd, cumulative_usd, \
             owner, manager, cost_group, cost_center, classification FROM cost_rollup \
             WHERE project_id = ? AND day >= ? AND day <= ?{scope_sql} ORDER BY day, service, source"
        );
        let qsql = self.db.q(&sql);
        let mut q = sqlx::query(&qsql)
            .bind(project_id)
            .bind(from_day)
            .bind(to_day);
        if let Some(email) = &self.scope_email {
            q = q.bind(email.clone()).bind(email.clone());
        }
        let rows = q.fetch_all(self.db.pool()).await?;
        Ok(rows.into_iter().map(row_to_rollup).collect())
    }

    /// Spend rolled up by a denormalized dimension over `[from, until]`. Sums the
    /// measured actual and the estimate per key.
    pub async fn by_dimension(
        &self,
        dim: RollupDim,
        from_day: &str,
        until_day: &str,
    ) -> Result<Vec<DimRow>, ProvisionError> {
        let col = dim.column();
        let key_expr = format!("COALESCE(NULLIF({col}, ''), 'unknown')");
        let scope_sql = if self.scope_email.is_some() {
            " AND (owner = ? OR manager = ?)"
        } else {
            ""
        };
        let sql = format!(
            "SELECT {key_expr} AS k, COALESCE(SUM(actual_usd), 0) AS actual, \
             COALESCE(SUM(estimated_usd), 0) AS est FROM cost_rollup \
             WHERE day >= ? AND day <= ?{scope_sql} GROUP BY {col} ORDER BY actual DESC, est DESC"
        );
        let qsql = self.db.q(&sql);
        let mut q = sqlx::query(&qsql).bind(from_day).bind(until_day);
        if let Some(email) = &self.scope_email {
            q = q.bind(email.clone()).bind(email.clone());
        }
        let rows = q.fetch_all(self.db.pool()).await?;
        Ok(rows
            .into_iter()
            .map(|r| DimRow {
                key: r.get::<String, _>("k"),
                actual_usd: r.get::<Option<f64>, _>("actual").unwrap_or(0.0),
                estimated_usd: r.get::<Option<f64>, _>("est").unwrap_or(0.0),
            })
            .collect())
    }

    /// Per-project spend + denormalized dims over `[from, until]`. Text dims are
    /// constant per project (denormalized at insert), so `MAX` just picks them up.
    pub async fn project_facts(
        &self,
        from_day: &str,
        until_day: &str,
    ) -> Result<Vec<ProjectFact>, ProvisionError> {
        let scope_sql = if self.scope_email.is_some() {
            " AND (owner = ? OR manager = ?)"
        } else {
            ""
        };
        let sql = format!(
            "SELECT project_id, MAX(owner) AS owner, MAX(manager) AS manager, \
             MAX(cost_group) AS cost_group, MAX(cost_center) AS cost_center, \
             MAX(classification) AS classification, COALESCE(SUM(actual_usd), 0) AS mtd, \
             COALESCE(SUM(estimated_usd), 0) AS est FROM cost_rollup \
             WHERE day >= ? AND day <= ?{scope_sql} GROUP BY project_id"
        );
        let qsql = self.db.q(&sql);
        let mut q = sqlx::query(&qsql).bind(from_day).bind(until_day);
        if let Some(email) = &self.scope_email {
            q = q.bind(email.clone()).bind(email.clone());
        }
        let rows = q.fetch_all(self.db.pool()).await?;
        Ok(rows
            .into_iter()
            .map(|r| ProjectFact {
                project_id: r.get("project_id"),
                owner: r.get::<Option<String>, _>("owner").unwrap_or_default(),
                manager: r.get::<Option<String>, _>("manager").unwrap_or_default(),
                cost_group: r.get::<Option<String>, _>("cost_group").unwrap_or_default(),
                cost_center: r
                    .get::<Option<String>, _>("cost_center")
                    .unwrap_or_default(),
                classification: r
                    .get::<Option<String>, _>("classification")
                    .unwrap_or_default(),
                mtd_usd: r.get::<Option<f64>, _>("mtd").unwrap_or(0.0),
                estimated_usd: r.get::<Option<f64>, _>("est").unwrap_or(0.0),
            })
            .collect())
    }

    pub async fn write_forecast(&self, f: &ForecastRow) -> Result<(), ProvisionError> {
        sqlx::query(&self.db.q("INSERT INTO cost_forecast \
             (project_id, as_of_day, method, eom_usd, low_usd, high_usd, r2, n_days, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(project_id, as_of_day) DO UPDATE SET \
              method = excluded.method, eom_usd = excluded.eom_usd, low_usd = excluded.low_usd, \
              high_usd = excluded.high_usd, r2 = excluded.r2, n_days = excluded.n_days"))
        .bind(&f.project_id)
        .bind(&f.as_of_day)
        .bind(&f.method)
        .bind(f.eom_usd)
        .bind(f.low_usd)
        .bind(f.high_usd)
        .bind(f.r2)
        .bind(f.n_days)
        .bind(asgard_storage::now())
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn latest_forecast(
        &self,
        project_id: &str,
    ) -> Result<Option<ForecastRow>, ProvisionError> {
        let row = sqlx::query(&self.db.q(
            "SELECT project_id, as_of_day, method, eom_usd, low_usd, high_usd, r2, n_days \
             FROM cost_forecast WHERE project_id = ? ORDER BY as_of_day DESC LIMIT 1",
        ))
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(row_to_forecast))
    }

    pub async fn record_anomaly(&self, a: &AnomalyRow) -> Result<(), ProvisionError> {
        sqlx::query(&self.db.q(
            "INSERT INTO cost_anomaly \
             (id, project_id, day, service, expected_usd, actual_usd, z_score, severity, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(project_id, day, service) DO UPDATE SET \
             expected_usd = excluded.expected_usd, actual_usd = excluded.actual_usd, \
             z_score = excluded.z_score, severity = excluded.severity, created_at = excluded.created_at",
        ))
        .bind(asgard_storage::new_uid())
        .bind(&a.project_id)
        .bind(&a.day)
        .bind(&a.service)
        .bind(a.expected_usd)
        .bind(a.actual_usd)
        .bind(a.z_score)
        .bind(&a.severity)
        .bind(asgard_storage::now())
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Recent anomalies, newest first. Filters to one project when given.
    pub async fn anomalies(
        &self,
        project_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<AnomalyRow>, ProvisionError> {
        let mut sql = String::from(
            "SELECT project_id, day, service, expected_usd, actual_usd, z_score, severity \
             FROM cost_anomaly WHERE 1=1",
        );
        if project_id.is_some() {
            sql.push_str(" AND project_id = ?");
        }
        // cost_anomaly carries no relationship columns, so scope via the projects
        // this email owns/manages in the rollup table.
        if self.scope_email.is_some() {
            sql.push_str(
                " AND project_id IN (SELECT DISTINCT project_id FROM cost_rollup \
                 WHERE owner = ? OR manager = ?)",
            );
        }
        sql.push_str(" ORDER BY day DESC, created_at DESC LIMIT ?");
        let sql = self.db.q(&sql);
        let mut q = sqlx::query(&sql);
        if let Some(p) = project_id {
            q = q.bind(p.to_string());
        }
        if let Some(email) = &self.scope_email {
            q = q.bind(email.clone()).bind(email.clone());
        }
        let rows = q.bind(limit).fetch_all(self.db.pool()).await?;
        Ok(rows
            .into_iter()
            .map(|r| AnomalyRow {
                project_id: r.get("project_id"),
                day: r.get("day"),
                service: r.get("service"),
                expected_usd: r.get("expected_usd"),
                actual_usd: r.get("actual_usd"),
                z_score: r.get("z_score"),
                severity: r.get("severity"),
            })
            .collect())
    }
}

fn row_to_rollup(r: sqlx::any::AnyRow) -> RollupRow {
    RollupRow {
        project_id: r.get("project_id"),
        day: r.get("day"),
        service: r.get("service"),
        source: r.get("source"),
        estimated_usd: r.get("estimated_usd"),
        actual_usd: r.get("actual_usd"),
        cumulative_usd: r.get("cumulative_usd"),
        owner: r.get("owner"),
        manager: r.get("manager"),
        cost_group: r.get("cost_group"),
        cost_center: r.get("cost_center"),
        classification: r.get("classification"),
    }
}

fn row_to_forecast(r: sqlx::any::AnyRow) -> ForecastRow {
    ForecastRow {
        project_id: r.get("project_id"),
        as_of_day: r.get("as_of_day"),
        method: r.get("method"),
        eom_usd: r.get("eom_usd"),
        low_usd: r.get("low_usd"),
        high_usd: r.get("high_usd"),
        r2: r.get("r2"),
        n_days: r.get("n_days"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repo() -> CostRollupRepo {
        let path =
            std::env::temp_dir().join(format!("asgard-rollup-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        CostRollupRepo::new(db)
    }

    fn row(project: &str, day: &str, source: &str, actual: f64, cumulative: f64) -> RollupRow {
        RollupRow {
            project_id: project.into(),
            day: day.into(),
            service: source.into(),
            source: source.into(),
            estimated_usd: 1.0,
            actual_usd: Some(actual),
            cumulative_usd: Some(cumulative),
            owner: "o@x".into(),
            manager: "m@x".into(),
            cost_group: "platform".into(),
            cost_center: "CC-100".into(),
            classification: "poc".into(),
        }
    }

    #[tokio::test]
    async fn upsert_is_idempotent_per_key() {
        let r = repo().await;
        r.upsert_daily(&row("p1", "2026-06-10", "gateway", 2.0, 2.0))
            .await
            .unwrap();
        r.upsert_daily(&row("p1", "2026-06-10", "gateway", 5.0, 7.0))
            .await
            .unwrap();
        let s = r.series("p1", "2026-06-01", "2026-06-30").await.unwrap();
        assert_eq!(s.len(), 1, "same key must overwrite, not duplicate");
        assert_eq!(s[0].actual_usd, Some(5.0));
        assert_eq!(s[0].cumulative_usd, Some(7.0));
    }

    #[tokio::test]
    async fn cumulative_for_returns_prior_day() {
        let r = repo().await;
        r.upsert_daily(&row("p1", "2026-06-10", "aws-cost-explorer", 2.0, 2.0))
            .await
            .unwrap();
        r.upsert_daily(&row("p1", "2026-06-11", "aws-cost-explorer", 3.0, 5.0))
            .await
            .unwrap();
        let prior = r
            .cumulative_for("p1", "aws-cost-explorer", "aws-cost-explorer", "2026-06-11")
            .await
            .unwrap();
        assert_eq!(prior, Some(2.0));
        let none = r
            .cumulative_for("p1", "aws-cost-explorer", "aws-cost-explorer", "2026-06-10")
            .await
            .unwrap();
        assert!(none.is_none(), "nothing before the first day");
    }

    #[tokio::test]
    async fn by_dimension_groups_and_sums() {
        let r = repo().await;
        r.upsert_daily(&row("p1", "2026-06-10", "gateway", 2.0, 2.0))
            .await
            .unwrap();
        r.upsert_daily(&row("p2", "2026-06-10", "gateway", 4.0, 4.0))
            .await
            .unwrap();
        let by = r
            .by_dimension(RollupDim::Group, "2026-06-01", "2026-06-30")
            .await
            .unwrap();
        assert_eq!(by.len(), 1, "both projects share group platform");
        assert_eq!(by[0].key, "platform");
        assert_eq!(by[0].actual_usd, 6.0);
    }
}
