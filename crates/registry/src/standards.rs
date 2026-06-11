//! Standards: the normative engineering/security/workflow rules an agent's output
//! must conform to. Moved from embedded `include_str!` constants into the DB so an
//! admin can edit them and every edit is versioned — but, unlike guidance/recipes,
//! there is no draft queue: standards are normative, so rows are always published
//! and edits are admin-only. The embedded `frontkeep_catalog::standards::STANDARDS`
//! const remains the single seed source into an empty table.

use frontkeep_storage::Db;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::RegistryError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Standard {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub updated_at: String,
}

fn row_to_standard(row: &sqlx::any::AnyRow) -> Standard {
    Standard {
        id: row.get("id"),
        title: row.get("title"),
        summary: row.get("summary"),
        body: row.get("body"),
        author: row.get("author"),
        status: row.get("status"),
        updated_at: row.get("updated_at"),
    }
}

/// All standards, ordered by id. An optional case-insensitive `q` matches title,
/// summary, or body (the three docs are short, so returning bodies for search is
/// trivial and keeps client + server search uniform).
pub async fn list(db: &Db, q: Option<&str>) -> Result<Vec<Standard>, RegistryError> {
    let mut sql =
        "SELECT id, title, summary, body, author, status, updated_at FROM standards".to_string();
    if q.is_some() {
        sql.push_str(" WHERE LOWER(title || ' ' || summary || ' ' || body) LIKE ?");
    }
    sql.push_str(" ORDER BY id");
    let sql = db.q(&sql);
    let mut query = sqlx::query(&sql);
    if let Some(term) = q {
        query = query.bind(format!("%{}%", term.to_lowercase()));
    }
    let rows = query.fetch_all(db.pool()).await?;
    Ok(rows.iter().map(row_to_standard).collect())
}

pub async fn get(db: &Db, id: &str) -> Result<Option<Standard>, RegistryError> {
    let row = sqlx::query(&db.q(
        "SELECT id, title, summary, body, author, status, updated_at FROM standards WHERE id = ?",
    ))
    .bind(id)
    .fetch_optional(db.pool())
    .await?;
    Ok(row.as_ref().map(row_to_standard))
}

/// Create or update a standard, keyed by id. Always published (normative).
pub async fn put(
    db: &Db,
    id: &str,
    title: &str,
    summary: &str,
    body: &str,
    author: &str,
) -> Result<Standard, RegistryError> {
    let id = id.trim();
    if id.is_empty() {
        return Err(RegistryError::Validation("standard needs an id".into()));
    }
    let now = frontkeep_storage::now();
    sqlx::query(&db.q(
        "INSERT INTO standards (id, title, summary, body, author, status, updated_at) \
         VALUES (?, ?, ?, ?, ?, 'published', ?) \
         ON CONFLICT(id) DO UPDATE SET title = excluded.title, summary = excluded.summary, \
          body = excluded.body, author = excluded.author, status = 'published', \
          updated_at = excluded.updated_at",
    ))
    .bind(id)
    .bind(title)
    .bind(summary)
    .bind(body)
    .bind(author)
    .bind(&now)
    .execute(db.pool())
    .await?;
    Ok(Standard {
        id: id.to_string(),
        title: title.to_string(),
        summary: summary.to_string(),
        body: body.to_string(),
        author: author.to_string(),
        status: "published".to_string(),
        updated_at: now,
    })
}

pub async fn count(db: &Db) -> Result<i64, RegistryError> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM standards")
        .fetch_one(db.pool())
        .await?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> Db {
        let path =
            std::env::temp_dir().join(format!("frontkeep-std-{}.db", frontkeep_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        db
    }

    #[tokio::test]
    async fn put_get_list_and_search() {
        let db = db().await;
        put(
            &db,
            "coding",
            "Engineering Standards",
            "code + tests",
            "Use conventional commits.",
            "frontkeep",
        )
        .await
        .unwrap();
        put(
            &db,
            "security",
            "Security",
            "secrets + least privilege",
            "No shadow AI.",
            "frontkeep",
        )
        .await
        .unwrap();
        assert_eq!(count(&db).await.unwrap(), 2);
        let got = get(&db, "coding").await.unwrap().unwrap();
        assert_eq!(got.title, "Engineering Standards");
        assert_eq!(got.status, "published");
        // Upsert, not duplicate.
        put(
            &db,
            "coding",
            "Engineering Standards v2",
            "",
            "Updated body.",
            "admin",
        )
        .await
        .unwrap();
        assert_eq!(count(&db).await.unwrap(), 2);
        // Case-insensitive search over body.
        let hits = list(&db, Some("SHADOW")).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "security");
    }
}
