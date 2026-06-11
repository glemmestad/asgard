//! Eval runner: evals are CI for non-determinism (brief §4.4). A suite runs cases
//! through a `Responder` (the gateway, or a mock), scores them, and produces a
//! verdict against thresholds. Verdicts render as PR comments and gate merges.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use frontkeep_storage::Db;

#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("responder error: {0}")]
    Responder(String),
    #[error("db: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("bad regex: {0}")]
    Regex(String),
}

/// The system under test. The gateway implements this for real runs; tests use a mock.
#[async_trait]
pub trait Responder: Send + Sync {
    async fn respond(&self, input: &str) -> Result<String, EvalError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Scorer {
    /// Trimmed output equals expected.
    ExactMatch,
    /// Output contains expected.
    Contains,
    /// Output matches the expected string as a regex.
    Regex,
}

impl Scorer {
    pub fn score(&self, output: &str, expected: Option<&str>) -> Result<f64, EvalError> {
        let expected = expected.unwrap_or("");
        let hit = match self {
            Scorer::ExactMatch => output.trim() == expected.trim(),
            Scorer::Contains => output.contains(expected),
            Scorer::Regex => regex::Regex::new(expected)
                .map_err(|e| EvalError::Regex(e.to_string()))?
                .is_match(output),
        };
        Ok(if hit { 1.0 } else { 0.0 })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Case {
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Thresholds {
    pub min_pass_rate: f64,
    pub min_avg_score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalSuite {
    pub eval_ref: String,
    pub scorer: Scorer,
    pub thresholds: Thresholds,
    pub cases: Vec<Case>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseResult {
    pub input: String,
    pub output: String,
    pub score: f64,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalRun {
    pub eval_ref: String,
    pub pass_rate: f64,
    pub avg_score: f64,
    pub verdict: Verdict,
    pub cases: Vec<CaseResult>,
}

impl EvalRun {
    /// Merge gate: true means safe to merge.
    pub fn gate_pass(&self) -> bool {
        self.verdict == Verdict::Pass
    }

    /// PR-comment markdown.
    pub fn to_markdown(&self) -> String {
        let icon = if self.verdict == Verdict::Pass {
            "✅"
        } else {
            "❌"
        };
        let mut s = format!(
            "### {icon} Eval `{}` — **{}**\n\n| metric | value |\n|---|---|\n| pass rate | {:.0}% |\n| avg score | {:.2} |\n| cases | {} |\n",
            self.eval_ref,
            self.verdict.as_str().to_uppercase(),
            self.pass_rate * 100.0,
            self.avg_score,
            self.cases.len(),
        );
        let failed: Vec<&CaseResult> = self.cases.iter().filter(|c| !c.passed).collect();
        if !failed.is_empty() {
            s.push_str("\n<details><summary>Failing cases</summary>\n\n");
            for c in failed.iter().take(20) {
                s.push_str(&format!("- input: `{}` → score {:.2}\n", c.input, c.score));
            }
            s.push_str("\n</details>\n");
        }
        s
    }
}

/// Returns a message if `proposed` weakens any threshold versus `baseline`
/// (used to merge-block PRs that quietly lower eval bars — brief §7).
pub fn threshold_regression(baseline: Thresholds, proposed: Thresholds) -> Option<String> {
    let mut msgs = Vec::new();
    if proposed.min_pass_rate < baseline.min_pass_rate {
        msgs.push(format!(
            "min_pass_rate lowered {:.2} → {:.2}",
            baseline.min_pass_rate, proposed.min_pass_rate
        ));
    }
    if proposed.min_avg_score < baseline.min_avg_score {
        msgs.push(format!(
            "min_avg_score lowered {:.2} → {:.2}",
            baseline.min_avg_score, proposed.min_avg_score
        ));
    }
    (!msgs.is_empty()).then(|| msgs.join("; "))
}

pub struct EvalRunner {
    db: Db,
}

impl EvalRunner {
    pub fn new(db: Db) -> Self {
        EvalRunner { db }
    }

    pub async fn run(
        &self,
        suite: &EvalSuite,
        responder: &dyn Responder,
    ) -> Result<EvalRun, EvalError> {
        let mut cases = Vec::with_capacity(suite.cases.len());
        let mut total = 0.0;
        for case in &suite.cases {
            let output = responder.respond(&case.input).await?;
            let score = suite.scorer.score(&output, case.expected.as_deref())?;
            total += score;
            cases.push(CaseResult {
                input: case.input.clone(),
                output,
                score,
                passed: score >= 1.0,
            });
        }
        let n = suite.cases.len().max(1) as f64;
        let avg_score = total / n;
        let pass_rate = cases.iter().filter(|c| c.passed).count() as f64 / n;
        let verdict = if pass_rate >= suite.thresholds.min_pass_rate
            && avg_score >= suite.thresholds.min_avg_score
        {
            Verdict::Pass
        } else {
            Verdict::Fail
        };
        let run = EvalRun {
            eval_ref: suite.eval_ref.clone(),
            pass_rate,
            avg_score,
            verdict,
            cases,
        };
        self.record(&run).await?;
        Ok(run)
    }

    async fn record(&self, run: &EvalRun) -> Result<(), EvalError> {
        sqlx::query(&self.db.q(
            "INSERT INTO eval_runs (id, eval_ref, ts, pass_rate, avg_score, verdict, detail) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        ))
        .bind(frontkeep_storage::new_uid())
        .bind(&run.eval_ref)
        .bind(frontkeep_storage::now())
        .bind(run.pass_rate)
        .bind(run.avg_score)
        .bind(run.verdict.as_str())
        .bind(serde_json::to_string(&run.cases).unwrap_or_default())
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    pub async fn history(
        &self,
        eval_ref: &str,
        limit: i64,
    ) -> Result<Vec<(String, f64, String)>, EvalError> {
        let rows = sqlx::query(&self.db.q(
            "SELECT ts, pass_rate, verdict FROM eval_runs WHERE eval_ref = ? ORDER BY ts DESC LIMIT ?",
        ))
        .bind(eval_ref)
        .bind(limit)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("ts"),
                    r.get::<f64, _>("pass_rate"),
                    r.get::<String, _>("verdict"),
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MockResponder {
        answers: HashMap<String, String>,
    }
    #[async_trait]
    impl Responder for MockResponder {
        async fn respond(&self, input: &str) -> Result<String, EvalError> {
            Ok(self.answers.get(input).cloned().unwrap_or_default())
        }
    }

    async fn runner() -> EvalRunner {
        let path =
            std::env::temp_dir().join(format!("frontkeep-ev-{}.db", frontkeep_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        EvalRunner::new(db)
    }

    fn suite(min_pass: f64) -> EvalSuite {
        EvalSuite {
            eval_ref: "eval:default/greeting".into(),
            scorer: Scorer::Contains,
            thresholds: Thresholds {
                min_pass_rate: min_pass,
                min_avg_score: 0.5,
            },
            cases: vec![
                Case {
                    input: "hi".into(),
                    expected: Some("hello".into()),
                },
                Case {
                    input: "bye".into(),
                    expected: Some("goodbye".into()),
                },
            ],
        }
    }

    #[tokio::test]
    async fn passing_suite_gates_open() {
        let r = runner().await;
        let mut answers = HashMap::new();
        answers.insert("hi".to_string(), "well hello there".to_string());
        answers.insert("bye".to_string(), "ok goodbye now".to_string());
        let run = r
            .run(&suite(1.0), &MockResponder { answers })
            .await
            .unwrap();
        assert_eq!(run.verdict, Verdict::Pass);
        assert!(run.gate_pass());
        assert!((run.pass_rate - 1.0).abs() < 1e-9);
        assert!(run.to_markdown().contains("PASS"));
    }

    #[tokio::test]
    async fn failing_suite_blocks_merge() {
        let r = runner().await;
        let mut answers = HashMap::new();
        answers.insert("hi".to_string(), "nope".to_string());
        answers.insert("bye".to_string(), "ok goodbye".to_string());
        let run = r
            .run(&suite(1.0), &MockResponder { answers })
            .await
            .unwrap();
        assert_eq!(run.verdict, Verdict::Fail);
        assert!(!run.gate_pass());
        let hist = r.history("eval:default/greeting", 10).await.unwrap();
        assert_eq!(hist.len(), 1);
    }

    #[test]
    fn threshold_lowering_detected() {
        let base = Thresholds {
            min_pass_rate: 0.9,
            min_avg_score: 0.8,
        };
        let lowered = Thresholds {
            min_pass_rate: 0.7,
            min_avg_score: 0.8,
        };
        assert!(threshold_regression(base, lowered).is_some());
        assert!(threshold_regression(base, base).is_none());
    }
}
