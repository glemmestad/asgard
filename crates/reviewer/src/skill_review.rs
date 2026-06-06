//! Code-review assist for a submitted skill bundle. Reuses the deep reviewer's
//! machinery ([`run_tool_loop`] over an in-memory [`RepoReader::from_bundle`], with
//! `grounded` keeping only findings about files the model actually read) but is
//! **advisory and escalate-only**: it surfaces findings for the human approver and
//! never approves or relaxes anything. Off the real path (mock model or no system
//! key) it returns a deterministic stub so the flow is testable offline.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

use asgard_gateway::{run_tool_loop, Gateway};

use crate::code_review::{CodeReviewTools, FAIL_MARKER};
use crate::repo::RepoReader;
use crate::{extract_json, ReviewVerdict};

const REVIEWER_ID: &str = "skill-review";
const KIND: &str = "code-review";

const SKILL_GROUNDING: &str = "You are reviewing an agent SKILL before it is published to a shared catalog. \
A skill is a SKILL.md (instructions + YAML frontmatter) plus optional bundled scripts, config, and \
references. Read the ACTUAL files with read_file — the listing alone is not evidence. Judge whether \
this skill is safe to share and well-formed. Use the standards below as reference for the bundled code.";

const SKILL_QUESTION: &str =
    "Decide whether this skill is safe and well-formed enough to share in a company catalog. \
     You MUST read file CONTENTS with read_file before judging. \
     Reply with ONE JSON object and nothing else: \
     {\"disposition\":\"pass\"|\"concern\",\
     \"findings\":[{\"file\":\"<exact path you read>\",\"issue\":\"<the material problem>\"}],\
     \"confidence\":0.0-1.0}. \
     Raise \"concern\" ONLY for a material problem you verified inside a file you actually read — \
     for example: a secret or credential committed in the bundle; dangerous shell (curl|sh, rm -rf \
     on a variable, eval of remote input); unexpected network calls that exfiltrate data; the SKILL.md \
     lacking a clear description/trigger so it can't be invoked correctly; declared allowed-tools that \
     don't match what the scripts actually do. Do NOT raise findings for formatting, naming, or style. \
     Every finding's `file` MUST be a file you opened with read_file. When in doubt, pass.";

/// Review a stored skill bundle's files. `files` is the decoded tree (path → bytes).
/// `system_key` is a platform gateway key for the model call; `None` (or a `mock`
/// model) takes the deterministic offline path. Returns an advisory verdict.
pub async fn review_skill_bundle(
    gateway: &Gateway,
    system_key: Option<&str>,
    model: &str,
    files: BTreeMap<String, Vec<u8>>,
    standards: &str,
    max_rounds: usize,
) -> ReviewVerdict {
    let reader = RepoReader::from_bundle(files);
    if system_key.is_none() || model.contains("mock") {
        return stub(&reader, model).await;
    }
    let grounding = format!("{SKILL_GROUNDING}\n\n## Standards\n{standards}");
    let tools = CodeReviewTools::new(reader);
    let key = system_key.unwrap_or_default();
    match run_tool_loop(
        gateway,
        key,
        model,
        None,
        &grounding,
        SKILL_QUESTION,
        &tools,
        max_rounds,
        Some("read_file"),
    )
    .await
    {
        Ok(outcome) => match extract_json(&outcome.answer) {
            Some(o) => grounded(&o, &tools.read_paths(), model, outcome.cost_usd),
            None => ReviewVerdict::abstain(REVIEWER_ID, KIND, "reviewer produced no verdict"),
        },
        // Advisory, not a gate: an unreachable model abstains (no findings), it does
        // not block — a human still decides.
        Err(e) => {
            ReviewVerdict::abstain(REVIEWER_ID, KIND, format!("review model unavailable: {e}"))
        }
    }
}

/// Keep only findings about files the model actually opened; a `concern` with no
/// grounded finding downgrades to a non-blocking pass (mirrors the code reviewer).
fn grounded(obj: &Value, files_read: &[String], model: &str, cost: f64) -> ReviewVerdict {
    let read: HashSet<&str> = files_read.iter().map(String::as_str).collect();
    let confidence = obj
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);
    let grounded: Vec<String> = obj
        .get("findings")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let file = f.get("file").and_then(|v| v.as_str())?;
                    let issue = f.get("issue").and_then(|v| v.as_str())?;
                    read.contains(file).then(|| format!("{file}: {issue}"))
                })
                .collect()
        })
        .unwrap_or_default();
    let disposition = obj
        .get("disposition")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_concern = matches!(disposition.as_str(), "concern" | "fail" | "block");
    if is_concern && !grounded.is_empty() {
        let signal = format!("{REVIEWER_ID}: {}", grounded.join("; "));
        ReviewVerdict::concern(
            REVIEWER_ID,
            KIND,
            grounded,
            signal,
            confidence,
            model.to_string(),
            cost,
        )
        .with_files_read(files_read.to_vec())
    } else {
        let mut v = ReviewVerdict::pass(REVIEWER_ID, KIND, confidence, model.to_string(), cost);
        if is_concern {
            v.findings = vec![
                "reviewer raised only findings about files it did not read — none verified".into(),
            ];
        }
        v.with_files_read(files_read.to_vec())
    }
}

/// Offline judgment: read the bundle (proving the read path) and pass a clean tree;
/// a bundle carrying the fail marker raises a concern. Deterministic, no model call.
async fn stub(reader: &RepoReader, model: &str) -> ReviewVerdict {
    let files = reader.list_files().await.unwrap_or_default();
    let n = files.len();
    let flagged = files
        .iter()
        .any(|f| f.ends_with(FAIL_MARKER) || f.contains("REVIEW_FAIL"));
    if flagged {
        ReviewVerdict::concern(
            REVIEWER_ID,
            KIND,
            vec![format!(
                "offline skill-review stub: bundle carries a review-fail marker ({n} file(s))"
            )],
            format!("{REVIEWER_ID}: bundle flagged by offline review"),
            1.0,
            model.to_string(),
            0.0,
        )
    } else {
        let mut v = ReviewVerdict::pass(REVIEWER_ID, KIND, 1.0, model.to_string(), 0.0);
        v.findings = vec![format!(
            "offline skill-review stub: read {n} file(s), clean"
        )];
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Disposition;

    fn reader(extra: Option<(&str, &[u8])>) -> RepoReader {
        let mut f = BTreeMap::new();
        f.insert(
            "SKILL.md".to_string(),
            b"---\nname: x\ndescription: y\n---\nbody".to_vec(),
        );
        if let Some((p, b)) = extra {
            f.insert(p.to_string(), b.to_vec());
        }
        RepoReader::from_bundle(f)
    }

    // The offline stub reads the bundle and judges it on its own — clean passes, a
    // fail-marker raises a concern. No gateway needed.
    #[tokio::test]
    async fn offline_stub_passes_clean_and_flags_marker() {
        let clean = stub(&reader(None), "model:default/mock").await;
        assert_eq!(clean.disposition, Disposition::Pass);
        assert!(clean.findings[0].contains("clean"));

        let flagged = stub(
            &reader(Some((".asgard-review-fail", b""))),
            "model:default/mock",
        )
        .await;
        assert_eq!(flagged.disposition, Disposition::Concern);
    }

    #[test]
    fn grounded_keeps_only_read_file_findings() {
        let read = vec!["scripts/run.sh".to_string()];
        let reply = serde_json::json!({
            "disposition": "concern",
            "findings": [
                {"file": "scripts/run.sh", "issue": "curl | sh"},
                {"file": "unread.py", "issue": "made up"}
            ],
            "confidence": 0.7
        });
        let v = grounded(&reply, &read, "model:x", 0.0);
        assert_eq!(v.disposition, Disposition::Concern);
        assert_eq!(v.findings, vec!["scripts/run.sh: curl | sh"]);

        // Every finding cites an unread file → unverified → downgraded to pass.
        let reply2 = serde_json::json!({
            "disposition": "concern",
            "findings": [{"file": "unread.py", "issue": "x"}],
            "confidence": 0.9
        });
        let v2 = grounded(&reply2, &read, "model:x", 0.0);
        assert_eq!(v2.disposition, Disposition::Pass);
    }
}
