//! Catalog persistence over the shared `Db`. Portable SQL (`?` placeholders,
//! TEXT columns) so SQLite and Postgres behave identically.

use frontkeep_storage::Db;
use serde_json::Value;
use sqlx::Row;

use crate::entity::{Entity, Lifecycle, Metadata, Origin, Relation};
use crate::error::CatalogError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upsert {
    Inserted,
    Updated,
    Unchanged,
}

#[derive(Debug, Default, Clone)]
pub struct ListFilter {
    pub kind: Option<String>,
    pub namespace: Option<String>,
    /// Substring match against name/title/description.
    pub query: Option<String>,
    pub include_deleted: bool,
    pub limit: Option<i64>,
}

impl ListFilter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn kind(mut self, k: impl Into<String>) -> Self {
        self.kind = Some(k.into());
        self
    }
    pub fn query(mut self, q: impl Into<String>) -> Self {
        self.query = Some(q.into());
        self
    }
}

#[derive(Clone)]
pub struct CatalogRepo {
    db: Db,
}

impl CatalogRepo {
    pub fn new(db: Db) -> Self {
        CatalogRepo { db }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Insert or update by (kind, namespace, name). Preserves `uid`,
    /// `created_at`, and `lifecycle` on update (lifecycle is owned by the
    /// workflow layer, never overwritten by ingestion). Reconciliation revives
    /// a previously soft-deleted entity if it reappears.
    pub async fn upsert(&self, entity: &Entity) -> Result<(Upsert, String), CatalogError> {
        let pool = self.db.pool();
        let existing: Option<(String, String, Option<String>)> = sqlx::query(&self.db.q(
            "SELECT uid, content_hash, deleted_at FROM entities WHERE kind = ? AND namespace = ? AND name = ?",
        ))
        .bind(&entity.kind)
        .bind(&entity.metadata.namespace)
        .bind(&entity.metadata.name)
        .fetch_optional(pool)
        .await?
        .map(|r| {
            (
                r.get::<String, _>("uid"),
                r.get::<String, _>("content_hash"),
                r.get::<Option<String>, _>("deleted_at"),
            )
        });

        let now = frontkeep_storage::now();

        match existing {
            Some((uid, hash, deleted_at)) => {
                if hash == entity.content_hash && deleted_at.is_none() {
                    sqlx::query(&self.db.q("UPDATE entities SET seen_at = ? WHERE uid = ?"))
                        .bind(&now)
                        .bind(&uid)
                        .execute(pool)
                        .await?;
                    return Ok((Upsert::Unchanged, uid));
                }
                sqlx::query(&self.db.q(
                    "UPDATE entities SET title = ?, description = ?, spec = ?, metadata = ?, \
                     origin_repo = ?, origin_path = ?, origin_commit = ?, source_id = ?, \
                     content_hash = ?, seen_at = ?, updated_at = ?, deleted_at = NULL WHERE uid = ?",
                ))
                .bind(&entity.metadata.title)
                .bind(&entity.metadata.description)
                .bind(entity.spec.to_string())
                .bind(serde_json::to_string(&entity.metadata)?)
                .bind(&entity.origin.repo)
                .bind(&entity.origin.path)
                .bind(&entity.origin.commit)
                .bind(&entity.origin.source_id)
                .bind(&entity.content_hash)
                .bind(&now)
                .bind(&now)
                .bind(&uid)
                .execute(pool)
                .await?;
                self.replace_relations(&uid, &entity.relations).await?;
                Ok((Upsert::Updated, uid))
            }
            None => {
                sqlx::query(&self.db.q(
                    "INSERT INTO entities (uid, kind, namespace, name, title, description, spec, metadata, \
                     lifecycle, origin_repo, origin_path, origin_commit, source_id, content_hash, \
                     seen_at, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                ))
                .bind(&entity.uid)
                .bind(&entity.kind)
                .bind(&entity.metadata.namespace)
                .bind(&entity.metadata.name)
                .bind(&entity.metadata.title)
                .bind(&entity.metadata.description)
                .bind(entity.spec.to_string())
                .bind(serde_json::to_string(&entity.metadata)?)
                .bind(entity.lifecycle.as_str())
                .bind(&entity.origin.repo)
                .bind(&entity.origin.path)
                .bind(&entity.origin.commit)
                .bind(&entity.origin.source_id)
                .bind(&entity.content_hash)
                .bind(&now)
                .bind(&entity.created_at)
                .bind(&now)
                .execute(pool)
                .await?;
                self.replace_relations(&entity.uid, &entity.relations)
                    .await?;
                Ok((Upsert::Inserted, entity.uid.clone()))
            }
        }
    }

    pub async fn replace_relations(
        &self,
        from_uid: &str,
        relations: &[Relation],
    ) -> Result<(), CatalogError> {
        let pool = self.db.pool();
        sqlx::query(&self.db.q("DELETE FROM relations WHERE from_uid = ?"))
            .bind(from_uid)
            .execute(pool)
            .await?;
        for r in relations {
            sqlx::query(
                &self
                    .db
                    .q("INSERT INTO relations (from_uid, rel_type, to_ref) VALUES (?, ?, ?)"),
            )
            .bind(from_uid)
            .bind(&r.rel_type)
            .bind(&r.target)
            .execute(pool)
            .await?;
        }
        Ok(())
    }

    pub async fn get(
        &self,
        kind: &str,
        namespace: &str,
        name: &str,
    ) -> Result<Option<Entity>, CatalogError> {
        let row = sqlx::query(&self.db.q(&format!(
            "{SELECT_ENTITY} WHERE kind = ? AND namespace = ? AND name = ? AND deleted_at IS NULL"
        )))
        .bind(kind)
        .bind(namespace)
        .bind(name)
        .fetch_optional(self.db.pool())
        .await?;
        row.map(row_to_entity).transpose()
    }

    pub async fn get_by_uid(&self, uid: &str) -> Result<Option<Entity>, CatalogError> {
        let row = sqlx::query(&self.db.q(&format!("{SELECT_ENTITY} WHERE uid = ?")))
            .bind(uid)
            .fetch_optional(self.db.pool())
            .await?;
        row.map(row_to_entity).transpose()
    }

    pub async fn list(&self, filter: &ListFilter) -> Result<Vec<Entity>, CatalogError> {
        let mut sql = format!("{SELECT_ENTITY} WHERE 1=1");
        if !filter.include_deleted {
            sql.push_str(" AND deleted_at IS NULL");
        }
        if filter.kind.is_some() {
            sql.push_str(" AND kind = ?");
        }
        if filter.namespace.is_some() {
            sql.push_str(" AND namespace = ?");
        }
        if filter.query.is_some() {
            sql.push_str(" AND (name LIKE ? OR title LIKE ? OR description LIKE ?)");
        }
        sql.push_str(" ORDER BY kind, namespace, name");
        sql.push_str(&format!(" LIMIT {}", filter.limit.unwrap_or(1000)));

        let sql = self.db.q(&sql);
        let mut q = sqlx::query(&sql);
        if let Some(k) = &filter.kind {
            q = q.bind(k);
        }
        if let Some(ns) = &filter.namespace {
            q = q.bind(ns);
        }
        if let Some(query) = &filter.query {
            let like = format!("%{query}%");
            q = q.bind(like.clone()).bind(like.clone()).bind(like);
        }
        let rows = q.fetch_all(self.db.pool()).await?;
        rows.into_iter().map(row_to_entity).collect()
    }

    /// (uid, canonical-ref) for every non-deleted entity attributed to a source.
    /// Used by the reconciler to detect removals.
    pub async fn active_refs_for_source(
        &self,
        source_id: &str,
    ) -> Result<Vec<(String, String)>, CatalogError> {
        let rows = sqlx::query(&self.db.q(
            "SELECT uid, kind, namespace, name FROM entities WHERE source_id = ? AND deleted_at IS NULL",
        ))
        .bind(source_id)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let kind: String = r.get("kind");
                let ns: String = r.get("namespace");
                let name: String = r.get("name");
                let uid: String = r.get("uid");
                (uid, format!("{}:{}/{}", kind.to_lowercase(), ns, name))
            })
            .collect())
    }

    pub async fn soft_delete(&self, uid: &str) -> Result<(), CatalogError> {
        sqlx::query(
            &self
                .db
                .q("UPDATE entities SET deleted_at = ?, updated_at = ? WHERE uid = ?"),
        )
        .bind(frontkeep_storage::now())
        .bind(frontkeep_storage::now())
        .bind(uid)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn set_lifecycle(&self, uid: &str, lifecycle: Lifecycle) -> Result<(), CatalogError> {
        sqlx::query(
            &self
                .db
                .q("UPDATE entities SET lifecycle = ?, updated_at = ? WHERE uid = ?"),
        )
        .bind(lifecycle.as_str())
        .bind(frontkeep_storage::now())
        .bind(uid)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn relations_of(&self, from_uid: &str) -> Result<Vec<Relation>, CatalogError> {
        let rows = sqlx::query(
            &self
                .db
                .q("SELECT rel_type, to_ref FROM relations WHERE from_uid = ?"),
        )
        .bind(from_uid)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| Relation {
                rel_type: r.get("rel_type"),
                target: r.get("to_ref"),
            })
            .collect())
    }

    pub async fn count(&self) -> Result<i64, CatalogError> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities WHERE deleted_at IS NULL")
            .fetch_one(self.db.pool())
            .await?;
        Ok(n)
    }
}

const SELECT_ENTITY: &str =
    "SELECT uid, kind, namespace, name, title, description, spec, metadata, \
     lifecycle, origin_repo, origin_path, origin_commit, source_id, content_hash, \
     seen_at, created_at, updated_at, deleted_at FROM entities";

fn row_to_entity(row: sqlx::any::AnyRow) -> Result<Entity, CatalogError> {
    let spec_str: String = row.get("spec");
    let meta_str: String = row.get("metadata");
    let metadata: Metadata = serde_json::from_str(&meta_str)?;
    let lifecycle = Lifecycle::parse(&row.get::<String, _>("lifecycle"));
    Ok(Entity {
        uid: row.get("uid"),
        kind: row.get("kind"),
        metadata,
        spec: serde_json::from_str(&spec_str).unwrap_or(Value::Null),
        relations: vec![],
        lifecycle,
        origin: Origin {
            source_id: row.get("source_id"),
            repo: row.get("origin_repo"),
            path: row.get("origin_path"),
            commit: row.get("origin_commit"),
        },
        content_hash: row.get("content_hash"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        seen_at: row.get("seen_at"),
        deleted_at: row.get("deleted_at"),
    })
}
