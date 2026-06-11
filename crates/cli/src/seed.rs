//! `frontkeep seed apply` / `bootstrap --write`: take the `bootstrap` tool's output
//! (`{ files: [{path, body}] }`) and write each file to disk under a destination
//! directory. The MCP only returns the bodies and tells the agent to write them;
//! the CLI is what actually creates the repo — the biggest ergonomic win over the
//! agent surface.

use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::STANDARD, Engine};
use frontkeep_skills::SkillFile;
use serde_json::Value;

use crate::CliError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Wrote,
    Skipped,
    Would,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Wrote => "wrote",
            Action::Skipped => "skipped (exists; use --force)",
            Action::Would => "would write",
        }
    }
}

/// Apply a `bootstrap` result to disk under `dest`. With `write == false` it's a
/// dry run (reports what *would* be written). Existing files are skipped unless
/// `force`.
pub fn apply(
    value: &Value,
    dest: &Path,
    write: bool,
    force: bool,
) -> Result<Vec<(PathBuf, Action)>, CliError> {
    let files = value
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| CliError::Mcp("bootstrap output has no `files` array".into()))?;
    let mut results = Vec::new();
    for f in files {
        let rel = file_path(f)?;
        let body = f.get("body").and_then(Value::as_str).unwrap_or("");
        write_entry(dest, rel, body.as_bytes(), write, force, &mut results)?;
    }
    Ok(results)
}

/// Write a skill bundle from the export shape (`[{path, content_b64}]`) to disk.
pub fn apply_b64(
    value: &Value,
    dest: &Path,
    write: bool,
    force: bool,
) -> Result<Vec<(PathBuf, Action)>, CliError> {
    let files = file_array(value)?;
    let mut results = Vec::new();
    for f in files {
        let rel = file_path(f)?;
        let b64 = f.get("content_b64").and_then(Value::as_str).unwrap_or("");
        let bytes = STANDARD
            .decode(b64)
            .map_err(|e| CliError::Mcp(format!("invalid base64 in '{rel}': {e}")))?;
        write_entry(dest, rel, &bytes, write, force, &mut results)?;
    }
    Ok(results)
}

/// Write a skill bundle from the install shape (`[{path, content, encoding}]`) to
/// disk. `encoding` is `utf-8` (text in `content`) or `base64` (decode `content`).
pub fn apply_install(
    value: &Value,
    dest: &Path,
    write: bool,
    force: bool,
) -> Result<Vec<(PathBuf, Action)>, CliError> {
    let files = file_array(value)?;
    let mut results = Vec::new();
    for f in files {
        let rel = file_path(f)?;
        let content = f.get("content").and_then(Value::as_str).unwrap_or("");
        let bytes = match f.get("encoding").and_then(Value::as_str) {
            Some("base64") => STANDARD
                .decode(content)
                .map_err(|e| CliError::Mcp(format!("invalid base64 in '{rel}': {e}")))?,
            _ => content.as_bytes().to_vec(),
        };
        write_entry(dest, rel, &bytes, write, force, &mut results)?;
    }
    Ok(results)
}

/// Walk `dir` recursively and base64-encode every file into a `SkillFile` bundle
/// JSON array (`[{path, content_b64}]`), each `path` relative to `dir` and
/// `/`-separated. Paths/contents are validated server-side on publish.
pub fn dir_to_bundle(dir: &Path) -> Result<Value, CliError> {
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files)?;
    if files.is_empty() {
        return Err(CliError::Io(format!(
            "no files found under {}",
            dir.display()
        )));
    }
    serde_json::to_value(files).map_err(|e| CliError::Mcp(e.to_string()))
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<SkillFile>) -> Result<(), CliError> {
    for entry in std::fs::read_dir(dir).map_err(|e| CliError::Io(e.to_string()))? {
        let path = entry.map_err(|e| CliError::Io(e.to_string()))?.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let bytes = std::fs::read(&path).map_err(|e| CliError::Io(e.to_string()))?;
            out.push(SkillFile::from_bytes(rel, &bytes));
        }
    }
    Ok(())
}

fn file_array(value: &Value) -> Result<&Vec<Value>, CliError> {
    value
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| CliError::Mcp("tool output has no `files` array".into()))
}

fn file_path(f: &Value) -> Result<&str, CliError> {
    f.get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::Mcp("file entry is missing `path`".into()))
}

fn write_entry(
    dest: &Path,
    rel: &str,
    bytes: &[u8],
    write: bool,
    force: bool,
    results: &mut Vec<(PathBuf, Action)>,
) -> Result<(), CliError> {
    let path = dest.join(rel);
    if !write {
        results.push((path, Action::Would));
        return Ok(());
    }
    if path.exists() && !force {
        results.push((path, Action::Skipped));
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| CliError::Io(e.to_string()))?;
    }
    std::fs::write(&path, bytes).map_err(|e| CliError::Io(e.to_string()))?;
    results.push((path, Action::Wrote));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn writes_files_and_respects_dry_run_and_force() {
        let dir =
            std::env::temp_dir().join(format!("frontkeep-seed-{}", frontkeep_storage::new_uid()));
        let plan = json!({
            "files": [
                {"path": "AGENTS.md", "body": "hello"},
                {"path": ".agent/RUST.md", "body": "rust"},
            ]
        });

        // Dry run writes nothing.
        let res = apply(&plan, &dir, false, false).unwrap();
        assert!(res.iter().all(|(_, a)| *a == Action::Would));
        assert!(!dir.join("AGENTS.md").exists());

        // Write creates files (including nested dirs).
        let res = apply(&plan, &dir, true, false).unwrap();
        assert!(res.iter().all(|(_, a)| *a == Action::Wrote));
        assert_eq!(
            std::fs::read_to_string(dir.join("AGENTS.md")).unwrap(),
            "hello"
        );
        assert!(dir.join(".agent/RUST.md").exists());

        // Re-applying without force skips existing files.
        let res = apply(&plan, &dir, true, false).unwrap();
        assert!(res.iter().all(|(_, a)| *a == Action::Skipped));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_files_array_errors() {
        assert!(apply(&json!({"x": 1}), Path::new("."), false, false).is_err());
    }

    #[test]
    fn apply_b64_decodes_and_writes() {
        let dir =
            std::env::temp_dir().join(format!("frontkeep-skb64-{}", frontkeep_storage::new_uid()));
        let value = json!({
            "files": [
                {"path": "SKILL.md", "content_b64": STANDARD.encode(b"hello")},
                {"path": "scripts/run.sh", "content_b64": STANDARD.encode(b"echo hi")},
            ]
        });
        let res = apply_b64(&value, &dir, true, false).unwrap();
        assert!(res.iter().all(|(_, a)| *a == Action::Wrote));
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            "hello"
        );
        assert!(dir.join("scripts/run.sh").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_install_handles_both_encodings() {
        let dir =
            std::env::temp_dir().join(format!("frontkeep-skin-{}", frontkeep_storage::new_uid()));
        let binary = [0u8, 159, 146, 150]; // not valid UTF-8
        let value = json!({
            "files": [
                {"path": "SKILL.md", "content": "plain", "encoding": "utf-8"},
                {"path": "logo.bin", "content": STANDARD.encode(binary), "encoding": "base64"},
            ]
        });
        let res = apply_install(&value, &dir, true, false).unwrap();
        assert!(res.iter().all(|(_, a)| *a == Action::Wrote));
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            "plain"
        );
        assert_eq!(std::fs::read(dir.join("logo.bin")).unwrap(), binary);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dir_to_bundle_walks_nested_and_binary() {
        let dir =
            std::env::temp_dir().join(format!("frontkeep-skdir-{}", frontkeep_storage::new_uid()));
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: t\n---\n").unwrap();
        let binary = [0u8, 159, 146, 150];
        std::fs::write(dir.join("scripts/bin"), binary).unwrap();

        let bundle = dir_to_bundle(&dir).unwrap();
        let files = bundle.as_array().unwrap();
        assert_eq!(files.len(), 2);
        let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
        assert!(paths.contains(&"SKILL.md"));
        assert!(paths.contains(&"scripts/bin"));

        // Round-trips the binary file back to disk through the export writer.
        let out =
            std::env::temp_dir().join(format!("frontkeep-skout-{}", frontkeep_storage::new_uid()));
        apply_b64(&json!({ "files": bundle }), &out, true, false).unwrap();
        assert_eq!(std::fs::read(out.join("scripts/bin")).unwrap(), binary);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn dir_to_bundle_errors_on_empty_dir() {
        let dir =
            std::env::temp_dir().join(format!("frontkeep-skmt-{}", frontkeep_storage::new_uid()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(dir_to_bundle(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
