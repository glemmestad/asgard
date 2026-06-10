//! Storage layer for Frontkeep.
//!
//! One connection abstraction (`Db`) over SQLite and Postgres via sqlx's `Any`
//! driver. Schema is portable SQL; ids/timestamps/json are app-side TEXT so both
//! backends behave identically (see RFC-0001 §5 and BUILD_LOG D-005).

pub mod audit;
pub mod leases;
mod migrations;

use std::sync::Once;

use sqlx::any::AnyPoolOptions;
use sqlx::AnyPool;
use thiserror::Error;

pub use sqlx::AnyPool as Pool;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migration(String),
}

static INSTALL_DRIVERS: Once = Once::new();

/// A database handle shared across the application. Cloning is cheap (the
/// underlying pool is `Arc`-backed).
#[derive(Clone)]
pub struct Db {
    pool: AnyPool,
    backend: Backend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
}

impl Db {
    /// Connect to `database_url` (e.g. `sqlite://asgard.db`, `sqlite::memory:`,
    /// or `postgres://user:pass@host/db`) and ready the pool.
    pub async fn connect(database_url: &str) -> Result<Db, StorageError> {
        INSTALL_DRIVERS.call_once(sqlx::any::install_default_drivers);

        let (url, backend) = normalize_url(database_url);
        let max = if backend == Backend::Sqlite { 1 } else { 10 };
        let pool = AnyPoolOptions::new()
            .max_connections(max)
            .connect(&url)
            .await?;
        Ok(Db { pool, backend })
    }

    pub fn pool(&self) -> &AnyPool {
        &self.pool
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Rewrite a `?`-placeholder query for this backend (`$1..$n` on Postgres,
    /// unchanged on SQLite). Call sites wrap their SQL in `db.q(...)` so the same
    /// query strings run identically on both backends (BUILD_LOG D-006).
    pub fn q(&self, sql: &str) -> String {
        rebind(sql, self.backend)
    }

    /// Apply all pending migrations. Idempotent.
    pub async fn migrate(&self) -> Result<(), StorageError> {
        migrations::run(self).await
    }

    /// Cheap connectivity check for readiness probes. Errors if the pool can't
    /// reach the database.
    pub async fn ping(&self) -> Result<(), StorageError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }
}

/// Rewrite `?` placeholders to `$1..$n` for Postgres; no-op for SQLite. Our SQL
/// contains no `?` inside string literals, so a flat scan is correct.
pub fn rebind(sql: &str, backend: Backend) -> String {
    if backend != Backend::Postgres {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = 0u32;
    for ch in sql.chars() {
        if ch == '?' {
            n += 1;
            out.push('$');
            out.push_str(&n.to_string());
        } else {
            out.push(ch);
        }
    }
    out
}

/// Normalize a user-supplied url: ensure SQLite files are created on demand and
/// classify the backend for callers.
fn normalize_url(input: &str) -> (String, Backend) {
    if input.starts_with("postgres://") || input.starts_with("postgresql://") {
        return (input.to_string(), Backend::Postgres);
    }
    // SQLite. In-memory stays as-is; file urls get create-if-missing.
    if input.contains(":memory:") {
        return (input.to_string(), Backend::Sqlite);
    }
    let mut url = input.to_string();
    if !url.contains("mode=") {
        url.push_str(if url.contains('?') { "&" } else { "?" });
        url.push_str("mode=rwc");
    }
    (url, Backend::Sqlite)
}

/// Generate a fresh unique id (UUIDv4 hyphenated).
pub fn new_uid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Current time as an RFC3339 UTC string (sortable, portable).
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// An ISO-8601 instant `n` days after `iso` (RFC3339, millis, UTC). Used for
/// review deadlines. Falls back to `now() + n days` when `iso` doesn't parse.
pub fn plus_days(iso: &str, n: i64) -> String {
    let base = chrono::DateTime::parse_from_rfc3339(iso)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now());
    (base + chrono::Duration::days(n)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// An ISO-8601 instant `secs` seconds after `iso` (RFC3339, millis, UTC). Same
/// format as [`now`], so lease `expires_at` values sort lexicographically against
/// it. Falls back to `now() + secs` when `iso` doesn't parse.
pub fn plus_seconds(iso: &str, secs: i64) -> String {
    let base = chrono::DateTime::parse_from_rfc3339(iso)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now());
    (base + chrono::Duration::seconds(secs)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Whole seconds between two RFC3339 instants (`end - start`). `None` if either
/// input is unparseable. Used for promotion cycle-time metrics.
pub fn seconds_between(start_iso: &str, end_iso: &str) -> Option<i64> {
    let start = chrono::DateTime::parse_from_rfc3339(start_iso).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(end_iso).ok()?;
    Some((end - start).num_seconds())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{self, AuditQuery, AuditRecord};

    async fn fresh_sqlite() -> Db {
        let path = std::env::temp_dir().join(format!("asgard-st-{}.db", new_uid()));
        let url = format!("sqlite://{}", path.display());
        let db = Db::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        db
    }

    #[tokio::test]
    async fn migrate_is_idempotent() {
        let db = fresh_sqlite().await;
        db.migrate().await.unwrap();
        db.migrate().await.unwrap();
        assert_eq!(db.backend(), Backend::Sqlite);
    }

    #[tokio::test]
    async fn audit_roundtrip() {
        let db = fresh_sqlite().await;
        let rec = AuditRecord::new("alice", "entity.upserted")
            .entity("agent:default/x")
            .trace("trace-1")
            .outcome("allow")
            .reason("ingested");
        audit::append(&db, &rec).await.unwrap();

        let by_trace = audit::query(&db, &AuditQuery::new().trace("trace-1"))
            .await
            .unwrap();
        assert_eq!(by_trace.len(), 1);
        assert_eq!(by_trace[0].actor, "alice");
        assert_eq!(by_trace[0].entity_ref.as_deref(), Some("agent:default/x"));

        let none = audit::query(&db, &AuditQuery::new().trace("nope"))
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn rebind_rewrites_for_postgres_only() {
        let sql = "INSERT INTO t (a, b) VALUES (?, ?)";
        assert_eq!(rebind(sql, Backend::Sqlite), sql);
        assert_eq!(
            rebind(sql, Backend::Postgres),
            "INSERT INTO t (a, b) VALUES ($1, $2)"
        );
    }
}
