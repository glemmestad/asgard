//! PAT-authenticated MCP client over Streamable HTTP (`/mcp`). This is the CLI's
//! parity engine: the generic `call` and every typed subcommand reach the server
//! through the exact same tool path the agent-facing MCP uses, authed by the same
//! `fk_pat_…` token. New server tools are reachable here with no CLI change.

use rmcp::model::{CallToolRequestParams, JsonObject};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::Value;

use crate::CliError;

/// One tool result: the parsed JSON value, whether the tool reported an error,
/// and the raw text content (the error message when `is_error`).
pub struct ToolOutput {
    pub value: Value,
    pub is_error: bool,
    pub raw_text: String,
}

/// A tool's advertised name + description (from `tools/list`).
pub struct ToolMeta {
    pub name: String,
    pub description: String,
}

pub struct McpClient {
    endpoint: String,
    pat: String,
}

impl McpClient {
    /// `base` is the server origin (e.g. `https://frontkeep.example`); `/mcp` is
    /// appended. `pat` is the bare `fk_pat_…` token (no `Bearer ` prefix).
    pub fn new(base: impl AsRef<str>, pat: impl Into<String>) -> Self {
        let base = base.as_ref().trim_end_matches('/');
        McpClient {
            endpoint: format!("{base}/mcp"),
            pat: pat.into(),
        }
    }

    /// The transport config (`/mcp` URI + bearer). `from_config` is called at the
    /// use site so the reqwest-backed transport type stays inferred — rmcp vendors
    /// a different reqwest major than the workspace, so we never name it.
    fn config(&self) -> StreamableHttpClientTransportConfig {
        StreamableHttpClientTransportConfig::with_uri(self.endpoint.clone())
            .auth_header(self.pat.clone())
    }

    /// Invoke one tool. Connects (initialize handshake), calls it, tears down.
    pub async fn call(&self, tool: &str, args: JsonObject) -> Result<ToolOutput, CliError> {
        let transport = StreamableHttpClientTransport::from_config(self.config());
        let client = ().serve(transport).await.map_err(map_err)?;
        let mut params = CallToolRequestParams::new(tool.to_string());
        if !args.is_empty() {
            params = params.with_arguments(args);
        }
        let res = client.call_tool(params).await;
        let _ = client.cancel().await;
        let result = res.map_err(map_err)?;
        let raw_text = result
            .content
            .iter()
            .find_map(|c| c.as_text().map(|t| t.text.clone()))
            .unwrap_or_default();
        let value =
            serde_json::from_str(&raw_text).unwrap_or_else(|_| Value::String(raw_text.clone()));
        Ok(ToolOutput {
            value,
            is_error: result.is_error.unwrap_or(false),
            raw_text,
        })
    }

    /// List every tool the server advertises (the live parity surface).
    pub async fn tools(&self) -> Result<Vec<ToolMeta>, CliError> {
        let transport = StreamableHttpClientTransport::from_config(self.config());
        let client = ().serve(transport).await.map_err(map_err)?;
        let res = client.list_all_tools().await;
        let _ = client.cancel().await;
        let tools = res.map_err(map_err)?;
        Ok(tools
            .into_iter()
            .map(|t| ToolMeta {
                name: t.name.to_string(),
                description: t.description.map(|d| d.to_string()).unwrap_or_default(),
            })
            .collect())
    }
}

/// Map an rmcp service/transport error to a `CliError`. A 401 from `/mcp` carries
/// an actionable "mint a PAT" message from the server; surface it as `Auth` so the
/// process exits with the auth code and the hint reaches the user verbatim.
fn map_err<E: std::fmt::Display>(e: E) -> CliError {
    let s = e.to_string();
    let low = s.to_lowercase();
    if low.contains("401") || low.contains("unauthorized") || low.contains("authenticat") {
        CliError::Auth(s)
    } else {
        CliError::Mcp(s)
    }
}

/// Build a tool argument object for the generic `call`, from either a single
/// `--json` blob (or `-` for stdin) or repeated `--arg key=value` pairs. Each
/// `--arg` value is parsed as JSON if it parses, else kept as a string — so
/// `budget_usd=100` is a number, `name=foo` a string, `spec={"size":1}` an object.
pub fn args_from(json: Option<String>, kvs: &[String]) -> Result<JsonObject, CliError> {
    if let Some(j) = json {
        let text = if j == "-" {
            use std::io::Read;
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| CliError::Io(e.to_string()))?;
            s
        } else {
            j
        };
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| CliError::Args(format!("--json is not valid JSON: {e}")))?;
        return match v {
            Value::Object(m) => Ok(m),
            _ => Err(CliError::Args("--json must be a JSON object".into())),
        };
    }
    let mut map = JsonObject::new();
    for kv in kvs {
        let (k, val) = kv
            .split_once('=')
            .ok_or_else(|| CliError::Args(format!("--arg must be key=value, got '{kv}'")))?;
        let parsed =
            serde_json::from_str::<Value>(val).unwrap_or_else(|_| Value::String(val.to_string()));
        map.insert(k.to_string(), parsed);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arg_values_coerce_by_json() {
        let m = args_from(
            None,
            &[
                "budget_usd=100".into(),
                "name=foo".into(),
                "spec={\"size\":1}".into(),
                "active=true".into(),
            ],
        )
        .unwrap();
        assert!(m["budget_usd"].is_number());
        assert_eq!(m["name"], Value::String("foo".into()));
        assert!(m["spec"].is_object());
        assert_eq!(m["active"], Value::Bool(true));
    }

    #[test]
    fn json_blob_must_be_object() {
        assert!(args_from(Some("{\"a\":1}".into()), &[]).is_ok());
        assert!(args_from(Some("[1,2]".into()), &[]).is_err());
        assert!(args_from(Some("not json".into()), &[]).is_err());
    }

    #[test]
    fn arg_without_equals_errors() {
        assert!(args_from(None, &["oops".into()]).is_err());
    }
}
