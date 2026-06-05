//! Persistence for provisioned resources. `project_id` is the join key that ties
//! infra spend into the same per-project rollup as model spend.

use asgard_storage::Db;
use serde::Serialize;
use sqlx::Row;

use crate::ProvisionError;

#[derive(Debug, Clone, Serialize)]
pub struct ProvisionedRecord {
    pub id: String,
    pub project_id: String,
    pub rtype: String,
    pub name: String,
    pub spec: serde_json::Value,
    pub outputs: serde_json::Value,
    pub tags: std::collections::BTreeMap<String, String>,
    pub est_monthly_usd: f64,
    pub state: String,
    pub backend: String,
    pub dry_run: bool,
    pub request_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Failure detail for a `failed` / `destroy_failed` row; empty otherwise.
    #[serde(default)]
    pub error: String,
}

#[derive(Clone)]
pub struct ProvisionRepo {
    db: Db,
}

impl ProvisionRepo {
    pub fn new(db: Db) -> Self {
        ProvisionRepo { db }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub async fn record(&self, rec: &ProvisionedRecord) -> Result<(), ProvisionError> {
        sqlx::query(&self.db.q("INSERT INTO provisioned_resources \
             (id, project_id, rtype, name, spec, outputs, tags, est_monthly_usd, state, \
              backend, dry_run, request_id, created_at, updated_at, error) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"))
        .bind(&rec.id)
        .bind(&rec.project_id)
        .bind(&rec.rtype)
        .bind(&rec.name)
        .bind(rec.spec.to_string())
        .bind(rec.outputs.to_string())
        .bind(serde_json::to_string(&rec.tags).unwrap_or_else(|_| "{}".into()))
        .bind(rec.est_monthly_usd)
        .bind(&rec.state)
        .bind(&rec.backend)
        .bind(rec.dry_run as i64)
        .bind(&rec.request_id)
        .bind(&rec.created_at)
        .bind(&rec.updated_at)
        .bind(&rec.error)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn list_by_project(
        &self,
        project_id: &str,
    ) -> Result<Vec<ProvisionedRecord>, ProvisionError> {
        let rows = sqlx::query(&self.db.q(
            "SELECT id, project_id, rtype, name, spec, outputs, tags, est_monthly_usd, state, \
             backend, dry_run, request_id, created_at, updated_at, error \
             FROM provisioned_resources WHERE project_id = ? ORDER BY created_at DESC",
        ))
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows.into_iter().map(row_to_record).collect())
    }

    /// The resource already recorded for a workflow request, if any. Lets
    /// fulfillment be retried idempotently after a mid-operation failure without
    /// writing a duplicate row.
    pub async fn get_by_request(
        &self,
        request_id: &str,
    ) -> Result<Option<ProvisionedRecord>, ProvisionError> {
        let row = sqlx::query(&self.db.q(
            "SELECT id, project_id, rtype, name, spec, outputs, tags, est_monthly_usd, state, \
             backend, dry_run, request_id, created_at, updated_at, error \
             FROM provisioned_resources WHERE request_id = ? ORDER BY created_at LIMIT 1",
        ))
        .bind(request_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(row_to_record))
    }

    pub async fn get(&self, id: &str) -> Result<Option<ProvisionedRecord>, ProvisionError> {
        let row = sqlx::query(&self.db.q(
            "SELECT id, project_id, rtype, name, spec, outputs, tags, est_monthly_usd, state, \
             backend, dry_run, request_id, created_at, updated_at, error \
             FROM provisioned_resources WHERE id = ?",
        ))
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(row_to_record))
    }

    /// Set a resource's lifecycle `state` (free-text: `provisioned`, `suspending`,
    /// `suspended`, `destroying`, `destroyed`). Only `provisioned` accrues
    /// estimated cost, so a transition is what drops a resource off the bill.
    pub async fn mark_state(&self, id: &str, state: &str) -> Result<(), ProvisionError> {
        sqlx::query(
            &self
                .db
                .q("UPDATE provisioned_resources SET state = ?, updated_at = ? WHERE id = ?"),
        )
        .bind(state)
        .bind(asgard_storage::now())
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn mark_destroyed(&self, id: &str) -> Result<(), ProvisionError> {
        self.mark_state(id, "destroyed").await
    }

    /// Committed recurring infra estimate for one project (provisioned only).
    /// Feeds the auto-approval project-headroom check.
    pub async fn infra_total_for_project(&self, project_id: &str) -> Result<f64, ProvisionError> {
        let total: Option<f64> = sqlx::query_scalar(
            &self
                .db
                .q("SELECT SUM(est_monthly_usd) FROM provisioned_resources \
             WHERE project_id = ? AND state = 'provisioned'"),
        )
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(total.unwrap_or(0.0))
    }

    /// Like [`infra_total_for_project`](Self::infra_total_for_project) but counts
    /// in-flight (`provisioning`) rows as committed too, so concurrent async
    /// requests can't both clear the headroom check before either row flips to
    /// `provisioned`. Admission-only — billing/rollup still counts `provisioned`.
    pub async fn infra_committed_for_project(
        &self,
        project_id: &str,
    ) -> Result<f64, ProvisionError> {
        let total: Option<f64> = sqlx::query_scalar(
            &self
                .db
                .q("SELECT SUM(est_monthly_usd) FROM provisioned_resources \
             WHERE project_id = ? AND state IN ('provisioning', 'provisioned')"),
        )
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(total.unwrap_or(0.0))
    }

    /// The active record for a name, if any — `provisioning` (in-flight) or
    /// `provisioned` (live). Used to make a repeat request idempotent (return the
    /// existing record) without blocking recreate after `failed`/`destroyed`.
    pub async fn get_active_by_name(
        &self,
        project_id: &str,
        rtype: &str,
        name: &str,
    ) -> Result<Option<ProvisionedRecord>, ProvisionError> {
        let row = sqlx::query(&self.db.q(
            "SELECT id, project_id, rtype, name, spec, outputs, tags, est_monthly_usd, state, \
             backend, dry_run, request_id, created_at, updated_at, error \
             FROM provisioned_resources \
             WHERE project_id = ? AND rtype = ? AND name = ? \
             AND state IN ('provisioning', 'provisioned') ORDER BY created_at DESC LIMIT 1",
        ))
        .bind(project_id)
        .bind(rtype)
        .bind(name)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(row_to_record))
    }

    /// Work-state rows that no live worker is driving: state `provisioning` or
    /// `destroying`, and either unclaimed (`worker_owner IS NULL`) or whose claim
    /// heartbeat (`updated_at`) is older than `stale`. The reconciler re-drives
    /// these — the orphan/crash recovery path.
    pub async fn list_reclaimable(
        &self,
        stale: &str,
        limit: i64,
    ) -> Result<Vec<ProvisionedRecord>, ProvisionError> {
        let sql = format!(
            "SELECT id, project_id, rtype, name, spec, outputs, tags, est_monthly_usd, state, \
             backend, dry_run, request_id, created_at, updated_at, error \
             FROM provisioned_resources \
             WHERE state IN ('provisioning', 'destroying') \
             AND (worker_owner IS NULL OR updated_at < ?) \
             ORDER BY created_at LIMIT {limit}"
        );
        let rows = sqlx::query(&self.db.q(&sql))
            .bind(stale)
            .fetch_all(self.db.pool())
            .await?;
        Ok(rows.into_iter().map(row_to_record).collect())
    }

    /// Claim a work-state row for this worker (CAS): succeeds only if the row is
    /// still in `expect_state` and is unclaimed or its heartbeat is stale. Returns
    /// `true` if this caller won the claim. This is the dedup guard — only one
    /// worker (eager spawn or reconciler, across replicas) proceeds to apply.
    pub async fn claim(
        &self,
        id: &str,
        expect_state: &str,
        owner: &str,
        stale: &str,
    ) -> Result<bool, ProvisionError> {
        let res = sqlx::query(&self.db.q(
            "UPDATE provisioned_resources SET worker_owner = ?, updated_at = ? \
             WHERE id = ? AND state = ? AND (worker_owner IS NULL OR updated_at < ?)",
        ))
        .bind(owner)
        .bind(asgard_storage::now())
        .bind(id)
        .bind(expect_state)
        .bind(stale)
        .execute(self.db.pool())
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Refresh the claim heartbeat while a long apply/destroy runs, so the
    /// reconciler's stale check doesn't reclaim work that's still in progress.
    pub async fn heartbeat(&self, id: &str, owner: &str) -> Result<(), ProvisionError> {
        sqlx::query(&self.db.q(
            "UPDATE provisioned_resources SET updated_at = ? WHERE id = ? AND worker_owner = ?",
        ))
        .bind(asgard_storage::now())
        .bind(id)
        .bind(owner)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Terminal transition: set the final `state` (`provisioned`/`failed`/
    /// `destroyed`/`destroy_failed`), persist `outputs` + `error`, and release the
    /// claim (`worker_owner = NULL`).
    pub async fn finish(
        &self,
        id: &str,
        state: &str,
        outputs: &serde_json::Value,
        error: &str,
    ) -> Result<(), ProvisionError> {
        sqlx::query(&self.db.q(
            "UPDATE provisioned_resources SET state = ?, outputs = ?, error = ?, \
             worker_owner = NULL, updated_at = ? WHERE id = ?",
        ))
        .bind(state)
        .bind(outputs.to_string())
        .bind(error)
        .bind(asgard_storage::now())
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Recurring infra cost estimate per project (provisioned resources only).
    pub async fn infra_cost_by_project(&self) -> Result<Vec<(String, f64)>, ProvisionError> {
        let rows = sqlx::query(
            "SELECT project_id, SUM(est_monthly_usd) AS total FROM provisioned_resources \
             WHERE state = 'provisioned' GROUP BY project_id ORDER BY total DESC",
        )
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("project_id"),
                    r.get::<Option<f64>, _>("total").unwrap_or(0.0),
                )
            })
            .collect())
    }
}

fn row_to_record(r: sqlx::any::AnyRow) -> ProvisionedRecord {
    let spec: String = r.get("spec");
    let outputs: String = r.get("outputs");
    let tags: String = r.get("tags");
    ProvisionedRecord {
        id: r.get("id"),
        project_id: r.get("project_id"),
        rtype: r.get("rtype"),
        name: r.get("name"),
        spec: serde_json::from_str(&spec).unwrap_or(serde_json::Value::Null),
        outputs: serde_json::from_str(&outputs).unwrap_or(serde_json::Value::Null),
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        est_monthly_usd: r.get("est_monthly_usd"),
        state: r.get("state"),
        backend: r.get("backend"),
        dry_run: r.get::<i64, _>("dry_run") != 0,
        request_id: r.get("request_id"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        error: r.get("error"),
    }
}
