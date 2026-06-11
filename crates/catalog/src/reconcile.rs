//! Pull-based reconciliation (RFC-0001 §7). The set observed from a source is
//! diffed against the set stored for that source: new/changed are upserted,
//! entities absent from the latest observation are soft-deleted — which is how
//! deletes propagate (the anti-drift property). Backstage-compat kinds without a
//! schema get a lenient envelope check so federation still works.

use std::collections::HashSet;

use frontkeep_storage::audit::{self, AuditRecord};

use crate::entity::{Entity, Origin};
use crate::error::CatalogError;
use crate::ingest::{parse_manifests, SourceProvider};
use crate::repo::{CatalogRepo, Upsert};
use crate::validation::SchemaRegistry;

#[derive(Debug, Default, Clone)]
pub struct ReconcileReport {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub invalid: Vec<(String, Vec<String>)>,
}

impl ReconcileReport {
    pub fn observed(&self) -> usize {
        self.inserted + self.updated + self.unchanged
    }
}

pub async fn reconcile(
    repo: &CatalogRepo,
    registry: &SchemaRegistry,
    provider: &dyn SourceProvider,
) -> Result<ReconcileReport, CatalogError> {
    let source_id = provider.source_id();
    let raws = provider.fetch().await?;
    let db = repo.db();
    let mut report = ReconcileReport::default();
    let mut seen: HashSet<String> = HashSet::new();

    for raw in raws {
        let manifests = match parse_manifests(&raw.content) {
            Ok(m) => m,
            Err(e) => {
                report.invalid.push((raw.path.clone(), vec![e.to_string()]));
                continue;
            }
        };

        for m in manifests {
            // Strict validation for kinds we own a schema for; lenient envelope
            // check for Backstage-compat federation kinds.
            if registry.known_kind(&m.kind) {
                if let Err(errs) = registry.validate(&m.kind, &m.as_value()) {
                    let label = format!("{}#{}", raw.path, m.metadata.name);
                    report.invalid.push((label.clone(), errs.clone()));
                    let _ = audit::append(
                        db,
                        &AuditRecord::new("reconciler", "entity.invalid")
                            .entity(label)
                            .outcome("denied")
                            .reason(errs.join("; ")),
                    )
                    .await;
                    continue;
                }
            } else if m.metadata.name.is_empty() {
                report
                    .invalid
                    .push((raw.path.clone(), vec!["metadata.name required".into()]));
                continue;
            }

            let origin = Origin {
                source_id: Some(source_id.clone()),
                repo: Some(raw.repo.clone()),
                path: Some(raw.path.clone()),
                commit: raw.commit.clone(),
            };
            let entity = Entity::from_manifest(m, origin);
            let entity_ref = entity.entity_ref();
            seen.insert(entity_ref.clone());

            let (outcome, _uid) = repo.upsert(&entity).await?;
            match outcome {
                Upsert::Inserted => {
                    report.inserted += 1;
                    audit_entity(db, "entity.upserted", &entity_ref, "inserted").await;
                }
                Upsert::Updated => {
                    report.updated += 1;
                    audit_entity(db, "entity.upserted", &entity_ref, "updated").await;
                }
                Upsert::Unchanged => report.unchanged += 1,
            }
        }
    }

    // Removals: anything stored for this source we did not observe this pass.
    let stored = repo.active_refs_for_source(&source_id).await?;
    for (uid, entity_ref) in stored {
        if !seen.contains(&entity_ref) {
            repo.soft_delete(&uid).await?;
            report.removed += 1;
            audit_entity(db, "entity.removed", &entity_ref, "removed").await;
        }
    }

    Ok(report)
}

async fn audit_entity(db: &frontkeep_storage::Db, action: &str, entity_ref: &str, outcome: &str) {
    let _ = audit::append(
        db,
        &AuditRecord::new("reconciler", action)
            .entity(entity_ref)
            .outcome(outcome),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::ListFilter;
    use frontkeep_storage::Db;

    async fn fresh_db() -> Db {
        let path =
            std::env::temp_dir().join(format!("frontkeep-cat-{}.db", frontkeep_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        db
    }

    fn write(dir: &std::path::Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[tokio::test]
    async fn reconcile_inserts_updates_and_propagates_deletes() {
        let db = fresh_db().await;
        let repo = CatalogRepo::new(db.clone());
        let registry = SchemaRegistry::embedded().unwrap();

        let dir =
            std::env::temp_dir().join(format!("frontkeep-fix-{}", frontkeep_storage::new_uid()));
        std::fs::create_dir_all(&dir).unwrap();
        write(
            &dir,
            "agent.yaml",
            "apiVersion: frontkeep.dev/v1\nkind: Agent\nmetadata:\n  name: reviewer\nspec:\n  owner: group:default/platform\n  model: model:default/gpt\n",
        );
        write(
            &dir,
            "prompt.yaml",
            "apiVersion: frontkeep.dev/v1\nkind: Prompt\nmetadata:\n  name: sys\nspec:\n  owner: group:default/platform\n  template: hello\n",
        );

        let provider = crate::ingest::FixtureProvider::with_id(&dir, "test-source");

        // First pass: both inserted.
        let r1 = reconcile(&repo, &registry, &provider).await.unwrap();
        assert_eq!(r1.inserted, 2, "report: {r1:?}");
        assert_eq!(repo.count().await.unwrap(), 2);

        // Second pass, no changes: both unchanged.
        let r2 = reconcile(&repo, &registry, &provider).await.unwrap();
        assert_eq!(r2.unchanged, 2);
        assert_eq!(r2.inserted, 0);

        // Change the agent: one updated.
        write(
            &dir,
            "agent.yaml",
            "apiVersion: frontkeep.dev/v1\nkind: Agent\nmetadata:\n  name: reviewer\n  title: Reviewer\nspec:\n  owner: group:default/platform\n  model: model:default/claude\n",
        );
        let r3 = reconcile(&repo, &registry, &provider).await.unwrap();
        assert_eq!(r3.updated, 1);
        assert_eq!(r3.unchanged, 1);

        // Remove the prompt file: delete must propagate.
        std::fs::remove_file(dir.join("prompt.yaml")).unwrap();
        let r4 = reconcile(&repo, &registry, &provider).await.unwrap();
        assert_eq!(r4.removed, 1, "report: {r4:?}");
        assert_eq!(repo.count().await.unwrap(), 1);

        // The surviving entity is the agent, now titled.
        let list = repo.list(&ListFilter::new()).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].metadata.name, "reviewer");
        assert_eq!(list[0].metadata.title.as_deref(), Some("Reviewer"));

        // Audit captured inserts, an update, and a removal.
        let audits = audit::query(&db, &audit::AuditQuery::new().limit(100))
            .await
            .unwrap();
        assert!(audits.iter().any(|a| a.action == "entity.removed"));
        assert!(audits.iter().any(|a| a.outcome == "updated"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn invalid_manifest_is_reported_not_stored() {
        let db = fresh_db().await;
        let repo = CatalogRepo::new(db.clone());
        let registry = SchemaRegistry::embedded().unwrap();
        let dir =
            std::env::temp_dir().join(format!("frontkeep-fix-{}", frontkeep_storage::new_uid()));
        std::fs::create_dir_all(&dir).unwrap();
        // Agent missing required spec.model.
        write(
            &dir,
            "agent.yaml",
            "apiVersion: frontkeep.dev/v1\nkind: Agent\nmetadata:\n  name: broken\nspec:\n  owner: group:default/platform\n",
        );
        let provider = crate::ingest::FixtureProvider::with_id(&dir, "bad-source");
        let r = reconcile(&repo, &registry, &provider).await.unwrap();
        assert_eq!(r.inserted, 0);
        assert_eq!(r.invalid.len(), 1);
        assert_eq!(repo.count().await.unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
