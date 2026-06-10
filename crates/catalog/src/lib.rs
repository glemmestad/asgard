//! Frontkeep catalog: the typed entity graph, schema validation, Git ingestion, and
//! pull-based reconciliation. See RFC-0001.

pub mod entity;
pub mod error;
pub mod ingest;
pub mod reconcile;
pub mod repo;
pub mod seed;
pub mod standards;
pub mod validation;

pub use entity::{Entity, EntityRef, Lifecycle, Manifest, Metadata, Origin, Relation, API_VERSION};
pub use error::CatalogError;
pub use ingest::{
    is_manifest_file, parse_manifests, FixtureProvider, GitHubProvider, GitLabProvider,
    RawManifest, SourceProvider, MANIFEST_NAMES,
};
pub use reconcile::{reconcile, ReconcileReport};
pub use repo::{CatalogRepo, ListFilter, Upsert};
pub use validation::SchemaRegistry;
