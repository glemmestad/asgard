//! Per-project virtual keys, project runtime state (budget/kill switch), and
//! usage events. Keys are stored only as SHA-256 hashes plus a short prefix.

use frontkeep_storage::Db;
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::error::GatewayError;

pub fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone)]
pub struct MintedKey {
    pub id: String,
    /// Shown to the caller exactly once.
    pub plaintext: String,
    pub prefix: String,
}

#[derive(Debug, Clone)]
pub struct ProjectRuntime {
    pub project_id: String,
    pub budget_usd: f64,
    pub spent_usd: f64,
    pub killed: bool,
    pub data_class: String,
    pub owner: String,
    pub manager: String,
    pub cost_group: String,
    pub cost_center: String,
    pub classification: String,
    pub lifecycle: String,
    pub registered: bool,
}

#[derive(Debug, Clone)]
pub struct UsageEvent {
    pub project_id: String,
    pub trace_id: Option<String>,
    pub model: String,
    pub provider: String,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cost_usd: f64,
    pub latency_ms: u32,
    pub owner: String,
    pub manager: String,
    pub cost_group: String,
    pub cost_center: String,
    pub classification: String,
}

#[derive(Clone)]
pub struct GatewayRepo {
    db: Db,
}

impl GatewayRepo {
    pub fn new(db: Db) -> Self {
        GatewayRepo { db }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Create the project runtime row if absent; never clobbers existing budget/spend.
    pub async fn ensure_project(
        &self,
        project_id: &str,
        budget_usd: f64,
        data_class: &str,
    ) -> Result<(), GatewayError> {
        sqlx::query(&self.db.q(
            "INSERT INTO projects_runtime (project_id, budget_usd, spent_usd, killed, data_class, updated_at) \
             VALUES (?, ?, 0, 0, ?, ?) ON CONFLICT(project_id) DO NOTHING",
        ))
        .bind(project_id)
        .bind(budget_usd)
        .bind(data_class)
        .bind(frontkeep_storage::now())
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn get_project(
        &self,
        project_id: &str,
    ) -> Result<Option<ProjectRuntime>, GatewayError> {
        let row = sqlx::query(&self.db.q(&format!(
            "SELECT {RUNTIME_COLS} FROM projects_runtime WHERE project_id = ?"
        )))
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(row_to_runtime))
    }

    pub async fn set_killed(&self, project_id: &str, killed: bool) -> Result<(), GatewayError> {
        sqlx::query(
            &self
                .db
                .q("UPDATE projects_runtime SET killed = ?, updated_at = ? WHERE project_id = ?"),
        )
        .bind(killed as i64)
        .bind(frontkeep_storage::now())
        .bind(project_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn set_identity(
        &self,
        project_id: &str,
        display_name: &str,
        description: &str,
    ) -> Result<(), GatewayError> {
        sqlx::query(&self.db.q(
            "UPDATE projects_runtime SET display_name = ?, description = ?, updated_at = ? \
             WHERE project_id = ?",
        ))
        .bind(display_name)
        .bind(description)
        .bind(frontkeep_storage::now())
        .bind(project_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn set_budget(&self, project_id: &str, budget_usd: f64) -> Result<(), GatewayError> {
        sqlx::query(
            &self.db.q(
                "UPDATE projects_runtime SET budget_usd = ?, updated_at = ? WHERE project_id = ?",
            ),
        )
        .bind(budget_usd)
        .bind(frontkeep_storage::now())
        .bind(project_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Add to spend and return the new total.
    pub async fn add_spend(&self, project_id: &str, amount: f64) -> Result<f64, GatewayError> {
        let spent: f64 = sqlx::query_scalar(&self.db.q(
            "UPDATE projects_runtime SET spent_usd = spent_usd + ?, updated_at = ? \
             WHERE project_id = ? RETURNING spent_usd",
        ))
        .bind(amount)
        .bind(frontkeep_storage::now())
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(spent)
    }

    pub async fn mint_key(
        &self,
        project_id: &str,
        name: Option<&str>,
    ) -> Result<MintedKey, GatewayError> {
        let plaintext = format!(
            "fk_{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let prefix = plaintext[..12].to_string();
        let id = frontkeep_storage::new_uid();
        sqlx::query(&self.db.q(
            "INSERT INTO virtual_keys (id, project_id, key_hash, key_prefix, name, active, created_at) \
             VALUES (?, ?, ?, ?, ?, 1, ?)",
        ))
        .bind(&id)
        .bind(project_id)
        .bind(sha256_hex(&plaintext))
        .bind(&prefix)
        .bind(name)
        .bind(frontkeep_storage::now())
        .execute(self.db.pool())
        .await?;
        Ok(MintedKey {
            id,
            plaintext,
            prefix,
        })
    }

    /// Returns the owning project id for an active key, if any.
    pub async fn verify_key(&self, plaintext: &str) -> Result<Option<String>, GatewayError> {
        let pid: Option<String> = sqlx::query_scalar(&self.db.q(
            "SELECT project_id FROM virtual_keys WHERE key_hash = ? AND active = 1 AND revoked_at IS NULL",
        ))
        .bind(sha256_hex(plaintext))
        .fetch_optional(self.db.pool())
        .await?;
        Ok(pid)
    }

    /// Resolve a key to its project's runtime state in a single query (hot path).
    pub async fn resolve_key(
        &self,
        plaintext: &str,
    ) -> Result<Option<ProjectRuntime>, GatewayError> {
        let row = sqlx::query(&self.db.q(&format!(
            "SELECT {RUNTIME_COLS_P} \
             FROM virtual_keys k JOIN projects_runtime p ON p.project_id = k.project_id \
             WHERE k.key_hash = ? AND k.active = 1 AND k.revoked_at IS NULL"
        )))
        .bind(sha256_hex(plaintext))
        .fetch_optional(self.db.pool())
        .await?;
        Ok(row.map(row_to_runtime))
    }

    pub async fn revoke_key(&self, key_id: &str) -> Result<(), GatewayError> {
        sqlx::query(
            &self
                .db
                .q("UPDATE virtual_keys SET active = 0, revoked_at = ? WHERE id = ?"),
        )
        .bind(frontkeep_storage::now())
        .bind(key_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn record_usage(&self, ev: &UsageEvent) -> Result<(), GatewayError> {
        sqlx::query(&self.db.q(
            "INSERT INTO usage_events (id, ts, project_id, trace_id, model, provider, \
             prompt_tokens, completion_tokens, cost_usd, latency_ms, \
             owner, manager, cost_group, cost_center, classification) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(frontkeep_storage::new_uid())
        .bind(frontkeep_storage::now())
        .bind(&ev.project_id)
        .bind(&ev.trace_id)
        .bind(&ev.model)
        .bind(&ev.provider)
        .bind(ev.prompt_tokens as i64)
        .bind(ev.completion_tokens as i64)
        .bind(ev.cost_usd)
        .bind(ev.latency_ms as i64)
        .bind(&ev.owner)
        .bind(&ev.manager)
        .bind(&ev.cost_group)
        .bind(&ev.cost_center)
        .bind(&ev.classification)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Update the registration/attribution columns on a project's runtime row.
    /// The runtime row is the system of record for cost dimensions and gating;
    /// `register_project` writes it, the gateway reads it on every call.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_registration(
        &self,
        project_id: &str,
        display_name: &str,
        description: &str,
        owner: &str,
        manager: &str,
        cost_group: &str,
        cost_center: &str,
        classification: &str,
        data_class: &str,
        budget_usd: f64,
    ) -> Result<(), GatewayError> {
        let now = frontkeep_storage::now();
        sqlx::query(&self.db.q("INSERT INTO projects_runtime \
             (project_id, budget_usd, spent_usd, killed, data_class, owner, manager, \
              cost_group, cost_center, classification, lifecycle, registered, \
              display_name, description, created_at, updated_at) \
             VALUES (?, ?, 0, 0, ?, ?, ?, ?, ?, ?, 'active', 1, ?, ?, ?, ?) \
             ON CONFLICT(project_id) DO UPDATE SET \
              budget_usd = excluded.budget_usd, data_class = excluded.data_class, \
              owner = excluded.owner, manager = excluded.manager, \
              cost_group = excluded.cost_group, cost_center = excluded.cost_center, \
              classification = excluded.classification, lifecycle = 'active', \
              registered = 1, display_name = excluded.display_name, \
              description = excluded.description, updated_at = excluded.updated_at"))
        .bind(project_id)
        .bind(budget_usd)
        .bind(data_class)
        .bind(owner)
        .bind(manager)
        .bind(cost_group)
        .bind(cost_center)
        .bind(classification)
        .bind(display_name)
        .bind(description)
        .bind(&now)
        .bind(&now)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Re-stamp the denormalized owner/manager on the runtime row. Unlike
    /// `set_registration` this touches nothing else (no lifecycle/registered reset),
    /// so an ownership transfer can't accidentally revive a killed project.
    pub async fn set_ownership(
        &self,
        project_id: &str,
        owner: &str,
        manager: &str,
    ) -> Result<(), GatewayError> {
        sqlx::query(&self.db.q(
            "UPDATE projects_runtime SET owner = ?, manager = ?, updated_at = ? WHERE project_id = ?",
        ))
        .bind(owner)
        .bind(manager)
        .bind(frontkeep_storage::now())
        .bind(project_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn set_lifecycle(
        &self,
        project_id: &str,
        lifecycle: &str,
    ) -> Result<(), GatewayError> {
        sqlx::query(
            &self.db.q(
                "UPDATE projects_runtime SET lifecycle = ?, updated_at = ? WHERE project_id = ?",
            ),
        )
        .bind(lifecycle)
        .bind(frontkeep_storage::now())
        .bind(project_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn set_classification(
        &self,
        project_id: &str,
        classification: &str,
    ) -> Result<(), GatewayError> {
        sqlx::query(&self.db.q(
            "UPDATE projects_runtime SET classification = ?, updated_at = ? WHERE project_id = ?",
        ))
        .bind(classification)
        .bind(frontkeep_storage::now())
        .bind(project_id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Total attributed spend for a project from usage events.
    pub async fn project_spend(&self, project_id: &str) -> Result<f64, GatewayError> {
        let total: Option<f64> = sqlx::query_scalar(
            &self
                .db
                .q("SELECT SUM(cost_usd) FROM usage_events WHERE project_id = ?"),
        )
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;
        Ok(total.unwrap_or(0.0))
    }
}

/// Runtime/attribution columns selected from `projects_runtime`, unqualified.
const RUNTIME_COLS: &str = "project_id, budget_usd, spent_usd, killed, data_class, \
     owner, manager, cost_group, cost_center, classification, lifecycle, registered";

/// Same columns, qualified for the `resolve_key` JOIN (alias `p`).
const RUNTIME_COLS_P: &str = "p.project_id, p.budget_usd, p.spent_usd, p.killed, p.data_class, \
     p.owner, p.manager, p.cost_group, p.cost_center, p.classification, p.lifecycle, p.registered";

fn row_to_runtime(r: sqlx::any::AnyRow) -> ProjectRuntime {
    ProjectRuntime {
        project_id: r.get("project_id"),
        budget_usd: r.get("budget_usd"),
        spent_usd: r.get("spent_usd"),
        killed: r.get::<i64, _>("killed") != 0,
        data_class: r.get("data_class"),
        owner: r.get("owner"),
        manager: r.get("manager"),
        cost_group: r.get("cost_group"),
        cost_center: r.get("cost_center"),
        classification: r.get("classification"),
        lifecycle: r.get("lifecycle"),
        registered: r.get::<i64, _>("registered") != 0,
    }
}
