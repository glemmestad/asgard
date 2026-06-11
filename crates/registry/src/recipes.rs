//! Recipes: parameterized provisioning compositions. A recipe names a set of
//! catalog primitives and how to wire one's outputs into the next's inputs
//! (`${steps.<id>.<output>}` / `${inputs.<name>}` placeholders). The agent reads
//! a recipe, fills the inputs, and issues each `request_resource` itself — Frontkeep
//! never executes the steps, so per-resource cost/approval/tiering still apply.

use frontkeep_storage::Db;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::guidance::slugify;
use crate::RegistryError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipe {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub summary: String,
    /// The narrated runbook (markdown) — the primary content an agent follows.
    #[serde(default)]
    pub body: String,
    /// `{ description, inputs: [...], steps: [...], outputs: {...} }` — the
    /// machine-readable at-a-glance composition. Opaque to the store.
    pub spec: serde_json::Value,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub author: String,
    /// `pending` (submitted, awaiting admin approval) | `published` (visible to
    /// readers). Mirrors guidance moderation.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub updated_at: String,
}

fn row_to_recipe(row: &sqlx::any::AnyRow) -> Recipe {
    let tags: String = row.get("tags");
    let spec: String = row.get("spec");
    Recipe {
        slug: row.get("slug"),
        name: row.get("name"),
        summary: row.get("summary"),
        body: row.get("body"),
        spec: serde_json::from_str(&spec).unwrap_or(serde_json::Value::Null),
        tags: tags
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        author: row.get("author"),
        status: row.get("status"),
        updated_at: row.get("updated_at"),
    }
}

const COLS: &str = "slug, name, summary, body, spec, tags, author, status, updated_at";

/// Recipes, by name. `include_pending` returns drafts awaiting approval as well —
/// for admins; readers get published only. `q` is a case-insensitive match over
/// name/summary/body.
pub async fn list(
    db: &Db,
    include_pending: bool,
    q: Option<&str>,
) -> Result<Vec<Recipe>, RegistryError> {
    let mut sql = format!("SELECT {COLS} FROM recipes WHERE 1 = 1");
    if !include_pending {
        sql.push_str(" AND status = 'published'");
    }
    if q.is_some() {
        sql.push_str(" AND LOWER(name || ' ' || summary || ' ' || body) LIKE ?");
    }
    sql.push_str(" ORDER BY name");
    let sql = db.q(&sql);
    let mut query = sqlx::query(&sql);
    if let Some(term) = q {
        query = query.bind(format!("%{}%", term.to_lowercase()));
    }
    let rows = query.fetch_all(db.pool()).await?;
    Ok(rows.iter().map(row_to_recipe).collect())
}

/// Approve a draft (or any recipe) — admin-gated at the API layer.
pub async fn set_status(db: &Db, slug: &str, status: &str) -> Result<(), RegistryError> {
    sqlx::query(&db.q("UPDATE recipes SET status = ? WHERE slug = ?"))
        .bind(status)
        .bind(slug)
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn get(db: &Db, slug: &str) -> Result<Option<Recipe>, RegistryError> {
    let row = sqlx::query(&db.q(&format!("SELECT {COLS} FROM recipes WHERE slug = ?")))
        .bind(slug)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.as_ref().map(row_to_recipe))
}

#[allow(clippy::too_many_arguments)]
pub async fn put(
    db: &Db,
    slug: Option<&str>,
    name: &str,
    summary: &str,
    body: &str,
    spec: &serde_json::Value,
    tags: &[String],
    author: &str,
    published: bool,
) -> Result<Recipe, RegistryError> {
    let slug = match slug {
        Some(s) if !s.trim().is_empty() => slugify(s),
        _ => slugify(name),
    };
    if slug.is_empty() {
        return Err(RegistryError::Validation(
            "recipe needs a name or slug".into(),
        ));
    }
    let tags_s = tags.join(",");
    let spec_s = serde_json::to_string(spec).unwrap_or_else(|_| "{}".into());
    let now = frontkeep_storage::now();
    let status = if published { "published" } else { "pending" };
    sqlx::query(&db.q(
        "INSERT INTO recipes (slug, name, summary, body, spec, tags, author, status, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(slug) DO UPDATE SET name = excluded.name, summary = excluded.summary, \
          body = excluded.body, spec = excluded.spec, tags = excluded.tags, \
          author = excluded.author, status = excluded.status, updated_at = excluded.updated_at",
    ))
    .bind(&slug)
    .bind(name)
    .bind(summary)
    .bind(body)
    .bind(&spec_s)
    .bind(&tags_s)
    .bind(author)
    .bind(status)
    .bind(&now)
    .execute(db.pool())
    .await?;
    Ok(Recipe {
        slug,
        name: name.to_string(),
        summary: summary.to_string(),
        body: body.to_string(),
        spec: spec.clone(),
        tags: tags.to_vec(),
        author: author.to_string(),
        status: status.to_string(),
        updated_at: now,
    })
}

pub async fn count(db: &Db) -> Result<i64, RegistryError> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recipes")
        .fetch_one(db.pool())
        .await?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_get_preserves_spec() {
        let path =
            std::env::temp_dir().join(format!("frontkeep-rec-{}.db", frontkeep_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let spec = serde_json::json!({
            "inputs": [{"name": "image", "required": true}],
            "steps": [{"id": "svc", "service": "ecs-service", "inputs": {"name": "${inputs.image}"}}],
        });
        let r = put(
            &db,
            None,
            "MCP Server",
            "",
            "# Runbook\n\nStep through it.",
            &spec,
            &["mcp".into()],
            "agent",
            true,
        )
        .await
        .unwrap();
        assert_eq!(r.slug, "mcp-server");
        let got = get(&db, "mcp-server").await.unwrap().unwrap();
        assert_eq!(got.spec["steps"][0]["service"], "ecs-service");
        assert!(got.body.contains("Runbook"));
        assert_eq!(count(&db).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn drafts_hidden_until_approved_and_search() {
        let path =
            std::env::temp_dir().join(format!("frontkeep-rec-{}.db", frontkeep_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let spec = serde_json::json!({});
        put(
            &db,
            None,
            "Realtime Collab",
            "",
            "Build with websockets.",
            &spec,
            &[],
            "agent",
            false,
        )
        .await
        .unwrap();
        assert_eq!(list(&db, false, None).await.unwrap().len(), 0); // readers see nothing
        assert_eq!(list(&db, true, None).await.unwrap().len(), 1); // admin sees the draft
        set_status(&db, "realtime-collab", "published")
            .await
            .unwrap();
        let pub_list = list(&db, false, None).await.unwrap();
        assert_eq!(pub_list.len(), 1);
        assert_eq!(pub_list[0].status, "published");
        // Case-insensitive search over body.
        assert_eq!(list(&db, false, Some("WEBSOCKETS")).await.unwrap().len(), 1);
        assert_eq!(list(&db, false, Some("nope")).await.unwrap().len(), 0);
    }
}
