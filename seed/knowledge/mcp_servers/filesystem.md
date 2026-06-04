# Filesystem MCP server

Exposes a single directory tree to an agent as read/write tools: list, read,
write, move, and search files — confined to the path(s) you allow.

## Prerequisites

- Node.js (the install runs it via `npx`; no global install needed).
- Replace `/path/to/allowed/dir` with the directory you want to expose. You can
  pass more than one path to allow several roots.

## Notes

- The server **cannot escape the allowed roots** — that boundary is the whole
  point. Keep the roots narrow.
- Pair it with a project's working directory so an agent can read context and
  write artifacts without broader machine access.

Source: <https://github.com/modelcontextprotocol/servers/tree/main/src/filesystem>
