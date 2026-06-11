use thiserror::Error;

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("storage: {0}")]
    Storage(#[from] frontkeep_storage::StorageError),
    #[error("db: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("yaml: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("http: {0}")]
    Http(String),
    #[error("schema compile error for {kind}: {msg}")]
    Schema { kind: String, msg: String },
    #[error("validation failed for {kind}: {}", errors.join("; "))]
    Invalid { kind: String, errors: Vec<String> },
    #[error("bad entity ref: {0}")]
    BadRef(String),
    #[error("not found: {0}")]
    NotFound(String),
}
