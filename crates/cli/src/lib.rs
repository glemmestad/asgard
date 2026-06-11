//! CLI client logic. The control-plane surface is a PAT-authenticated MCP client
//! over `/mcp` (`mcp`), with profile config (`config`), output rendering
//! (`render`), and the inference path (`chat`). The offline helpers below
//! (config scaffold, template scaffold, manifest validation) need no server.
//! The `frontkeep` binary owns the clap tree and dispatches into these.

use std::path::{Path, PathBuf};

use frontkeep_catalog::{Manifest, SchemaRegistry};

pub mod chat;
pub mod config;
pub mod mcp;
pub mod render;
pub mod seed;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("http: {0}")]
    Http(String),
    #[error("mcp: {0}")]
    Mcp(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("server returned {status}: {body}")]
    Server { status: u16, body: String },
    #[error("io: {0}")]
    Io(String),
    #[error("yaml: {0}")]
    Yaml(String),
    #[error("config: {0}")]
    Config(String),
    #[error("bad arguments: {0}")]
    Args(String),
    #[error("validation failed:\n{0}")]
    Invalid(String),
    #[error("unknown template: {0}")]
    UnknownTemplate(String),
}

impl CliError {
    /// Stable process exit code: `3` for auth failures (so scripts can branch on
    /// "need a new PAT"), `1` for everything else. Tool-level errors (the tool ran
    /// but returned an error result) are mapped to `2` by the caller.
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Auth(_) | CliError::Server { status: 401, .. } => 3,
            _ => 1,
        }
    }
}

/// Write a starter `frontkeep.yaml` config into `dir`.
pub fn init_config(dir: &Path) -> Result<PathBuf, CliError> {
    let path = dir.join("frontkeep.yaml");
    let body = "# Frontkeep configuration.\n\
                database_url: sqlite://frontkeep.db\n\
                bind: 0.0.0.0:8080\n\
                # Catalog source repos to reconcile (needs a Git token in FRONTKEEP_GIT_TOKEN).\n\
                sources: []\n\
                #  - provider: github\n\
                #    owner: your-org\n\
                #    repo: your-repo\n\
                #    ref: main\n\
                # Cost-centers a project may register against. Leave empty to accept\n\
                # any group (open mode); list entries to enforce an allowlist.\n\
                groups: []\n\
                #  - key: platform\n\
                #    display_name: Platform Engineering\n\
                #    cost_center: CC-1001\n\
                #  - key: research\n\
                #    display_name: Research\n\
                #    cost_center: CC-2002\n\
                # Provisioning + cost. Credentials come from the environment, never here.\n\
                # provisioning:\n\
                #   rollup_secs: 3600          # cost-rollup cadence (idempotent per day)\n\
                #   forecast_window_days: 60\n\
                #   anomaly_z: 3.0\n\
                #   default_cloud: aws\n\
                #   default_account: \"123456789012\"\n\
                #   allowed:\n\
                #     - { cloud: aws, account: \"123456789012\" }\n\
                #   # Terraform connector: the universal provisioning path. Relative manifest\n\
                #   # `module` paths resolve against modules_dir; the subprocess inherits this\n\
                #   # process's env (AWS_PROFILE/AWS_REGION, AUTH0_* from .env).\n\
                #   terraform:\n\
                #     bin: terraform\n\
                #     modules_dir: /path/to/frontkeep/modules-root\n\
                #     work_dir: /tmp/frontkeep-tf\n\
                #   aws:                         # AWS *cost* reads (provisioning is terraform)\n\
                #     region: us-west-2\n\
                #     profile: my-aws-profile\n\
                #     cost_explorer: true      # live, read-only billing (real spend in the dashboard)\n";
    std::fs::write(&path, body).map_err(|e| CliError::Io(e.to_string()))?;
    Ok(path)
}

const TPL_CODE_REVIEW: &[(&str, &str)] = &[
    (
        "agent.yaml",
        include_str!("../../../templates/code-review/agent.yaml"),
    ),
    (
        "prompt.yaml",
        include_str!("../../../templates/code-review/prompt.yaml"),
    ),
    (
        "eval.yaml",
        include_str!("../../../templates/code-review/eval.yaml"),
    ),
    (
        "cases.json",
        include_str!("../../../templates/code-review/cases.json"),
    ),
    (
        "README.md",
        include_str!("../../../templates/code-review/README.md"),
    ),
];

/// Scaffold a golden-path template into `dir`.
pub fn agent_new(template: &str, dir: &Path) -> Result<Vec<PathBuf>, CliError> {
    let files = match template {
        "code-review" => TPL_CODE_REVIEW,
        other => return Err(CliError::UnknownTemplate(other.to_string())),
    };
    std::fs::create_dir_all(dir).map_err(|e| CliError::Io(e.to_string()))?;
    let mut written = Vec::new();
    for (name, content) in files {
        let p = dir.join(name);
        std::fs::write(&p, content).map_err(|e| CliError::Io(e.to_string()))?;
        written.push(p);
    }
    Ok(written)
}

/// Validate a manifest file against its kind's JSON Schema (offline).
pub fn validate_manifest(path: &Path) -> Result<String, CliError> {
    let content = std::fs::read_to_string(path).map_err(|e| CliError::Io(e.to_string()))?;
    let registry = SchemaRegistry::embedded().map_err(|e| CliError::Yaml(e.to_string()))?;
    let manifests: Vec<Manifest> =
        frontkeep_catalog::parse_manifests(&content).map_err(|e| CliError::Yaml(e.to_string()))?;
    for m in &manifests {
        if registry.known_kind(&m.kind) {
            if let Err(errs) = registry.validate(&m.kind, &m.as_value()) {
                return Err(CliError::Invalid(errs.join("\n")));
            }
        }
    }
    Ok(format!("{} manifest(s) valid", manifests.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffolds_and_validates_template() {
        let dir =
            std::env::temp_dir().join(format!("frontkeep-cli-{}", frontkeep_storage::new_uid()));
        let written = agent_new("code-review", &dir).unwrap();
        assert!(written.iter().any(|p| p.ends_with("agent.yaml")));
        // The scaffolded agent manifest validates against the schema.
        let report = validate_manifest(&dir.join("agent.yaml")).unwrap();
        assert!(report.contains("valid"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_template_errors() {
        let dir = std::env::temp_dir().join("frontkeep-cli-x");
        assert!(matches!(
            agent_new("nope", &dir),
            Err(CliError::UnknownTemplate(_))
        ));
    }

    #[test]
    fn init_writes_config() {
        let dir = std::env::temp_dir().join(format!(
            "frontkeep-cli-init-{}",
            frontkeep_storage::new_uid()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = init_config(&dir).unwrap();
        assert!(p.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
