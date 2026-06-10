//! Read a project's source for review **without cloning it** — over the same
//! GitHub/GitLab HTTP API the catalog ingest uses (auth + tree + raw content),
//! plus a local-path backend for tests and air-gapped review. No git binary, no
//! scratch dir. Read-only: the reviewer judges the code, it never runs it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::ReviewError;

const MAX_FILES: usize = 4000;
const MAX_FILE_BYTES: usize = 80_000;
/// GitLab tree pagination: page size and a hard page cap (bounds API calls on a
/// huge monorepo — `MAX_TREE_PAGES * PER_PAGE` entries scanned at most).
const PER_PAGE: usize = 100;
const MAX_TREE_PAGES: usize = 50;

/// Where (and how) to read a repo's files from.
enum Backend {
    GitHub {
        api_base: String,
        owner: String,
        repo: String,
        git_ref: String,
        token: Option<String>,
    },
    GitLab {
        api_base: String,
        project: String,
        git_ref: String,
        token: Option<String>,
    },
    Local {
        root: PathBuf,
    },
    /// An in-memory file tree (a skill bundle already stored in Frontkeep) — reviewed
    /// the same way as a repo, no fetch.
    Bundle {
        files: BTreeMap<String, Vec<u8>>,
    },
}

pub struct RepoReader {
    backend: Backend,
    client: reqwest::Client,
}

impl RepoReader {
    /// Resolve a `repo_or_source_url` (or local path) to a reader, picking the host
    /// token from `FRONTKEEP_{GITHUB,GITLAB}_TOKEN`, falling back to `FRONTKEEP_GIT_TOKEN`.
    pub fn from_url(url: &str) -> Result<Self, ReviewError> {
        let backend = parse_backend(url)?;
        Ok(RepoReader {
            backend,
            client: reqwest::Client::new(),
        })
    }

    /// A reader over an in-memory file tree (path → bytes), for reviewing a stored
    /// skill bundle without any network fetch.
    pub fn from_bundle(files: BTreeMap<String, Vec<u8>>) -> Self {
        RepoReader {
            backend: Backend::Bundle { files },
            client: reqwest::Client::new(),
        }
    }

    /// Reviewable file paths (blobs), excluding noise (vcs/build/vendor dirs and
    /// obvious binaries), capped at [`MAX_FILES`].
    pub async fn list_files(&self) -> Result<Vec<String>, ReviewError> {
        let mut files = match &self.backend {
            Backend::GitHub { .. } => self.github_tree().await?,
            Backend::GitLab { .. } => self.gitlab_tree().await?,
            Backend::Local { root } => local_tree(root, root),
            Backend::Bundle { files } => files.keys().cloned().collect(),
        };
        files.retain(|p| reviewable(p));
        files.truncate(MAX_FILES);
        Ok(files)
    }

    /// One file's contents (truncated to [`MAX_FILE_BYTES`]).
    pub async fn read_file(&self, path: &str) -> Result<String, ReviewError> {
        if !reviewable(path) {
            return Err(ReviewError::Backend(format!("path not reviewable: {path}")));
        }
        let body = match &self.backend {
            Backend::GitHub { .. } => self.github_raw(path).await?,
            Backend::GitLab { .. } => self.gitlab_raw(path).await?,
            Backend::Local { root } => std::fs::read_to_string(root.join(path))
                .map_err(|e| ReviewError::Backend(format!("read {path}: {e}")))?,
            Backend::Bundle { files } => match files.get(path) {
                Some(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                None => return Err(ReviewError::Backend(format!("file not in bundle: {path}"))),
            },
        };
        Ok(truncate(body))
    }

    async fn github_tree(&self) -> Result<Vec<String>, ReviewError> {
        let Backend::GitHub {
            api_base,
            owner,
            repo,
            git_ref,
            token,
        } = &self.backend
        else {
            unreachable!()
        };
        #[derive(serde::Deserialize)]
        struct Tree {
            tree: Vec<Entry>,
        }
        #[derive(serde::Deserialize)]
        struct Entry {
            path: String,
            #[serde(rename = "type")]
            kind: String,
        }
        let url = format!("{api_base}/repos/{owner}/{repo}/git/trees/{git_ref}?recursive=1");
        let rb = self
            .client
            .get(&url)
            .header("User-Agent", "asgard-reviewer");
        let t: Tree = send_json(gh_auth(rb, token)).await?;
        Ok(t.tree
            .into_iter()
            .filter(|e| e.kind == "blob")
            .map(|e| e.path)
            .collect())
    }

    async fn github_raw(&self, path: &str) -> Result<String, ReviewError> {
        let Backend::GitHub {
            api_base,
            owner,
            repo,
            git_ref,
            token,
        } = &self.backend
        else {
            unreachable!()
        };
        let url = format!("{api_base}/repos/{owner}/{repo}/contents/{path}?ref={git_ref}");
        let rb = self
            .client
            .get(&url)
            .header("User-Agent", "asgard-reviewer")
            .header("Accept", "application/vnd.github.raw");
        send_text(gh_auth(rb, token)).await
    }

    async fn gitlab_tree(&self) -> Result<Vec<String>, ReviewError> {
        let Backend::GitLab {
            api_base,
            project,
            git_ref,
            token,
        } = &self.backend
        else {
            unreachable!()
        };
        #[derive(serde::Deserialize)]
        struct Entry {
            path: String,
            #[serde(rename = "type")]
            kind: String,
        }
        // GitLab paginates the tree (100/page max). Walk pages until a short page or
        // the file cap — a single page would let us review only the first 100 entries
        // of the repo, which produces confidently-wrong "X is missing" findings.
        let mut out = Vec::new();
        for page in 1..=MAX_TREE_PAGES {
            let url = format!(
                "{api_base}/projects/{}/repository/tree?recursive=true&per_page={PER_PAGE}&page={page}&ref={git_ref}",
                enc(project)
            );
            let entries: Vec<Entry> = send_json(gl_auth(self.client.get(&url), token)).await?;
            let got = entries.len();
            out.extend(
                entries
                    .into_iter()
                    .filter(|e| e.kind == "blob")
                    .map(|e| e.path),
            );
            if got < PER_PAGE || out.len() >= MAX_FILES {
                break;
            }
        }
        Ok(out)
    }

    async fn gitlab_raw(&self, path: &str) -> Result<String, ReviewError> {
        let Backend::GitLab {
            api_base,
            project,
            git_ref,
            token,
        } = &self.backend
        else {
            unreachable!()
        };
        let url = format!(
            "{api_base}/projects/{}/repository/files/{}/raw?ref={git_ref}",
            enc(project),
            enc(path)
        );
        send_text(gl_auth(self.client.get(&url), token)).await
    }
}

fn gh_auth(rb: reqwest::RequestBuilder, token: &Option<String>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => rb.header("Authorization", format!("Bearer {t}")),
        None => rb,
    }
}

fn gl_auth(rb: reqwest::RequestBuilder, token: &Option<String>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => rb.header("PRIVATE-TOKEN", t.clone()),
        None => rb,
    }
}

async fn send_json<T: serde::de::DeserializeOwned>(
    rb: reqwest::RequestBuilder,
) -> Result<T, ReviewError> {
    rb.send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| ReviewError::Backend(e.to_string()))?
        .json()
        .await
        .map_err(|e| ReviewError::Backend(e.to_string()))
}

async fn send_text(rb: reqwest::RequestBuilder) -> Result<String, ReviewError> {
    rb.send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| ReviewError::Backend(e.to_string()))?
        .text()
        .await
        .map_err(|e| ReviewError::Backend(e.to_string()))
}

fn token_for(host_env: &str) -> Option<String> {
    std::env::var(host_env)
        .ok()
        .or_else(|| std::env::var("FRONTKEEP_GIT_TOKEN").ok())
        .filter(|t| !t.trim().is_empty())
}

fn parse_backend(url: &str) -> Result<Backend, ReviewError> {
    let url = url.trim();
    // Local path: explicit file://, or an existing filesystem path.
    if let Some(p) = url.strip_prefix("file://") {
        return Ok(Backend::Local { root: p.into() });
    }
    if !url.contains("://") {
        let p = Path::new(url);
        if p.exists() {
            return Ok(Backend::Local { root: p.into() });
        }
    }

    let after = url.split("://").nth(1).unwrap_or(url);
    let mut parts = after.splitn(2, '/');
    let host = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.is_empty() {
        return Err(ReviewError::Backend(format!(
            "unrecognized repo url: {url}"
        )));
    }

    if host == "github.com" || host.starts_with("github.") {
        let mut seg = path.splitn(3, '/');
        let owner = seg.next().unwrap_or_default().to_string();
        let repo = seg.next().unwrap_or_default().to_string();
        if owner.is_empty() || repo.is_empty() {
            return Err(ReviewError::Backend(format!(
                "not an owner/repo url: {url}"
            )));
        }
        let api_base = if host == "github.com" {
            "https://api.github.com".to_string()
        } else {
            format!("https://{host}/api/v3")
        };
        return Ok(Backend::GitHub {
            api_base,
            owner,
            repo,
            git_ref: "HEAD".into(),
            token: token_for("FRONTKEEP_GITHUB_TOKEN"),
        });
    }

    // Treat everything else as GitLab (gitlab.com or self-hosted): the project is
    // the full namespaced path.
    Ok(Backend::GitLab {
        api_base: format!("https://{host}/api/v4"),
        project: path.to_string(),
        git_ref: "HEAD".into(),
        token: token_for("FRONTKEEP_GITLAB_TOKEN"),
    })
}

/// URL-encode a path segment (`/` → `%2F`) for the GitLab API.
fn enc(s: &str) -> String {
    s.replace('/', "%2F")
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_FILE_BYTES {
        s.truncate(MAX_FILE_BYTES);
        s.push_str("\n…[truncated]");
    }
    s
}

fn local_tree(dir: &Path, root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(local_tree(&p, root));
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    out
}

/// Skip version-control, build, vendor noise, and obvious binaries.
fn reviewable(path: &str) -> bool {
    const SKIP_DIRS: &[&str] = &[
        ".git/",
        "target/",
        "node_modules/",
        "dist/",
        "build/",
        "vendor/",
        ".venv/",
        "__pycache__/",
    ];
    const SKIP_EXT: &[&str] = &[
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".ico", ".pdf", ".zip", ".gz", ".tar", ".bin",
        ".so", ".dylib", ".dll", ".a", ".o", ".class", ".jar", ".lock", ".woff", ".woff2", ".ttf",
        ".mp4", ".mov", ".wasm",
    ];
    if SKIP_DIRS
        .iter()
        .any(|d| path.starts_with(d) || path.contains(&format!("/{d}")))
    {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    !SKIP_EXT.iter().any(|e| lower.ends_with(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_backend_lists_and_reads_filtering_noise() {
        let dir = std::env::temp_dir().join(format!("asgard-repo-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("src/main.rs"), b"fn main() {}\n").unwrap();
        std::fs::write(dir.join("README.md"), b"# hi\n").unwrap();
        std::fs::write(dir.join("logo.png"), b"\x89PNG").unwrap();
        std::fs::write(dir.join("target/junk.rs"), b"// built").unwrap();

        let r = RepoReader::from_url(dir.to_str().unwrap()).unwrap();
        let mut files = r.list_files().await.unwrap();
        files.sort();
        assert_eq!(files, vec!["README.md", "src/main.rs"]); // png + target/ filtered
        assert!(r
            .read_file("src/main.rs")
            .await
            .unwrap()
            .contains("fn main"));
        assert!(r.read_file("logo.png").await.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn bundle_backend_lists_and_reads_filtering_noise() {
        let mut files = BTreeMap::new();
        files.insert("SKILL.md".to_string(), b"---\nname: x\n---\nbody".to_vec());
        files.insert("scripts/run.sh".to_string(), b"echo hi".to_vec());
        files.insert("logo.png".to_string(), b"\x89PNG".to_vec());
        let r = RepoReader::from_bundle(files);
        let mut listed = r.list_files().await.unwrap();
        listed.sort();
        assert_eq!(listed, vec!["SKILL.md", "scripts/run.sh"]); // png filtered
        assert!(r
            .read_file("scripts/run.sh")
            .await
            .unwrap()
            .contains("echo hi"));
        assert!(r.read_file("logo.png").await.is_err());
        assert!(r.read_file("nope.md").await.is_err());
    }

    #[test]
    fn url_parsing_picks_backend() {
        assert!(matches!(
            parse_backend("https://github.com/acme/widget.git").unwrap(),
            Backend::GitHub { owner, repo, .. } if owner == "acme" && repo == "widget"
        ));
        assert!(matches!(
            parse_backend("https://gitlab.com/group/sub/proj").unwrap(),
            Backend::GitLab { project, .. } if project == "group/sub/proj"
        ));
        assert!(matches!(
            parse_backend("file:///tmp/x").unwrap(),
            Backend::Local { .. }
        ));
    }
}
