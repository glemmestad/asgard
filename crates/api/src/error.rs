use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(Debug)]
pub enum ApiError {
    NotFound(String),
    Unauthorized(String),
    Forbidden(String),
    BadRequest(String),
    TooManyRequests(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::Unauthorized(m) => (StatusCode::UNAUTHORIZED, m),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::TooManyRequests(m) => (StatusCode::TOO_MANY_REQUESTS, m),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

impl From<frontkeep_catalog::CatalogError> for ApiError {
    fn from(e: frontkeep_catalog::CatalogError) -> Self {
        ApiError::Internal(format!("catalog: {e}"))
    }
}

impl From<frontkeep_gateway::GatewayError> for ApiError {
    fn from(e: frontkeep_gateway::GatewayError) -> Self {
        use frontkeep_gateway::GatewayError as G;
        match e {
            G::Unauthorized => ApiError::Unauthorized("invalid or revoked key".into()),
            G::ProjectKilled => ApiError::Forbidden("project is killed".into()),
            G::ProjectInactive => {
                ApiError::Forbidden("project is not active (decommissioned)".into())
            }
            G::BudgetExceeded => ApiError::TooManyRequests("budget exceeded".into()),
            G::PolicyDenied(r) => ApiError::Forbidden(format!("policy denied: {r}")),
            G::GuardrailBlocked(r) => ApiError::BadRequest(format!("guardrail blocked: {r}")),
            G::UnknownModel(m) => ApiError::BadRequest(format!("unknown model: {m}")),
            G::NoProvider(p) => ApiError::Internal(format!("no provider: {p}")),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<frontkeep_workflow::WorkflowError> for ApiError {
    fn from(e: frontkeep_workflow::WorkflowError) -> Self {
        use frontkeep_workflow::WorkflowError as W;
        match e {
            W::NotFound(id) => ApiError::NotFound(format!("request {id}")),
            W::InvalidTransition { from, to } => {
                ApiError::BadRequest(format!("invalid transition {from} -> {to}"))
            }
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<frontkeep_registry::RegistryError> for ApiError {
    fn from(e: frontkeep_registry::RegistryError) -> Self {
        use frontkeep_registry::RegistryError as R;
        match e {
            R::Validation(m) => ApiError::BadRequest(m),
            R::NotRegistered(_) => ApiError::Forbidden(e.to_string()),
            R::Inactive(_) => ApiError::Forbidden(e.to_string()),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<frontkeep_provision::ProvisionError> for ApiError {
    fn from(e: frontkeep_provision::ProvisionError) -> Self {
        use frontkeep_provision::ProvisionError as P;
        match e {
            P::Unsupported(m) => ApiError::BadRequest(format!("unsupported resource: {m}")),
            P::InvalidSpec(m) => ApiError::BadRequest(m),
            P::NotPermitted(m) => ApiError::Forbidden(m),
            P::NotFound(m) => ApiError::NotFound(format!("request {m}")),
            P::Registry(r) => ApiError::from(r),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<frontkeep_identity::IdentityError> for ApiError {
    fn from(e: frontkeep_identity::IdentityError) -> Self {
        use frontkeep_identity::IdentityError as I;
        match e {
            I::InvalidCredentials => ApiError::Unauthorized("invalid credentials".into()),
            I::InvalidSession => ApiError::Unauthorized("invalid session".into()),
            I::UserExists(u) => ApiError::BadRequest(format!("user exists: {u}")),
            other => ApiError::Internal(other.to_string()),
        }
    }
}
