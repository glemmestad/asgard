//! The reviewer catalog: one YAML per reviewer (`reviewers/<id>/reviewer.yaml`)
//! declares which handler `kind` runs it and the kind-specific knobs. Adding a
//! reviewer is dropping a manifest — no recompile. The built-in `llm-judge` ships
//! embedded and enabled; an operator overlay directory adds or overrides reviewers
//! (e.g. an external `webhook` reviewer) at runtime. Mirrors the service catalog.

use std::collections::BTreeMap;
use std::path::Path;

use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};

use crate::ReviewError;

#[derive(RustEmbed)]
#[folder = "../../reviewers"]
struct DefaultReviewers;

/// Handler kinds a manifest may select. `llm-judge` is the built-in floor;
/// `webhook` delegates to an external reviewer.
pub const KINDS: &[&str] = &["llm-judge", "webhook", "code-review"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewerManifest {
    pub id: String,
    pub name: String,
    /// Handler key (validated against [`KINDS`]).
    pub kind: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Which review dimensions this reviewer owns (informational in slice 1).
    #[serde(default)]
    pub dimensions: Vec<String>,
    /// Target tiers this reviewer runs for; empty = all.
    #[serde(default)]
    pub targets: Vec<String>,
    /// `llm-judge`: the gateway model ref to route through.
    #[serde(default = "default_model")]
    pub model: String,
    /// `webhook`: env var holding the reviewer endpoint URL. Absent/unset = inert.
    #[serde(default)]
    pub endpoint_env: Option<String>,
    /// `webhook`: optional env var holding a bearer token for the endpoint.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// `webhook`: request deadline in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_true() -> bool {
    true
}
fn default_model() -> String {
    "model:default/mock".to_string()
}
fn default_timeout() -> u64 {
    20
}

impl ReviewerManifest {
    /// Whether this reviewer runs for a promotion to `target` (enabled + tier match).
    pub fn applies_to(&self, target: &str) -> bool {
        self.enabled && (self.targets.is_empty() || self.targets.iter().any(|t| t == target))
    }

    /// Async reviewers (they clone/read a repo and make many model calls) run in the
    /// background review job rather than inline. Currently only `code-review`.
    pub fn is_async(&self) -> bool {
        self.kind == "code-review"
    }

    fn validate(&self) -> Result<(), ReviewError> {
        if self.id.trim().is_empty() {
            return Err(ReviewError::InvalidManifest("manifest has empty id".into()));
        }
        if !KINDS.contains(&self.kind.as_str()) {
            return Err(ReviewError::InvalidManifest(format!(
                "reviewer '{}': unknown kind '{}'",
                self.id, self.kind
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct ReviewerCatalog {
    reviewers: BTreeMap<String, ReviewerManifest>,
}

impl ReviewerCatalog {
    /// The reviewers embedded in the binary (the built-in defaults).
    pub fn embedded() -> Result<Self, ReviewError> {
        let mut reviewers = BTreeMap::new();
        for path in DefaultReviewers::iter() {
            if !path.ends_with("reviewer.yaml") {
                continue;
            }
            let f = DefaultReviewers::get(&path).ok_or_else(|| {
                ReviewError::InvalidManifest(format!("embedded reviewer missing: {path}"))
            })?;
            let m = parse(&path, f.data.as_ref())?;
            reviewers.insert(m.id.clone(), m);
        }
        Ok(ReviewerCatalog { reviewers })
    }

    /// Embedded defaults overlaid by an optional operator directory. Files named
    /// `reviewer.yaml` anywhere under `overlay_dir` add or replace reviewers by id.
    pub fn load(overlay_dir: Option<&Path>) -> Result<Self, ReviewError> {
        let mut cat = Self::embedded()?;
        if let Some(dir) = overlay_dir {
            cat.overlay_dir(dir)?;
        }
        Ok(cat)
    }

    fn overlay_dir(&mut self, dir: &Path) -> Result<(), ReviewError> {
        let mut files = Vec::new();
        collect_manifests(dir, &mut files);
        for path in files {
            let bytes = std::fs::read(&path)
                .map_err(|e| ReviewError::Backend(format!("read {}: {e}", path.display())))?;
            let m = parse(&path.display().to_string(), &bytes)?;
            self.reviewers.insert(m.id.clone(), m);
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&ReviewerManifest> {
        self.reviewers.get(id)
    }

    pub fn list(&self) -> Vec<&ReviewerManifest> {
        self.reviewers.values().collect()
    }

    /// Enabled reviewers that run for a promotion to `target`.
    pub fn enabled_for(&self, target: &str) -> Vec<&ReviewerManifest> {
        self.reviewers
            .values()
            .filter(|m| m.applies_to(target))
            .collect()
    }

    /// Whether any enabled reviewer for `target` is async (runs in the review job).
    pub fn has_async_for(&self, target: &str) -> bool {
        self.reviewers
            .values()
            .any(|m| m.applies_to(target) && m.is_async())
    }
}

fn parse(path: &str, bytes: &[u8]) -> Result<ReviewerManifest, ReviewError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| ReviewError::InvalidManifest(format!("{path}: not utf-8: {e}")))?;
    let m: ReviewerManifest = serde_yaml::from_str(text)
        .map_err(|e| ReviewError::InvalidManifest(format!("{path}: {e}")))?;
    m.validate()?;
    Ok(m)
}

fn collect_manifests(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_manifests(&path, out);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("reviewer.yaml") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_loads_builtin_reviewers_enabled() {
        let cat = ReviewerCatalog::embedded().unwrap();
        let j = cat.get("llm-judge").expect("built-in llm-judge");
        assert_eq!(j.kind, "llm-judge");
        assert!(j.enabled);
        // `auto` resolves to the platform's real model at runtime.
        assert_eq!(j.model, "auto");
        // The deep async reviewer also ships embedded + enabled.
        let cr = cat.get("code-review").expect("built-in code-review");
        assert_eq!(cr.kind, "code-review");
        assert!(cr.is_async());
        // Both built-ins apply to a Light promotion; one of them is async.
        assert_eq!(cat.enabled_for("light-operational").len(), 2);
        assert!(cat.has_async_for("light-operational"));
    }

    #[test]
    fn disabled_overlay_drops_the_reviewer_from_the_panel() {
        let dir = std::env::temp_dir().join(format!("frontkeep-rev-{}", std::process::id()));
        let sub = dir.join("llm-judge");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("reviewer.yaml"),
            b"id: llm-judge\nname: LLM Judge\nkind: llm-judge\nenabled: false\n",
        )
        .unwrap();
        let cat = ReviewerCatalog::load(Some(&dir)).unwrap();
        assert!(!cat.get("llm-judge").unwrap().enabled);
        // The overlay drops llm-judge from the panel; the other built-in remains.
        let enabled = cat.enabled_for("light-operational");
        assert!(enabled.iter().all(|m| m.id != "llm-judge"));
        assert!(enabled.iter().any(|m| m.id == "code-review"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_unknown_kind() {
        let bad = b"id: bad\nname: Bad\nkind: nope\n";
        assert!(parse("bad.yaml", bad).is_err());
    }
}
