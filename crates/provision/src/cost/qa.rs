//! The cost Q&A dogfood: an agentic answer to a natural-language spend question,
//! grounded only in the rollup store and routed through Frontkeep's own governed
//! gateway. The model can call thin tools over [`CostRollupRepo`]; it never sees
//! anything but their outputs, so it can't invent numbers.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::{json, Value};

use asgard_gateway::{run_tool_loop, Gateway, GatewayError, ToolDef, ToolExecutor};

use super::report;
use super::rollup::{CostRollupRepo, RollupDim};

const MAX_ROUNDS: usize = 4;

/// Cost tools bound to a rollup store and a reference day (the month under
/// question is `as_of_day`'s month). `budgets` (project → monthly cap) comes from
/// the registration, not the rollup, so budget questions can be answered.
pub struct CostQaTools {
    repo: CostRollupRepo,
    as_of_day: String,
    budgets: HashMap<String, f64>,
    /// The whole cloud bill for the month, if an `account-total` source is wired.
    /// The honest denominator for tagged-% — `None` reports `n/a`, never a
    /// misleading 100%.
    account_total: Option<f64>,
}

impl CostQaTools {
    pub fn new(repo: CostRollupRepo, as_of_day: impl Into<String>) -> Self {
        CostQaTools {
            repo,
            as_of_day: as_of_day.into(),
            budgets: HashMap::new(),
            account_total: None,
        }
    }

    pub fn with_budgets(mut self, budgets: HashMap<String, f64>) -> Self {
        self.budgets = budgets;
        self
    }

    pub fn with_account_total(mut self, account_total: Option<f64>) -> Self {
        self.account_total = account_total;
        self
    }
}

#[async_trait]
impl ToolExecutor for CostQaTools {
    fn tools(&self) -> Vec<ToolDef> {
        [
            ("project_spend", "Month-to-date spend for one project. args: {\"project_id\"}"),
            ("org_spend", "Spend grouped by a dimension (project|owner|manager|group|cost_center|classification|service). args: {\"dimension\"}"),
            ("top_movers", "Biggest movers this month vs last, by project and group. args: {}"),
            ("forecast", "Latest end-of-month forecast for one project. args: {\"project_id\"}"),
            ("untagged", "Tagged spend vs the cloud account total (tagged-%). args: {}"),
            ("anomalies", "Recent cost anomalies (a day's spend far from a source's trailing norm). args: {}"),
            ("budget_status", "Every project's MTD spend, EOM forecast, monthly budget cap, and whether it is projected over budget. args: {}"),
        ]
        .into_iter()
        .map(|(name, description)| ToolDef {
            name: name.into(),
            description: description.into(),
        })
        .collect()
    }

    async fn call(&self, name: &str, args: &Value) -> Result<String, String> {
        let month_start = crate::month_start_str(&self.as_of_day);
        let arg = |k: &str| {
            args.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let out = match name {
            "project_spend" => {
                let pid = arg("project_id");
                let facts = self
                    .repo
                    .project_facts(&month_start, &self.as_of_day)
                    .await
                    .map_err(|e| e.to_string())?;
                let mtd = facts
                    .iter()
                    .find(|f| f.project_id == pid)
                    .map(|f| f.mtd_usd)
                    .unwrap_or(0.0);
                json!({ "project_id": pid, "mtd_usd": mtd })
            }
            "org_spend" => {
                let dim = RollupDim::parse(&arg("dimension")).unwrap_or(RollupDim::Project);
                let rows = self
                    .repo
                    .by_dimension(dim, &month_start, &self.as_of_day)
                    .await
                    .map_err(|e| e.to_string())?;
                json!({ "by": dim.as_str(), "rows": rows })
            }
            "top_movers" => {
                let prev_from = crate::month_start_str(&crate::minus_days(&month_start, 1));
                let prev_to = crate::minus_days(&month_start, 1);
                let current = self
                    .repo
                    .project_facts(&month_start, &self.as_of_day)
                    .await
                    .map_err(|e| e.to_string())?;
                let previous = self
                    .repo
                    .project_facts(&prev_from, &prev_to)
                    .await
                    .map_err(|e| e.to_string())?;
                serde_json::to_value(report::movers(&current, &previous, 5)).unwrap_or(Value::Null)
            }
            "forecast" => {
                let pid = arg("project_id");
                match self
                    .repo
                    .latest_forecast(&pid)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    Some(f) => serde_json::to_value(f).unwrap_or(Value::Null),
                    None => json!({ "project_id": pid, "forecast": "insufficient history" }),
                }
            }
            "untagged" => {
                let facts = self
                    .repo
                    .project_facts(&month_start, &self.as_of_day)
                    .await
                    .map_err(|e| e.to_string())?;
                let tagged: f64 = facts.iter().map(|f| f.mtd_usd).sum();
                serde_json::to_value(report::tagged_report(tagged, self.account_total))
                    .unwrap_or(Value::Null)
            }
            "anomalies" => {
                let rows = self
                    .repo
                    .anomalies(None, 20)
                    .await
                    .map_err(|e| e.to_string())?;
                json!({ "anomalies": rows })
            }
            "budget_status" => {
                let facts = self
                    .repo
                    .project_facts(&month_start, &self.as_of_day)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut rows = Vec::new();
                for f in &facts {
                    let eom = self
                        .repo
                        .latest_forecast(&f.project_id)
                        .await
                        .map_err(|e| e.to_string())?
                        .map(|fc| fc.eom_usd);
                    let budget = self.budgets.get(&f.project_id).copied().unwrap_or(0.0);
                    let over = budget > 0.0 && eom.map(|e| e > budget * 0.9).unwrap_or(false);
                    rows.push(json!({
                        "project_id": f.project_id, "mtd_usd": f.mtd_usd,
                        "eom_forecast_usd": eom, "budget_usd": budget, "over_budget": over,
                    }));
                }
                json!({ "projects": rows })
            }
            other => return Err(format!("unknown tool: {other}")),
        };
        Ok(out.to_string())
    }
}

fn money(n: f64) -> String {
    let a = n.abs();
    if n == 0.0 {
        "$0".into()
    } else if a >= 1000.0 {
        format!("${:.1}k", n / 1000.0)
    } else {
        format!("${:.0}", n)
    }
}

impl CostQaTools {
    /// A deterministic, grounded spend briefing assembled straight from the rollup
    /// tools — used when no reasoning model is configured (the mock provider).
    pub async fn briefing(&self) -> String {
        let ms = crate::month_start_str(&self.as_of_day);
        let groups = self
            .repo
            .by_dimension(RollupDim::Group, &ms, &self.as_of_day)
            .await
            .unwrap_or_default();
        let total: f64 = groups.iter().map(|g| g.actual_usd).sum();
        let cur = self
            .repo
            .project_facts(&ms, &self.as_of_day)
            .await
            .unwrap_or_default();
        let prev_from = crate::month_start_str(&crate::minus_days(&ms, 1));
        let prev_to = crate::minus_days(&ms, 1);
        let prev = self
            .repo
            .project_facts(&prev_from, &prev_to)
            .await
            .unwrap_or_default();
        let movers = report::movers(&cur, &prev, 1);
        let anoms = self.repo.anomalies(None, 50).await.unwrap_or_default();

        let mut s = format!(
            "Month-to-date spend is {} across {} project(s).",
            money(total),
            cur.len()
        );
        if let Some(g) = groups
            .iter()
            .max_by(|a, b| a.actual_usd.total_cmp(&b.actual_usd))
        {
            s.push_str(&format!(
                " Highest group is {} at {}.",
                g.key,
                money(g.actual_usd)
            ));
        }
        if let Some(m) = movers.by_project.first() {
            s.push_str(&format!(
                " Biggest mover vs last month: {} ({:+.0}%, {}).",
                m.key,
                m.delta_pct,
                money(m.delta_usd)
            ));
        }
        s.push_str(&match anoms.len() {
            0 => " No cost anomalies are open.".to_string(),
            n => format!(" {n} cost anomaly(ies) are open."),
        });
        let mut over = 0usize;
        for f in &cur {
            let budget = self.budgets.get(&f.project_id).copied().unwrap_or(0.0);
            if budget <= 0.0 {
                continue;
            }
            if let Ok(Some(fc)) = self.repo.latest_forecast(&f.project_id).await {
                if fc.eom_usd > budget * 0.9 {
                    over += 1;
                }
            }
        }
        if over > 0 {
            s.push_str(&format!(" {over} project(s) projected over budget."));
        }
        s.push_str(" (Deterministic briefing — wire a reasoning model for free-form Q&A.)");
        s
    }
}

/// Answer a cost question through the governed gateway, grounded in the rollup
/// store. `virtual_key` attributes the meta-cost like any other gateway call.
#[allow(clippy::too_many_arguments)]
pub async fn answer_cost_question(
    gateway: &Gateway,
    repo: CostRollupRepo,
    virtual_key: &str,
    model: &str,
    data_class: Option<String>,
    as_of_day: &str,
    question: &str,
    budgets: HashMap<String, f64>,
    account_total: Option<f64>,
) -> Result<String, GatewayError> {
    let tools = CostQaTools::new(repo, as_of_day.to_string())
        .with_budgets(budgets)
        .with_account_total(account_total);
    // The deterministic mock provider can't reason over tool calls, so out of the
    // box (no real model wired) we answer with a grounded briefing built directly
    // from the tools. Real providers run the genuine tool-calling loop below.
    if model.contains("mock") {
        return Ok(tools.briefing().await);
    }
    let grounding = format!(
        "You are Frontkeep's cost assistant. Today is {as_of_day}. Answer questions about \
         cloud infrastructure and model spend strictly from the cost tools."
    );
    run_tool_loop(
        gateway,
        virtual_key,
        model,
        data_class,
        &grounding,
        question,
        &tools,
        MAX_ROUNDS,
        None,
    )
    .await
    .map(|o| o.answer)
}

#[cfg(test)]
mod tests {
    use super::super::rollup::RollupRow;
    use super::*;
    use asgard_storage::Db;

    async fn seeded() -> CostQaTools {
        let path = std::env::temp_dir().join(format!("asgard-qa-{}.db", asgard_storage::new_uid()));
        let db = Db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        db.migrate().await.unwrap();
        let repo = CostRollupRepo::new(db);
        repo.upsert_daily(&RollupRow {
            project_id: "proj-2026-0001".into(),
            day: "2026-06-10".into(),
            service: "gateway".into(),
            source: "gateway".into(),
            estimated_usd: 0.0,
            actual_usd: Some(3.5),
            cumulative_usd: Some(3.5),
            owner: "o@x".into(),
            manager: "m@x".into(),
            cost_group: "platform".into(),
            cost_center: "CC-100".into(),
            classification: "poc".into(),
        })
        .await
        .unwrap();
        CostQaTools::new(repo, "2026-06-15")
    }

    #[tokio::test]
    async fn tools_read_grounded_facts() {
        let t = seeded().await;
        let spend = t
            .call("project_spend", &json!({"project_id": "proj-2026-0001"}))
            .await
            .unwrap();
        assert!(spend.contains("3.5"), "got {spend}");
        let by = t
            .call("org_spend", &json!({"dimension": "group"}))
            .await
            .unwrap();
        assert!(by.contains("platform"));
        // Tagged-% has no denominator wired → honest n/a, never 100.
        let tagged = t.call("untagged", &json!({})).await.unwrap();
        assert!(tagged.contains("\"tagged_pct\":null"), "got {tagged}");
    }

    #[tokio::test]
    async fn untagged_uses_the_wired_account_total_denominator() {
        // With a real account-total the NL Q&A reports a genuine tagged-% instead
        // of n/a — the "governed share of spend" headline.
        let t = seeded().await.with_account_total(Some(10.0));
        let tagged = t.call("untagged", &json!({})).await.unwrap();
        assert!(
            tagged.contains("\"account_total_usd\":10.0"),
            "got {tagged}"
        );
        assert!(!tagged.contains("\"tagged_pct\":null"), "got {tagged}");
    }
}
