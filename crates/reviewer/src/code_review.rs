//! The deep reviewer (`kind: code-review`): reads the project's actual source over
//! the [`RepoReader`] (no clone) and judges it against the org's coding standards,
//! with depth scaled by the target tier. Drives a bounded `run_tool_loop` so the
//! model navigates the repo (`list_files`/`read_file`) — read-only, never executed.
//! Async by nature; runs inside the background review job. Offline (mock) it falls
//! back to a deterministic stub that still reads the repo, so the path is testable.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use asgard_gateway::{run_tool_loop, Gateway, ToolDef, ToolExecutor};

use crate::manifest::ReviewerManifest;
use crate::repo::RepoReader;
use crate::{
    extract_json, verdict_from_reply, ReviewError, ReviewRequest, ReviewVerdict, Reviewer,
};

const REVIEW_QUESTION: &str =
    "Review this repository against the standards above. Use list_files then read_file to inspect \
     the code you need. Reply with ONE JSON object and nothing else: \
     {\"disposition\":\"pass\"|\"concern\",\"findings\":[\"...\"],\"confidence\":0.0-1.0}. \
     Use \"concern\" only for concrete, fixable violations of the standards.";

/// How hard to look, per target tier. `skip` tiers run no code review.
#[derive(Debug, Clone)]
pub struct ReviewDepth {
    pub skip: bool,
    pub standard_ids: Vec<String>,
    pub max_rounds: usize,
}

impl ReviewDepth {
    fn skipped() -> Self {
        ReviewDepth {
            skip: true,
            standard_ids: vec![],
            max_rounds: 0,
        }
    }
}

/// Per-tier review depth, default-then-override like `ClassificationRequirements`.
#[derive(Debug, Clone)]
pub struct ReviewDepthMap(BTreeMap<String, ReviewDepth>);

impl Default for ReviewDepthMap {
    fn default() -> Self {
        let mut m = BTreeMap::new();
        m.insert(
            "light-operational".into(),
            ReviewDepth {
                skip: false,
                standard_ids: vec!["coding".into()],
                max_rounds: 6,
            },
        );
        m.insert(
            "wide-operational".into(),
            ReviewDepth {
                skip: false,
                standard_ids: vec!["coding".into(), "security".into()],
                max_rounds: 8,
            },
        );
        m.insert(
            "critical-path".into(),
            ReviewDepth {
                skip: false,
                standard_ids: vec!["coding".into(), "security".into(), "workflow".into()],
                max_rounds: 10,
            },
        );
        ReviewDepthMap(m)
    }
}

impl ReviewDepthMap {
    /// Replace a tier's depth from operator config; absent tiers keep the default.
    pub fn with_override(mut self, tier: impl Into<String>, depth: ReviewDepth) -> Self {
        self.0.insert(tier.into(), depth);
        self
    }
    fn for_tier(&self, target: &str) -> ReviewDepth {
        self.0
            .get(target)
            .cloned()
            .unwrap_or_else(ReviewDepth::skipped)
    }
}

/// Supplies the rubric — the org's standards bodies, by id. Backed by the registry
/// standards store at runtime (operator-editable).
#[async_trait]
pub trait StandardsSource: Send + Sync {
    async fn standard(&self, id: &str) -> Option<String>;
}

/// A [`StandardsSource`] backed by the registry's operator-editable standards
/// store. Holds the shared `Db` directly (not the registry) to avoid an `Arc`
/// cycle registry → reviewer → registry; the reviewer only reads.
pub struct RegistryStandards {
    db: asgard_storage::Db,
}

impl RegistryStandards {
    pub fn new(db: asgard_storage::Db) -> Self {
        RegistryStandards { db }
    }
}

#[async_trait]
impl StandardsSource for RegistryStandards {
    async fn standard(&self, id: &str) -> Option<String> {
        asgard_registry::standards::get(&self.db, id)
            .await
            .ok()
            .flatten()
            .map(|s| s.body)
    }
}

pub struct CodeReview {
    gateway: Arc<Gateway>,
    system_key: Option<String>,
    standards: Arc<dyn StandardsSource>,
    depth: ReviewDepthMap,
}

impl CodeReview {
    pub fn new(
        gateway: Arc<Gateway>,
        system_key: Option<String>,
        standards: Arc<dyn StandardsSource>,
        depth: ReviewDepthMap,
    ) -> Self {
        CodeReview {
            gateway,
            system_key,
            standards,
            depth,
        }
    }

    async fn gather_standards(&self, depth: &ReviewDepth) -> String {
        let mut out = String::new();
        for id in &depth.standard_ids {
            if let Some(body) = self.standards.standard(id).await {
                out.push_str(&format!("\n## Standard: {id}\n{body}\n"));
            }
        }
        if out.is_empty() {
            out.push_str("(no standards configured — judge general code quality)");
        }
        out
    }
}

/// Marker a repo can carry to deterministically fail the offline review (a file
/// named `.asgard-review-fail`, or any path containing `REVIEW_FAIL`). Lets tests
/// and the offline e2e drive a concern on an otherwise machine-clean repo.
const FAIL_MARKER: &str = ".asgard-review-fail";

/// Offline/mock judgment: read the repo (proving the read path) and judge it
/// *independently* of the machine verdict — a clean tree passes; a tree carrying
/// the fail marker raises a concern. Deterministic, no model call.
async fn stub_verdict(
    m: &ReviewerManifest,
    _req: &ReviewRequest,
    reader: &RepoReader,
) -> ReviewVerdict {
    let files = reader.list_files().await.unwrap_or_default();
    let n = files.len();
    let flagged = files
        .iter()
        .any(|f| f.ends_with(FAIL_MARKER) || f.contains("REVIEW_FAIL"));
    if flagged {
        ReviewVerdict::concern(
            &m.id,
            "code-review",
            vec![format!(
                "offline code-review stub: repo carries a review-fail marker ({n} file(s) read)"
            )],
            format!("{}: repo flagged by offline review", m.id),
            1.0,
            m.model.clone(),
            0.0,
        )
    } else {
        let mut v = ReviewVerdict::pass(&m.id, "code-review", 1.0, m.model.clone(), 0.0);
        v.findings = vec![format!("offline code-review stub: read {n} file(s), clean")];
        v
    }
}

#[async_trait]
impl Reviewer for CodeReview {
    fn kind(&self) -> &str {
        "code-review"
    }

    async fn review(
        &self,
        m: &ReviewerManifest,
        req: &ReviewRequest,
    ) -> Result<ReviewVerdict, ReviewError> {
        let depth = self.depth.for_tier(&req.target);
        if depth.skip {
            return Ok(ReviewVerdict::pass(
                &m.id,
                "code-review",
                1.0,
                m.model.clone(),
                0.0,
            ));
        }
        // A tier that needs code review but whose repo we can't read fails closed.
        let reader = match RepoReader::from_url(&req.repo_url) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ReviewVerdict::concern(
                    &m.id,
                    "code-review",
                    vec![format!("cannot read repo '{}': {e}", req.repo_url)],
                    format!("{}: repo unreadable for review", m.id),
                    0.0,
                    m.model.clone(),
                    0.0,
                ))
            }
        };

        if self.system_key.is_none() || m.model.contains("mock") {
            return Ok(stub_verdict(m, req, &reader).await);
        }

        let grounding = format!(
            "You are a governance code reviewer for a promotion to the '{}' tier. Judge the \
             repository strictly against these standards:\n{}",
            req.target,
            self.gather_standards(&depth).await
        );
        let tools = CodeReviewTools { reader };
        let key = self.system_key.as_deref().unwrap_or_default();
        match run_tool_loop(
            &self.gateway,
            key,
            &m.model,
            Some(req.data_class.clone()),
            &grounding,
            REVIEW_QUESTION,
            &tools,
            depth.max_rounds,
        )
        .await
        {
            Ok(answer) => Ok(match extract_json(&answer) {
                Some(o) => verdict_from_reply(&m.id, "code-review", &o, m.model.clone(), 0.0),
                None => {
                    ReviewVerdict::abstain(&m.id, "code-review", "reviewer produced no verdict")
                }
            }),
            // Fail closed: an unreachable model on a tier that requires review is a concern.
            Err(e) => Ok(ReviewVerdict::concern(
                &m.id,
                "code-review",
                vec![format!("review model unavailable: {e}")],
                format!("{}: review model unavailable", m.id),
                0.0,
                m.model.clone(),
                0.0,
            )),
        }
    }
}

struct CodeReviewTools {
    reader: RepoReader,
}

#[async_trait]
impl ToolExecutor for CodeReviewTools {
    fn tools(&self) -> Vec<ToolDef> {
        [
            (
                "list_files",
                "List the repository's reviewable file paths. args: {}",
            ),
            (
                "read_file",
                "Read one file's contents. args: {\"path\":\"...\"}",
            ),
        ]
        .into_iter()
        .map(|(name, description)| ToolDef {
            name: name.into(),
            description: description.into(),
        })
        .collect()
    }

    async fn call(&self, name: &str, args: &Value) -> Result<String, String> {
        match name {
            "list_files" => {
                let files = self.reader.list_files().await.map_err(|e| e.to_string())?;
                Ok(json!({ "files": files }).to_string())
            }
            "read_file" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    return Err("read_file needs a 'path'".into());
                }
                let body = self
                    .reader
                    .read_file(path)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(json!({ "path": path, "content": body }).to_string())
            }
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Disposition;
    use asgard_registry::EvidenceVerdict;

    fn req(repo: &str, complete: bool) -> ReviewRequest {
        ReviewRequest {
            project_id: "p".into(),
            target: "light-operational".into(),
            data_class: "internal".into(),
            repo_url: repo.into(),
            evidence: Default::default(),
            machine_verdict: EvidenceVerdict {
                target: "light-operational".into(),
                evidence_complete: complete,
                missing: vec![],
                exception_signals: vec![],
                unverified_signals: vec![],
            },
        }
    }

    fn manifest() -> ReviewerManifest {
        ReviewerManifest {
            id: "code-review".into(),
            name: "Code Review".into(),
            kind: "code-review".into(),
            enabled: true,
            dimensions: vec![],
            targets: vec![],
            model: "model:default/mock".into(),
            endpoint_env: None,
            api_key_env: None,
            timeout_secs: 20,
        }
    }

    // The stub reads the repo and judges it on its own (a fail-marker file), not by
    // echoing the machine verdict. Exercised over a local fixture repo, no gateway.
    #[tokio::test]
    async fn stub_reads_repo_and_judges_independently() {
        let dir = std::env::temp_dir().join(format!("asgard-cr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.rs"), b"fn main() {}\n").unwrap();
        let reader = RepoReader::from_url(dir.to_str().unwrap()).unwrap();
        let m = manifest();

        // Clean tree → pass, regardless of the machine verdict.
        let pass = stub_verdict(&m, &req(dir.to_str().unwrap(), false), &reader).await;
        assert_eq!(pass.disposition, Disposition::Pass);
        assert!(pass.findings[0].contains("read 1 file"));

        // A fail marker → concern, even though the machine verdict is clean.
        std::fs::write(dir.join(FAIL_MARKER), b"").unwrap();
        let concern = stub_verdict(&m, &req(dir.to_str().unwrap(), true), &reader).await;
        assert_eq!(concern.disposition, Disposition::Concern);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn depth_scales_by_tier_and_skips_poc() {
        let d = ReviewDepthMap::default();
        assert!(d.for_tier("poc").skip);
        assert_eq!(d.for_tier("light-operational").standard_ids, vec!["coding"]);
        assert_eq!(d.for_tier("critical-path").standard_ids.len(), 3);
    }
}
