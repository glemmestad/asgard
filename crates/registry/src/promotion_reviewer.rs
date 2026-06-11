//! The promotion-reviewer seam: a pluggable panel that scrutinizes a promotion's
//! evidence and returns *additional* exception signals. The concrete panel lives
//! in `frontkeep-reviewer` (gateway-backed); `request_promotion` calls through this
//! trait so the registry needn't depend on the gateway. Distinct from the
//! recurring compliance-review window in `review.rs`.

use async_trait::async_trait;
use serde::Serialize;

use crate::{EvidenceVerdict, Registration};

/// What the reviewer panel contributes to a promotion decision. **Escalate-only:**
/// it may add exception signals (which return the promotion to the submitter or a
/// human) but can never clear evidence gaps or enable an auto-approve. The default
/// is the identity — a clean pass that changes nothing — so a registry with no
/// panel wired behaves exactly as before.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewerOutcome {
    pub passed: bool,
    pub added_exception_signals: Vec<String>,
    pub findings: Vec<String>,
    pub summary: String,
    pub reviewer_ids: Vec<String>,
    /// One serialized verdict per reviewer that ran, persisted for audit.
    pub verdicts_json: Vec<serde_json::Value>,
}

impl Default for ReviewerOutcome {
    fn default() -> Self {
        ReviewerOutcome {
            passed: true,
            added_exception_signals: Vec::new(),
            findings: Vec::new(),
            summary: String::new(),
            reviewer_ids: Vec::new(),
            verdicts_json: Vec::new(),
        }
    }
}

impl ReviewerOutcome {
    pub fn empty() -> Self {
        Self::default()
    }
}

#[async_trait]
pub trait ReviewerPanel: Send + Sync {
    /// Review a promotion to `target`. `verdict` is the pure machine evaluation, so
    /// a reviewer can reason about what already passed or failed.
    async fn review(
        &self,
        reg: &Registration,
        target: &str,
        verdict: &EvidenceVerdict,
    ) -> ReviewerOutcome;

    /// Whether an enabled reviewer for `target` runs asynchronously (reads a repo,
    /// many model calls) and so must be dispatched to the background worker rather
    /// than run inline. Default `false`: a panel of only inline reviewers.
    fn has_async(&self, _target: &str) -> bool {
        false
    }
}
