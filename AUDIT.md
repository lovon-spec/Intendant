# Codebase Audit Report

**Date**: 2026-03-14
**Scope**: Full codebase audit — tech debt, dead code, unfinished features, legacy patterns, recommendations

---

## Executive Summary

The codebase is **well-maintained overall** with strong test coverage, consistent patterns, and no unsafe code. The main areas of concern are:
- **59 `#[allow(dead_code)]` suppressions** masking genuinely unused code
- **14 active compiler warnings** for unused items
- **Dual-format memory system** (old HashMap + new Vec) adding unnecessary complexity
- **Entire modules declared but unused** (worktree, parts of knowledge pub/sub)
- **PTY session memory leak** — sessions are added but never removed

---

## 1. Dead Code & Unused Items

### 1.1 Compiler Warnings (14 total)

These are items the compiler flags as unused — not suppressed by `#[allow(dead_code)]`:

| Location | Item | Recommendation |
|----------|------|----------------|
| `main.rs:78` | `LoopExitReason::Error` variant | Remove or implement error exit path |
| `event.rs:142` | `PresenceConnected.server_session_id` and `.last_event_seq` fields | Remove if not needed |
| `frontend.rs:40` | `UserAction::SubmitFollowUp` variant | Remove — never constructed |
| `presence.rs:135` | `paused_flag()` method | Remove |
| `presence.rs:669` | `CheckpointState.last_event_seq` field | Remove |
| `presence.rs:727` | `PresenceSession::is_connected()` and `session_id()` | Remove |
| `provider.rs:295,929,1395` | `new_plain()` on all 3 providers | Remove — superseded by `new()` |
| `provider.rs:2013` | `select_provider_with_overrides()` | Remove |
| `session_log.rs:396` | `tool_request()` and `tool_response()` | Remove or integrate |
| `tui/app.rs:97` | `App.focused_line` field | Remove |
| `tui/widgets.rs:412` | `format_log_entry()` | Remove — wrapper of `format_log_entry_with_turn()` that's never called |
| `types.rs:90` | `Verbosity::hint()` | Remove |

### 1.2 Entire Modules Unused

**`worktree.rs`** — Declared as `mod worktree` in `main.rs` but **no functions are called** from anywhere outside the module. All 6 public functions (`create`, `remove`, `merge`, `list`, `WorktreeInfo`, `WorktreeOp`) are suppressed with `#[allow(dead_code)]`. The module has tests but is not integrated into any code path.

**Recommendation**: Either integrate into the sub-agent/implementation flow or remove entirely. If this is a planned feature, document the intent.

### 1.3 Partially Used Modules

**`knowledge.rs`** — Only 4 of 10+ public functions are used externally (`load`, `query`, `format_for_injection`, `KnowledgeQuery`). The entire pub/sub system is unused:
- `publish()` — never called
- `subscribe()` — never called
- `get_unseen()` — never called
- `advance_cursor()` — never called
- `save()` — never called
- `route_knowledge()` — never called

These are all suppressed with `#[allow(dead_code)]`. They represent an unfinished pub/sub knowledge routing system for multi-agent coordination.

**`sub_agent.rs`** — `ProjectState`, `write_project_state()`, `read_project_state()` are defined, tested, but never called. This is orchestrator state checkpointing that was never integrated.

### 1.4 `#[allow(dead_code)]` Audit (59 instances)

Breakdown by category:

| Category | Count | Files | Assessment |
|----------|-------|-------|------------|
| Unused pub/sub knowledge functions | 9 | `knowledge.rs` | **Remove** — unfinished feature |
| Unused worktree module | 6 | `worktree.rs` | **Remove or integrate** |
| Unused conversation methods | 6 | `conversation.rs` | Investigate — may be library API |
| Unused project config structs | 6 | `project.rs` | **Remove** `ModelConfig`, `OrchestratorConfig` if not parsed |
| Provider struct fields | 4 | `provider.rs` | Likely needed for serialization |
| Theme constants/helpers | 4 | `tui/theme.rs` | **Remove** `LOG_BG`, `INPUT_FG`, `bold()`, `dim()` |
| Event fields | 5 | `event.rs` | Investigate — may be needed for MCP |
| Sub-agent state persistence | 2 | `sub_agent.rs` | **Remove** — unfinished feature |
| Sandbox config | 2 | `sandbox.rs` | Keep — used via `agent_runner.rs` |
| Misc (single items) | 15 | various | Case-by-case |

### 1.5 `utils.rs` — Entirely Redundant

`get_timestamp()` is the only function in this file. It is **never called** from any production code. Meanwhile, `agent.rs` inlines the same `SystemTime::now().duration_since(UNIX_EPOCH)` pattern directly (lines 942, 1327).

**Recommendation**: Remove `utils.rs` entirely, or refactor `agent.rs` to use it.

---

## 2. Legacy Code & Backward Compatibility Debt

### 2.1 Dual-Format Memory System (agent.rs:962-1204)

The `store_memory()` and `recall_memory()` functions maintain **two parallel code paths** for old (HashMap) and new (Vec/KnowledgeStore) JSON formats. This doubles the code and testing surface:

- `store_memory()`: ~95 lines with two branches (lines 981-1056)
- `recall_memory()`: ~145 lines with two branches (lines 1101-1191)
- Old format doesn't support tags, channels, or source filtering
- Format detection happens on every read

**Recommendation**: Migrate to new format only. Add a one-time migration on read (convert old format → new format, write back). Remove old format code paths.

### 2.2 Duplicate Xauth Merge Logic (agent.rs:132-204)

Two nearly identical code blocks for merging xauth cookies:
1. Lines 135-169: User's `~/.Xauthority`
2. Lines 172-203: Lightdm root cookie `/var/run/lightdm/root/:N`

Both follow the exact same pattern: `xauth nlist` → check output → `xauth nmerge`. ~70 lines of duplicated code.

**Recommendation**: Extract a helper function like `merge_xauth_from_source(source_path, display, merged_path)`.

---

## 3. Resource Management Issues

### 3.1 PTY Session Memory Leak (agent.rs:40, 521-528)

`pty_sessions: Arc<tokio::sync::Mutex<HashMap<String, PtySession>>>` grows unboundedly. Sessions are inserted on `execPty` calls but **never removed or expired**. Each session holds a PTY master handle.

**Impact**: Memory and file descriptor leak if many unique `shell_id` values are used.

**Recommendation**: Add session expiration (e.g., remove after 30 minutes of inactivity) or an explicit close/cleanup operation.

### 3.2 RwLock Poisoning Risk (agent.rs:1330, 1335)

```rust
self.process_state.write().unwrap()  // line 1330
self.process_state.read().unwrap()   // line 1335
```

If any thread panics while holding the lock, subsequent calls will panic too. While panics are unlikely in this codebase, this is a latent risk.

**Recommendation**: Use `.expect("process_state lock poisoned")` for clearer panic messages, or handle poisoned locks gracefully.

---

## 4. Production Code Quality

### 4.1 Unchecked `.unwrap()` in Production Paths

Key production-code unwraps that could panic:

| Location | Code | Risk |
|----------|------|------|
| `main.rs:289` | `args[i + 1].parse().unwrap()` | Panics on invalid `--web` port |
| `agent.rs:942` | `SystemTime::now().duration_since(UNIX_EPOCH).unwrap()` | Panics if clock before epoch |
| `agent.rs:1020,1048` | `serde_json::to_string_pretty(&data).unwrap()` | Theoretically infallible but pattern is fragile |
| `agent.rs:1330,1335` | `.write().unwrap()` / `.read().unwrap()` | Panics on poisoned lock |

Note: The vast majority of `.unwrap()` calls (800+) are in test code, which is appropriate.

### 4.2 Magic Number: Timeout Sentinel (agent_runner.rs:108)

```rust
let hard_timeout_secs: u64 = if has_human { u64::MAX / 2 } else { 120 };
```

`u64::MAX / 2` as a "no timeout" sentinel is non-obvious and brittle.

**Recommendation**: Define `const NO_TIMEOUT_SECS: u64 = u64::MAX / 2;` with a comment, or use `Option<Duration>`.

### 4.3 Static Global State (main.rs:42)

```rust
static JSON_OUTPUT: AtomicBool = AtomicBool::new(false);
```

Module-level mutable global state for output mode. Works but makes testing and concurrent use harder.

**Recommendation**: Thread as a parameter through the agent loop instead.

---

## 5. Unfinished Features

### 5.1 Knowledge Pub/Sub System

The `knowledge.rs` module contains a complete pub/sub system (`publish`, `subscribe`, `get_unseen`, `advance_cursor`, `route_knowledge`) that was designed for multi-agent knowledge routing but never integrated. The functions are tested but dead.

**Status**: Fully implemented, not wired up.

### 5.2 Git Worktree Management

The `worktree.rs` module provides full worktree lifecycle management (`create`, `remove`, `merge`, `list`) for isolated implementation agents. None of these are called.

**Status**: Fully implemented, not wired up.

### 5.3 Project State Checkpointing

`sub_agent.rs` contains `ProjectState`, `write_project_state()`, `read_project_state()` for persisting orchestrator state across context compactions. Never called from orchestrator flow.

**Status**: Fully implemented, not wired up.

### 5.4 Unused Config Structures

`project.rs` defines `ModelConfig` and `OrchestratorConfig` structs for `intendant.toml` parsing. These are deserialized but their values are never used in provider selection or orchestration.

**Status**: Parsed but ignored.

### 5.5 `UserAction::SubmitFollowUp`

Defined in `frontend.rs` but never constructed anywhere. Likely intended for follow-up input from MCP/web but never wired up.

---

## 6. Code Duplication

### 6.1 Xauth Merge (agent.rs)
~70 lines of duplicated xauth cookie merging logic (see section 2.2).

### 6.2 Memory Format Branching (agent.rs)
Duplicated store/recall logic for old vs new format (see section 2.1).

### 6.3 `result_json()` vs Inline JSON

`agent.rs:1206` defines `result_json(nonce, data)` for standardized output, but many functions build JSON inline with `serde_json::json!()`. Inconsistent pattern.

---

## 7. Recommendations Summary

### High Priority (correctness/safety)
1. Fix `args[i + 1].parse().unwrap()` in CLI parsing — will panic on `--web` without argument
2. Add PTY session cleanup/expiration to prevent resource leaks
3. Use `.expect()` on RwLock operations for clear panic messages

### Medium Priority (tech debt reduction)
4. Remove the 14 compiler-warned unused items
5. Consolidate memory system to new format only, remove old HashMap code path
6. Extract xauth merge helper to eliminate duplication
7. Remove or document the 3 unfinished features (knowledge pub/sub, worktree, project state)
8. Remove entirely unused `utils.rs`
9. Clean up `#[allow(dead_code)]` suppressions — remove for genuinely dead code, keep only for intentional library APIs

### Low Priority (code quality)
10. Replace `u64::MAX / 2` timeout sentinel with named constant
11. Remove unused theme constants (`LOG_BG`, `INPUT_FG`, `bold()`, `dim()`)
12. Remove unused provider functions (`new_plain()` x3, `select_provider_with_overrides()`)
13. Standardize JSON output building (use `result_json()` consistently or remove it)
14. Thread `JSON_OUTPUT` as parameter instead of static global

---

## 8. What's NOT a Problem

- **No TODO/FIXME/HACK/XXX comments** in Rust source (only 1 TODO in auto-generated WASM JS)
- **No `unsafe` code** anywhere
- **No `panic!()`, `unimplemented!()`, or `todo!()` macros** in production code
- **No commented-out code blocks**
- **No orphaned `.rs` files** — all properly declared in module tree
- **Test coverage is excellent** with inline `#[cfg(test)]` modules in every file
- **Error handling is generally solid** with `thiserror`-based enums
- **Dependencies are all actively used** — no unused crate dependencies in Cargo.toml
- **Async patterns are correct** — proper use of tokio, channels, Arc/RwLock
