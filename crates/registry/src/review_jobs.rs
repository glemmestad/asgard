//! The async code-review job queue. A `code-review` promotion enqueues one row,
//! transitions to `Reviewing`, and returns; the background worker in `serve`
//! leases the row, runs the panel, finalizes the promotion, and marks it `done`.
//! State lives here (not in memory), so a restart resumes: a crashed run leaves a
//! stale lease that [`ReviewJobs::reclaim_stale`] returns to `pending`.

use frontkeep_storage::Db;
use sqlx::Row;

use crate::RegistryError;

/// A queued (or in-flight) code-review job. `status` is `pending|running|done|failed`.
#[derive(Debug, Clone)]
pub struct ReviewJob {
    pub id: String,
    pub request_id: String,
    pub project_id: String,
    pub target: String,
    pub status: String,
    pub attempts: i64,
}

#[derive(Clone)]
pub struct ReviewJobs {
    db: Db,
}

impl ReviewJobs {
    pub fn new(db: Db) -> Self {
        ReviewJobs { db }
    }

    /// Queue a review for a promotion request. Idempotent per request: a re-run
    /// supersedes the prior workflow request (new `request_id`), so a fresh row is
    /// correct; we don't dedupe across attempts here.
    pub async fn enqueue(
        &self,
        request_id: &str,
        project_id: &str,
        target: &str,
    ) -> Result<String, RegistryError> {
        let id = frontkeep_storage::new_uid();
        let now = frontkeep_storage::now();
        sqlx::query(&self.db.q(
            "INSERT INTO review_jobs (id, request_id, project_id, target, status, attempts, created_at, updated_at) \
             VALUES (?, ?, ?, ?, 'pending', 0, ?, ?)",
        ))
        .bind(&id)
        .bind(request_id)
        .bind(project_id)
        .bind(target)
        .bind(&now)
        .bind(&now)
        .execute(self.db.pool())
        .await?;
        Ok(id)
    }

    /// Return `running` jobs whose lease has expired to `pending` (a worker crashed
    /// mid-run). Returns how many were reclaimed.
    pub async fn reclaim_stale(&self) -> Result<u64, RegistryError> {
        let now = frontkeep_storage::now();
        let res = sqlx::query(&self.db.q(
            "UPDATE review_jobs SET status = 'pending', lease_until = NULL, updated_at = ? \
             WHERE status = 'running' AND lease_until IS NOT NULL AND lease_until < ?",
        ))
        .bind(&now)
        .bind(&now)
        .execute(self.db.pool())
        .await?;
        Ok(res.rows_affected())
    }

    /// Atomically claim one pending job: flip it to `running`, set the lease, bump
    /// `attempts`. The conditional `WHERE ... status = 'pending'` makes the claim
    /// safe without `FOR UPDATE` (portable across SQLite + Postgres) — a losing
    /// racer updates zero rows and we retry. Returns `None` when the queue is empty.
    pub async fn claim_next(&self, lease_secs: i64) -> Result<Option<ReviewJob>, RegistryError> {
        let lease_until = frontkeep_storage::plus_seconds(&frontkeep_storage::now(), lease_secs);
        loop {
            let row = sqlx::query(&self.db.q(
                "SELECT id, request_id, project_id, target, status, attempts FROM review_jobs \
                 WHERE status = 'pending' ORDER BY created_at LIMIT 1",
            ))
            .fetch_optional(self.db.pool())
            .await?;
            let Some(row) = row else { return Ok(None) };
            let id: String = row.get("id");
            let now = frontkeep_storage::now();
            let res = sqlx::query(&self.db.q(
                "UPDATE review_jobs SET status = 'running', lease_until = ?, attempts = attempts + 1, updated_at = ? \
                 WHERE id = ? AND status = 'pending'",
            ))
            .bind(&lease_until)
            .bind(&now)
            .bind(&id)
            .execute(self.db.pool())
            .await?;
            if res.rows_affected() == 1 {
                return Ok(Some(ReviewJob {
                    id,
                    request_id: row.get("request_id"),
                    project_id: row.get("project_id"),
                    target: row.get("target"),
                    status: "running".into(),
                    attempts: row.get::<i64, _>("attempts") + 1,
                }));
            }
            // Lost the race; try the next pending row.
        }
    }

    /// Mark a job complete.
    pub async fn finish(&self, id: &str) -> Result<(), RegistryError> {
        let now = frontkeep_storage::now();
        sqlx::query(&self.db.q(
            "UPDATE review_jobs SET status = 'done', lease_until = NULL, updated_at = ? WHERE id = ?",
        ))
        .bind(&now)
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Record a failed attempt. Below `max_attempts` the job returns to `pending`
    /// for a retry; at the limit it goes `failed` (the caller then finalizes the
    /// promotion fail-closed). Returns `true` when the job is terminally `failed`.
    pub async fn fail(
        &self,
        id: &str,
        error: &str,
        attempts: i64,
        max_attempts: i64,
    ) -> Result<bool, RegistryError> {
        let now = frontkeep_storage::now();
        let terminal = attempts >= max_attempts;
        let status = if terminal { "failed" } else { "pending" };
        sqlx::query(&self.db.q(
            "UPDATE review_jobs SET status = ?, lease_until = NULL, error = ?, updated_at = ? WHERE id = ?",
        ))
        .bind(status)
        .bind(error)
        .bind(&now)
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(terminal)
    }

    /// The newest job for a request (status surface for the UI / e2e).
    pub async fn latest_for_request(
        &self,
        request_id: &str,
    ) -> Result<Option<ReviewJob>, RegistryError> {
        let row = sqlx::query(&self.db.q(
            "SELECT id, request_id, project_id, target, status, attempts FROM review_jobs \
             WHERE request_id = ? ORDER BY created_at DESC LIMIT 1",
        ))
        .bind(request_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(|row| ReviewJob {
            id: row.get("id"),
            request_id: row.get("request_id"),
            project_id: row.get("project_id"),
            target: row.get("target"),
            status: row.get("status"),
            attempts: row.get("attempts"),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn jobs() -> ReviewJobs {
        let path =
            std::env::temp_dir().join(format!("frontkeep-rj-{}.db", frontkeep_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        ReviewJobs::new(db)
    }

    #[tokio::test]
    async fn enqueue_claim_finish_roundtrip() {
        let j = jobs().await;
        j.enqueue("req-1", "proj-1", "wide-operational")
            .await
            .unwrap();

        let claimed = j.claim_next(300).await.unwrap().expect("a pending job");
        assert_eq!(claimed.request_id, "req-1");
        assert_eq!(claimed.attempts, 1);
        assert_eq!(claimed.status, "running");

        // Nothing else pending while it's leased.
        assert!(j.claim_next(300).await.unwrap().is_none());

        j.finish(&claimed.id).await.unwrap();
        let latest = j.latest_for_request("req-1").await.unwrap().unwrap();
        assert_eq!(latest.status, "done");
    }

    #[tokio::test]
    async fn fail_retries_then_goes_terminal() {
        let j = jobs().await;
        j.enqueue("req-2", "proj-1", "critical-path").await.unwrap();
        let c = j.claim_next(300).await.unwrap().unwrap();

        // attempts=1 < max=2 → back to pending, reclaimable.
        assert!(!j.fail(&c.id, "boom", c.attempts, 2).await.unwrap());
        let c = j.claim_next(300).await.unwrap().expect("retried");
        assert_eq!(c.attempts, 2);

        // attempts=2 >= max=2 → terminal failed.
        assert!(j.fail(&c.id, "boom again", c.attempts, 2).await.unwrap());
        assert_eq!(
            j.latest_for_request("req-2").await.unwrap().unwrap().status,
            "failed"
        );
        assert!(j.claim_next(300).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reclaim_returns_expired_lease_to_pending() {
        let j = jobs().await;
        j.enqueue("req-3", "proj-1", "wide-operational")
            .await
            .unwrap();
        // A lease that's already expired (negative duration) → reclaimable.
        let c = j.claim_next(-10).await.unwrap().unwrap();
        assert_eq!(j.reclaim_stale().await.unwrap(), 1);
        let again = j.claim_next(300).await.unwrap().expect("reclaimed");
        assert_eq!(again.id, c.id);
        assert_eq!(again.attempts, 2);
    }
}
