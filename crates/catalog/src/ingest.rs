//! Source providers and manifest discovery. Providers are a trait so Git hosts
//! slot in uniformly; reconciliation (see `reconcile`) is provider-agnostic.

use async_trait::async_trait;
use serde::Deserialize;

use crate::entity::Manifest;
use crate::error::CatalogError;

/// Manifest filenames recognized in a repo (Backstage's `catalog-info.yaml`
/// included for federation).
pub const MANIFEST_NAMES: &[&str] = &[
    "agent.yaml",
    "prompt.yaml",
    "mcp.yaml",
    "eval.yaml",
    "dataset.yaml",
    "project.yaml",
    "catalog-info.yaml",
    "agent.yml",
    "prompt.yml",
    "mcp.yml",
    "eval.yml",
    "dataset.yml",
    "project.yml",
    "catalog-info.yml",
];

pub fn is_manifest_file(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    MANIFEST_NAMES.contains(&base)
}

/// A manifest file fetched from a source, not yet parsed.
#[derive(Debug, Clone)]
pub struct RawManifest {
    pub repo: String,
    pub path: String,
    pub commit: Option<String>,
    pub content: String,
}

#[async_trait]
pub trait SourceProvider: Send + Sync {
    /// Stable id used to attribute entities and scope reconciliation.
    fn source_id(&self) -> String;
    async fn fetch(&self) -> Result<Vec<RawManifest>, CatalogError>;
}

/// Parse a manifest file's content (supports multi-document YAML).
pub fn parse_manifests(content: &str) -> Result<Vec<Manifest>, CatalogError> {
    let mut out = Vec::new();
    for doc in serde_yaml::Deserializer::from_str(content) {
        let value = serde_yaml::Value::deserialize(doc)?;
        if value.is_null() {
            continue;
        }
        let m: Manifest = serde_yaml::from_value(value)?;
        out.push(m);
    }
    Ok(out)
}

/// Reads manifests from a local directory tree. Backs tests and local/Git-clone
/// ingestion without a network round-trip.
pub struct FixtureProvider {
    root: std::path::PathBuf,
    id: String,
}

impl FixtureProvider {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        let root = root.into();
        let id = format!("fixture:{}", root.display());
        FixtureProvider { root, id }
    }

    pub fn with_id(root: impl Into<std::path::PathBuf>, id: impl Into<String>) -> Self {
        FixtureProvider {
            root: root.into(),
            id: id.into(),
        }
    }

    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::walk(&path, out);
            } else if is_manifest_file(&path.to_string_lossy()) {
                out.push(path);
            }
        }
    }
}

#[async_trait]
impl SourceProvider for FixtureProvider {
    fn source_id(&self) -> String {
        self.id.clone()
    }

    async fn fetch(&self) -> Result<Vec<RawManifest>, CatalogError> {
        let mut files = Vec::new();
        Self::walk(&self.root, &mut files);
        let mut out = Vec::new();
        for f in files {
            let content = std::fs::read_to_string(&f)
                .map_err(|e| CatalogError::Http(format!("read {}: {e}", f.display())))?;
            let rel = f
                .strip_prefix(&self.root)
                .unwrap_or(&f)
                .to_string_lossy()
                .to_string();
            out.push(RawManifest {
                repo: self.id.clone(),
                path: rel,
                commit: None,
                content,
            });
        }
        Ok(out)
    }
}

#[derive(Deserialize)]
struct GhTree {
    tree: Vec<GhEntry>,
}
#[derive(Deserialize)]
struct GhEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
}

/// GitHub source provider. Lists the repo tree at a ref and pulls recognized
/// manifest files via the contents API (raw media type, so private repos work).
pub struct GitHubProvider {
    owner: String,
    repo: String,
    git_ref: String,
    token: Option<String>,
    api_base: String,
    client: reqwest::Client,
}

impl GitHubProvider {
    pub fn new(
        owner: impl Into<String>,
        repo: impl Into<String>,
        git_ref: impl Into<String>,
        token: Option<String>,
    ) -> Self {
        GitHubProvider {
            owner: owner.into(),
            repo: repo.into(),
            git_ref: git_ref.into(),
            token,
            api_base: "https://api.github.com".to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let rb = rb.header("User-Agent", "frontkeep-catalog");
        match &self.token {
            Some(t) => rb.header("Authorization", format!("Bearer {t}")),
            None => rb,
        }
    }
}

#[async_trait]
impl SourceProvider for GitHubProvider {
    fn source_id(&self) -> String {
        format!("github:{}/{}@{}", self.owner, self.repo, self.git_ref)
    }

    async fn fetch(&self) -> Result<Vec<RawManifest>, CatalogError> {
        let tree_url = format!(
            "{}/repos/{}/{}/git/trees/{}?recursive=1",
            self.api_base, self.owner, self.repo, self.git_ref
        );
        let tree: GhTree = self
            .auth(self.client.get(&tree_url))
            .send()
            .await
            .map_err(|e| CatalogError::Http(e.to_string()))?
            .error_for_status()
            .map_err(|e| CatalogError::Http(e.to_string()))?
            .json()
            .await
            .map_err(|e| CatalogError::Http(e.to_string()))?;

        let mut out = Vec::new();
        for entry in tree.tree {
            if entry.kind != "blob" || !is_manifest_file(&entry.path) {
                continue;
            }
            let raw_url = format!(
                "{}/repos/{}/{}/contents/{}?ref={}",
                self.api_base, self.owner, self.repo, entry.path, self.git_ref
            );
            let content = self
                .auth(self.client.get(&raw_url))
                .header("Accept", "application/vnd.github.raw")
                .send()
                .await
                .map_err(|e| CatalogError::Http(e.to_string()))?
                .error_for_status()
                .map_err(|e| CatalogError::Http(e.to_string()))?
                .text()
                .await
                .map_err(|e| CatalogError::Http(e.to_string()))?;
            out.push(RawManifest {
                repo: format!("{}/{}", self.owner, self.repo),
                path: entry.path,
                commit: Some(self.git_ref.clone()),
                content,
            });
        }
        Ok(out)
    }
}

#[derive(Deserialize)]
struct GlEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
}

/// GitLab source provider. Single tree page (per_page=100); pagination beyond
/// 100 manifest files is a documented follow-up, not needed for typical repos.
pub struct GitLabProvider {
    project: String,
    git_ref: String,
    token: Option<String>,
    api_base: String,
    client: reqwest::Client,
}

impl GitLabProvider {
    pub fn new(
        project: impl Into<String>,
        git_ref: impl Into<String>,
        token: Option<String>,
    ) -> Self {
        GitLabProvider {
            project: project.into(),
            git_ref: git_ref.into(),
            token,
            api_base: "https://gitlab.com/api/v4".to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into();
        self
    }

    fn project_enc(&self) -> String {
        self.project.replace('/', "%2F")
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => rb.header("PRIVATE-TOKEN", t.clone()),
            None => rb,
        }
    }
}

#[async_trait]
impl SourceProvider for GitLabProvider {
    fn source_id(&self) -> String {
        format!("gitlab:{}@{}", self.project, self.git_ref)
    }

    async fn fetch(&self) -> Result<Vec<RawManifest>, CatalogError> {
        let tree_url = format!(
            "{}/projects/{}/repository/tree?recursive=true&per_page=100&ref={}",
            self.api_base,
            self.project_enc(),
            self.git_ref
        );
        let entries: Vec<GlEntry> = self
            .auth(self.client.get(&tree_url))
            .send()
            .await
            .map_err(|e| CatalogError::Http(e.to_string()))?
            .error_for_status()
            .map_err(|e| CatalogError::Http(e.to_string()))?
            .json()
            .await
            .map_err(|e| CatalogError::Http(e.to_string()))?;

        let mut out = Vec::new();
        for entry in entries {
            if entry.kind != "blob" || !is_manifest_file(&entry.path) {
                continue;
            }
            let file_enc = entry.path.replace('/', "%2F");
            let raw_url = format!(
                "{}/projects/{}/repository/files/{}/raw?ref={}",
                self.api_base,
                self.project_enc(),
                file_enc,
                self.git_ref
            );
            let content = self
                .auth(self.client.get(&raw_url))
                .send()
                .await
                .map_err(|e| CatalogError::Http(e.to_string()))?
                .error_for_status()
                .map_err(|e| CatalogError::Http(e.to_string()))?
                .text()
                .await
                .map_err(|e| CatalogError::Http(e.to_string()))?;
            out.push(RawManifest {
                repo: self.project.clone(),
                path: entry.path,
                commit: Some(self.git_ref.clone()),
                content,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_manifest_files() {
        assert!(is_manifest_file("agent.yaml"));
        assert!(is_manifest_file("path/to/eval.yml"));
        assert!(is_manifest_file("catalog-info.yaml"));
        assert!(!is_manifest_file("README.md"));
        assert!(!is_manifest_file("src/agent.rs"));
    }

    #[test]
    fn parses_multi_doc() {
        let yaml = "apiVersion: frontkeep.dev/v1\nkind: Agent\nmetadata:\n  name: a\nspec:\n  owner: group:default/p\n  model: model:default/m\n---\napiVersion: frontkeep.dev/v1\nkind: Prompt\nmetadata:\n  name: b\nspec:\n  owner: group:default/p\n  template: hi\n";
        let ms = parse_manifests(yaml).unwrap();
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].kind, "Agent");
        assert_eq!(ms[1].kind, "Prompt");
    }
}
