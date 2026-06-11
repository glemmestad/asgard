//! Guidance: governed how-to playbooks, authored by a human (UI) or an agent
//! (MCP) and read by both. Advisory and runtime-editable — distinct from the
//! embedded `standards` (normative) and the agent-seed (repo bootstrap).

use frontkeep_storage::Db;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::RegistryError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Guidance {
    pub slug: String,
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub author: String,
    /// `pending` (submitted, awaiting admin approval) | `published` (visible to
    /// readers). Anyone may submit; only an admin publishes.
    #[serde(default)]
    pub status: String,
    /// Facet for the source IA: `best-practice` | `guide` | `reference`.
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub updated_at: String,
}

/// Allowed guidance categories; `guide` is the neutral default.
pub const CATEGORIES: &[&str] = &["best-practice", "guide", "reference"];

/// Derive a url-safe slug from a title (lowercase, non-alphanumeric → '-').
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.trim().to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn row_to_guidance(row: &sqlx::any::AnyRow) -> Guidance {
    let tags: String = row.get("tags");
    Guidance {
        slug: row.get("slug"),
        title: row.get("title"),
        summary: row.get("summary"),
        body: row.get("body"),
        tags: tags
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        author: row.get("author"),
        status: row.get("status"),
        category: row.get("category"),
        updated_at: row.get("updated_at"),
    }
}

const COLS: &str = "slug, title, summary, body, tags, author, status, category, updated_at";

/// Guidance, newest first. `include_pending` returns drafts awaiting approval as
/// well — for admins reviewing the queue; readers get published only. `category`
/// filters to one facet; `q` is a case-insensitive match over title/summary/body.
pub async fn list(
    db: &Db,
    include_pending: bool,
    category: Option<&str>,
    q: Option<&str>,
) -> Result<Vec<Guidance>, RegistryError> {
    let mut sql = format!("SELECT {COLS} FROM guidance WHERE 1 = 1");
    if !include_pending {
        sql.push_str(" AND status = 'published'");
    }
    if category.is_some() {
        sql.push_str(" AND category = ?");
    }
    if q.is_some() {
        sql.push_str(" AND LOWER(title || ' ' || summary || ' ' || body) LIKE ?");
    }
    sql.push_str(" ORDER BY updated_at DESC");
    let sql = db.q(&sql);
    let mut query = sqlx::query(&sql);
    if let Some(c) = category {
        query = query.bind(c.to_string());
    }
    if let Some(term) = q {
        query = query.bind(format!("%{}%", term.to_lowercase()));
    }
    let rows = query.fetch_all(db.pool()).await?;
    Ok(rows.iter().map(row_to_guidance).collect())
}

/// Approve a draft (or any doc) — admin-gated at the API layer.
pub async fn set_status(db: &Db, slug: &str, status: &str) -> Result<(), RegistryError> {
    sqlx::query(&db.q("UPDATE guidance SET status = ? WHERE slug = ?"))
        .bind(status)
        .bind(slug)
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn count(db: &Db) -> Result<i64, RegistryError> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM guidance")
        .fetch_one(db.pool())
        .await?;
    Ok(n)
}

pub async fn get(db: &Db, slug: &str) -> Result<Option<Guidance>, RegistryError> {
    let row = sqlx::query(&db.q(&format!("SELECT {COLS} FROM guidance WHERE slug = ?")))
        .bind(slug)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.as_ref().map(row_to_guidance))
}

/// Create or update a guidance doc, keyed by slug (derived from the title when
/// not given). `category` is validated against [`CATEGORIES`] (empty → `guide`).
/// Returns the stored doc.
#[allow(clippy::too_many_arguments)]
pub async fn put(
    db: &Db,
    slug: Option<&str>,
    title: &str,
    summary: &str,
    body: &str,
    tags: &[String],
    author: &str,
    published: bool,
    category: &str,
) -> Result<Guidance, RegistryError> {
    let slug = match slug {
        Some(s) if !s.trim().is_empty() => slugify(s),
        _ => slugify(title),
    };
    if slug.is_empty() {
        return Err(RegistryError::Validation(
            "guidance needs a title or slug".into(),
        ));
    }
    let category = if category.trim().is_empty() {
        "guide"
    } else {
        category
    };
    if !CATEGORIES.contains(&category) {
        return Err(RegistryError::Validation(format!(
            "unknown guidance category '{category}'"
        )));
    }
    let tags_s = tags.join(",");
    let now = frontkeep_storage::now();
    let status = if published { "published" } else { "pending" };
    sqlx::query(&db.q(
        "INSERT INTO guidance (slug, title, summary, body, tags, author, status, category, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(slug) DO UPDATE SET title = excluded.title, summary = excluded.summary, \
          body = excluded.body, tags = excluded.tags, author = excluded.author, \
          status = excluded.status, category = excluded.category, updated_at = excluded.updated_at",
    ))
    .bind(&slug)
    .bind(title)
    .bind(summary)
    .bind(body)
    .bind(&tags_s)
    .bind(author)
    .bind(status)
    .bind(category)
    .bind(&now)
    .execute(db.pool())
    .await?;
    Ok(Guidance {
        slug,
        title: title.to_string(),
        summary: summary.to_string(),
        body: body.to_string(),
        tags: tags.to_vec(),
        author: author.to_string(),
        status: status.to_string(),
        category: category.to_string(),
        updated_at: now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> Db {
        let path = std::env::temp_dir().join(format!(
            "frontkeep-guid-{}.db",
            frontkeep_storage::new_uid()
        ));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        db
    }

    #[test]
    fn slugify_normalizes() {
        assert_eq!(slugify("Wire Auth0 into a SPA!"), "wire-auth0-into-a-spa");
        assert_eq!(slugify("  --Hello-- "), "hello");
    }

    #[tokio::test]
    async fn put_get_list_roundtrip_and_upsert() {
        let db = db().await;
        let g = put(
            &db,
            None,
            "Choose a model",
            "how to pick",
            "Route through the gateway.",
            &["models".into(), "gateway".into()],
            "owner@corp.example",
            true,
            "best-practice",
        )
        .await
        .unwrap();
        assert_eq!(g.slug, "choose-a-model");
        assert_eq!(g.category, "best-practice");

        let got = get(&db, "choose-a-model").await.unwrap().unwrap();
        assert_eq!(got.title, "Choose a model");
        assert_eq!(got.tags, vec!["models", "gateway"]);

        // Same slug upserts rather than duplicating. Empty category → 'guide'.
        put(
            &db,
            Some("choose-a-model"),
            "Choose a model (v2)",
            "",
            "Updated.",
            &[],
            "a@b.c",
            true,
            "",
        )
        .await
        .unwrap();
        let all = list(&db, true, None, None).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "Choose a model (v2)");
        assert_eq!(all[0].category, "guide");
    }

    #[tokio::test]
    async fn drafts_are_hidden_until_approved() {
        let db = db().await;
        put(
            &db,
            None,
            "Draft doc",
            "",
            "body",
            &[],
            "agent",
            false,
            "guide",
        )
        .await
        .unwrap();
        assert_eq!(list(&db, false, None, None).await.unwrap().len(), 0); // readers see nothing
        assert_eq!(list(&db, true, None, None).await.unwrap().len(), 1); // admin sees the draft
        set_status(&db, "draft-doc", "published").await.unwrap();
        assert_eq!(list(&db, false, None, None).await.unwrap().len(), 1); // now published
    }

    #[tokio::test]
    async fn category_filter_and_search() {
        let db = db().await;
        put(
            &db,
            None,
            "Eval harness",
            "how to eval",
            "Run offline evals nightly.",
            &[],
            "a",
            true,
            "best-practice",
        )
        .await
        .unwrap();
        put(
            &db,
            None,
            "Glossary",
            "terms",
            "Token: a unit of text.",
            &[],
            "a",
            true,
            "reference",
        )
        .await
        .unwrap();
        assert_eq!(
            list(&db, false, Some("reference"), None)
                .await
                .unwrap()
                .len(),
            1
        );
        let hits = list(&db, false, None, Some("NIGHTLY")).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Eval harness");
    }

    #[tokio::test]
    async fn rejects_unknown_category() {
        let db = db().await;
        assert!(put(&db, None, "X", "", "", &[], "a", true, "bogus")
            .await
            .is_err());
    }
}
