---
name: intendant-cli
description: Use when an agent needs Intendant control beyond the small MCP bootstrap set. Prefer `intendant ctl` over broad MCP tools to keep model context small.
---

# Intendant CLI

Use `intendant ctl` for Intendant control that is not already available as a small MCP bootstrap tool. The CLI talks to the running dashboard/MCP endpoint and exposes broad capabilities lazily through subcommand help.

When `$INTENDANT` is set, run `"$INTENDANT" ctl ...`; Intendant sets it for supervised Codex sessions so the exact controller binary is available even when `intendant` is not on PATH. Otherwise use `intendant ctl ...`.

Start with:

```bash
"${INTENDANT:-intendant}" ctl --help
"${INTENDANT:-intendant}" ctl status --json
"${INTENDANT:-intendant}" ctl tools list
```

Useful groups:

- `"${INTENDANT:-intendant}" ctl display --help` for displays, frames, screenshots, and display claims.
- `"${INTENDANT:-intendant}" ctl browser --help` for browser workspaces, including local CDP-backed browsers and lease management.
- `"${INTENDANT:-intendant}" ctl cu --help` for computer-use actions.
- `"${INTENDANT:-intendant}" ctl shared --help` for shared display collaboration.
- `"${INTENDANT:-intendant}" ctl approval --help` and `"${INTENDANT:-intendant}" ctl input --help` for pending approval/input flows.
- `"${INTENDANT:-intendant}" ctl context --help` for managed-context rewind/backout.
- `"${INTENDANT:-intendant}" ctl controller --help` for controller-loop and restart controls.
- `"${INTENDANT:-intendant}" ctl tools schema TOOL` and `"${INTENDANT:-intendant}" ctl tools call TOOL --args JSON` for rare or newly-added MCP tools.

Prefer `--json` when the output will be inspected by an agent. Use `--session ID` when operating on a specific session. Use `--managed-context managed` for rewind/backout commands.
