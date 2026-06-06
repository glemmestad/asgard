//! Skills catalog: user-publishable agent skills (a `SKILL.md` plus optional bundled
//! scripts/config/references), a sibling of the MCP catalog ([`crate::mcp_servers`])
//! and decoupled from the provisioning catalog. A user publishes a skill they own (it
//! lists `community`/"user-submitted" with them as the contact); an admin can promote
//! it to `approved`/"company-approved". Both tiers are visible — `status` is a trust
//! signal, not a visibility gate; a separate `state` (`active`/`disabled`/`archived`)
//! hides and prunes entries.
//!
//! Unlike the MCP catalog, a skill is a file tree, not a text blob. The bundle is
//! validated and stored as canonical JSON (file bytes base64'd inside) in one `bundle`
//! TEXT column — kept out of list/get and fetched only via [`get_bundle`] for export.
//! `name`/`summary`/`manifest`/`portability` are derived from the bundle's `SKILL.md`
//! at write time so the catalog is searchable without unpacking it.

use asgard_skills::SkillBundle;
use asgard_storage::Db;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::RegistryError;

/// Lifecycle states. `active` is the only one shown in the public catalog.
pub const STATES: &[&str] = &["active", "disabled", "archived"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub readme: String,
    /// Authored runtime: `claude-code` | `codex`.
    #[serde(default)]
    pub runtime: String,
    /// Parsed `SKILL.md` frontmatter.
    #[serde(default)]
    pub manifest: serde_json::Value,
    /// `portable` | `runtime-specific` (from the portability lint).
    #[serde(default)]
    pub portability: String,
    #[serde(default)]
    pub bundle_sha256: String,
    #[serde(default)]
    pub bundle_bytes: i64,
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
    /// Latest machine code-review assist verdict (advisory; escalate-only).
    #[serde(default)]
    pub review_json: Option<serde_json::Value>,
    #[serde(default)]
    pub reviewed_at: Option<String>,
}

/// The mutable content of a skill — what a publisher supplies. Identity (`id`/`owner`),
/// tier (`status`), lifecycle (`state`), derived fields, and timestamps are managed by
/// the store. `name`/`summary` fall back to the `SKILL.md` frontmatter when left blank.
#[derive(Debug, Clone, Default)]
pub struct SkillInput {
    pub name: String,
    pub summary: String,
    pub readme: String,
    pub runtime: String,
    pub repository: String,
    pub homepage: String,
    pub version: String,
    pub tags: Vec<String>,
    pub bundle: SkillBundle,
}

const COLS: &str = "id, name, summary, readme, runtime, manifest, portability, bundle_sha256, \
    bundle_bytes, repository, homepage, version, tags, owner, status, state, created_at, \
    updated_at, approved_at, approved_by, review_json, reviewed_at";

fn row_to_skill(row: &sqlx::any::AnyRow) -> Skill {
    let tags: String = row.get("tags");
    let manifest: String = row.get("manifest");
    let review_json: Option<String> = row.get("review_json");
    Skill {
        id: row.get("id"),
        name: row.get("name"),
        summary: row.get("summary"),
        readme: row.get("readme"),
        runtime: row.get("runtime"),
        manifest: serde_json::from_str(&manifest).unwrap_or(serde_json::Value::Null),
        portability: row.get("portability"),
        bundle_sha256: row.get("bundle_sha256"),
        bundle_bytes: row.get("bundle_bytes"),
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
        review_json: review_json.and_then(|s| serde_json::from_str(&s).ok()),
        reviewed_at: row.get("reviewed_at"),
    }
}

/// Derived, validated fields ready to persist.
struct Prepared {
    name: String,
    summary: String,
    runtime: String,
    manifest: String,
    portability: String,
    bundle: String,
    sha256: String,
    bytes: i64,
}

fn prepare(input: &SkillInput) -> Result<Prepared, RegistryError> {
    let runtime = if input.runtime.trim().is_empty() {
        "claude-code".to_string()
    } else {
        input.runtime.trim().to_string()
    };
    if !asgard_skills::RUNTIMES.contains(&runtime.as_str()) {
        return Err(RegistryError::Validation(format!(
            "runtime must be one of: {}",
            asgard_skills::RUNTIMES.join(", ")
        )));
    }
    let stored = asgard_skills::store(&input.bundle)
        .map_err(|e| RegistryError::Validation(e.to_string()))?;
    let skill_md = input.bundle.skill_md().unwrap_or_default();
    let manifest = asgard_skills::frontmatter_json(&skill_md);
    let fm_name = manifest.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let fm_desc = manifest
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let name = if input.name.trim().is_empty() {
        fm_name.trim().to_string()
    } else {
        input.name.trim().to_string()
    };
    if name.is_empty() {
        return Err(RegistryError::Validation(
            "a skill needs a name (in the form or the SKILL.md frontmatter)".into(),
        ));
    }
    let summary = if input.summary.trim().is_empty() {
        fm_desc.to_string()
    } else {
        input.summary.clone()
    };
    let portability = asgard_skills::lint_portability(&input.bundle)
        .portability
        .as_str()
        .to_string();
    Ok(Prepared {
        name,
        summary,
        runtime,
        manifest: serde_json::to_string(&manifest).unwrap_or_else(|_| "{}".into()),
        portability,
        bundle: stored.json,
        sha256: stored.sha256,
        bytes: stored.bytes,
    })
}

/// Catalog entries, newest first. `state` defaults to `active`; `status` filters by
/// trust tier without hiding either. `q` is a case-insensitive match over
/// name/summary/readme. The `bundle` blob is never selected here.
pub async fn list(
    db: &Db,
    q: Option<&str>,
    status: Option<&str>,
    state: Option<&str>,
) -> Result<Vec<Skill>, RegistryError> {
    let state_filter = state.unwrap_or("active");
    let mut sql = format!("SELECT {COLS} FROM skills WHERE 1 = 1");
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
    Ok(rows.iter().map(row_to_skill).collect())
}

pub async fn get(db: &Db, id: &str) -> Result<Option<Skill>, RegistryError> {
    let row = sqlx::query(&db.q(&format!("SELECT {COLS} FROM skills WHERE id = ?")))
        .bind(id)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.as_ref().map(row_to_skill))
}

/// The stored bundle JSON for `id` (parse with `asgard_skills::from_json`). Fetched
/// only for download/export so the blob never rides along with list/get.
pub async fn get_bundle(db: &Db, id: &str) -> Result<Option<String>, RegistryError> {
    let row = sqlx::query(&db.q("SELECT bundle FROM skills WHERE id = ?"))
        .bind(id)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.map(|r| r.get::<String, _>("bundle")))
}

pub async fn count(db: &Db) -> Result<i64, RegistryError> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM skills")
        .fetch_one(db.pool())
        .await?;
    Ok(n)
}

/// Publish a new skill owned by `owner`. `approved` mints it as the company tier
/// (admins / the boot seed); otherwise `community`. Always starts `active`.
pub async fn create(
    db: &Db,
    owner: &str,
    input: &SkillInput,
    approved: bool,
) -> Result<Skill, RegistryError> {
    let p = prepare(input)?;
    let id = asgard_storage::new_uid();
    let now = asgard_storage::now();
    let status = if approved { "approved" } else { "community" };
    let approved_at = approved.then(|| now.clone());
    let approved_by = approved.then(|| owner.to_string());
    let tags_s = input.tags.join(",");
    sqlx::query(&db.q("INSERT INTO skills \
         (id, name, summary, readme, runtime, manifest, portability, bundle, bundle_sha256, \
          bundle_bytes, repository, homepage, version, tags, owner, status, state, created_at, \
          updated_at, approved_at, approved_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?, ?, ?)"))
    .bind(&id)
    .bind(&p.name)
    .bind(&p.summary)
    .bind(&input.readme)
    .bind(&p.runtime)
    .bind(&p.manifest)
    .bind(&p.portability)
    .bind(&p.bundle)
    .bind(&p.sha256)
    .bind(p.bytes)
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
    get(db, &id)
        .await?
        .ok_or_else(|| RegistryError::Validation("skill vanished after insert".into()))
}

/// Edit a skill's content by id, refreshing `updated_at` and re-deriving the bundle.
/// Owner/tier/lifecycle are preserved. `None` if no skill has that id.
pub async fn update(db: &Db, id: &str, input: &SkillInput) -> Result<Option<Skill>, RegistryError> {
    if get(db, id).await?.is_none() {
        return Ok(None);
    }
    let p = prepare(input)?;
    let now = asgard_storage::now();
    let tags_s = input.tags.join(",");
    sqlx::query(&db.q(
        "UPDATE skills SET name = ?, summary = ?, readme = ?, runtime = ?, manifest = ?, \
         portability = ?, bundle = ?, bundle_sha256 = ?, bundle_bytes = ?, repository = ?, \
         homepage = ?, version = ?, tags = ?, updated_at = ? WHERE id = ?",
    ))
    .bind(&p.name)
    .bind(&p.summary)
    .bind(&input.readme)
    .bind(&p.runtime)
    .bind(&p.manifest)
    .bind(&p.portability)
    .bind(&p.bundle)
    .bind(&p.sha256)
    .bind(p.bytes)
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

/// Promote to `approved` (recording the approver) or demote to `community`. Admin-gated
/// at the API layer.
pub async fn set_status(
    db: &Db,
    id: &str,
    status: &str,
    approver: Option<&str>,
) -> Result<(), RegistryError> {
    let now = asgard_storage::now();
    if status == "approved" {
        sqlx::query(&db.q(
            "UPDATE skills SET status = 'approved', approved_at = ?, approved_by = ?, \
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
            "UPDATE skills SET status = 'community', approved_at = NULL, approved_by = NULL, \
             updated_at = ? WHERE id = ?",
        ))
        .bind(&now)
        .bind(id)
        .execute(db.pool())
        .await?;
    }
    Ok(())
}

/// Move a skill through its lifecycle (`active`/`disabled`/`archived`). Owner- or
/// admin-gated at the API layer.
pub async fn set_state(db: &Db, id: &str, state: &str) -> Result<(), RegistryError> {
    if !STATES.contains(&state) {
        return Err(RegistryError::Validation(format!(
            "unknown state '{state}' (expected one of: {})",
            STATES.join(", ")
        )));
    }
    let now = asgard_storage::now();
    sqlx::query(&db.q("UPDATE skills SET state = ?, updated_at = ? WHERE id = ?"))
        .bind(state)
        .bind(&now)
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
}

/// Store the latest code-review assist verdict. Does not touch `updated_at` so a
/// review never reorders the catalog.
pub async fn set_review(
    db: &Db,
    id: &str,
    verdict: &serde_json::Value,
) -> Result<(), RegistryError> {
    let now = asgard_storage::now();
    let s = serde_json::to_string(verdict).unwrap_or_else(|_| "null".into());
    sqlx::query(&db.q("UPDATE skills SET review_json = ?, reviewed_at = ? WHERE id = ?"))
        .bind(&s)
        .bind(&now)
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
}

pub async fn delete(db: &Db, id: &str) -> Result<(), RegistryError> {
    sqlx::query(&db.q("DELETE FROM skills WHERE id = ?"))
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asgard_skills::SkillFile;

    async fn db() -> Db {
        let path =
            std::env::temp_dir().join(format!("asgard-skills-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        db
    }

    fn input(name: &str) -> SkillInput {
        let md = format!("---\nname: {name}\ndescription: does things\n---\nbody\n");
        SkillInput {
            name: name.into(),
            runtime: "claude-code".into(),
            tags: vec!["test".into()],
            bundle: SkillBundle {
                files: vec![
                    SkillFile::from_text("SKILL.md", &md),
                    SkillFile::from_text("scripts/run.sh", "echo hi\n"),
                ],
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn create_get_list_roundtrip_keeps_bundle() {
        let db = db().await;
        let s = create(&db, "alice@corp.example", &input("Changelog"), false)
            .await
            .unwrap();
        assert_eq!(s.status, "community");
        assert_eq!(s.state, "active");
        assert_eq!(s.owner, "alice@corp.example");
        assert_eq!(s.runtime, "claude-code");
        assert!(s.bundle_bytes > 0);
        let got = get(&db, &s.id).await.unwrap().unwrap();
        assert_eq!(got.name, "Changelog");
        assert_eq!(got.tags, vec!["test"]);
        let blob = get_bundle(&db, &s.id).await.unwrap().unwrap();
        let bundle = asgard_skills::from_json(&blob).unwrap();
        assert!(bundle.get("scripts/run.sh").is_some());
        assert_eq!(list(&db, None, None, None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn name_and_summary_fall_back_to_frontmatter() {
        let db = db().await;
        let md = "---\nname: FromFrontmatter\ndescription: front desc\n---\nbody\n";
        let mut i = SkillInput {
            runtime: "claude-code".into(),
            bundle: SkillBundle {
                files: vec![SkillFile::from_text("SKILL.md", md)],
            },
            ..Default::default()
        };
        i.name = String::new();
        let s = create(&db, "u", &i, false).await.unwrap();
        assert_eq!(s.name, "FromFrontmatter");
        assert_eq!(s.summary, "front desc");
    }

    #[tokio::test]
    async fn both_tiers_visible_and_status_filters() {
        let db = db().await;
        create(&db, "u", &input("community-one"), false)
            .await
            .unwrap();
        create(&db, "asgard", &input("approved-one"), true)
            .await
            .unwrap();
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
        let s = create(&db, "u", &input("svc"), false).await.unwrap();
        set_status(&db, &s.id, "approved", Some("admin@corp.example"))
            .await
            .unwrap();
        let got = get(&db, &s.id).await.unwrap().unwrap();
        assert_eq!(got.status, "approved");
        assert_eq!(got.approved_by.as_deref(), Some("admin@corp.example"));
        set_status(&db, &s.id, "community", None).await.unwrap();
        let back = get(&db, &s.id).await.unwrap().unwrap();
        assert_eq!(back.status, "community");
        assert_eq!(back.approved_by, None);
    }

    #[tokio::test]
    async fn update_preserves_owner_and_tier_and_rederives_bundle() {
        let db = db().await;
        let s = create(&db, "owner@corp.example", &input("svc"), true)
            .await
            .unwrap();
        let mut edit = input("svc renamed");
        edit.bundle = SkillBundle {
            files: vec![SkillFile::from_text(
                "SKILL.md",
                "---\nname: svc renamed\ndescription: more\n---\nx",
            )],
        };
        let updated = update(&db, &s.id, &edit).await.unwrap().unwrap();
        assert_eq!(updated.name, "svc renamed");
        assert_eq!(updated.owner, "owner@corp.example");
        assert_eq!(updated.status, "approved");
        assert!(update(&db, "nope", &edit).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn lifecycle_and_review_and_delete() {
        let db = db().await;
        let s = create(&db, "u", &input("svc"), true).await.unwrap();
        set_state(&db, &s.id, "disabled").await.unwrap();
        assert_eq!(list(&db, None, None, None).await.unwrap().len(), 0);
        assert_eq!(
            list(&db, None, None, Some("disabled")).await.unwrap().len(),
            1
        );
        set_state(&db, &s.id, "active").await.unwrap();
        assert!(set_state(&db, &s.id, "bogus").await.is_err());
        set_review(&db, &s.id, &serde_json::json!({"passed": true}))
            .await
            .unwrap();
        let got = get(&db, &s.id).await.unwrap().unwrap();
        assert_eq!(got.review_json.unwrap()["passed"], true);
        assert!(got.reviewed_at.is_some());
        delete(&db, &s.id).await.unwrap();
        assert!(get(&db, &s.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_missing_skill_md_and_bad_runtime() {
        let db = db().await;
        let mut no_md = input("x");
        no_md.bundle = SkillBundle {
            files: vec![SkillFile::from_text("notes.md", "hi")],
        };
        assert!(create(&db, "u", &no_md, false).await.is_err());
        let mut bad_rt = input("x");
        bad_rt.runtime = "vim".into();
        assert!(create(&db, "u", &bad_rt, false).await.is_err());
    }
}
