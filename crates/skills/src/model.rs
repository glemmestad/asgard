use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// The skill's root instruction file; required in every bundle.
pub const SKILL_FILE: &str = "SKILL.md";
pub const MAX_FILES: usize = 64;
pub const MAX_FILE_BYTES: usize = 512 * 1024;
pub const MAX_BUNDLE_BYTES: usize = 2 * 1024 * 1024;
/// Runtimes a skill can be authored for / exported to.
pub const RUNTIMES: &[&str] = &["claude-code", "codex"];

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("{0}")]
    Bundle(String),
    #[error("{0}")]
    Translate(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Runtime {
    #[default]
    ClaudeCode,
    Codex,
}

impl Runtime {
    pub fn as_str(&self) -> &'static str {
        match self {
            Runtime::ClaudeCode => "claude-code",
            Runtime::Codex => "codex",
        }
    }
    pub fn parse(s: &str) -> Option<Runtime> {
        match s {
            "claude-code" => Some(Runtime::ClaudeCode),
            "codex" => Some(Runtime::Codex),
            _ => None,
        }
    }
}

/// One file in a skill bundle. `content_b64` is the file's bytes, base64-encoded —
/// so the whole bundle serializes to 7-bit-clean JSON and survives a TEXT column
/// identically across SQLite and Postgres.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct SkillFile {
    pub path: String,
    pub content_b64: String,
}

impl SkillFile {
    pub fn from_text(path: impl Into<String>, text: &str) -> SkillFile {
        SkillFile {
            path: path.into(),
            content_b64: encode_b64(text.as_bytes()),
        }
    }
    pub fn from_bytes(path: impl Into<String>, bytes: &[u8]) -> SkillFile {
        SkillFile {
            path: path.into(),
            content_b64: encode_b64(bytes),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct SkillBundle {
    #[serde(default)]
    pub files: Vec<SkillFile>,
}

impl SkillBundle {
    pub fn get(&self, path: &str) -> Option<&SkillFile> {
        self.files.iter().find(|f| f.path == path)
    }
    /// Decoded text of `SKILL.md`, if present and valid UTF-8.
    pub fn skill_md(&self) -> Option<String> {
        let f = self.get(SKILL_FILE)?;
        let bytes = decode_b64(&f.content_b64).ok()?;
        String::from_utf8(bytes).ok()
    }

    /// The bundle decoded to a `path -> bytes` map (for the reviewer / extraction).
    pub fn decoded(&self) -> Result<BTreeMap<String, Vec<u8>>, SkillError> {
        let mut map = BTreeMap::new();
        for f in &self.files {
            map.insert(f.path.clone(), decode_b64(&f.content_b64)?);
        }
        Ok(map)
    }
}

/// Parsed `SKILL.md`: raw frontmatter fields (order not preserved) plus the body.
#[derive(Debug, Clone, Default)]
pub struct SkillManifest {
    pub fields: BTreeMap<String, serde_yaml::Value>,
    pub body: String,
}

pub(crate) fn decode_b64(s: &str) -> Result<Vec<u8>, SkillError> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD
        .decode(cleaned)
        .map_err(|e| SkillError::Bundle(format!("invalid base64 content: {e}")))
}

pub(crate) fn encode_b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn is_safe_path(p: &str) -> bool {
    !(p.is_empty()
        || p.starts_with('/')
        || p.contains('\\')
        || p.split('/').any(|seg| seg == "..")
        || p.chars().any(|c| c.is_control()))
}

/// Split a `---`-fenced YAML frontmatter block off the front of a Markdown doc.
/// Returns `(Some(yaml), body)` when a well-formed block is present, else `(None, whole)`.
pub fn split_frontmatter(s: &str) -> (Option<String>, String) {
    let s = s.strip_prefix('\u{feff}').unwrap_or(s);
    let norm = s.replace("\r\n", "\n");
    let after = match norm.strip_prefix("---\n") {
        Some(rest) => rest,
        None => return (None, s.to_string()),
    };
    let mut pos = 0usize;
    for line in after.split_inclusive('\n') {
        if line.trim_end_matches('\n') == "---" {
            let yaml = after[..pos].to_string();
            let body = after[pos + line.len()..]
                .trim_start_matches('\n')
                .to_string();
            return (Some(yaml), body);
        }
        pos += line.len();
    }
    (None, s.to_string())
}

pub fn parse_manifest(skill_md: &str) -> SkillManifest {
    let (yaml, body) = split_frontmatter(skill_md);
    let fields = yaml
        .as_deref()
        .and_then(|y| serde_yaml::from_str::<serde_yaml::Mapping>(y).ok())
        .map(|m| {
            m.into_iter()
                .filter_map(|(k, v)| k.as_str().map(|s| (s.to_string(), v)))
                .collect()
        })
        .unwrap_or_default();
    SkillManifest { fields, body }
}

/// Frontmatter of the bundle's `SKILL.md` as a JSON object (for the `manifest` column).
pub fn frontmatter_json(skill_md: &str) -> serde_json::Value {
    let (yaml, _) = split_frontmatter(skill_md);
    yaml.as_deref()
        .and_then(|y| serde_yaml::from_str::<serde_yaml::Value>(y).ok())
        .and_then(|v| serde_json::to_value(v).ok())
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()))
}

pub fn validate(bundle: &SkillBundle) -> Result<(), SkillError> {
    if bundle.files.len() > MAX_FILES {
        return Err(SkillError::Bundle(format!(
            "too many files ({} > {MAX_FILES})",
            bundle.files.len()
        )));
    }
    let mut seen = BTreeSet::new();
    let mut total = 0usize;
    let mut has_skill_md = false;
    for f in &bundle.files {
        if !is_safe_path(&f.path) {
            return Err(SkillError::Bundle(format!(
                "unsafe or invalid path '{}'",
                f.path
            )));
        }
        if !seen.insert(f.path.as_str()) {
            return Err(SkillError::Bundle(format!("duplicate path '{}'", f.path)));
        }
        if f.path == SKILL_FILE {
            has_skill_md = true;
        }
        let bytes = decode_b64(&f.content_b64)?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(SkillError::Bundle(format!(
                "file '{}' exceeds {MAX_FILE_BYTES} bytes",
                f.path
            )));
        }
        total += bytes.len();
    }
    if total > MAX_BUNDLE_BYTES {
        return Err(SkillError::Bundle(format!(
            "bundle exceeds {MAX_BUNDLE_BYTES} bytes (total {total})"
        )));
    }
    if !has_skill_md {
        return Err(SkillError::Bundle(format!(
            "bundle must contain a {SKILL_FILE}"
        )));
    }
    Ok(())
}

/// Total decoded content size, in bytes.
pub fn content_bytes(bundle: &SkillBundle) -> Result<i64, SkillError> {
    let mut total = 0i64;
    for f in &bundle.files {
        total += decode_b64(&f.content_b64)?.len() as i64;
    }
    Ok(total)
}

/// The canonical stored form of a bundle.
pub struct Stored {
    /// Canonical JSON (files sorted by path) for the `bundle` TEXT column.
    pub json: String,
    /// sha256 of `json` — change detection / dedupe.
    pub sha256: String,
    /// Total decoded content size.
    pub bytes: i64,
}

/// Validate a bundle and produce its canonical stored form.
pub fn store(bundle: &SkillBundle) -> Result<Stored, SkillError> {
    validate(bundle)?;
    let mut sorted = bundle.clone();
    sorted.files.sort_by(|a, b| a.path.cmp(&b.path));
    let json = serde_json::to_string(&sorted).map_err(|e| SkillError::Bundle(e.to_string()))?;
    let mut h = Sha256::new();
    h.update(json.as_bytes());
    let sha256 = format!("{:x}", h.finalize());
    let bytes = content_bytes(&sorted)?;
    Ok(Stored {
        json,
        sha256,
        bytes,
    })
}

pub fn from_json(s: &str) -> Result<SkillBundle, SkillError> {
    serde_json::from_str(s).map_err(|e| SkillError::Bundle(format!("invalid bundle json: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, text: &str) -> SkillFile {
        SkillFile {
            path: path.into(),
            content_b64: encode_b64(text.as_bytes()),
        }
    }

    #[test]
    fn frontmatter_splits_and_parses() {
        let md = "---\nname: foo\ndescription: bar\n---\nbody line\n";
        let (yaml, body) = split_frontmatter(md);
        assert!(yaml.unwrap().contains("name: foo"));
        assert_eq!(body, "body line\n");
        let m = parse_manifest(md);
        assert_eq!(m.fields.get("name").unwrap().as_str(), Some("foo"));
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let md = "# just markdown\n";
        let (yaml, body) = split_frontmatter(md);
        assert!(yaml.is_none());
        assert_eq!(body, md);
    }

    #[test]
    fn crlf_frontmatter() {
        let md = "---\r\nname: x\r\n---\r\nhello\r\n";
        let (yaml, _) = split_frontmatter(md);
        assert!(yaml.unwrap().contains("name: x"));
    }

    #[test]
    fn validate_ok_and_requires_skill_md() {
        let ok = SkillBundle {
            files: vec![
                file(SKILL_FILE, "---\nname: x\n---\nb"),
                file("scripts/a.py", "print(1)"),
            ],
        };
        assert!(validate(&ok).is_ok());
        let missing = SkillBundle {
            files: vec![file("notes.md", "hi")],
        };
        assert!(validate(&missing).is_err());
    }

    #[test]
    fn validate_rejects_unsafe_paths_and_dupes() {
        for bad in ["/etc/passwd", "../x", "a/../b", "a\\b"] {
            let b = SkillBundle {
                files: vec![file(SKILL_FILE, "x"), file(bad, "y")],
            };
            assert!(validate(&b).is_err(), "{bad} should be rejected");
        }
        let dup = SkillBundle {
            files: vec![file(SKILL_FILE, "x"), file(SKILL_FILE, "y")],
        };
        assert!(validate(&dup).is_err());
    }

    #[test]
    fn validate_enforces_size_caps() {
        let big = "x".repeat(MAX_FILE_BYTES + 1);
        let b = SkillBundle {
            files: vec![file(SKILL_FILE, "x"), file("big.txt", &big)],
        };
        assert!(validate(&b).is_err());
    }

    #[test]
    fn store_roundtrip_and_stable_sha() {
        let a = SkillBundle {
            files: vec![
                file("scripts/a.py", "print(1)"),
                file(SKILL_FILE, "---\nname: x\n---\nb"),
            ],
        };
        let b = SkillBundle {
            files: vec![
                file(SKILL_FILE, "---\nname: x\n---\nb"),
                file("scripts/a.py", "print(1)"),
            ],
        };
        let sa = store(&a).unwrap();
        let sb = store(&b).unwrap();
        assert_eq!(sa.sha256, sb.sha256, "sha must be order-independent");
        let back = from_json(&sa.json).unwrap();
        assert_eq!(back.skill_md().unwrap(), "---\nname: x\n---\nb");
        assert_eq!(back.files.len(), 2);
    }
}
