//! The review engine: pluggable reviewers that scrutinize a promotion and return
//! structured verdicts feeding the existing gate. Mirrors the gateway — a built-in
//! floor (`llm-judge`, in-process) plus external delegation (`webhook`), both
//! dispatched by manifest `kind`. **Escalate-only:** a reviewer may add exception
//! signals (returning the promotion to the submitter or a human) but never clear
//! evidence gaps or enable an auto-approve.

pub mod code_review;
pub mod llm_judge;
pub mod manifest;
pub mod repo;
pub mod webhook;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use asgard_registry::{EvidenceVerdict, Registration, ReviewerOutcome, ReviewerPanel};

pub use code_review::{
    CodeReview, RegistryStandards, ReviewDepth, ReviewDepthMap, StandardsSource,
};
pub use llm_judge::LlmJudge;
pub use manifest::{ReviewerCatalog, ReviewerManifest};
pub use webhook::WebhookReviewer;

#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error("invalid reviewer manifest: {0}")]
    InvalidManifest(String),
    #[error("reviewer backend: {0}")]
    Backend(String),
}

/// A reviewer's judgment. `Abstain` = couldn't judge (offline mock can't parse, a
/// webhook with no endpoint) — it never blocks. `Concern` returns the promotion to
/// the submitter; `Pass` adds nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Disposition {
    Pass,
    Concern,
    Abstain,
}

/// Everything a reviewer reads about a promotion.
#[derive(Debug, Clone)]
pub struct ReviewRequest {
    pub project_id: String,
    pub target: String,
    pub data_class: String,
    pub repo_url: String,
    pub evidence: asgard_registry::Evidence,
    /// The pure machine verdict, so a reviewer can reason about what already passed.
    pub machine_verdict: EvidenceVerdict,
}

/// One reviewer's structured output.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewVerdict {
    pub reviewer_id: String,
    pub kind: String,
    pub disposition: Disposition,
    pub findings: Vec<String>,
    /// Exception signals to ADD (escalate-only lever). Honored as-is by the gate.
    pub add_exception_signals: Vec<String>,
    pub confidence: f64,
    pub model: String,
    pub cost_usd: f64,
    /// Repo files the reviewer actually opened to reach this verdict — the read
    /// provenance (empty for reviewers that read no files, e.g. `llm-judge`).
    /// Persisted in `verdict_json` so an auditor can see what the AI inspected.
    #[serde(default)]
    pub files_read: Vec<String>,
}

impl ReviewVerdict {
    pub(crate) fn pass(id: &str, kind: &str, confidence: f64, model: String, cost: f64) -> Self {
        ReviewVerdict {
            reviewer_id: id.into(),
            kind: kind.into(),
            disposition: Disposition::Pass,
            findings: Vec::new(),
            add_exception_signals: Vec::new(),
            confidence,
            model,
            cost_usd: cost,
            files_read: Vec::new(),
        }
    }

    /// Couldn't judge — records the reason but adds no signal (never blocks).
    pub(crate) fn abstain(id: &str, kind: &str, note: impl Into<String>) -> Self {
        ReviewVerdict {
            reviewer_id: id.into(),
            kind: kind.into(),
            disposition: Disposition::Abstain,
            findings: vec![note.into()],
            add_exception_signals: Vec::new(),
            confidence: 0.0,
            model: String::new(),
            cost_usd: 0.0,
            files_read: Vec::new(),
        }
    }

    /// Attach the read provenance (the files the reviewer opened) to a verdict.
    pub(crate) fn with_files_read(mut self, files: Vec<String>) -> Self {
        self.files_read = files;
        self
    }

    pub(crate) fn concern(
        id: &str,
        kind: &str,
        findings: Vec<String>,
        signal: String,
        confidence: f64,
        model: String,
        cost: f64,
    ) -> Self {
        ReviewVerdict {
            reviewer_id: id.into(),
            kind: kind.into(),
            disposition: Disposition::Concern,
            findings,
            add_exception_signals: vec![signal],
            confidence,
            model,
            cost_usd: cost,
            files_read: Vec::new(),
        }
    }
}

/// Parse a reviewer's `{ "disposition", "findings", "confidence" }` reply (from a
/// model completion or a webhook body) into a verdict. Unrecognized/garbage →
/// `Abstain` (fails open: a malformed verdict never blocks on its own).
pub(crate) fn verdict_from_reply(
    id: &str,
    kind: &str,
    obj: &Value,
    model: String,
    cost: f64,
) -> ReviewVerdict {
    let disp = obj
        .get("disposition")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let findings: Vec<String> = obj
        .get("findings")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let confidence = obj
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);
    match disp.as_str() {
        "pass" | "ok" | "approve" => ReviewVerdict::pass(id, kind, confidence, model, cost),
        "concern" | "fail" | "block" => {
            let signal = if findings.is_empty() {
                format!("{id}: raised a concern")
            } else {
                format!("{id}: {}", findings.join("; "))
            };
            ReviewVerdict::concern(id, kind, findings, signal, confidence, model, cost)
        }
        _ => ReviewVerdict::abstain(id, kind, "reviewer returned no recognized disposition"),
    }
}

/// Extract the first balanced JSON object from free text (models wrap JSON in prose).
pub(crate) fn extract_json(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&text[start..=end]).ok()
}

#[async_trait]
pub trait Reviewer: Send + Sync {
    fn kind(&self) -> &str;
    async fn review(
        &self,
        manifest: &ReviewerManifest,
        req: &ReviewRequest,
    ) -> Result<ReviewVerdict, ReviewError>;
}

/// A reviewer that always abstains — the safe fallback for an unknown `kind`.
struct NoopReviewer;

#[async_trait]
impl Reviewer for NoopReviewer {
    fn kind(&self) -> &str {
        "noop"
    }
    async fn review(
        &self,
        m: &ReviewerManifest,
        _req: &ReviewRequest,
    ) -> Result<ReviewVerdict, ReviewError> {
        Ok(ReviewVerdict::abstain(
            &m.id,
            &m.kind,
            format!("no handler registered for kind '{}'", m.kind),
        ))
    }
}

/// Pluggable handlers keyed by manifest `kind`, with a `noop` fallback.
#[derive(Clone)]
pub struct ReviewerRegistry {
    handlers: HashMap<String, Arc<dyn Reviewer>>,
}

impl Default for ReviewerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ReviewerRegistry {
    pub fn new() -> Self {
        let mut handlers: HashMap<String, Arc<dyn Reviewer>> = HashMap::new();
        handlers.insert("noop".into(), Arc::new(NoopReviewer));
        ReviewerRegistry { handlers }
    }

    pub fn register(&mut self, kind: impl Into<String>, handler: Arc<dyn Reviewer>) {
        self.handlers.insert(kind.into(), handler);
    }

    fn handler(&self, kind: &str) -> Arc<dyn Reviewer> {
        self.handlers
            .get(kind)
            .cloned()
            .unwrap_or_else(|| self.handlers.get("noop").cloned().expect("noop registered"))
    }
}

/// Ties the reviewer catalog to its handler registry and runs the panel. Implements
/// [`asgard_registry::ReviewerPanel`] so `request_promotion` can call it through the
/// registry without depending on the gateway.
pub struct ReviewService {
    catalog: ReviewerCatalog,
    registry: ReviewerRegistry,
    /// Model a manifest's `auto` (or empty) `model:` resolves to — the platform's
    /// configured real model. Lets the built-in reviewers track whatever LLM the
    /// operator wired without pinning a ref in the manifest.
    default_model: String,
}

impl ReviewService {
    pub fn new(
        catalog: ReviewerCatalog,
        registry: ReviewerRegistry,
        default_model: String,
    ) -> Self {
        ReviewService {
            catalog,
            registry,
            default_model,
        }
    }

    /// The manifest a handler actually runs with: `auto`/empty `model:` resolves to
    /// the platform default; an explicitly-pinned model is kept.
    fn effective(&self, m: &ReviewerManifest) -> ReviewerManifest {
        if m.model.trim().is_empty() || m.model == "auto" {
            let mut m = m.clone();
            m.model = self.default_model.clone();
            m
        } else {
            m.clone()
        }
    }
}

#[async_trait]
impl ReviewerPanel for ReviewService {
    async fn review(
        &self,
        reg: &Registration,
        target: &str,
        verdict: &EvidenceVerdict,
    ) -> ReviewerOutcome {
        let req = ReviewRequest {
            project_id: reg.project_id.clone(),
            target: target.to_string(),
            data_class: reg.data_class.clone(),
            repo_url: reg.evidence.repo_or_source_url.clone(),
            evidence: reg.evidence.clone(),
            machine_verdict: verdict.clone(),
        };

        let mut out = ReviewerOutcome::empty();
        for m in self.catalog.enabled_for(target) {
            let m = self.effective(m);
            let handler = self.registry.handler(&m.kind);
            // A hard handler error fails closed: a reviewer that can't run becomes a
            // concern (route to the submitter), never a silent pass.
            let v = handler.review(&m, &req).await.unwrap_or_else(|e| {
                tracing::warn!("reviewer '{}' errored: {e}", m.id);
                ReviewVerdict::concern(
                    &m.id,
                    &m.kind,
                    vec![format!("reviewer '{}' failed to run: {e}", m.id)],
                    format!("reviewer '{}' unavailable", m.id),
                    0.0,
                    String::new(),
                    0.0,
                )
            });
            out.reviewer_ids.push(v.reviewer_id.clone());
            if v.disposition == Disposition::Concern {
                out.passed = false;
                out.added_exception_signals
                    .extend(v.add_exception_signals.iter().cloned());
                out.findings.extend(v.findings.iter().cloned());
            }
            out.verdicts_json
                .push(serde_json::to_value(&v).unwrap_or(Value::Null));
        }
        out.summary = if out.findings.is_empty() {
            String::new()
        } else {
            format!("{} reviewer finding(s)", out.findings.len())
        };
        out
    }

    fn has_async(&self, target: &str) -> bool {
        self.catalog.has_async_for(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concern_reply_becomes_a_blocking_signal() {
        let v = verdict_from_reply(
            "llm-judge",
            "llm-judge",
            &serde_json::json!({"disposition": "concern", "findings": ["ci_status_url 'N/A' is a placeholder"], "confidence": 0.8}),
            "model:x".into(),
            0.0,
        );
        assert_eq!(v.disposition, Disposition::Concern);
        assert_eq!(v.add_exception_signals.len(), 1);
        assert!(v.add_exception_signals[0].contains("placeholder"));
    }

    #[test]
    fn pass_reply_adds_no_signal() {
        let v = verdict_from_reply(
            "llm-judge",
            "llm-judge",
            &serde_json::json!({"disposition": "pass"}),
            "m".into(),
            0.0,
        );
        assert_eq!(v.disposition, Disposition::Pass);
        assert!(v.add_exception_signals.is_empty());
    }

    #[test]
    fn garbage_reply_abstains_and_never_blocks() {
        let v = verdict_from_reply(
            "llm-judge",
            "llm-judge",
            &serde_json::json!({"x": 1}),
            "m".into(),
            0.0,
        );
        assert_eq!(v.disposition, Disposition::Abstain);
        assert!(v.add_exception_signals.is_empty());
    }

    #[test]
    fn extract_json_pulls_object_from_prose() {
        let v =
            extract_json("sure, here you go: {\"disposition\":\"pass\"} hope that helps").unwrap();
        assert_eq!(v["disposition"], "pass");
        assert!(extract_json("no json here").is_none());
    }

    #[tokio::test]
    async fn unknown_kind_falls_back_to_noop_abstain() {
        let reg = ReviewerRegistry::new();
        let m = ReviewerManifest {
            id: "x".into(),
            name: "X".into(),
            kind: "does-not-exist".into(),
            enabled: true,
            dimensions: vec![],
            targets: vec![],
            model: "model:default/mock".into(),
            endpoint_env: None,
            api_key_env: None,
            timeout_secs: 20,
        };
        let req = ReviewRequest {
            project_id: "p".into(),
            target: "light-operational".into(),
            data_class: "internal".into(),
            repo_url: String::new(),
            evidence: Default::default(),
            machine_verdict: EvidenceVerdict {
                target: "light-operational".into(),
                evidence_complete: true,
                missing: vec![],
                exception_signals: vec![],
                unverified_signals: vec![],
            },
        };
        let v = reg.handler(&m.kind).review(&m, &req).await.unwrap();
        assert_eq!(v.disposition, Disposition::Abstain);
    }
}
