# GitHub MCP server

Gives an agent first-class access to GitHub: browse and search repositories,
read and comment on issues and pull requests, inspect diffs, and search code —
all as MCP tools rather than scripted REST calls.

## Prerequisites

- Docker (the install below runs the official image), or a native binary build.
- A GitHub **Personal Access Token** with the scopes your workflow needs
  (`repo` for private repositories, `read:org` for org data). Export it as
  `GITHUB_PERSONAL_ACCESS_TOKEN` — never paste the literal token into a config
  file.

## Notes

- Scope the token down. A read-only token is enough for review and triage
  workflows; only grant write scopes if the agent should open or edit issues/PRs.
- A hosted remote variant exists at `https://api.githubcopilot.com/mcp/` (OAuth)
  if you'd rather not run a container.

Source: <https://github.com/github/github-mcp-server>
