//! Append-only version history shared by guidance, recipes, and standards. Each
//! `put` records a snapshot (the whole doc as JSON) with the next sequential
//! version for its `(doc_type, slug)` key. History is a passthrough; the diff is
//! computed UI-side over snapshot bodies, so this layer stays doc-type agnostic.

use frontkeep_storage::Db;
use serde::Serialize;
use serde_json::Value;
use sqlx::Row;

use crate::RegistryError;

#[derive(Debug, Clone, Serialize)]
pub struct Version {
    pub version: i64,
    /// `created` | `updated` | `approved`.
    pub action: String,
    pub author: String,
    pub changed_at: String,
    pub snapshot: Value,
}

/// Append a new version for a doc, returning its version number. The version is
/// `MAX(version)+1` for the `(doc_type, slug)` key (1 for the first).
pub async fn append(
    db: &Db,
    doc_type: &str,
    slug: &str,
    action: &str,
    author: &str,
    snapshot: &Value,
) -> Result<i64, RegistryError> {
    let prev: Option<i64> = sqlx::query_scalar(
        &db.q("SELECT MAX(version) FROM knowledge_versions WHERE doc_type = ? AND slug = ?"),
    )
    .bind(doc_type)
    .bind(slug)
    .fetch_one(db.pool())
    .await?;
    let version = prev.unwrap_or(0) + 1;
    sqlx::query(&db.q("INSERT INTO knowledge_versions \
         (id, doc_type, slug, version, action, author, snapshot, changed_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)"))
    .bind(frontkeep_storage::new_uid())
    .bind(doc_type)
    .bind(slug)
    .bind(version)
    .bind(action)
    .bind(author)
    .bind(serde_json::to_string(snapshot).unwrap_or_else(|_| "{}".into()))
    .bind(frontkeep_storage::now())
    .execute(db.pool())
    .await?;
    Ok(version)
}

/// A doc's version history, newest first.
pub async fn history(db: &Db, doc_type: &str, slug: &str) -> Result<Vec<Version>, RegistryError> {
    let rows = sqlx::query(&db.q(
        "SELECT version, action, author, snapshot, changed_at FROM knowledge_versions \
         WHERE doc_type = ? AND slug = ? ORDER BY version DESC",
    ))
    .bind(doc_type)
    .bind(slug)
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let snapshot: String = r.get("snapshot");
            Version {
                version: r.get("version"),
                action: r.get("action"),
                author: r.get("author"),
                changed_at: r.get("changed_at"),
                snapshot: serde_json::from_str(&snapshot).unwrap_or(Value::Null),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> Db {
        let path =
            std::env::temp_dir().join(format!("frontkeep-ver-{}.db", frontkeep_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        db
    }

    #[tokio::test]
    async fn append_increments_per_key_and_history_is_newest_first() {
        let db = db().await;
        let v1 = append(
            &db,
            "guidance",
            "g1",
            "created",
            "a",
            &serde_json::json!({"body": "v1"}),
        )
        .await
        .unwrap();
        let v2 = append(
            &db,
            "guidance",
            "g1",
            "updated",
            "b",
            &serde_json::json!({"body": "v2"}),
        )
        .await
        .unwrap();
        assert_eq!((v1, v2), (1, 2));
        // A different slug starts its own sequence.
        let other = append(
            &db,
            "guidance",
            "g2",
            "created",
            "a",
            &serde_json::json!({}),
        )
        .await
        .unwrap();
        assert_eq!(other, 1);

        let hist = history(&db, "guidance", "g1").await.unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].version, 2);
        assert_eq!(hist[0].action, "updated");
        assert_eq!(hist[0].snapshot["body"], "v2");
        assert_eq!(hist[1].version, 1);
    }

    #[tokio::test]
    async fn approve_action_is_recorded() {
        let db = db().await;
        append(&db, "recipes", "r1", "created", "a", &serde_json::json!({}))
            .await
            .unwrap();
        let v = append(
            &db,
            "recipes",
            "r1",
            "approved",
            "admin",
            &serde_json::json!({}),
        )
        .await
        .unwrap();
        assert_eq!(v, 2);
        let hist = history(&db, "recipes", "r1").await.unwrap();
        assert_eq!(hist[0].action, "approved");
    }
}
