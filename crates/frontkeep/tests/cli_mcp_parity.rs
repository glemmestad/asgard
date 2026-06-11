//! CLI ↔ MCP lock-step gate.
//!
//! Frontkeep is "agents first, humans second": every capability is an MCP tool, and
//! the CLI is a thin typed client over those same tools. The two surfaces must
//! stay in step — a tool that an agent can call over `/mcp` but a human can't
//! reach with a first-class `frontkeep …` subcommand (or vice-versa) is a silent
//! divergence. This test fails the build the moment that happens.
//!
//! - MCP truth: the live tool router (`FrontkeepMcp::tool_names()`) — exactly what
//!   `tools/list` serves to agents, so a renamed/added/removed tool shows up here.
//! - CLI truth: every tool name a typed subcommand dispatches in `main.rs`, read
//!   from source. The generic `frontkeep call <tool>` escape hatch passes a runtime
//!   value (no string literal) and is deliberately not counted — parity means a
//!   *typed* command, not just reachability through the catch-all.
//!
//! To intentionally exempt a tool (e.g. one whose ergonomics genuinely don't map
//! to flags), add it to `EXEMPT` with a comment justifying it. Keep that list
//! empty whenever you can.

use regex::Regex;
use std::collections::BTreeSet;

/// Tools allowed to exist on the MCP side with no typed CLI subcommand. Document
/// every entry. Empty is the goal.
const EXEMPT: &[&str] = &[];

const MAIN_RS: &str = include_str!("../src/main.rs");

/// The set of MCP tools the CLI reaches through a typed subcommand.
///
/// Every tool-dispatching helper in `main.rs` follows one convention: the
/// connection (`r` or `&r`) is the first argument and the MCP tool name is a
/// string-literal second argument — `run_tool(&r, "tool", …)`,
/// `call_value(r, "tool", …)`, and any future wrapper. We match that *shape*
/// (anchored on the helper's open paren, whitespace/newlines allowed) rather
/// than specific helper names, so adding a new wrapper can't silently hide a
/// command from the parity gate. The generic `run_tool(&r, &tool, …)` catch-all
/// passes a variable, not a literal, and is correctly excluded.
///
/// The one exception is the direct `…call("tool", …)` on a freshly-built
/// `McpClient`, where the tool is the first argument; matched separately.
fn cli_tools() -> BTreeSet<String> {
    let dispatch = Regex::new(r#"\(\s*&?r\s*,\s*"([a-z0-9_]+)""#).unwrap();
    let direct = Regex::new(r#"\.call\(\s*"([a-z0-9_]+)""#).unwrap();
    dispatch
        .captures_iter(MAIN_RS)
        .chain(direct.captures_iter(MAIN_RS))
        .map(|c| c[1].to_string())
        .collect()
}

#[test]
fn cli_and_mcp_are_in_lockstep() {
    let mcp: BTreeSet<String> = frontkeep_mcp::FrontkeepMcp::tool_names()
        .into_iter()
        .collect();
    let exempt: BTreeSet<String> = EXEMPT.iter().map(|s| s.to_string()).collect();
    let cli = cli_tools();

    // Every MCP tool needs a typed CLI subcommand (unless explicitly exempted).
    let missing_in_cli: Vec<&String> = mcp
        .difference(&cli)
        .filter(|t| !exempt.contains(*t))
        .collect();

    // Every CLI tool reference must hit a real MCP tool — catches typos and tools
    // renamed/removed on the server while the CLI still points at the old name.
    let dead_cli_refs: Vec<&String> = cli.difference(&mcp).collect();

    // An EXEMPT entry that no longer corresponds to a real divergence is stale.
    let stale_exempt: Vec<&String> = exempt.difference(&mcp).collect();

    assert!(
        missing_in_cli.is_empty() && dead_cli_refs.is_empty() && stale_exempt.is_empty(),
        "CLI and MCP have drifted out of lock-step.\n\
         MCP tools with no typed CLI subcommand (add one in main.rs, or EXEMPT it): {missing_in_cli:?}\n\
         CLI references to non-existent MCP tools (fix the name in main.rs): {dead_cli_refs:?}\n\
         Stale EXEMPT entries (no longer a real MCP tool, remove them): {stale_exempt:?}",
    );
}
