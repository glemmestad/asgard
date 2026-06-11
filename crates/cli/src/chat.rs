//! `frontkeep chat` — real inference, which the control-plane MCP deliberately does
//! not do (it only mints credentials). Mint, or reuse a cached, per-project
//! gateway virtual key via the `gateway_credential` tool, then call the gateway's
//! chat endpoint with it. The key is cached per profile+project and silently
//! re-minted if the server has killed/rotated it.

use serde_json::{json, Value};

use crate::config;
use crate::mcp::McpClient;
use crate::CliError;

pub struct ChatRequest<'a> {
    pub url: &'a str,
    pub pat: &'a str,
    pub profile: &'a str,
    pub project: &'a str,
    pub model: String,
    pub message: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub data_class: Option<String>,
}

pub async fn chat(req: ChatRequest<'_>) -> Result<Value, CliError> {
    let http = reqwest::Client::new();
    if let Some(key) = cached_key(req.profile, req.project) {
        match post_chat(&http, &req, &key).await {
            Ok(v) => return Ok(v),
            // Cached key killed/rotated server-side — fall through and re-mint.
            Err(CliError::Server { status, .. }) if status == 401 || status == 403 => {}
            Err(e) => return Err(e),
        }
    }
    let key = mint_key(&req).await?;
    cache_key(req.profile, req.project, &key);
    post_chat(&http, &req, &key).await
}

async fn mint_key(req: &ChatRequest<'_>) -> Result<String, CliError> {
    let mut args = rmcp::model::JsonObject::new();
    args.insert("project_id".into(), json!(req.project));
    let out = McpClient::new(req.url, req.pat)
        .call("gateway_credential", args)
        .await?;
    if out.is_error {
        return Err(CliError::Mcp(out.raw_text));
    }
    out.value
        .get("key")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            CliError::Mcp(format!(
                "gateway_credential returned no key: {}",
                out.raw_text
            ))
        })
}

async fn post_chat(
    http: &reqwest::Client,
    req: &ChatRequest<'_>,
    key: &str,
) -> Result<Value, CliError> {
    let mut body = json!({
        "model": req.model,
        "messages": [{ "role": "user", "content": req.message }],
    });
    if let Some(mt) = req.max_tokens {
        body["max_tokens"] = json!(mt);
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(dc) = &req.data_class {
        body["data_class"] = json!(dc);
    }
    let url = format!("{}/api/gateway/chat", req.url.trim_end_matches('/'));
    let resp = http
        .post(&url)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|e| CliError::Http(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(CliError::Server {
            status: status.as_u16(),
            body: resp.text().await.unwrap_or_default(),
        });
    }
    resp.json::<Value>()
        .await
        .map_err(|e| CliError::Http(e.to_string()))
}

fn key_path(profile: &str, project: &str) -> std::path::PathBuf {
    config::keys_dir().join(format!("{}-{}.key", sanitize(profile), sanitize(project)))
}

fn cached_key(profile: &str, project: &str) -> Option<String> {
    std::fs::read_to_string(key_path(profile, project))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn cache_key(profile: &str, project: &str, key: &str) {
    let path = key_path(profile, project);
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if std::fs::write(&path, key).is_ok() {
        config::set_mode_600(&path);
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
