//! Exec source: the open escape hatch for any billing feed. Runs an
//! operator-configured command with `--project <id> --start <s> --end <e>`; the
//! command's stdout must be a bare USD number (or JSON `{"usd": <n>}`). Lets a
//! site attribute cost from a billing system Frontkeep has no native adapter for.

use async_trait::async_trait;
use tokio::process::Command;

use super::CostSource;
use crate::{CostWindow, ProvisionError, ServiceCost};

pub struct ExecCostSource {
    command: Vec<String>,
}

impl ExecCostSource {
    pub fn new(command: Vec<String>) -> Self {
        ExecCostSource { command }
    }
}

#[async_trait]
impl CostSource for ExecCostSource {
    fn name(&self) -> &str {
        "exec"
    }

    async fn cost(
        &self,
        project_id: &str,
        window: &CostWindow,
    ) -> Result<ServiceCost, ProvisionError> {
        let Some((bin, rest)) = self.command.split_first() else {
            return Ok(ServiceCost {
                backend: "exec".into(),
                actual_usd: None,
                source: "exec unconfigured".into(),
            });
        };
        let out = Command::new(bin)
            .args(rest)
            .arg("--project")
            .arg(project_id)
            .arg("--start")
            .arg(&window.start)
            .arg("--end")
            .arg(&window.end)
            .output()
            .await
            .map_err(|e| ProvisionError::Backend(format!("spawn exec cost source: {e}")))?;
        if !out.status.success() {
            return Ok(ServiceCost {
                backend: "exec".into(),
                actual_usd: None,
                source: "exec cost source failed".into(),
            });
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let trimmed = stdout.trim();
        let usd = trimmed.parse::<f64>().ok().or_else(|| {
            serde_json::from_str::<serde_json::Value>(trimmed)
                .ok()
                .and_then(|v| v.get("usd").and_then(|n| n.as_f64()))
        });
        Ok(ServiceCost {
            backend: "exec".into(),
            actual_usd: usd,
            source: "exec".into(),
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parses_usd_from_stdout() {
        let src = ExecCostSource::new(vec!["sh".into(), "-c".into(), "echo 12.50".into()]);
        let w = CostWindow {
            start: "2026-06-01".into(),
            end: "2026-07-01".into(),
        };
        assert_eq!(src.cost("p", &w).await.unwrap().actual_usd, Some(12.50));
    }

    #[tokio::test]
    async fn empty_command_is_unconfigured() {
        let src = ExecCostSource::new(vec![]);
        let w = CostWindow {
            start: "2026-06-01".into(),
            end: "2026-07-01".into(),
        };
        let c = src.cost("p", &w).await.unwrap();
        assert!(c.actual_usd.is_none());
        assert!(c.source.contains("unconfigured"));
    }
}
