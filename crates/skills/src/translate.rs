use crate::model::{
    decode_b64, encode_b64, split_frontmatter, Runtime, SkillBundle, SkillError, SkillFile,
    SKILL_FILE,
};
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

/// What happens to a Claude Code frontmatter field when targeting Codex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Shared field — carried verbatim.
    Portable,
    /// No Codex field; its text is appended to `description`.
    FoldIntoDescription,
    /// Maps to `agents/openai.yaml` `policy.allow_implicit_invocation`.
    MapToOpenAiPolicy,
    /// No equivalent — dropped, with a loss entry (and a body note for tool scoping).
    DropWarn,
}

pub struct FieldRule {
    pub field: &'static str,
    pub action: Action,
    pub reason: &'static str,
}

/// The drift-prone layer, kept as data so updates are one-line edits.
pub const CLAUDE_TO_CODEX: &[FieldRule] = &[
    FieldRule {
        field: "name",
        action: Action::Portable,
        reason: "shared SKILL.md field",
    },
    FieldRule {
        field: "description",
        action: Action::Portable,
        reason: "shared SKILL.md field",
    },
    FieldRule {
        field: "when_to_use",
        action: Action::FoldIntoDescription,
        reason: "Codex has no when_to_use field",
    },
    FieldRule {
        field: "allowed-tools",
        action: Action::DropWarn,
        reason: "Codex has no per-skill tool allowlist",
    },
    FieldRule {
        field: "disallowed-tools",
        action: Action::DropWarn,
        reason: "Codex has no per-skill tool denylist",
    },
    FieldRule {
        field: "disable-model-invocation",
        action: Action::MapToOpenAiPolicy,
        reason: "maps to agents/openai.yaml policy.allow_implicit_invocation",
    },
    FieldRule {
        field: "user-invocable",
        action: Action::DropWarn,
        reason: "Codex has no model-only/user-only invocation flag",
    },
    FieldRule {
        field: "model",
        action: Action::DropWarn,
        reason: "Codex has no per-skill model override",
    },
    FieldRule {
        field: "effort",
        action: Action::DropWarn,
        reason: "Codex has no per-skill effort override",
    },
    FieldRule {
        field: "context",
        action: Action::DropWarn,
        reason: "Codex has no per-skill subagent fork",
    },
    FieldRule {
        field: "agent",
        action: Action::DropWarn,
        reason: "Codex has no per-skill subagent type",
    },
    FieldRule {
        field: "hooks",
        action: Action::DropWarn,
        reason: "Codex has no per-skill lifecycle hooks",
    },
    FieldRule {
        field: "paths",
        action: Action::DropWarn,
        reason: "Codex has no path-gated activation",
    },
    FieldRule {
        field: "shell",
        action: Action::DropWarn,
        reason: "Codex has no per-skill shell selection",
    },
    FieldRule {
        field: "argument-hint",
        action: Action::DropWarn,
        reason: "Codex has no argument-hint autocomplete",
    },
    FieldRule {
        field: "arguments",
        action: Action::DropWarn,
        reason: "Codex has no named-argument declaration",
    },
];

const RUNTIME_SPECIFIC_FIELDS: &[&str] = &[
    "allowed-tools",
    "disallowed-tools",
    "disable-model-invocation",
    "user-invocable",
    "model",
    "effort",
    "context",
    "agent",
    "hooks",
    "paths",
    "shell",
    "argument-hint",
    "arguments",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct LossEntry {
    pub field: String,
    pub action: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct TranslationResult {
    pub bundle: SkillBundle,
    pub loss: Vec<LossEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Portability {
    Portable,
    RuntimeSpecific,
}

impl Portability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Portability::Portable => "portable",
            Portability::RuntimeSpecific => "runtime-specific",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompatReport {
    pub portability: Portability,
    pub runtime_specific: Vec<String>,
    pub blocking: Vec<String>,
}

/// Render a skill bundle for `to`, given it was authored for `from`. Same-runtime is
/// a no-op with no loss. Other files (`scripts/`, `references/`, …) are carried verbatim.
pub fn translate(
    src: &SkillBundle,
    from: Runtime,
    to: Runtime,
) -> Result<TranslationResult, SkillError> {
    if from == to {
        return Ok(TranslationResult {
            bundle: src.clone(),
            loss: vec![],
        });
    }
    match (from, to) {
        (Runtime::ClaudeCode, Runtime::Codex) => claude_to_codex(src),
        (Runtime::Codex, Runtime::ClaudeCode) => codex_to_claude(src),
        _ => Ok(TranslationResult {
            bundle: src.clone(),
            loss: vec![],
        }),
    }
}

/// Classify a bundle as portable or using runtime-specific features (for a UI badge
/// and a submission-time hint). Pure; no model.
pub fn lint_portability(bundle: &SkillBundle) -> CompatReport {
    let mut runtime_specific = Vec::new();
    if let Some(md) = bundle.skill_md() {
        let (yaml, body) = split_frontmatter(&md);
        let keys = frontmatter_keys(yaml.as_deref());
        for f in RUNTIME_SPECIFIC_FIELDS {
            if keys.iter().any(|k| k == f) {
                runtime_specific.push((*f).to_string());
            }
        }
        if body.contains("!`") || body.contains("```!") {
            runtime_specific.push("dynamic command injection (!`cmd`)".into());
        }
        if has_arg_subst(&body) {
            runtime_specific.push("argument substitution ($ARGUMENTS/$N)".into());
        }
    }
    let portability = if runtime_specific.is_empty() {
        Portability::Portable
    } else {
        Portability::RuntimeSpecific
    };
    CompatReport {
        portability,
        runtime_specific,
        blocking: vec![],
    }
}

fn claude_to_codex(src: &SkillBundle) -> Result<TranslationResult, SkillError> {
    let map = parse_skill_md_map(src)?;
    let (_, body) = split_frontmatter(&skill_md_text(src)?);

    let mut loss = Vec::new();
    let mut advisories = Vec::new();
    let mut emit_openai = false;
    let mut name_val: Option<Value> = None;
    let mut description = map
        .iter()
        .find(|(k, _)| k.as_str() == Some("description"))
        .and_then(|(_, v)| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut rest = Mapping::new();

    for (k, v) in &map {
        let key = match k.as_str() {
            Some(s) => s,
            None => continue,
        };
        if key == "description" {
            continue;
        }
        if key == "name" {
            name_val = Some(v.clone());
            continue;
        }
        match CLAUDE_TO_CODEX
            .iter()
            .find(|r| r.field == key)
            .map(|r| r.action)
        {
            None | Some(Action::Portable) => {
                rest.insert(k.clone(), v.clone());
            }
            Some(Action::FoldIntoDescription) => {
                if let Some(s) = v.as_str() {
                    if !s.trim().is_empty() {
                        if !description.is_empty() {
                            description.push_str("\n\n");
                        }
                        description.push_str(s.trim());
                    }
                }
                loss.push(rule_loss(key, "folded into description"));
            }
            Some(Action::MapToOpenAiPolicy) => {
                if is_truthy(v) {
                    emit_openai = true;
                    advisories.push(
                        "Manual invocation only — do not invoke this skill automatically."
                            .to_string(),
                    );
                }
                loss.push(rule_loss(key, "mapped to agents/openai.yaml + advisory"));
            }
            Some(Action::DropWarn) => {
                loss.push(rule_loss(key, "dropped"));
                if key == "allowed-tools" || key == "disallowed-tools" {
                    advisories.push(format!(
                        "This skill declared `{key}`; Codex has no per-skill tool scoping — review tool use manually."
                    ));
                }
            }
        }
    }

    let (new_body, body_loss, body_advisories) = transform_body(&body);
    loss.extend(body_loss);
    advisories.extend(body_advisories);

    let mut front = Mapping::new();
    if let Some(n) = name_val {
        front.insert(Value::String("name".into()), n);
    }
    if !description.is_empty() {
        front.insert(
            Value::String("description".into()),
            Value::String(description),
        );
    }
    for (k, v) in &rest {
        front.insert(k.clone(), v.clone());
    }

    let new_md = render_skill_md(&front, &new_body, &advisories);
    let mut files = carry_files(src, &["agents/openai.yaml"]);
    files.push(SkillFile {
        path: SKILL_FILE.into(),
        content_b64: encode_b64(new_md.as_bytes()),
    });
    if emit_openai {
        let y = "policy:\n  allow_implicit_invocation: false\n";
        files.push(SkillFile {
            path: "agents/openai.yaml".into(),
            content_b64: encode_b64(y.as_bytes()),
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(TranslationResult {
        bundle: SkillBundle { files },
        loss,
    })
}

fn codex_to_claude(src: &SkillBundle) -> Result<TranslationResult, SkillError> {
    let mut map = parse_skill_md_map(src)?;
    let (_, body) = split_frontmatter(&skill_md_text(src)?);
    let mut loss = Vec::new();

    if let Some(f) = src.get("agents/openai.yaml") {
        if let Ok(txt) = String::from_utf8(decode_b64(&f.content_b64)?) {
            if let Ok(v) = serde_yaml::from_str::<Value>(&txt) {
                let allow = v
                    .get("policy")
                    .and_then(|p| p.get("allow_implicit_invocation"))
                    .and_then(|b| b.as_bool());
                if allow == Some(false) {
                    map.insert(
                        Value::String("disable-model-invocation".into()),
                        Value::Bool(true),
                    );
                    loss.push(LossEntry {
                        field: "agents/openai.yaml policy.allow_implicit_invocation".into(),
                        action: "mapped to disable-model-invocation: true".into(),
                        reason: "Claude Code equivalent of the Codex implicit-invocation policy"
                            .into(),
                    });
                }
            }
        }
    }

    let new_md = render_skill_md(&map, &body, &[]);
    let mut files = carry_files(src, &["agents/openai.yaml"]);
    files.push(SkillFile {
        path: SKILL_FILE.into(),
        content_b64: encode_b64(new_md.as_bytes()),
    });
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(TranslationResult {
        bundle: SkillBundle { files },
        loss,
    })
}

fn transform_body(body: &str) -> (String, Vec<LossEntry>, Vec<String>) {
    let mut loss = Vec::new();
    let mut advisories = Vec::new();
    let mut out = body.to_string();

    if out.contains("```!") {
        out = out.replace("```!", "```");
        loss.push(LossEntry {
            field: "body: ```! block".into(),
            action: "converted to a plain code block".into(),
            reason: "Codex does not pre-execute ```! blocks".into(),
        });
    }
    if out.contains("!`") {
        out = out.replace("!`", "`");
        loss.push(LossEntry {
            field: "body: !`cmd` injection".into(),
            action: "converted to an inert code span".into(),
            reason: "Codex does not pre-run commands before reading the skill".into(),
        });
        advisories.push(
            "This skill used dynamic command injection. On Codex those commands are not auto-run — run them yourself and paste the output where referenced.".to_string(),
        );
    }
    if has_arg_subst(&out) {
        loss.push(LossEntry {
            field: "body: argument substitution".into(),
            action: "left literal + advisory".into(),
            reason: "Codex may not expand $ARGUMENTS/$N/$name the same way".into(),
        });
        advisories.push(
            "This skill referenced arguments (e.g. `$ARGUMENTS`, `$1`). Codex argument handling differs — pass inputs as prose.".to_string(),
        );
    }
    (out, loss, advisories)
}

fn render_skill_md(front: &Mapping, body: &str, advisories: &[String]) -> String {
    let mut out = String::new();
    if !front.is_empty() {
        out.push_str("---\n");
        out.push_str(&serde_yaml::to_string(front).unwrap_or_default());
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("---\n");
    }
    if !advisories.is_empty() {
        out.push_str("\n> **Portability notes**\n>\n");
        for a in advisories {
            out.push_str(&format!("> - {a}\n"));
        }
        out.push('\n');
    }
    out.push_str(body.trim_start_matches('\n'));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn carry_files(src: &SkillBundle, exclude: &[&str]) -> Vec<SkillFile> {
    src.files
        .iter()
        .filter(|f| f.path != SKILL_FILE && !exclude.contains(&f.path.as_str()))
        .cloned()
        .collect()
}

fn skill_md_text(src: &SkillBundle) -> Result<String, SkillError> {
    src.skill_md()
        .ok_or_else(|| SkillError::Translate(format!("bundle has no {SKILL_FILE}")))
}

fn parse_skill_md_map(src: &SkillBundle) -> Result<Mapping, SkillError> {
    let (yaml, _) = split_frontmatter(&skill_md_text(src)?);
    Ok(yaml
        .as_deref()
        .and_then(|y| serde_yaml::from_str::<Mapping>(y).ok())
        .unwrap_or_default())
}

fn frontmatter_keys(yaml: Option<&str>) -> Vec<String> {
    yaml.and_then(|y| serde_yaml::from_str::<Mapping>(y).ok())
        .map(|m| {
            m.iter()
                .filter_map(|(k, _)| k.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn rule_loss(field: &str, action: &str) -> LossEntry {
    let reason = CLAUDE_TO_CODEX
        .iter()
        .find(|r| r.field == field)
        .map(|r| r.reason)
        .unwrap_or("");
    LossEntry {
        field: field.into(),
        action: action.into(),
        reason: reason.into(),
    }
}

fn is_truthy(v: &Value) -> bool {
    matches!(v, Value::Bool(true)) || v.as_str() == Some("true")
}

fn has_arg_subst(s: &str) -> bool {
    s.contains("$ARGUMENTS")
        || s.contains("${CLAUDE_")
        || (0..=9).any(|n| s.contains(&format!("${n}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SkillFile;

    fn file(path: &str, text: &str) -> SkillFile {
        SkillFile {
            path: path.into(),
            content_b64: encode_b64(text.as_bytes()),
        }
    }
    fn skill_md(b: &SkillBundle) -> String {
        b.skill_md().unwrap()
    }

    #[test]
    fn same_runtime_is_noop() {
        let b = SkillBundle {
            files: vec![file(SKILL_FILE, "---\nname: x\n---\nbody")],
        };
        let r = translate(&b, Runtime::ClaudeCode, Runtime::ClaudeCode).unwrap();
        assert!(r.loss.is_empty());
    }

    #[test]
    fn claude_to_codex_degrades_fields_and_keeps_scripts() {
        let md = "---\nname: deploy\ndescription: Deploy it\nallowed-tools: Bash(git *)\nhooks:\n  pre: x\nwhen_to_use: when shipping\n---\nDo the thing.\n";
        let b = SkillBundle {
            files: vec![file(SKILL_FILE, md), file("scripts/run.sh", "echo hi")],
        };
        let r = translate(&b, Runtime::ClaudeCode, Runtime::Codex).unwrap();
        let (yaml, _) = split_frontmatter(&skill_md(&r.bundle));
        let yaml = yaml.expect("frontmatter present");
        assert!(
            !yaml.contains("allowed-tools"),
            "allowed-tools must be dropped"
        );
        assert!(!yaml.contains("hooks"), "hooks must be dropped");
        assert!(
            yaml.contains("when shipping"),
            "when_to_use folds into description"
        );
        assert!(r.loss.iter().any(|l| l.field == "allowed-tools"));
        assert!(
            r.bundle.get("scripts/run.sh").is_some(),
            "scripts carried verbatim"
        );
    }

    #[test]
    fn disable_model_invocation_emits_openai_policy() {
        let md = "---\nname: x\ndescription: y\ndisable-model-invocation: true\n---\nbody";
        let b = SkillBundle {
            files: vec![file(SKILL_FILE, md)],
        };
        let r = translate(&b, Runtime::ClaudeCode, Runtime::Codex).unwrap();
        let oa = r
            .bundle
            .get("agents/openai.yaml")
            .expect("openai.yaml synthesized");
        let txt = String::from_utf8(decode_b64(&oa.content_b64).unwrap()).unwrap();
        assert!(txt.contains("allow_implicit_invocation: false"));
        assert!(skill_md(&r.bundle).contains("Manual invocation only"));
    }

    #[test]
    fn dynamic_injection_is_neutralized() {
        let md = "---\nname: x\ndescription: y\n---\nContext: !`git status`\n";
        let b = SkillBundle {
            files: vec![file(SKILL_FILE, md)],
        };
        let r = translate(&b, Runtime::ClaudeCode, Runtime::Codex).unwrap();
        let (_, body) = split_frontmatter(&skill_md(&r.bundle));
        assert!(
            !body.contains("!`git status`"),
            "injection must be neutralized"
        );
        assert!(body.contains("`git status`"));
        assert!(r.loss.iter().any(|l| l.field.contains("injection")));
    }

    #[test]
    fn codex_to_claude_maps_policy_back() {
        let md = "---\nname: x\ndescription: y\n---\nbody";
        let oa = "policy:\n  allow_implicit_invocation: false\n";
        let b = SkillBundle {
            files: vec![file(SKILL_FILE, md), file("agents/openai.yaml", oa)],
        };
        let r = translate(&b, Runtime::Codex, Runtime::ClaudeCode).unwrap();
        let out = skill_md(&r.bundle);
        assert!(out.contains("disable-model-invocation: true"));
        assert!(
            r.bundle.get("agents/openai.yaml").is_none(),
            "openai.yaml folded away"
        );
    }

    #[test]
    fn lint_flags_runtime_specific_features() {
        let portable = SkillBundle {
            files: vec![file(SKILL_FILE, "---\nname: x\ndescription: y\n---\nbody")],
        };
        assert_eq!(
            lint_portability(&portable).portability,
            Portability::Portable
        );

        let specific = SkillBundle {
            files: vec![file(
                SKILL_FILE,
                "---\nname: x\nhooks:\n  a: b\npaths:\n  - '*.rs'\n---\nbody",
            )],
        };
        let rep = lint_portability(&specific);
        assert_eq!(rep.portability, Portability::RuntimeSpecific);
        assert!(rep.runtime_specific.iter().any(|s| s == "hooks"));
        assert!(rep.runtime_specific.iter().any(|s| s == "paths"));
    }
}
