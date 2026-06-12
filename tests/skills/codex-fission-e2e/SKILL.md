---
name: codex-fission-e2e
description: >
  Live smoke test of model-driven fission on a managed Codex session: spawn
  two chartered branches in a toy git repo (one read-only, one write-scoped
  with an isolated worktree), verify the fission ledger and the dashboard
  endpoint key the group at the real spawn tool-call anchor, wait on and
  import a completed branch into the parent, claim the canonical branch, then
  rewind the parent past the spawn anchor and assert the group detaches.
compatibility: Requires OpenAI Codex auth in ~/.codex/auth.json, the patched Codex fork checkout at /Users/vm/projects/codex-minimal-lineage, and Intendant in daemon/dashboard mode (branch sessions launch through the session supervisor; do not run with --no-web). Makes real model calls; not for CI.
allowed-tools: Bash Read
disable-model-invocation: false
---

# Codex Fission E2E

## Purpose

Smoke-test the whole model-driven fission loop on a real managed Codex
session: `fission_spawn` → branches as real supervised sessions with injected
charters → ledger lifecycle → `fission_control` wait/import →
`claim_fission_canonical` → detach-on-rewind. This exercises the supervisor
(`apply_fission_spawn_action`, `apply_fission_import_action`,
`apply_external_context_rewind`), the MCP surface, the fission ledger, and the
dashboard endpoint together, end to end.

## What It Verifies

- A managed Codex parent can spawn two chartered branches from a single
  `fission_spawn` call: a read-only one (no worktree) and a write-scoped one
  (isolated git worktree + `fission/<short>-2` branch, forked from `HEAD`).
- The fission group is keyed at the **real spawn tool-call anchor**: the
  ledger's `anchor_item_id` is the parent's in-flight `fission_spawn` MCP tool
  item (group `tool` is `fission_spawn`, not the `fission_spawn:head`
  fallback), cross-checked against the parent rollout file.
- Branches run as real sessions and receive their `<fission_charter>`
  developer message; the lifecycle watcher records `completed` + summary.
- `fission_control(op="wait")` returns the tagged group snapshot
  (`still_running` on timeout is normal); `op="import"` injects a
  `<fission_import>` payload into the parent transcript and stamps
  `imported_at`.
- `claim_fission_canonical` records the canonical branch on the group.
- A `rewind_context` to an anchor **before** the spawn call detaches the
  group: `detached: true` with `detach_reason: anchor-unreachable`, wait and
  import are refused, and the rewind record carries
  `detached_fission_group_ids`.

## Setup

```bash
# 1. Patched Codex (minimal-lineage fork)
cd /Users/vm/projects/codex-minimal-lineage/codex-rs
cargo build -p codex-cli --bin codex

# 2. Intendant
cd /Users/vm/projects/intendant
cargo build --bin intendant

# 3. Toy git repo (disposable)
REPO=$(mktemp -d /tmp/fission-e2e-repo.XXXX)
git -C "$REPO" init -q
mkdir -p "$REPO/notes" "$REPO/src"
printf 'seed note: the answer is 42\n' > "$REPO/notes/seed.md"
printf 'fn main() {}\n' > "$REPO/src/main.rs"
git -C "$REPO" add -A && git -C "$REPO" -c user.email=e2e@local -c user.name=e2e commit -qm seed

# 4. Per-project managed Codex config
cat > "$REPO/intendant.toml" <<'EOF'
[agent.codex]
command = "/Users/vm/projects/codex-minimal-lineage/codex-rs/target/debug/codex"
managed_context = "managed"
approval_policy = "never"
sandbox = "workspace-write"
EOF
```

Pick a free port (never the shared 8765) and launch from the toy repo with the
web dashboard ON (the default — branch sessions are resumed through the
daemon's session supervisor, so `--no-web` breaks spawning). Keep this command
in its own terminal; everything else runs against it.

```bash
PORT=18901
cd "$REPO"
/Users/vm/projects/intendant/target/debug/intendant --agent codex \
  --web $PORT --no-tls --bind 127.0.0.1 \
  "You are running a fission smoke test in $REPO. Do exactly this, in order: \
   (1) run \`git log --oneline -1\` and \`ls\`. \
   (2) call the Intendant MCP tool fission_spawn with exactly two branches: \
   first {\"objective\": \"READ-ONLY survey: read notes/seed.md in this repo and end your turn with a one-line summary of its contents. Do not modify any files.\", \"name\": \"survey\"}; \
   second {\"objective\": \"EDIT: in your assigned checkout, append the line 'branch was here' to notes/seed.md, commit it with message 'fission edit', then end your turn with a one-line summary.\", \"write_scope\": [\"notes/\"], \"name\": \"editor\"}. \
   (3) report the group_id and branch session ids from the tool result, then end your turn and wait for further instructions."
```

Shell helpers used below (plaintext local gateway, per-session MCP-over-HTTP —
the same `POST /mcp?session_id=…` JSON-RPC `tools/call` surface the dashboard
uses):

```bash
BASE=http://127.0.0.1:$PORT
mcp() { # mcp <session_id> <tool> '<json args>'
  curl -s "$BASE/mcp?session_id=$1" -H 'Content-Type: application/json' -d '{
    "jsonrpc":"2.0","id":1,"method":"tools/call",
    "params":{"name":"'"$2"'","arguments":'"$3"'}}' \
  | jq -r '.result.content[0].text'
}
```

## Phase A+B — spawn and verify anchoring

Wait for the parent to finish step (2) (watch the dashboard Activity log, or
poll until the ledger appears). The fission ledger lives in the **parent's**
session log dir under `~/.intendant/logs/`, named by the `Session ID:` line
the launch printed at startup — select it by that id, never by newest-mtime
(other Intendant instances on this machine write ledgers too):

```bash
SID=<Session ID printed at launch>
LOG_DIR=~/.intendant/logs/$SID
LEDGER=$LOG_DIR/fission_ledger.json
PARENT=$(jq -r '.groups[0].parent_session_id' "$LEDGER")   # Codex thread id
GROUP=$(jq -r '.groups[0].group_id' "$LEDGER")
ANCHOR=$(jq -r '.groups[0].anchor_item_id' "$LEDGER")
EDITOR=$(jq -r '.groups[0].branches[] | select(.task | test("EDIT")) | .session_id' "$LEDGER")
SURVEY=$(jq -r '.groups[0].branches[] | select(.task | test("READ-ONLY")) | .session_id' "$LEDGER")
jq '{group: .groups[0].group_id, tool: .groups[0].tool, anchor: .groups[0].anchor_item_id}' "$LEDGER"
```

Assertions:

```bash
# Real spawn anchor, not the catalog-head fallback.
test "$(jq -r '.groups[0].tool' "$LEDGER")" = fission_spawn

# Cross-check against the parent rollout: the line carrying the anchor item id
# is the fission_spawn MCP tool call itself. The rollout filename carries the
# parent thread id when the ledger recorded the backend id; otherwise fall
# back to any rollout containing the anchor (a forked branch may replay the
# inherited item line — the assertion holds in either file).
ROLLOUT=$(ls ~/.codex/sessions/$(date +%Y/%m/%d)/rollout-*"$PARENT"*.jsonl 2>/dev/null | head -1)
[ -n "$ROLLOUT" ] || ROLLOUT=$(grep -rl "$ANCHOR" ~/.codex/sessions/$(date +%Y/%m/%d)/ | head -1)
grep "$ANCHOR" "$ROLLOUT" | grep -c fission_spawn          # expect >= 1

# Worktree default: editor (write_scope) got one, survey did not.
curl -s "$BASE/api/managed-context/fission?session_id=$PARENT" \
  | jq -r '.groups[0].branches[] | [.session_id, (.worktree_path // "none"), (.charter.write_scope // "read-only")] | @tsv'
git -C "$REPO" worktree list      # expect .intendant/worktrees/fission/<short8>-2

# Branches are real sessions with injected charters: each branch rollout
# contains the <fission_charter> developer message (which embeds this group's
# id — scope by it, so reruns and other sessions from the same day don't
# inflate the count) and the kickoff task.
grep -rl "$GROUP" ~/.codex/sessions/$(date +%Y/%m/%d)/ | xargs grep -l '<fission_charter>' | wc -l   # == 2
grep -rl "$GROUP" ~/.codex/sessions/$(date +%Y/%m/%d)/ | xargs grep -l 'Begin your fission charter' | wc -l   # == 2

# get_status carries the same merged document (groups + ext charters).
mcp "$PARENT" get_status '{}' | jq '.fission_ledger.ext.groups[0].branches[].charter'
```

The two branches also appear as live session windows in the dashboard, named
by their charter `name` ("survey" / "editor"; unnamed branches default to
"Fission N: <objective>") and parented to the Codex session with a
`fission-branch` relationship.
Let them run to completion (a couple of minutes); the lifecycle watcher flips
the ledger:

```bash
jq -r '.groups[0].branches[] | [.session_id, .status, (.summary // "-")] | @tsv' "$LEDGER"
# expect both terminal (completed / ended) with one-line summaries
```

## Phase C+D — wait, import, claim

Drive the parent model (dashboard follow-up input on the parent session, or an
equivalent prompt) with:

```text
Now call fission_control with op="wait", group_id=<GROUP>,
branch_session_id=<EDITOR>, timeout_s=60; if the outcome is still_running,
call it again. When terminal, call fission_control with op="import" for that
branch, then claim_fission_canonical with group_id=<GROUP> and
branch_session_id=<EDITOR>, and end your turn reporting what you imported.
```

Scripted equivalent of the dashboard input (same `ControlMsg::FollowUp` the
dashboard sends, over the gateway WebSocket; `$SID` is the launch-printed
Session ID and `$TEXT` the prompt above with `<GROUP>`/`<EDITOR>` filled in):

```bash
python3 - "$PORT" "$SID" "$TEXT" <<'EOF'
import asyncio, json, sys, websockets
port, sid, text = sys.argv[1], sys.argv[2], sys.argv[3]
async def main():
    async with websockets.connect(f"ws://127.0.0.1:{port}/ws") as ws:
        await ws.send(json.dumps(
            {"action": "follow_up", "session_id": sid, "text": text, "direct": True}))
        await asyncio.sleep(2)
asyncio.run(main())
EOF
```

The direct equivalents (same tools, same effect on the parent thread) for
deterministic re-checks:

```bash
mcp "$PARENT" fission_control '{"group_id":"'$GROUP'","branch_session_id":"'$EDITOR'","op":"wait","timeout_s":60}' \
  | jq '{outcome, watched}'                       # "terminal" (still_running = normal, re-issue)
mcp "$PARENT" fission_control '{"group_id":"'$GROUP'","branch_session_id":"'$EDITOR'","op":"import"}'
mcp "$PARENT" claim_fission_canonical '{"group_id":"'$GROUP'","branch_session_id":"'$EDITOR'"}'
```

Assertions:

```bash
# Import payload landed in the parent transcript as developer context. The
# <fission_import> message exists only in the importing (parent) thread, so
# select on it and tie that rollout back to the spawn anchor.
PARENT_ROLLOUT=$(grep -rl '<fission_import>' ~/.codex/sessions/$(date +%Y/%m/%d)/ | head -1)
test -n "$PARENT_ROLLOUT"                                               # import payload exists
grep -c "$ANCHOR" "$PARENT_ROLLOUT"                                     # same thread as the spawn (>= 1)
# Imported marker + canonical claim in the ledger document and endpoint.
jq '.groups[0].canonical_session_id' "$LEDGER"                          # == $EDITOR
jq '.ext.groups[0].branches[] | {session_id, imported_at}' "$LEDGER"
curl -s "$BASE/api/managed-context/fission?session_id=$PARENT" \
  | jq '.groups[0].branches[] | {session_id, status, imported_at}'
# The editor's commit exists in its worktree branch.
git -C "$REPO" log --oneline --all | grep -c 'fission edit'             # == 1
```

The dashboard Managed tab now shows the group with a `canonical` chip on the
editor branch and an `imported` chip after the import.

## Phase E — detach-on-rewind

Rewind the parent to an anchor **before** the spawn call — the `git log
--oneline -1` exec from step (1) precedes it:

```bash
mcp "$PARENT" list_rewind_anchors '{"query":"git log"}'    # pick its exact item_id
ITEM=<item id of the git log exec call>
mcp "$PARENT" rewind_context '{"anchor":{"item_id":"'$ITEM'","position":"before"},
  "reason":"fission detach smoke: cut history back past the spawn anchor",
  "primer":"Fission smoke test state: toy repo seeded; a fission group was spawned, its editor branch imported and claimed canonical. This rewind deliberately severs the group to test detach-on-rewind."}'
```

(If that exact anchor is refused — recovery-eligibility or restore-headroom
validation — pick the next-earliest anchor that still precedes the
`fission_spawn` call. Any cut before the spawn anchor's first rollout line
detaches the group.)

Assertions:

```bash
# Group detached, sticky, with the rewind reason. Branches that were already
# terminal keep their statuses (their results stay real); only non-terminal
# branches would have flipped to "detached".
curl -s "$BASE/api/managed-context/fission?session_id=$PARENT" \
  | jq '.groups[0] | {detached, detach_reason, statuses: [.branches[].status]}'
# expect: detached: true, detach_reason: "anchor-unreachable"

# Wait and import are refused on the detached group.
mcp "$PARENT" fission_control '{"group_id":"'$GROUP'","op":"wait","timeout_s":5}' \
  | jq -r '.outcome'                                       # "detached"
mcp "$PARENT" fission_control '{"group_id":"'$GROUP'","branch_session_id":"'$EDITOR'","op":"import"}' \
  | grep -c 'cannot be auto-imported'                      # >= 1

# The durable rewind record carries the severed group ids.
jq -r 'select(.detached_fission_group_ids != null) | .record_id, .detached_fission_group_ids[]' \
  "$LOG_DIR"/context_rewinds/rewind-*.json
curl -s "$BASE/api/managed-context/records?session_id=$PARENT" \
  | jq '.records[0] | {record_id, detached_fission_group_ids}'
```

## Expected Pass Criteria

All of the following, in one run:

1. `fission_spawn` reports `spawned 2/2 branch(es)` with a `group_id` and two
   thread ids.
2. Ledger group `tool == "fission_spawn"` and its `anchor_item_id` greps to
   the `fission_spawn` MCP call line in the parent rollout.
3. Editor branch has `worktree_path` under
   `$REPO/.intendant/worktrees/fission/` and `git worktree list` shows it;
   survey branch has none.
4. Both branch rollouts contain `<fission_charter>`; ledger ext carries both
   charters (write scope on editor only).
5. Both branches reach a terminal status with non-empty summaries via the
   lifecycle watcher.
6. Wait returns `outcome: "terminal"` (after any normal `still_running`
   rounds); `<fission_import>` appears in the parent rollout; `imported_at`
   set on the editor branch.
7. `canonical_session_id == ` editor branch id after the claim.
8. Post-rewind: group `detached: true` with `detach_reason:
   "anchor-unreachable"`; wait returns `outcome: "detached"`; import is
   refused with `cannot be auto-imported`; the rewind record lists the group
   in `detached_fission_group_ids`.

## Cleanup

Stop only the Intendant process this run started (Ctrl-C in its terminal, or
`kill <PID>` with the explicit PID — never pattern-kill), then remove the toy
repo (`rm -rf "$REPO"` also removes its fission worktree). Session logs stay
under `~/.intendant/logs/` and rollouts under `~/.codex/sessions/` for
post-mortems.

## Notes

- Real model calls against real Codex auth: spawn fans out 3+ live sessions.
  Do not put this in normal CI, and do not run it against the shared default
  port 8765.
- The `fission_spawn`/`fission_control(op="import")` MCP tools wait up to 20 s
  for the supervisor's thread-action result; on a slow spawn the tool may
  report `dispatched but no spawn result was observed` while the spawn still
  completes — check the ledger before declaring a failure.
- Charters are the entire context contract: branches fork from the last
  *completed* turn and never see the spawning turn, so the scripted
  objectives above carry every path/constraint the branch needs.
- The phase E nuance is deliberate: branches already terminal at rewind time
  keep their statuses and a terminal canonical branch keeps its claim — group
  detachment alone gates wait/import/claim and new-branch registration.
