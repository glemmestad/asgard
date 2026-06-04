//! MCP catalog: user-publishable MCP servers, deliberately decoupled from the
//! project/service provisioning catalog. A user publishes a server they own (it
//! shows up `community`/"user-submitted" with them as the contact); an admin can
//! promote it to `approved`/"company-approved". Both tiers are visible — `status`
//! is a trust signal, not a visibility gate. A separate `state`
//! (`active`/`disabled`/`archived`) hides an entry from the catalog and supports
//! pruning stale servers over time.
//!
//! Unlike guidance/recipes (single-authored, slug-keyed, upsert-on-reuse), the
//! catalog is multi-owner, so entries are keyed by a generated id: publishing
//! creates a new entry, editing updates one by id (owner/admin only).

use asgard_storage::Db;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::RegistryError;

/// Lifecycle states. `active` is the only one shown in the public catalog.
pub const STATES: &[&str] = &["active", "disabled", "archived"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServer {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub summary: String,
    /// Optional rich getting-started / README (markdown).
    #[serde(default)]
    pub readme: String,
    /// Structured install spec, opaque to the store:
    /// `{ transport: "stdio"|"remote", command, args: [], env: [names], url }`.
    #[serde(default)]
    pub install: serde_json::Value,
    #[serde(default)]
    pub repository: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The submitter's identity (email when known) — the contact point. Seeded
    /// company entries use `asgard`.
    #[serde(default)]
    pub owner: String,
    /// Trust tier: `community` (user-submitted) | `approved` (company-sanctioned).
    #[serde(default)]
    pub status: String,
    /// Lifecycle: `active` | `disabled` | `archived`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub approved_at: Option<String>,
    #[serde(default)]
    pub approved_by: Option<String>,
}

/// The mutable content of a catalog entry — the fields a publisher supplies.
/// Identity (`id`/`owner`), tier (`status`), lifecycle (`state`), and timestamps
/// are managed by the store, not the caller.
#[derive(Debug, Clone, Default)]
pub struct McpServerInput {
    pub name: String,
    pub summary: String,
    pub readme: String,
    pub install: serde_json::Value,
    pub repository: String,
    pub homepage: String,
    pub version: String,
    pub tags: Vec<String>,
}

const COLS: &str = "id, name, summary, readme, install, repository, homepage, version, \
    tags, owner, status, state, created_at, updated_at, approved_at, approved_by";

fn row_to_server(row: &sqlx::any::AnyRow) -> McpServer {
    let tags: String = row.get("tags");
    let install: String = row.get("install");
    McpServer {
        id: row.get("id"),
        name: row.get("name"),
        summary: row.get("summary"),
        readme: row.get("readme"),
        install: serde_json::from_str(&install).unwrap_or(serde_json::Value::Null),
        repository: row.get("repository"),
        homepage: row.get("homepage"),
        version: row.get("version"),
        tags: tags
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        owner: row.get("owner"),
        status: row.get("status"),
        state: row.get("state"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        approved_at: row.get("approved_at"),
        approved_by: row.get("approved_by"),
    }
}

fn validate(input: &McpServerInput) -> Result<(), RegistryError> {
    if input.name.trim().is_empty() {
        return Err(RegistryError::Validation(
            "an MCP catalog entry needs a name".into(),
        ));
    }
    if let Some(t) = input.install.get("transport").and_then(|v| v.as_str()) {
        if !t.is_empty() && t != "stdio" && t != "remote" {
            return Err(RegistryError::Validation(format!(
                "install.transport must be 'stdio' or 'remote' (got '{t}')"
            )));
        }
    }
    Ok(())
}

/// Catalog entries, newest first. `state` defaults to `active` (the public
/// catalog) when `None`; pass `Some("all")` for every state. `status` filters by
/// trust tier but never hides — both tiers list. `q` is a case-insensitive match
/// over name/summary/readme.
pub async fn list(
    db: &Db,
    q: Option<&str>,
    status: Option<&str>,
    state: Option<&str>,
) -> Result<Vec<McpServer>, RegistryError> {
    let state_filter = state.unwrap_or("active");
    let mut sql = format!("SELECT {COLS} FROM mcp_servers WHERE 1 = 1");
    if state_filter != "all" {
        sql.push_str(" AND state = ?");
    }
    if status.is_some() {
        sql.push_str(" AND status = ?");
    }
    if q.is_some() {
        sql.push_str(" AND LOWER(name || ' ' || summary || ' ' || readme) LIKE ?");
    }
    sql.push_str(" ORDER BY updated_at DESC");
    let sql = db.q(&sql);
    let mut query = sqlx::query(&sql);
    if state_filter != "all" {
        query = query.bind(state_filter.to_string());
    }
    if let Some(s) = status {
        query = query.bind(s.to_string());
    }
    if let Some(term) = q {
        query = query.bind(format!("%{}%", term.to_lowercase()));
    }
    let rows = query.fetch_all(db.pool()).await?;
    Ok(rows.iter().map(row_to_server).collect())
}

pub async fn get(db: &Db, id: &str) -> Result<Option<McpServer>, RegistryError> {
    let row = sqlx::query(&db.q(&format!("SELECT {COLS} FROM mcp_servers WHERE id = ?")))
        .bind(id)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.as_ref().map(row_to_server))
}

pub async fn count(db: &Db) -> Result<i64, RegistryError> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mcp_servers")
        .fetch_one(db.pool())
        .await?;
    Ok(n)
}

/// Publish a new entry owned by `owner`. `approved` mints it as the company tier
/// (admins and the boot seed); otherwise it is `community`. Always starts `active`.
pub async fn create(
    db: &Db,
    owner: &str,
    input: &McpServerInput,
    approved: bool,
) -> Result<McpServer, RegistryError> {
    validate(input)?;
    let id = asgard_storage::new_uid();
    let now = asgard_storage::now();
    let status = if approved { "approved" } else { "community" };
    let approved_at = approved.then(|| now.clone());
    let approved_by = approved.then(|| owner.to_string());
    let tags_s = input.tags.join(",");
    let install_s = serde_json::to_string(&input.install).unwrap_or_else(|_| "{}".into());
    sqlx::query(&db.q("INSERT INTO mcp_servers \
         (id, name, summary, readme, install, repository, homepage, version, tags, \
          owner, status, state, created_at, updated_at, approved_at, approved_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?, ?, ?)"))
    .bind(&id)
    .bind(&input.name)
    .bind(&input.summary)
    .bind(&input.readme)
    .bind(&install_s)
    .bind(&input.repository)
    .bind(&input.homepage)
    .bind(&input.version)
    .bind(&tags_s)
    .bind(owner)
    .bind(status)
    .bind(&now)
    .bind(&now)
    .bind(&approved_at)
    .bind(&approved_by)
    .execute(db.pool())
    .await?;
    Ok(McpServer {
        id,
        name: input.name.clone(),
        summary: input.summary.clone(),
        readme: input.readme.clone(),
        install: input.install.clone(),
        repository: input.repository.clone(),
        homepage: input.homepage.clone(),
        version: input.version.clone(),
        tags: input.tags.clone(),
        owner: owner.to_string(),
        status: status.to_string(),
        state: "active".to_string(),
        created_at: now.clone(),
        updated_at: now,
        approved_at,
        approved_by,
    })
}

/// Edit an entry's content by id, refreshing `updated_at`. Owner/tier/lifecycle
/// are preserved. Returns `None` if no entry has that id.
pub async fn update(
    db: &Db,
    id: &str,
    input: &McpServerInput,
) -> Result<Option<McpServer>, RegistryError> {
    validate(input)?;
    let now = asgard_storage::now();
    let tags_s = input.tags.join(",");
    let install_s = serde_json::to_string(&input.install).unwrap_or_else(|_| "{}".into());
    sqlx::query(&db.q(
        "UPDATE mcp_servers SET name = ?, summary = ?, readme = ?, install = ?, \
         repository = ?, homepage = ?, version = ?, tags = ?, updated_at = ? WHERE id = ?",
    ))
    .bind(&input.name)
    .bind(&input.summary)
    .bind(&input.readme)
    .bind(&install_s)
    .bind(&input.repository)
    .bind(&input.homepage)
    .bind(&input.version)
    .bind(&tags_s)
    .bind(&now)
    .bind(id)
    .execute(db.pool())
    .await?;
    get(db, id).await
}

/// Promote to `approved` (recording the approver) or demote to `community`
/// (clearing the approval stamp). Admin-gated at the API layer.
pub async fn set_status(
    db: &Db,
    id: &str,
    status: &str,
    approver: Option<&str>,
) -> Result<(), RegistryError> {
    let now = asgard_storage::now();
    if status == "approved" {
        sqlx::query(&db.q(
            "UPDATE mcp_servers SET status = 'approved', approved_at = ?, approved_by = ?, \
             updated_at = ? WHERE id = ?",
        ))
        .bind(&now)
        .bind(approver.unwrap_or(""))
        .bind(&now)
        .bind(id)
        .execute(db.pool())
        .await?;
    } else {
        sqlx::query(&db.q(
            "UPDATE mcp_servers SET status = 'community', approved_at = NULL, \
             approved_by = NULL, updated_at = ? WHERE id = ?",
        ))
        .bind(&now)
        .bind(id)
        .execute(db.pool())
        .await?;
    }
    Ok(())
}

/// Move an entry through its lifecycle (`active`/`disabled`/`archived`). Owner- or
/// admin-gated at the API layer.
pub async fn set_state(db: &Db, id: &str, state: &str) -> Result<(), RegistryError> {
    if !STATES.contains(&state) {
        return Err(RegistryError::Validation(format!(
            "unknown state '{state}' (expected one of: {})",
            STATES.join(", ")
        )));
    }
    let now = asgard_storage::now();
    sqlx::query(&db.q("UPDATE mcp_servers SET state = ?, updated_at = ? WHERE id = ?"))
        .bind(state)
        .bind(&now)
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn delete(db: &Db, id: &str) -> Result<(), RegistryError> {
    sqlx::query(&db.q("DELETE FROM mcp_servers WHERE id = ?"))
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db() -> Db {
        let path =
            std::env::temp_dir().join(format!("asgard-mcp-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        db
    }

    fn input(name: &str) -> McpServerInput {
        McpServerInput {
            name: name.into(),
            summary: "does things".into(),
            install: serde_json::json!({"transport": "stdio", "command": "npx", "args": ["-y", "pkg"]}),
            tags: vec!["test".into()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn create_get_list_roundtrip_keeps_install() {
        let db = db().await;
        let m = create(&db, "alice@corp.example", &input("GitHub"), false)
            .await
            .unwrap();
        assert_eq!(m.status, "community");
        assert_eq!(m.state, "active");
        assert_eq!(m.owner, "alice@corp.example");
        let got = get(&db, &m.id).await.unwrap().unwrap();
        assert_eq!(got.name, "GitHub");
        assert_eq!(got.install["command"], "npx");
        assert_eq!(got.tags, vec!["test"]);
        assert_eq!(list(&db, None, None, None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn both_tiers_are_visible_and_status_filters() {
        let db = db().await;
        create(&db, "u", &input("community-one"), false)
            .await
            .unwrap();
        create(&db, "asgard", &input("approved-one"), true)
            .await
            .unwrap();
        // Default list shows both tiers (no status hiding).
        assert_eq!(list(&db, None, None, None).await.unwrap().len(), 2);
        assert_eq!(
            list(&db, None, Some("approved"), None).await.unwrap().len(),
            1
        );
        assert_eq!(
            list(&db, None, Some("community"), None)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn approve_sets_stamp_and_unapprove_clears_it() {
        let db = db().await;
        let m = create(&db, "u", &input("svc"), false).await.unwrap();
        set_status(&db, &m.id, "approved", Some("admin@corp.example"))
            .await
            .unwrap();
        let got = get(&db, &m.id).await.unwrap().unwrap();
        assert_eq!(got.status, "approved");
        assert_eq!(got.approved_by.as_deref(), Some("admin@corp.example"));
        assert!(got.approved_at.is_some());
        set_status(&db, &m.id, "community", None).await.unwrap();
        let back = get(&db, &m.id).await.unwrap().unwrap();
        assert_eq!(back.status, "community");
        assert_eq!(back.approved_by, None);
    }

    #[tokio::test]
    async fn update_preserves_owner_and_tier() {
        let db = db().await;
        let m = create(&db, "owner@corp.example", &input("svc"), true)
            .await
            .unwrap();
        let mut edit = input("svc renamed");
        edit.summary = "now does more".into();
        let updated = update(&db, &m.id, &edit).await.unwrap().unwrap();
        assert_eq!(updated.name, "svc renamed");
        assert_eq!(updated.owner, "owner@corp.example");
        assert_eq!(updated.status, "approved");
        assert_eq!(updated.state, "active");
    }

    #[tokio::test]
    async fn disabled_and_archived_drop_from_active_list_and_return() {
        let db = db().await;
        let m = create(&db, "u", &input("svc"), true).await.unwrap();
        set_state(&db, &m.id, "disabled").await.unwrap();
        assert_eq!(list(&db, None, None, None).await.unwrap().len(), 0); // active view hides it
        assert_eq!(
            list(&db, None, None, Some("disabled")).await.unwrap().len(),
            1
        );
        assert_eq!(list(&db, None, None, Some("all")).await.unwrap().len(), 1);
        set_state(&db, &m.id, "active").await.unwrap();
        assert_eq!(list(&db, None, None, None).await.unwrap().len(), 1); // back in the catalog
        set_state(&db, &m.id, "archived").await.unwrap();
        assert_eq!(list(&db, None, None, None).await.unwrap().len(), 0);
        assert_eq!(
            list(&db, None, None, Some("archived")).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn delete_removes_and_bad_state_rejected() {
        let db = db().await;
        let m = create(&db, "u", &input("svc"), false).await.unwrap();
        assert!(set_state(&db, &m.id, "bogus").await.is_err());
        delete(&db, &m.id).await.unwrap();
        assert!(get(&db, &m.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn name_is_required() {
        let db = db().await;
        assert!(create(&db, "u", &input("  "), false).await.is_err());
    }
}
