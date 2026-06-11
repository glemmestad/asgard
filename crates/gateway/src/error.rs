#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("unauthorized: invalid or revoked virtual key")]
    Unauthorized,
    #[error("project is killed")]
    ProjectKilled,
    #[error("project is not active (decommissioned or archived)")]
    ProjectInactive,
    #[error("budget exceeded for project")]
    BudgetExceeded,
    #[error("unknown model: {0}")]
    UnknownModel(String),
    #[error("no provider registered for '{0}'")]
    NoProvider(String),
    #[error("policy denied: {0}")]
    PolicyDenied(String),
    #[error("guardrail blocked: {0}")]
    GuardrailBlocked(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("storage: {0}")]
    Storage(#[from] frontkeep_storage::StorageError),
    #[error("db: {0}")]
    Sqlx(#[from] sqlx::Error),
}
