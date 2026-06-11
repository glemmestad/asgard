//! Lifecycle review-date engine (WS3). Stops "start light" from silently rotting
//! into unmanaged production: a review deadline per project, a sweep that flags
//! overdue ones, and the policy's one-automatic-extension rule. Expiry is a
//! *flag* (`review_state` ok→expired), never a lifecycle state — it blocks
//! nothing, it surfaces. "Notification" = an audit event; Frontkeep has no notify
//! sink (a real one is a future item).
//!
//! Thresholds are the org-specific mutable layer ([`ReviewConfig`], defaults from
//! the policy doc), not frozen constants.

use crate::RegistryError;
use frontkeep_storage::Db;
use serde::Serialize;
use sqlx::Row;

/// Operator-configured review thresholds. Defaults are the policy doc's values.
#[derive(Debug, Clone)]
pub struct ReviewConfig {
    /// Days from registration to a POC's first review deadline.
    pub poc_window_days: i64,
    /// How many automatic extensions a project gets before a human must decide.
    pub auto_extensions: i64,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        ReviewConfig {
            poc_window_days: 90,
            auto_extensions: 1,
        }
    }
}

/// The review fields read off a project's runtime row.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReviewState {
    pub review_date: String,
    pub review_state: String,
    pub review_extensions: i64,
    pub stack_exception_renewal_date: String,
}

/// What a sweep found. Idempotent: a project is only reported in `newly_expired`
/// on the ok→expired transition, so re-running doesn't re-notify.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SweepSummary {
    pub checked: i64,
    pub newly_expired: Vec<String>,
    pub expired_exceptions: Vec<String>,
}

/// The result of an extension request: either the automatic grant landed, or the
/// allowance is used up and a human-approval workflow request was created.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ExtendOutcome {
    Extended {
        review: ReviewState,
    },
    Pending {
        request: Box<frontkeep_workflow::WorkflowRequest>,
    },
}

pub(crate) async fn read(db: &Db, project_id: &str) -> Result<ReviewState, RegistryError> {
    let row = sqlx::query(&db.q(
        "SELECT review_date, review_state, review_extensions, stack_exception_renewal_date \
         FROM projects_runtime WHERE project_id = ?",
    ))
    .bind(project_id)
    .fetch_optional(db.pool())
    .await?;
    Ok(row
        .map(|r| ReviewState {
            review_date: r.get("review_date"),
            review_state: r.get("review_state"),
            review_extensions: r.get::<i64, _>("review_extensions"),
            stack_exception_renewal_date: r.get("stack_exception_renewal_date"),
        })
        .unwrap_or_default())
}

/// Set the initial review deadline at registration. A POC gets `created_at +
/// window`; any other tier uses the human-entered recurring review date if
/// present, else stays blank (no deadline).
pub(crate) async fn set_initial(
    db: &Db,
    project_id: &str,
    created_at: &str,
    classification: &str,
    recurring_review_date: &str,
    window_days: i64,
) -> Result<(), RegistryError> {
    let review_date = if classification == "poc" {
        frontkeep_storage::plus_days(created_at, window_days)
    } else if !recurring_review_date.trim().is_empty() {
        recurring_review_date.trim().to_string()
    } else {
        String::new()
    };
    if review_date.is_empty() {
        return Ok(());
    }
    sqlx::query(&db.q("UPDATE projects_runtime SET review_date = ? WHERE project_id = ?"))
        .bind(&review_date)
        .bind(project_id)
        .execute(db.pool())
        .await?;
    Ok(())
}
