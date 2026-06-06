use crate::{Backend, Db, StorageError};

/// Ordered (version, sql) pairs embedded at build time.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../migrations/0001_init.sql")),
    (2, include_str!("../migrations/0002_registry_cost.sql")),
    (3, include_str!("../migrations/0003_secrets.sql")),
    (4, include_str!("../migrations/0004_cost_rollup.sql")),
    (5, include_str!("../migrations/0005_user_roles.sql")),
    (6, include_str!("../migrations/0006_guidance.sql")),
    (7, include_str!("../migrations/0007_recipes.sql")),
    (8, include_str!("../migrations/0008_guidance_status.sql")),
    (9, include_str!("../migrations/0009_recipe_body.sql")),
    (
        10,
        include_str!("../migrations/0010_personal_access_tokens.sql"),
    ),
    (11, include_str!("../migrations/0011_evidence_fields.sql")),
    (12, include_str!("../migrations/0012_review_dates.sql")),
    (13, include_str!("../migrations/0013_recipe_status.sql")),
    (14, include_str!("../migrations/0014_guidance_category.sql")),
    (15, include_str!("../migrations/0015_standards.sql")),
    (
        16,
        include_str!("../migrations/0016_knowledge_versions.sql"),
    ),
    (17, include_str!("../migrations/0017_pat_suffix.sql")),
    (18, include_str!("../migrations/0018_tf_state.sql")),
    (19, include_str!("../migrations/0019_leases.sql")),
    (20, include_str!("../migrations/0020_tf_state_version.sql")),
    (
        21,
        include_str!("../migrations/0021_cost_anomaly_unique.sql"),
    ),
    (22, include_str!("../migrations/0022_mcp_servers.sql")),
    (
        23,
        include_str!("../migrations/0023_async_provisioning.sql"),
    ),
    (24, include_str!("../migrations/0024_promotion_reviews.sql")),
    (25, include_str!("../migrations/0025_review_jobs.sql")),
    (26, include_str!("../migrations/0026_skills.sql")),
];

pub async fn run(db: &Db) -> Result<(), StorageError> {
    // Serialize concurrent boots against one Postgres so N replicas don't race
    // check-then-DDL (ALTER ADD COLUMN is not idempotent, so the loser would
    // error). pg_advisory_lock is session-scoped: hold one connection for the
    // whole run and release it explicitly before the connection returns to the
    // pool. No-op on SQLite, where a single process and single connection already
    // serialize boots.
    let mut conn = db.pool().acquire().await?;
    if db.backend() == Backend::Postgres {
        sqlx::query("SELECT pg_advisory_lock(8472)")
            .execute(conn.as_mut())
            .await?;
    }
    let result = apply_all(db, conn.as_mut()).await;
    if db.backend() == Backend::Postgres {
        let _ = sqlx::query("SELECT pg_advisory_unlock(8472)")
            .execute(conn.as_mut())
            .await;
    }
    result
}

async fn apply_all(db: &Db, conn: &mut sqlx::AnyConnection) -> Result<(), StorageError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS _migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
    )
    .execute(&mut *conn)
    .await?;

    for (version, sql) in MIGRATIONS {
        let already: Option<i64> =
            sqlx::query_scalar(&db.q("SELECT version FROM _migrations WHERE version = ?"))
                .bind(*version)
                .fetch_optional(&mut *conn)
                .await?;
        if already.is_some() {
            continue;
        }

        // Statements run individually: the `Any` driver does not guarantee
        // multi-statement execution across backends. Our DDL contains no
        // semicolons inside literals, so a simple split is safe.
        for stmt in split_statements(sql) {
            sqlx::query(&stmt).execute(&mut *conn).await.map_err(|e| {
                StorageError::Migration(format!("v{version} stmt failed: {e}\n--\n{stmt}"))
            })?;
        }

        sqlx::query(&db.q("INSERT INTO _migrations (version, applied_at) VALUES (?, ?)"))
            .bind(*version)
            .bind(crate::now())
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

fn split_statements(sql: &str) -> Vec<String> {
    sql.split(';')
        .map(|s| {
            // Drop line comments so the trimmed statement is real DDL or empty.
            s.lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}
