# Intendant repository managed-context instructions

Project-specific guidance for managed Codex sessions working on the Intendant
repository itself. Intendant appends this file to the generic managed-context
developer instructions for every managed Codex session whose working directory
is this repo (see `managed_context_developer_instructions_for_project` in
`src/bin/caller/external_agent/codex.rs`).

## Browser / dashboard / Station validation

For browser/dashboard/Station validation, use `node scripts/validate-dashboard.cjs` and prefer its named probes such as `--station-probe rendered` over ad-hoc Chromium/CDP scripts or giant inline `--wait-for-function` expressions; its `--help` is the authoritative flag reference, and docs/src/external-agent-orchestration.md has the full Station QA recipes. For a temporary dashboard, use the helper's owned lifecycle: `--launch-dashboard --port <throwaway_port>` for a one-shot smoke, or `--hold-dashboard` kept in the foreground while separate CU/browser steps run against the printed URL, then interrupted for helper-owned cleanup. Do not start a separate foreground/nohup/setsid dashboard just so another tool can connect.

For meaningful headed Station QA, run the helper with `--require-station-state --require-managed-context-state --require-ai-provider-session --require-external-agent codex --station-interaction-probe --screenshot <png> --json` and review the returned screenshot path before counting the run as a product pass.

## Helper retry discipline

Browser validation retry discipline: run one primary helper smoke. If it fails or times out, run at most one compact diagnostic retry with `--diagnostics --json` and a targeted selector/function. Then either make a targeted code fix from those facts, or report a clear partial-validation conclusion with the helper reason/logs/diagnostics. Do not cycle through raw CDP, Node, Python, Browser, Chrome, and plugin automation stacks unless the user explicitly asks for deeper manual investigation or the helper itself is the suspected broken component.
