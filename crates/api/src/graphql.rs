//! GraphQL surface (async-graphql). Read-focused: entity discovery and audit
//! trail, alongside the REST API. Repos are injected as schema context data.

use async_graphql::{Context, EmptyMutation, EmptySubscription, Object, Schema, SimpleObject};

use frontkeep_catalog::{CatalogRepo, ListFilter};
use frontkeep_storage::audit::{self, AuditQuery};
use frontkeep_storage::Db;

pub type FrontkeepSchema = Schema<QueryRoot, EmptyMutation, EmptySubscription>;

#[derive(SimpleObject)]
pub struct GqlEntity {
    pub uid: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub title: Option<String>,
    pub lifecycle: String,
    pub entity_ref: String,
    /// spec serialized as a JSON string.
    pub spec_json: String,
}

#[derive(SimpleObject)]
pub struct GqlAudit {
    pub ts: String,
    pub actor: String,
    pub action: String,
    pub entity_ref: Option<String>,
    pub trace_id: Option<String>,
    pub outcome: String,
    pub reason: Option<String>,
}

pub struct QueryRoot;

#[Object]
impl QueryRoot {
    /// List catalog entities, optionally filtered by kind and a search query.
    async fn entities(
        &self,
        ctx: &Context<'_>,
        kind: Option<String>,
        query: Option<String>,
    ) -> async_graphql::Result<Vec<GqlEntity>> {
        let repo = ctx.data::<CatalogRepo>()?;
        let filter = ListFilter {
            kind,
            query,
            ..Default::default()
        };
        let entities = repo.list(&filter).await.map_err(|e| e.to_string())?;
        Ok(entities
            .into_iter()
            .map(|e| GqlEntity {
                entity_ref: e.entity_ref(),
                uid: e.uid,
                kind: e.kind,
                namespace: e.metadata.namespace,
                name: e.metadata.name,
                title: e.metadata.title,
                lifecycle: e.lifecycle.as_str().to_string(),
                spec_json: e.spec.to_string(),
            })
            .collect())
    }

    /// A single entity by kind/namespace/name.
    async fn entity(
        &self,
        ctx: &Context<'_>,
        kind: String,
        namespace: String,
        name: String,
    ) -> async_graphql::Result<Option<GqlEntity>> {
        let repo = ctx.data::<CatalogRepo>()?;
        let e = repo
            .get(&kind, &namespace, &name)
            .await
            .map_err(|e| e.to_string())?;
        Ok(e.map(|e| GqlEntity {
            entity_ref: e.entity_ref(),
            uid: e.uid,
            kind: e.kind,
            namespace: e.metadata.namespace,
            name: e.metadata.name,
            title: e.metadata.title,
            lifecycle: e.lifecycle.as_str().to_string(),
            spec_json: e.spec.to_string(),
        }))
    }

    /// Audit trail, optionally filtered by trace id or entity ref.
    async fn audit_trail(
        &self,
        ctx: &Context<'_>,
        trace_id: Option<String>,
        entity_ref: Option<String>,
    ) -> async_graphql::Result<Vec<GqlAudit>> {
        let db = ctx.data::<Db>()?;
        let mut q = AuditQuery::new();
        q.trace_id = trace_id;
        q.entity_ref = entity_ref;
        let rows = audit::query(db, &q).await.map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|r| GqlAudit {
                ts: r.ts,
                actor: r.actor,
                action: r.action,
                entity_ref: r.entity_ref,
                trace_id: r.trace_id,
                outcome: r.outcome,
                reason: r.reason,
            })
            .collect())
    }
}

pub fn build_schema(catalog: CatalogRepo, db: Db) -> FrontkeepSchema {
    Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
        .data(catalog)
        .data(db)
        .finish()
}
