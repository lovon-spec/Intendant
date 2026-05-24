mod agent_runner;
mod app_state_pricing;
mod approval;
mod audio_routing;
mod autonomy;
mod computer_use;
mod control;
mod control_plane;
mod conversation;
mod daemon_log_tee;
mod debug;
mod diagnostics;
mod display;
mod error;
mod event;
mod external_agent;
mod file_watcher;
mod frames;
mod frontend;
mod knowledge;
mod lan;
mod live_audio;
mod live_audio_types;
mod mcp;
mod mcp_client;
mod peer;
mod platform;
mod presence;
mod project;
mod prompts;
mod provider;
mod quarantine;
mod recording;
mod sandbox;
mod schema_validator;
mod session_log;
mod session_names;
mod session_supervisor;
mod skills;
mod sub_agent;
mod task_dispatch;
mod terminal;
mod tool_batch;
mod tools;
mod transcription;
mod tui;
mod types;
mod upload_store;
mod user_mode;
mod vision;
mod web_gateway;
mod web_tls;
mod worktree;
mod worktree_inventory;

use autonomy::{AutonomyLevel, AutonomyState, SharedAutonomy};
use conversation::Conversation;
use error::CallerError;
use event::{AppEvent, EventBus};
use project::Project;
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tool_batch::{assemble_batch_from_tool_calls, map_results_to_tool_responses};

type SharedSessionLog = Arc<Mutex<session_log::SessionLog>>;

/// Session log directory for the panic hook to write panic.log into.
/// Set once when a session starts; read by the panic hook on crash.
static PANIC_LOG_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Shared slot for JSON-mode approval responses.
/// The stdin reader stores approval senders here; the agent loop awaits them.
type JsonApprovalSlot =
    Arc<Mutex<Option<(u64, tokio::sync::oneshot::Sender<event::ApprovalResponse>)>>>;

fn new_json_approval_slot() -> JsonApprovalSlot {
    Arc::new(Mutex::new(None))
}

/// Helper to write to the session log without propagating errors.
fn slog(log: &SharedSessionLog, f: impl FnOnce(&mut session_log::SessionLog)) {
    if let Ok(mut l) = log.lock() {
        f(&mut l);
    }
}

fn session_log_id(session_log: &SharedSessionLog) -> Option<String> {
    session_log
        .lock()
        .ok()
        .map(|log| log.session_id().to_string())
        .filter(|id| !id.is_empty())
}

fn event_targets_session(target: &Option<String>, session_id: &Option<String>) -> bool {
    match target {
        Some(target) => session_id.as_deref() == Some(target.as_str()),
        None => true,
    }
}

fn event_targets_session_or_alias(
    target: &Option<String>,
    session_id: &Option<String>,
    alias_session_id: &Option<String>,
) -> bool {
    match target {
        Some(target) => {
            session_id.as_deref() == Some(target.as_str())
                || alias_session_id.as_deref() == Some(target.as_str())
        }
        None => true,
    }
}

fn event_targets_external_session_or_side(
    target: &Option<String>,
    session_id: &Option<String>,
    alias_session_id: &Option<String>,
    side_threads: &HashMap<String, String>,
) -> bool {
    match target {
        Some(target) => {
            event_targets_session_or_alias(&Some(target.clone()), session_id, alias_session_id)
                || side_threads.contains_key(target)
        }
        None => true,
    }
}

/// Build the [`peer::AuthRequirements`] this daemon advertises in
/// its own Agent Card from the project's `[server.auth]` config and
/// the LAN cert dir.
///
/// Resolution rules:
///
/// - `transport`:
///   - `advertised_transport = "none"` (default) → [`peer::TransportAuth::None`]
///   - `"mutual-tls"` → [`peer::TransportAuth::MutualTls`]
///   - `"pin-self-cert"` → read this daemon's own `server.crt` from
///     the LAN cert dir, compute its SHA-256 fingerprint, embed it
///     in [`peer::TransportAuth::PinnedMutualTls`]. Errors if no
///     cert is present (operator forgot to run `intendant lan
///     setup`).
///   - any other value → config error
/// - `application`:
///   - `bearer_token = "..."` set → `Some(Bearer { hint, rotation_url: None })`
///     where `hint` documents where the token comes from so peers
///     can give operators a useful "configure me" message
///   - unset → `None`
///
/// Called once per spawn_web_gateway invocation, at daemon startup.
/// Errors propagate as `CallerError::Config` so the operator sees
/// a clean startup failure rather than a silent misconfigure.
fn build_local_advertised_auth(
    server_auth: &project::ServerAuthConfig,
    cert_dir: &std::path::Path,
) -> Result<peer::AuthRequirements, CallerError> {
    let transport = match server_auth.advertised_transport.as_str() {
        "none" => peer::TransportAuth::None,
        "mutual-tls" => peer::TransportAuth::MutualTls,
        "pin-self-cert" => {
            // `pin-self-cert` reads the local server cert produced by
            // `intendant lan setup`. The `certs` module is now pure-Rust
            // (rcgen + p12-keystore) and compiles everywhere, so this works
            // on all platforms; only the nginx-based `lan setup` flow that
            // writes the cert is still deferred on Windows.
            let fp = lan::certs::read_server_cert_fingerprint(cert_dir).ok_or_else(|| {
                CallerError::Config(format!(
                    "[server.auth] advertised_transport = \"pin-self-cert\" requires \
                     a local server cert at {}/server.crt — run `intendant lan setup` \
                     first, or change advertised_transport to \"none\" / \"mutual-tls\"",
                    cert_dir.display()
                ))
            })?;
            peer::TransportAuth::PinnedMutualTls {
                server_cert_fingerprints: vec![fp],
            }
        }
        other => {
            return Err(CallerError::Config(format!(
                "[server.auth] advertised_transport = {other:?} is not a valid value \
                 (accepted: \"none\", \"mutual-tls\", \"pin-self-cert\")"
            )));
        }
    };
    let application = server_auth
        .bearer_token
        .as_ref()
        .map(|_| peer::ApplicationAuth::Bearer {
            hint: Some("[server.auth] bearer_token".to_string()),
            rotation_url: None,
        });
    Ok(peer::AuthRequirements {
        transport,
        application,
    })
}

/// Resolve the advertise-URL list passed to `spawn_web_gateway`,
/// applying CLI > config > auto-detect precedence.
///
/// - If `--advertise-url` was given (one or more times), the CLI list
///   wins entirely. The operator at the command line beats the
///   operator at the config file.
/// - Otherwise, if `[server.advertise]` in `intendant.toml` is non-
///   empty, that list is used.
/// - If both are empty, an empty `Vec` is returned, which signals
///   `spawn_web_gateway` to fall back to its single-URL auto-detection
///   from the listener's bind address (the historical behavior).
///
/// Returns owned `String`s so the caller can move the list directly
/// into `spawn_web_gateway` without an extra clone.
fn resolve_advertise_urls_from_flags_and_config(
    flags: &CliFlags,
    project: &Project,
) -> Vec<String> {
    if !flags.advertise_urls.is_empty() {
        flags.advertise_urls.clone()
    } else {
        project.config.server.advertise.clone()
    }
}

/// Build a peer registry for this daemon and hydrate it from the
/// `[[peer]]` sections in `intendant.toml`.
///
/// Spawns the durable log writer task (appending
/// `TaggedPeerEvent`s as JSONL to `<log_dir>/peers.jsonl`) and
/// creates a [`crate::peer::PeerRegistry`] wired to its sender.
/// Each config entry fires a background `add_peer` task so
/// slow/unreachable peers don't block daemon startup — the
/// registry's own reconnect state machine handles those
/// asynchronously once the card fetch returns.
///
/// The returned registry is cheaply cloneable (`Arc`-backed) and
/// gets passed into `spawn_web_gateway` so the `/api/peers`
/// handlers can inspect and mutate the same store. The log
/// writer's join handle is intentionally dropped — the writer
/// exits cleanly when all its senders go away (peer actors +
/// registry clones), and we don't currently have an explicit
/// daemon shutdown path that would await it.
fn build_and_hydrate_peer_registry(
    log_dir: &Path,
    peer_configs: &[project::PeerConfig],
) -> peer::PeerRegistry {
    let log_path = log_dir.join("peers.jsonl");
    let (log_tx, _log_handle) = peer::spawn_peer_log_writer(log_path);
    let registry = peer::PeerRegistry::new(log_tx);
    for cfg in peer_configs {
        let registry_for_task = registry.clone();
        let card_url = cfg.card_url.clone();
        let bearer_token = cfg.bearer_token.clone();
        let pinned_fingerprints = cfg.pinned_fingerprints.clone();
        let browser_tcp_via_url = cfg.browser_tcp_via_url.clone();
        tokio::spawn(async move {
            // Vec::new() for via_urls (could be threaded through
            // PeerConfig later if config-driven via overrides become
            // a need; per-peer dashboard adds already use the via_urls
            // field on AddPeerRequest). pinned_fingerprints, when
            // non-empty, replaces the card's auth.transport with
            // PinnedMutualTls — operator distrusts the card's claim
            // and pins against fingerprints they got out-of-band.
            // browser_tcp_via_url, when set, overrides the dashboard's
            // default `d.ws_url` fallback when opening WebRTC display
            // — used when the browser and primary can't share the
            // same URL (primary-side localhost tunnel, split
            // browser/primary machines, etc.).
            if let Err(e) = registry_for_task
                .add_peer_with_credentials(
                    &card_url,
                    Vec::new(),
                    bearer_token,
                    pinned_fingerprints,
                    browser_tcp_via_url,
                )
                .await
            {
                eprintln!(
                    "intendant: failed to register peer from intendant.toml \
                     ({card_url}): {e}"
                );
            }
        });
    }
    registry
}

/// Emit a "[runtime] Task dispatched" log entry from a backend task acceptance
/// point. Writes to the session log on disk and broadcasts a `LogEntry` event
/// for external consumers (web dashboard, control socket).
///
/// This is the single source of truth for the dispatch log line: it lives in
/// the backend (where the task is actually accepted for processing) rather
/// than in any frontend, so the log is consistent across TUI, headless, and
/// daemon modes regardless of which interface originated the task.
fn emit_task_dispatched_log(
    bus: &EventBus,
    session_log: &SharedSessionLog,
    task: &str,
    attachment_count: usize,
) {
    let suffix = if attachment_count > 0 {
        format!(
            " with {} attachment{}",
            attachment_count,
            if attachment_count == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    let message = format!(
        "[runtime] Task dispatched{}: {}",
        suffix,
        types::truncate_str(task, 80)
    );
    slog(session_log, |l| l.info(&message));
    bus.send(AppEvent::LogEntry {
        session_id: None,
        level: "info".to_string(),
        source: "system".to_string(),
        content: message,
        turn: None,
    });
}

fn emit_user_message_log(
    bus: &EventBus,
    session_log: &SharedSessionLog,
    session_id: Option<&str>,
    user_turn_index: Option<u32>,
    user_turn_revision: Option<u32>,
    replacement_for_user_turn_index: Option<u32>,
    text: &str,
) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    slog(session_log, |l| l.info(&format!("[user] {}", text)));
    bus.send(AppEvent::UserMessageLog {
        session_id: session_id.map(str::to_string),
        content: text.to_string(),
        user_turn_index,
        user_turn_revision,
        replacement_for_user_turn_index,
    });
}

fn json_string_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}

fn collect_jsonl_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, out);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|name| name.ends_with(".jsonl"))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
}

fn codex_session_file_id(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
            return obj
                .get("payload")
                .and_then(|payload| json_string_field(payload, "id"));
        }
    }
    None
}

fn find_codex_session_file_for_main(home: &Path, session_id: &str) -> Option<PathBuf> {
    let mut files = Vec::new();
    collect_jsonl_files(&home.join(".codex").join("sessions"), &mut files);
    collect_jsonl_files(&home.join(".codex").join("archived_sessions"), &mut files);
    files
        .into_iter()
        .find(|path| codex_session_file_id(path).as_deref() == Some(session_id))
}

fn codex_message_content_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .and_then(|v| v.as_str())
                        .or_else(|| item.get("content").and_then(|v| v.as_str()))
                        .map(str::to_string)
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

fn codex_payload_user_text(payload: &serde_json::Value) -> Option<String> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
        return None;
    }
    if payload.get("role").and_then(|v| v.as_str()) != Some("user") {
        return None;
    }
    let text = codex_message_content_text(payload.get("content")?)?;
    if is_codex_injected_user_text_for_main(&text) {
        None
    } else {
        Some(text)
    }
}

#[derive(Debug, Clone, Default)]
struct UserTurnRevisionState {
    active_count: u32,
    latest_revision_by_turn: HashMap<u32, u32>,
    active_revision_by_turn: HashMap<u32, u32>,
}

impl UserTurnRevisionState {
    fn active_count(&self) -> u32 {
        self.active_count
    }

    fn active_revision(&self, user_turn_index: u32) -> Option<u32> {
        self.active_revision_by_turn.get(&user_turn_index).copied()
    }

    fn seed_active_turns_to(&mut self, active_count: u32) {
        while self.active_count < active_count {
            self.record_next_turn();
        }
    }

    fn record_next_turn(&mut self) -> (u32, u32) {
        let user_turn_index = self.active_count.saturating_add(1);
        let revision = self.record_active_turn(user_turn_index);
        self.active_count = user_turn_index;
        (user_turn_index, revision)
    }

    fn record_active_turn(&mut self, user_turn_index: u32) -> u32 {
        let next_revision = self
            .latest_revision_by_turn
            .get(&user_turn_index)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        self.latest_revision_by_turn
            .insert(user_turn_index, next_revision);
        self.active_revision_by_turn
            .insert(user_turn_index, next_revision);
        self.active_count = self.active_count.max(user_turn_index);
        next_revision
    }

    fn rewind_last_turns(&mut self, turns_to_drop: u32) {
        if turns_to_drop == 0 || self.active_count == 0 {
            return;
        }
        let first_user_turn_index = self
            .active_count
            .saturating_sub(turns_to_drop)
            .saturating_add(1);
        self.rewind_from_turn(first_user_turn_index);
    }

    fn rewind_from_turn(&mut self, first_user_turn_index: u32) {
        if first_user_turn_index == 0 || first_user_turn_index > self.active_count {
            return;
        }
        for turn in first_user_turn_index..=self.active_count {
            self.active_revision_by_turn.remove(&turn);
        }
        self.active_count = first_user_turn_index.saturating_sub(1);
    }

    fn validate_expected_revision(
        &self,
        user_turn_index: u32,
        expected_revision: Option<u32>,
    ) -> Result<(), String> {
        let Some(expected_revision) = expected_revision else {
            return Err(format!(
                "Cannot edit user turn {}; missing active-message revision",
                user_turn_index
            ));
        };
        match self.active_revision(user_turn_index) {
            Some(active_revision) if active_revision == expected_revision => Ok(()),
            Some(active_revision) => Err(format!(
                "Cannot edit user turn {}; the displayed message revision {} is stale (active revision is {})",
                user_turn_index, expected_revision, active_revision
            )),
            None => Err(format!(
                "Cannot edit user turn {}; that message is no longer active context",
                user_turn_index
            )),
        }
    }
}

fn is_codex_injected_user_text_for_main(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("# AGENTS.md instructions for ")
        || trimmed.starts_with("<turn_aborted>")
        || trimmed.starts_with("<subagent_notification>")
}

fn codex_user_turn_state_from_history(session_id: &str) -> Option<UserTurnRevisionState> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()));
    let path = find_codex_session_file_for_main(&home, session_id)?;
    let contents = std::fs::read_to_string(path).ok()?;
    let mut saw_user_message_event = false;
    let mut event_state = UserTurnRevisionState::default();
    let mut fallback_state = UserTurnRevisionState::default();

    for line in contents.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "event_msg" => {
                let Some(payload) = obj.get("payload") else {
                    continue;
                };
                match payload.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "user_message" => {
                        saw_user_message_event = true;
                        event_state.record_next_turn();
                    }
                    "thread_rolled_back" => {
                        let turns = payload
                            .get("num_turns")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32;
                        event_state.rewind_last_turns(turns);
                        fallback_state.rewind_last_turns(turns);
                    }
                    _ => {}
                }
            }
            "response_item" => {
                if obj
                    .get("payload")
                    .and_then(codex_payload_user_text)
                    .is_some()
                {
                    fallback_state.record_next_turn();
                }
            }
            _ => {}
        }
    }

    Some(if saw_user_message_event {
        event_state
    } else {
        fallback_state
    })
}

/// Resolve external agent backend from an explicit override, falling back to
/// the project config's `agent.default_backend` setting.
fn resolve_agent_backend_from_config(
    explicit: Option<external_agent::AgentBackend>,
    project: &Project,
) -> Option<external_agent::AgentBackend> {
    explicit.or_else(|| {
        project
            .config
            .agent
            .default_backend
            .as_ref()
            .and_then(|s| external_agent::AgentBackend::from_str_loose(s))
    })
}

/// Structural equality for `CodexRuntimeConfig`. The struct itself doesn't
/// derive `PartialEq` because it's a public API surface and we don't want to
/// commit to field-by-field equality semantics for external callers; inside
/// the daemon loop we just need to detect drift across tasks, so we compare
/// the Codex-locked fields explicitly. Any change here that affects the
/// spawned Codex thread (sandbox, approvals, model, reasoning effort, tool
/// set, sandbox permissions) has to force a rebuild because Codex latches
/// those at `thread/start`.
fn codex_runtime_config_equal(
    a: &control_plane::CodexRuntimeConfig,
    b: &control_plane::CodexRuntimeConfig,
) -> bool {
    a.command == b.command
        && a.sandbox == b.sandbox
        && a.approval_policy == b.approval_policy
        && a.model == b.model
        && a.reasoning_effort == b.reasoning_effort
        && a.web_search == b.web_search
        && a.network_access == b.network_access
        && a.writable_roots == b.writable_roots
}

/// Structural equality for `GeminiRuntimeConfig`. Every field here is a
/// command-line arg Gemini latches at process spawn, so any drift forces a
/// teardown + respawn on the next task.
fn gemini_runtime_config_equal(
    a: &control_plane::GeminiRuntimeConfig,
    b: &control_plane::GeminiRuntimeConfig,
) -> bool {
    a.model == b.model
        && a.approval_mode == b.approval_mode
        && a.sandbox == b.sandbox
        && a.extensions == b.extensions
        && a.allowed_mcp_servers == b.allowed_mcp_servers
        && a.include_directories == b.include_directories
        && a.debug == b.debug
}

fn normalize_diff_file_path(path: &str) -> Option<String> {
    let path = path.split('\t').next().unwrap_or(path).trim();
    if path == "/dev/null" {
        return None;
    }
    // Strip exactly one git-style `a/` or `b/` prefix. Codex sometimes
    // produces `b//home/...` (double slash) for absolute paths; that
    // becomes `/home/...` after the single-prefix strip.
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    if path.is_empty() {
        return None;
    }
    Some(path.to_string())
}

/// Extract file paths from a unified-diff header. Reads `+++ b/<path>` lines
/// (git-style), with `--- a/<path>` used as a fallback for pure-delete diffs
/// where the `+++` side is `/dev/null`. Deduplicates while preserving order.
///
/// Used when the external agent's own `files_changed` list is empty, which
/// has been observed for Codex's `turn/diff/updated` notifications in
/// practice — the wire protocol carries the paths only inside the diff body.
fn parse_diff_file_paths(unified_diff: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in unified_diff.lines() {
        let path = if let Some(rest) = line.strip_prefix("+++ ") {
            rest
        } else if let Some(rest) = line.strip_prefix("--- ") {
            rest
        } else {
            continue;
        };
        if let Some(path) = normalize_diff_file_path(path) {
            if !out.iter().any(|p| p == &path) {
                out.push(path);
            }
        }
    }
    out
}

fn diff_line_text(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn is_unified_file_boundary(lines: &[&str], idx: usize) -> bool {
    let line = diff_line_text(lines[idx]);
    line.starts_with("diff --git ")
        || (line.starts_with("--- ")
            && lines
                .get(idx + 1)
                .is_some_and(|next| diff_line_text(next).starts_with("+++ ")))
}

fn split_unified_diff_by_file(unified_diff: &str) -> Vec<(String, String)> {
    if unified_diff.trim().is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<&str> = unified_diff.split_inclusive('\n').collect();
    if lines.is_empty() {
        lines.push(unified_diff);
    }

    let mut starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| {
            diff_line_text(line)
                .starts_with("diff --git ")
                .then_some(idx)
        })
        .collect();
    if starts.is_empty() {
        for idx in 0..lines.len() {
            if is_unified_file_boundary(&lines, idx) {
                starts.push(idx);
            }
        }
    }
    if starts.is_empty() {
        let files = parse_diff_file_paths(unified_diff);
        return files
            .into_iter()
            .next()
            .map(|path| vec![(path, unified_diff.to_string())])
            .unwrap_or_default();
    }

    let mut out = Vec::new();
    for (i, start) in starts.iter().copied().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(lines.len());
        let block = lines[start..end].concat();
        if let Some(path) = parse_diff_file_paths(&block).into_iter().next() {
            out.push((path, block));
        }
    }
    out
}

fn external_diff_log_body(message: &str) -> Option<&str> {
    if !message.starts_with("External agent diff") {
        return None;
    }
    let first_line_end = message.find('\n')?;
    let body = &message[first_line_end + 1..];
    if body.contains("diff --git ") || body.contains("--- ") || body.contains("@@ ") {
        Some(body)
    } else {
        None
    }
}

fn parse_session_diff_file_paths(log_dir: &Path) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(log_dir.join("session.jsonl")) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(message) = value.get("message").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(diff_body) = external_diff_log_body(message) else {
            continue;
        };
        for path in parse_diff_file_paths(diff_body) {
            if !out.iter().any(|p| p == &path) {
                out.push(path);
            }
        }
    }
    out
}

fn resolve_diff_file_path(project_root: &Path, display_path: &str) -> Option<PathBuf> {
    let path = Path::new(display_path);
    if path.is_absolute() {
        return (path.starts_with(project_root)
            || path.starts_with("/tmp")
            || path.starts_with("/private/tmp"))
        .then(|| path.to_path_buf());
    }

    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return None;
    }

    Some(project_root.join(path))
}

fn read_diff_file_text(project_root: &Path, display_path: &str) -> Option<Option<String>> {
    let path = resolve_diff_file_path(project_root, display_path)?;
    match std::fs::read_to_string(path) {
        Ok(text) => Some(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

struct ExternalDiffDelta {
    files_changed: Vec<String>,
    unified_diff: String,
}

#[derive(Default)]
struct ExternalDiffDeltaTracker {
    snapshots: HashMap<String, Option<String>>,
}

impl ExternalDiffDeltaTracker {
    fn seed_current_paths<'a>(
        &mut self,
        project_root: &Path,
        paths: impl IntoIterator<Item = &'a str>,
    ) {
        for path in paths {
            let Some(path) = normalize_diff_file_path(path) else {
                continue;
            };
            let Some(current) = read_diff_file_text(project_root, &path) else {
                continue;
            };
            self.snapshots.insert(path, current);
        }
    }

    fn seed_from_session_log(&mut self, project_root: &Path, log_dir: &Path) {
        let paths = parse_session_diff_file_paths(log_dir);
        self.seed_current_paths(project_root, paths.iter().map(String::as_str));
    }

    fn delta(
        &mut self,
        project_root: &Path,
        files_changed: &[String],
        unified_diff: &str,
    ) -> Option<ExternalDiffDelta> {
        let mut ordered_paths = Vec::new();
        let mut seen = HashSet::new();
        let mut block_by_path = HashMap::new();

        for (path, block) in split_unified_diff_by_file(unified_diff) {
            if seen.insert(path.clone()) {
                ordered_paths.push(path.clone());
            }
            block_by_path.entry(path).or_insert(block);
        }

        for path in files_changed {
            if let Some(path) = normalize_diff_file_path(path) {
                if seen.insert(path.clone()) {
                    ordered_paths.push(path);
                }
            }
        }

        let mut previously_tracked: Vec<String> = self.snapshots.keys().cloned().collect();
        previously_tracked.sort();
        for path in previously_tracked {
            if seen.insert(path.clone()) {
                ordered_paths.push(path);
            }
        }

        let mut delta_diff = String::new();
        let mut delta_files = Vec::new();

        for path in ordered_paths {
            let current = read_diff_file_text(project_root, &path).flatten();
            let maybe_delta = if let Some(previous) = self.snapshots.get(&path) {
                if previous == &current {
                    None
                } else {
                    Some(file_watcher::compute_unified_diff(
                        previous.as_deref().unwrap_or(""),
                        current.as_deref().unwrap_or(""),
                        &path,
                    ))
                }
            } else if let Some(block) = block_by_path.get(&path) {
                Some(block.clone())
            } else {
                current
                    .as_ref()
                    .map(|text| file_watcher::compute_unified_diff("", text, &path))
            };

            self.snapshots.insert(path.clone(), current);

            let Some(file_delta) = maybe_delta else {
                continue;
            };
            if file_delta.trim().is_empty() {
                continue;
            }
            delta_files.push(path);
            delta_diff.push_str(&file_delta);
            if !delta_diff.ends_with('\n') {
                delta_diff.push('\n');
            }
        }

        if delta_diff.trim().is_empty() {
            None
        } else {
            Some(ExternalDiffDelta {
                files_changed: delta_files,
                unified_diff: delta_diff,
            })
        }
    }
}

/// Resolve external agent backend from shared state (written by the web UI),
/// falling back to the project config default.
async fn resolve_agent_backend(
    shared: &Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    project: &Project,
) -> Option<external_agent::AgentBackend> {
    resolve_agent_backend_from_config(shared.read().await.clone(), project)
}

/// Construct, initialize, and start a thread for an external agent backend.
///
/// Returns the agent, thread handle, and event receiver. The caller owns the
/// agent lifetime and is responsible for sending messages and draining events.
async fn create_external_agent(
    backend: &external_agent::AgentBackend,
    project: &Project,
    session_log: &SharedSessionLog,
    web_port: Option<u16>,
    resume_session: Option<String>,
) -> Result<
    (
        Box<dyn external_agent::ExternalAgent>,
        external_agent::AgentThread,
        tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    ),
    CallerError,
> {
    use external_agent::{AgentBackend, AgentConfig};

    let request_trace_dir = session_log
        .lock()
        .ok()
        .map(|log| log.dir().join("model-request-traces"));

    let (mut agent, config): (Box<dyn external_agent::ExternalAgent>, AgentConfig) = match backend {
        AgentBackend::Codex => {
            let cfg = &project.config.agent.codex;
            let sandbox_mode = project::normalize_sandbox_mode(&cfg.sandbox);
            let reasoning_effort =
                project::normalize_reasoning_effort(cfg.reasoning_effort.as_deref());
            let opts = external_agent::codex::CodexAgentOptions {
                reasoning_effort: reasoning_effort.clone(),
                web_search: cfg.web_search,
                network_access: cfg.network_access,
                writable_roots: cfg.writable_roots.clone(),
            };
            let agent = Box::new(external_agent::codex::CodexAgent::with_options(
                cfg.command.clone(),
                cfg.model.clone(),
                cfg.approval_policy.clone(),
                sandbox_mode.clone(),
                web_port,
                opts,
            ));
            let config = AgentConfig {
                model: cfg.model.clone(),
                working_dir: project.root.clone(),
                request_trace_dir: request_trace_dir.clone(),
                approval_policy: cfg.approval_policy.clone(),
                sandbox: sandbox_mode,
                reasoning_effort,
                web_search: cfg.web_search,
                network_access: cfg.network_access,
                writable_roots: cfg.writable_roots.clone(),
                web_port,
                resume_session: resume_session.clone(),
            };
            (agent, config)
        }
        AgentBackend::GeminiCli => {
            let cfg = &project.config.agent.gemini_cli;
            let approval_mode = project::normalize_gemini_approval_mode(&cfg.approval_mode);
            let launch = external_agent::gemini::GeminiLaunchConfig {
                model: cfg.model.clone(),
                approval_mode: approval_mode.clone(),
                sandbox: cfg.sandbox,
                extensions: cfg.extensions.clone(),
                allowed_mcp_servers: cfg.allowed_mcp_servers.clone(),
                include_directories: cfg.include_directories.clone(),
                debug: cfg.debug,
            };
            let agent = Box::new(external_agent::gemini::GeminiAgent::new(
                cfg.command.clone(),
                launch,
                web_port,
            ));
            let config = AgentConfig {
                model: cfg.model.clone(),
                working_dir: project.root.clone(),
                request_trace_dir: request_trace_dir.clone(),
                // `AgentConfig.approval_policy` is Codex's `-a` flag; for
                // Gemini we reuse the field as the ACP approval hint so the
                // sandbox/prompt layer can adjust if needed. Storing the
                // normalized Gemini approval mode keeps the two backends
                // schema-compatible at the trait level.
                approval_policy: approval_mode,
                sandbox: String::new(),
                reasoning_effort: None,
                web_search: false,
                network_access: false,
                writable_roots: Vec::new(),
                web_port,
                resume_session: resume_session.clone(),
            };
            (agent, config)
        }
        AgentBackend::ClaudeCode => {
            let cfg = &project.config.agent.claude_code;
            let agent = Box::new(external_agent::claude_code::ClaudeCodeAgent::new(
                cfg.command.clone(),
                cfg.model.clone(),
                cfg.permission_mode.clone(),
                cfg.allowed_tools.clone(),
                web_port,
            ));
            let config = AgentConfig {
                model: cfg.model.clone(),
                working_dir: project.root.clone(),
                request_trace_dir: request_trace_dir.clone(),
                approval_policy: cfg.permission_mode.clone(),
                sandbox: String::new(),
                reasoning_effort: None,
                web_search: false,
                network_access: false,
                writable_roots: Vec::new(),
                web_port,
                resume_session: resume_session.clone(),
            };
            (agent, config)
        }
    };

    let event_rx = agent.initialize(config).await?;
    slog(session_log, |l| l.debug("External agent initialized"));

    let thread = agent.start_thread().await?;
    slog(session_log, |l| {
        l.debug(&format!("External agent thread: {}", thread.thread_id))
    });

    Ok((agent, thread, event_rx))
}

/// Configuration for `drain_external_agent_events`.
struct DrainConfig<'a> {
    bus: &'a EventBus,
    session_id: Option<String>,
    alias_session_id: Option<String>,
    autonomy: SharedAutonomy,
    session_log: &'a SharedSessionLog,
    project_root: &'a Path,
    log_dir: &'a Path,
    approval_registry: &'a event::ApprovalRegistry,
    json_approval: Option<&'a JsonApprovalSlot>,
    agent_source: Option<String>,
    /// When true, `ToolStarted` just increments the turn counter without
    /// emitting `AgentStarted`. The presence path sets this to avoid
    /// duplicating the model reasoning that's already shown via ModelResponse.
    suppress_agent_started: bool,
    /// When true and no `json_approval` slot is set, auto-deny approval
    /// requests (headless mode with no interactive input).
    headless: bool,
    /// Shared context-injection queue. Fallback target when the backend
    /// does not support mid-turn steering — queued items are drained on
    /// the next turn's follow-up message path.
    context_injection: &'a event::ContextInjectionQueue,
}

struct PendingRuntimeSteer {
    session_id: Option<String>,
    id: String,
    text: String,
}

fn pending_runtime_steer_targets_session(
    pending: &PendingRuntimeSteer,
    session_id: &Option<String>,
) -> bool {
    pending.session_id.as_deref() == session_id.as_deref()
}

fn flush_pending_runtime_steers_for_session(
    bus: &EventBus,
    pending_runtime_steers: &mut std::collections::VecDeque<PendingRuntimeSteer>,
    session_id: &Option<String>,
) -> usize {
    let mut delivered = 0usize;
    let mut retained = std::collections::VecDeque::with_capacity(pending_runtime_steers.len());
    while let Some(pending) = pending_runtime_steers.pop_front() {
        if pending_runtime_steer_targets_session(&pending, session_id) {
            delivered += 1;
            bus.send(AppEvent::SteerDelivered {
                session_id: pending.session_id,
                id: pending.id,
                mid_turn: true,
            });
        } else {
            retained.push_back(pending);
        }
    }
    *pending_runtime_steers = retained;
    delivered
}

const EXTERNAL_CONTEXT_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);
const EXTERNAL_TOOL_OUTPUT_ACTIVITY_LIMIT: usize = 64 * 1024;

#[derive(Default)]
struct ExternalContextSnapshotState {
    last_key: Option<u64>,
    last_error: Option<String>,
}

#[derive(Default)]
struct ExternalToolOutputLimiter {
    items: std::collections::HashMap<String, ExternalToolOutputState>,
}

#[derive(Default)]
struct ExternalToolOutputState {
    emitted_bytes: usize,
    truncated: bool,
}

impl ExternalToolOutputLimiter {
    fn filter(&mut self, item_id: &str, text: String) -> Option<String> {
        if text.is_empty() {
            return None;
        }

        let key = if item_id.is_empty() {
            "<unknown>".to_string()
        } else {
            item_id.to_string()
        };
        let state = self.items.entry(key).or_default();

        if state.emitted_bytes >= EXTERNAL_TOOL_OUTPUT_ACTIVITY_LIMIT {
            if state.truncated {
                return None;
            }
            state.truncated = true;
            return Some(external_tool_output_truncation_notice());
        }

        let remaining = EXTERNAL_TOOL_OUTPUT_ACTIVITY_LIMIT - state.emitted_bytes;
        if text.len() <= remaining {
            state.emitted_bytes += text.len();
            return Some(text);
        }

        let split_at = char_boundary_at_or_before(&text, remaining);
        let mut out = text[..split_at].to_string();
        out.push_str(&external_tool_output_truncation_notice());
        state.emitted_bytes = EXTERNAL_TOOL_OUTPUT_ACTIVITY_LIMIT;
        state.truncated = true;
        Some(out)
    }

    fn complete(&mut self, item_id: &str) {
        let key = if item_id.is_empty() {
            "<unknown>"
        } else {
            item_id
        };
        self.items.remove(key);
    }
}

fn external_tool_output_truncation_notice() -> String {
    format!(
        "\n\n[output truncated by Intendant after {} KiB for this tool; further output is hidden from Activity]\n",
        EXTERNAL_TOOL_OUTPUT_ACTIVITY_LIMIT / 1024
    )
}

fn char_boundary_at_or_before(text: &str, max_bytes: usize) -> usize {
    if max_bytes >= text.len() {
        return text.len();
    }
    let mut idx = max_bytes;
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn external_tool_preview_text(tool_name: &str, preview: &str) -> Option<String> {
    let tool_name = tool_name.trim();
    let preview = preview.trim();
    match (tool_name.is_empty(), preview.is_empty()) {
        (true, true) => None,
        (true, false) => Some(preview.to_string()),
        (false, true) => Some(tool_name.to_string()),
        (false, false) => Some(format!("{tool_name}: {preview}")),
    }
}

fn external_agent_log_source(agent_source: Option<&str>) -> String {
    agent_source
        .filter(|source| !source.trim().is_empty())
        .unwrap_or("worker")
        .to_string()
}

fn external_tool_failure_content(
    item_id: &str,
    message: &str,
    tool_preview: Option<&str>,
) -> String {
    let preview = tool_preview.map(str::trim).filter(|s| !s.is_empty());
    let command = preview.and_then(|preview| preview.strip_prefix("command: ").map(str::trim));
    let label = if command.is_some() {
        "Command failed"
    } else {
        "Tool failed"
    };

    let mut content = if item_id.trim().is_empty() {
        format!("{label}: {message}")
    } else {
        format!("{label} ({item_id}): {message}")
    };

    if let Some(command) = command {
        content.push_str("\nCommand: ");
        content.push_str(command);
    } else if let Some(preview) = preview {
        content.push_str("\nTool: ");
        content.push_str(preview);
    }
    content
}

/// Result of draining one batch of external agent events.
enum DrainOutcome {
    /// The agent's turn completed. The caller decides how to continue
    /// (e.g., wait for follow-up, emit DoneSignal, break inner loop).
    TurnCompleted {
        message: Option<String>,
        turns_in_round: usize,
    },
    /// The agent process terminated.
    Terminated {
        reason: String,
        exit_code: Option<i32>,
    },
    /// The event channel was closed unexpectedly.
    ChannelClosed,
    /// A user-requested interrupt completed cleanly. The agent was asked to
    /// cancel its turn (e.g. via `session/cancel` or `turn/interrupt`) and
    /// acknowledged with a terminal event. The caller should break its
    /// outer loop the same way it would for `TurnCompleted`, but MUST NOT
    /// wait for a follow-up message — the interrupt *is* the follow-up.
    Interrupted { reason: String },
}

fn external_context_snapshot_key(snapshot: &external_agent::AgentContextSnapshot) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    snapshot.source.hash(&mut h);
    snapshot.label.hash(&mut h);
    snapshot.format.hash(&mut h);
    snapshot.token_count.hash(&mut h);
    snapshot.context_window.hash(&mut h);
    snapshot.item_count.hash(&mut h);
    match serde_json::to_vec(&snapshot.raw) {
        Ok(bytes) => bytes.hash(&mut h),
        Err(_) => snapshot.raw.to_string().hash(&mut h),
    }
    h.finish()
}

fn external_context_snapshot_turn(stats: &LoopStats) -> Option<usize> {
    if stats.turns > 0 {
        Some(stats.turns)
    } else {
        None
    }
}

async fn emit_external_context_snapshot_if_changed(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    config: &DrainConfig<'_>,
    turn: Option<usize>,
    state: &mut ExternalContextSnapshotState,
) {
    match agent.context_snapshot().await {
        Ok(Some(snapshot)) => {
            let key = external_context_snapshot_key(&snapshot);
            if state.last_key == Some(key) {
                state.last_error = None;
                return;
            }
            state.last_key = Some(key);
            state.last_error = None;
            config.bus.send(AppEvent::ContextSnapshot {
                session_id: config.session_id.clone(),
                source: snapshot.source,
                label: snapshot.label,
                turn,
                format: snapshot.format,
                token_count: snapshot.token_count,
                context_window: snapshot.context_window,
                item_count: snapshot.item_count,
                raw: snapshot.raw,
            });
        }
        Ok(None) => {
            state.last_error = None;
        }
        Err(e) => {
            let message = format!(
                "Failed to read context snapshot from {}: {}",
                agent.name(),
                e
            );
            if state.last_error.as_deref() != Some(message.as_str()) {
                slog(config.session_log, |l| l.warn(&message));
                state.last_error = Some(message);
            }
        }
    }
}

fn forked_thread_id_from_message(message: &str) -> Option<String> {
    message
        .strip_prefix("forked into thread ")
        .map(str::trim)
        .filter(|id| !id.is_empty() && *id != "(unknown)")
        .map(str::to_string)
}

enum ExternalThreadActionEffect {
    None,
    SideTurnStarted {
        parent_thread_id: String,
        child_thread_id: String,
        prompt: Option<String>,
    },
    SideTurnClosed {
        child_thread_id: String,
    },
}

fn side_thread_ids_from_message(message: &str) -> Option<(String, String)> {
    let rest = message.strip_prefix("side conversation started in thread ")?;
    let (child, parent) = rest.split_once(" from parent ")?;
    let child = child.trim();
    let parent = parent.trim();
    if child.is_empty() || parent.is_empty() {
        return None;
    }
    Some((parent.to_string(), child.to_string()))
}

fn fork_session_name_from_params(params: &serde_json::Value) -> Option<String> {
    params
        .get("name")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn side_session_prompt_from_params(params: &serde_json::Value) -> Option<String> {
    ["prompt", "message", "text", "task"]
        .iter()
        .find_map(|key| params.get(*key).and_then(|v| v.as_str()))
        .or_else(|| params.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn side_child_thread_id_from_params(params: &serde_json::Value) -> Option<String> {
    params
        .get("threadId")
        .or_else(|| params.get("thread_id"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn thread_id_from_action_params(params: &serde_json::Value) -> Option<String> {
    params
        .pointer("/thread/id")
        .and_then(|value| value.as_str())
        .or_else(|| params.pointer("/threadId").and_then(|value| value.as_str()))
        .or_else(|| {
            params
                .pointer("/thread_id")
                .and_then(|value| value.as_str())
        })
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn thread_action_params_with_thread_id(
    op: &str,
    params: serde_json::Value,
    thread_id: Option<&str>,
) -> serde_json::Value {
    if thread_id_from_action_params(&params).is_some() {
        return params;
    }

    let Some(thread_id) = thread_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return params;
    };

    match params {
        serde_json::Value::Object(mut obj) => {
            obj.insert(
                "threadId".to_string(),
                serde_json::Value::String(thread_id.to_string()),
            );
            serde_json::Value::Object(obj)
        }
        serde_json::Value::Null => serde_json::json!({ "threadId": thread_id }),
        serde_json::Value::String(prompt) if matches!(op, "side" | "btw") => {
            serde_json::json!({
                "threadId": thread_id,
                "prompt": prompt,
            })
        }
        other => other,
    }
}

fn thread_action_params_for_target(
    op: &str,
    params: serde_json::Value,
    target_session_id: &Option<String>,
    config: &DrainConfig<'_>,
) -> serde_json::Value {
    if thread_id_from_action_params(&params).is_some() {
        return params;
    }

    let target = target_session_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .or(config.session_id.as_deref());
    let thread_id = target.map(|target| {
        if config.alias_session_id.as_deref() == Some(target) {
            config.session_id.as_deref().unwrap_or(target)
        } else {
            target
        }
    });

    thread_action_params_with_thread_id(op, params, thread_id)
}

fn emit_session_relationship(
    bus: &EventBus,
    parent_session_id: Option<&str>,
    child_session_id: &str,
    relationship: &str,
    ephemeral: bool,
) {
    let Some(parent_session_id) = parent_session_id.map(str::trim).filter(|id| !id.is_empty())
    else {
        return;
    };
    if parent_session_id == child_session_id {
        return;
    }
    bus.send(AppEvent::SessionRelationship {
        parent_session_id: parent_session_id.to_string(),
        child_session_id: child_session_id.to_string(),
        relationship: relationship.to_string(),
        ephemeral,
    });
}

fn emit_codex_fork_session_name(bus: &EventBus, child_id: &str, params: &serde_json::Value) {
    let Some(name) = fork_session_name_from_params(params) else {
        return;
    };
    bus.send(AppEvent::ControlCommand(event::ControlMsg::RenameSession {
        source: Some("codex".to_string()),
        session_id: child_id.to_string(),
        backend_session_id: Some(child_id.to_string()),
        name,
    }));
}

async fn handle_external_thread_action(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    op: String,
    params: serde_json::Value,
    target_session_id: Option<String>,
    config: &DrainConfig<'_>,
) -> ExternalThreadActionEffect {
    let params = thread_action_params_for_target(&op, params, &target_session_id, config);
    let action_thread_id = thread_id_from_action_params(&params);
    let result_session_id = target_session_id.or_else(|| config.session_id.clone());
    let result = agent
        .thread_action(&op, &params)
        .await
        .map_err(|e| e.to_string());
    let (success, message) = match result {
        Ok(msg) => (true, msg),
        Err(e) => (false, e),
    };
    slog(config.session_log, |l| {
        l.info(&format!(
            "Codex thread action /{}: {} — {}",
            op,
            if success { "ok" } else { "FAILED" },
            message
        ))
    });
    config.bus.send(AppEvent::CodexThreadActionResult {
        session_id: result_session_id.clone(),
        action: op.clone(),
        success,
        message: message.clone(),
    });

    if success && op == "fork" {
        if let Some(child_id) = forked_thread_id_from_message(&message) {
            emit_codex_fork_session_name(config.bus, &child_id, &params);
            emit_session_relationship(
                config.bus,
                action_thread_id
                    .as_deref()
                    .or(result_session_id.as_deref())
                    .or(config.session_id.as_deref()),
                &child_id,
                "fork",
                false,
            );
            config
                .bus
                .send(AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
                    source: "codex".to_string(),
                    session_id: child_id.clone(),
                    resume_id: Some(child_id),
                    project_root: Some(config.project_root.to_string_lossy().to_string()),
                    task: None,
                    direct: Some(true),
                }));
        }
    }

    if success && op == "side" {
        if let Some((parent_thread_id, child_thread_id)) = side_thread_ids_from_message(&message) {
            return ExternalThreadActionEffect::SideTurnStarted {
                parent_thread_id,
                child_thread_id,
                prompt: side_session_prompt_from_params(&params),
            };
        }
    }

    if success && matches!(op.as_str(), "side-close" | "side_close") {
        if let Some(child_thread_id) = side_child_thread_id_from_params(&params) {
            config.bus.send(AppEvent::SessionEnded {
                session_id: child_thread_id.clone(),
                reason: "side conversation closed".to_string(),
            });
            return ExternalThreadActionEffect::SideTurnClosed { child_thread_id };
        }
    }

    ExternalThreadActionEffect::None
}

fn undo_turns_from_params(params: &serde_json::Value) -> u32 {
    params.get("turns").and_then(|v| v.as_u64()).unwrap_or(1) as u32
}

fn side_rewind_first_turn_for_undo(
    current_turn_count: usize,
    turns: u32,
    side_thread_id: &str,
) -> Result<u32, String> {
    if turns == 0 {
        return Err("rollback count must be at least 1".to_string());
    }
    if turns as usize > current_turn_count {
        return Err(format!(
            "Cannot /undo {} turn(s) in side conversation {}; only {} side turn(s) exist after the /side boundary",
            turns, side_thread_id, current_turn_count
        ));
    }
    Ok(current_turn_count as u32 - turns + 1)
}

fn parent_rewind_first_turn_for_undo(current_turn_count: usize, turns: u32) -> Result<u32, String> {
    if turns == 0 {
        return Err("rollback count must be at least 1".to_string());
    }
    if turns as usize > current_turn_count {
        return Err(format!(
            "Cannot /undo {} turn(s); only {} user turn(s) are active",
            turns, current_turn_count
        ));
    }
    Ok(current_turn_count as u32 - turns + 1)
}

async fn rollback_parent_thread_from_turn(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    round: &mut usize,
    user_turn_revisions: &mut UserTurnRevisionState,
    first_user_turn_index: u32,
    config: &DrainConfig<'_>,
) -> Result<u32, String> {
    if first_user_turn_index == 0 {
        return Err("Cannot rewind user turn 0".to_string());
    }
    if first_user_turn_index as usize > *round {
        return Err(format!(
            "Cannot rewind to user turn {}; current user turn count is {}",
            first_user_turn_index, *round
        ));
    }

    let turns_to_drop = *round as u32 - first_user_turn_index + 1;
    agent
        .rollback_turns(turns_to_drop)
        .await
        .map_err(|e| format!("thread rollback failed: {}", e))?;

    user_turn_revisions.rewind_from_turn(first_user_turn_index);
    *round = first_user_turn_index.saturating_sub(1) as usize;
    config.bus.send(AppEvent::UserMessageRewind {
        session_id: config.session_id.clone(),
        user_turn_index: first_user_turn_index,
        turns_removed: turns_to_drop,
    });
    Ok(turns_to_drop)
}

async fn handle_parent_undo_thread_action(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    round: &mut usize,
    user_turn_revisions: &mut UserTurnRevisionState,
    params: serde_json::Value,
    config: &DrainConfig<'_>,
) {
    let turns = undo_turns_from_params(&params);
    let result = match parent_rewind_first_turn_for_undo(*round, turns) {
        Ok(first_user_turn_index) => rollback_parent_thread_from_turn(
            agent,
            round,
            user_turn_revisions,
            first_user_turn_index,
            config,
        )
        .await
        .map(|turns_removed| format!("rolled back {} turn(s)", turns_removed)),
        Err(message) => Err(message),
    };

    let (success, message) = match result {
        Ok(message) => (true, message),
        Err(message) => (false, message),
    };
    slog(config.session_log, |l| {
        l.info(&format!(
            "Codex thread action /undo: {} — {}",
            if success { "ok" } else { "FAILED" },
            message
        ))
    });
    config.bus.send(AppEvent::CodexThreadActionResult {
        session_id: config.session_id.clone(),
        action: "undo".to_string(),
        success,
        message,
    });
}

async fn rollback_side_thread_from_turn(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    side_rounds: &mut HashMap<String, usize>,
    side_turn_revisions: &mut HashMap<String, UserTurnRevisionState>,
    side_thread_id: &str,
    first_user_turn_index: u32,
    config: &DrainConfig<'_>,
) -> Result<u32, String> {
    if first_user_turn_index == 0 {
        return Err(format!(
            "Cannot rewind side conversation {}; user turn index must be at least 1",
            side_thread_id
        ));
    }

    let current_turn_count = *side_rounds.entry(side_thread_id.to_string()).or_insert(1);
    if first_user_turn_index as usize > current_turn_count {
        return Err(format!(
            "Cannot rewind side conversation {} to user turn {}; current side user turn count is {}",
            side_thread_id, first_user_turn_index, current_turn_count
        ));
    }

    let turns_to_drop = current_turn_count as u32 - first_user_turn_index + 1;
    agent
        .rollback_thread_turns(side_thread_id, turns_to_drop)
        .await
        .map_err(|e| format!("thread rollback failed: {}", e))?;

    let revisions = side_turn_revisions
        .entry(side_thread_id.to_string())
        .or_default();
    revisions.seed_active_turns_to(current_turn_count as u32);
    revisions.rewind_from_turn(first_user_turn_index);
    side_rounds.insert(
        side_thread_id.to_string(),
        first_user_turn_index.saturating_sub(1) as usize,
    );
    config.bus.send(AppEvent::UserMessageRewind {
        session_id: Some(side_thread_id.to_string()),
        user_turn_index: first_user_turn_index,
        turns_removed: turns_to_drop,
    });
    Ok(turns_to_drop)
}

async fn handle_side_undo_thread_action(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    side_rounds: &mut HashMap<String, usize>,
    side_turn_revisions: &mut HashMap<String, UserTurnRevisionState>,
    side_thread_id: &str,
    params: serde_json::Value,
    config: &DrainConfig<'_>,
) {
    let turns = undo_turns_from_params(&params);
    let current_turn_count = *side_rounds.entry(side_thread_id.to_string()).or_insert(1);
    let result = match side_rewind_first_turn_for_undo(current_turn_count, turns, side_thread_id) {
        Ok(first_user_turn_index) => rollback_side_thread_from_turn(
            agent,
            side_rounds,
            side_turn_revisions,
            side_thread_id,
            first_user_turn_index,
            config,
        )
        .await
        .map(|turns_removed| format!("rolled back {} turn(s)", turns_removed)),
        Err(message) => Err(message),
    };

    let (success, message) = match result {
        Ok(message) => (true, message),
        Err(message) => (false, message),
    };
    slog(config.session_log, |l| {
        l.info(&format!(
            "Codex side thread action /undo: {} — {}",
            if success { "ok" } else { "FAILED" },
            message
        ))
    });
    config.bus.send(AppEvent::CodexThreadActionResult {
        session_id: Some(side_thread_id.to_string()),
        action: "undo".to_string(),
        success,
        message,
    });
}

fn emit_side_session_started(
    config: &DrainConfig<'_>,
    parent_thread_id: &str,
    child_thread_id: &str,
    prompt: Option<&str>,
) {
    slog(config.session_log, |l| {
        l.info(&format!(
            "Codex /side: side conversation started in thread {} from parent {}",
            child_thread_id, parent_thread_id
        ))
    });
    config.bus.send(AppEvent::SessionStarted {
        session_id: child_thread_id.to_string(),
        task: Some(
            prompt
                .filter(|text| !text.trim().is_empty())
                .unwrap_or("Side conversation")
                .to_string(),
        ),
    });
    config.bus.send(AppEvent::SessionIdentity {
        session_id: child_thread_id.to_string(),
        source: "codex".to_string(),
        backend_session_id: child_thread_id.to_string(),
    });
    let parent_session_id = config.session_id.as_deref().unwrap_or(parent_thread_id);
    emit_session_relationship(
        config.bus,
        Some(parent_session_id),
        child_thread_id,
        "side",
        true,
    );
}

fn emit_codex_subagent_started(
    config: &DrainConfig<'_>,
    parent_thread_id: &str,
    child_thread_id: &str,
    prompt: Option<&str>,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) {
    let child_thread_id = child_thread_id.trim();
    if child_thread_id.is_empty() {
        return;
    }
    let parent_thread_id = parent_thread_id.trim();
    let parent_session_id = if parent_thread_id.is_empty() {
        config.session_id.as_deref().unwrap_or("")
    } else {
        parent_thread_id
    };
    if parent_session_id.is_empty() || parent_session_id == child_thread_id {
        return;
    }

    config.bus.send(AppEvent::SessionIdentity {
        session_id: child_thread_id.to_string(),
        source: "codex".to_string(),
        backend_session_id: child_thread_id.to_string(),
    });
    emit_session_relationship(
        config.bus,
        Some(parent_session_id),
        child_thread_id,
        "subagent",
        false,
    );
    config.bus.send(AppEvent::SessionCapabilities {
        session_id: child_thread_id.to_string(),
        capabilities: types::SessionCapabilities {
            follow_up: true,
            steer: false,
            interrupt: false,
            codex_thread_actions: Vec::new(),
        },
    });
    config.bus.send(AppEvent::SessionStarted {
        session_id: child_thread_id.to_string(),
        task: Some(
            prompt
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("Codex subagent")
                .to_string(),
        ),
    });

    let mut details = Vec::new();
    if let Some(model) = model.map(str::trim).filter(|s| !s.is_empty()) {
        details.push(format!("model {model}"));
    }
    if let Some(effort) = reasoning_effort.map(str::trim).filter(|s| !s.is_empty()) {
        details.push(format!("reasoning {effort}"));
    }
    let suffix = if details.is_empty() {
        String::new()
    } else {
        format!(" ({})", details.join(", "))
    };
    let content = prompt
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|prompt| format!("Codex subagent started{suffix}: {prompt}"))
        .unwrap_or_else(|| format!("Codex subagent started{suffix}"));
    config.bus.send(AppEvent::LogEntry {
        session_id: Some(child_thread_id.to_string()),
        level: "agent".to_string(),
        source: "Codex".to_string(),
        content,
        turn: None,
    });
}

fn emit_codex_subagent_state(config: &DrainConfig<'_>, state: &external_agent::SubAgentState) {
    let thread_id = state.thread_id.trim();
    if thread_id.is_empty() {
        return;
    }
    let status = state.status.trim();
    let message = state
        .message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (level, content) = match status {
        "completed" => (
            "info",
            message
                .map(|message| format!("Task complete: Codex subagent completed: {message}"))
                .unwrap_or_else(|| "Task complete: Codex subagent completed".to_string()),
        ),
        "interrupted" => (
            "warn",
            message
                .map(|message| format!("Agent interrupted: Codex subagent interrupted: {message}"))
                .unwrap_or_else(|| "Agent interrupted: Codex subagent interrupted".to_string()),
        ),
        "errored" => (
            "warn",
            message
                .map(|message| format!("Session ended: Codex subagent errored: {message}"))
                .unwrap_or_else(|| "Session ended: Codex subagent errored".to_string()),
        ),
        "shutdown" => (
            "info",
            message
                .map(|message| format!("Session ended: Codex subagent shut down: {message}"))
                .unwrap_or_else(|| "Session ended: Codex subagent shut down".to_string()),
        ),
        "notFound" => (
            "warn",
            message
                .map(|message| format!("Session ended: Codex subagent not found: {message}"))
                .unwrap_or_else(|| "Session ended: Codex subagent not found".to_string()),
        ),
        "pendingInit" | "running" => return,
        other => (
            "info",
            message
                .map(|message| format!("Codex subagent {other}: {message}"))
                .unwrap_or_else(|| format!("Codex subagent {other}")),
        ),
    };
    config.bus.send(AppEvent::LogEntry {
        session_id: Some(thread_id.to_string()),
        level: level.to_string(),
        source: "Codex".to_string(),
        content,
        turn: None,
    });
}

fn codex_subagent_terminal_reason(state: &external_agent::SubAgentState) -> Option<String> {
    let status = state.status.trim();
    let message = state
        .message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match status {
        "completed" => Some(
            message
                .map(|message| format!("Codex subagent completed: {message}"))
                .unwrap_or_else(|| "Codex subagent completed".to_string()),
        ),
        "interrupted" => Some(
            message
                .map(|message| format!("Codex subagent interrupted: {message}"))
                .unwrap_or_else(|| "Codex subagent interrupted".to_string()),
        ),
        "errored" => Some(
            message
                .map(|message| format!("Codex subagent errored: {message}"))
                .unwrap_or_else(|| "Codex subagent errored".to_string()),
        ),
        "shutdown" => Some(
            message
                .map(|message| format!("Codex subagent shut down: {message}"))
                .unwrap_or_else(|| "Codex subagent shut down".to_string()),
        ),
        "notFound" => Some(
            message
                .map(|message| format!("Codex subagent not found: {message}"))
                .unwrap_or_else(|| "Codex subagent not found".to_string()),
        ),
        _ => None,
    }
}

fn emit_codex_subagent_terminal(
    config: &DrainConfig<'_>,
    stats: &mut LoopStats,
    state: &external_agent::SubAgentState,
) {
    let thread_id = state.thread_id.trim();
    if thread_id.is_empty() {
        return;
    }
    let Some(reason) = codex_subagent_terminal_reason(state) else {
        return;
    };
    if !stats
        .codex_subagent_terminal_sessions
        .insert(thread_id.to_string())
    {
        return;
    }

    if state.status.trim() == "interrupted" {
        config.bus.send(AppEvent::Interrupted {
            session_id: Some(thread_id.to_string()),
            reason,
        });
    } else {
        config.bus.send(AppEvent::SessionEnded {
            session_id: thread_id.to_string(),
            reason,
        });
    }
}

fn json_u32_field(value: &serde_json::Value, key: &str) -> Option<u32> {
    value
        .get(key)
        .and_then(|v| v.as_u64())
        .and_then(|v| u32::try_from(v).ok())
}

fn emit_codex_subagent_transcript_entry(
    config: &DrainConfig<'_>,
    child_thread_id: &str,
    entry: &serde_json::Value,
) {
    let content = entry
        .get("content")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(content) = content else {
        return;
    };
    let source = entry
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("codex");
    if source.eq_ignore_ascii_case("user") {
        config.bus.send(AppEvent::UserMessageLog {
            session_id: Some(child_thread_id.to_string()),
            content: content.to_string(),
            user_turn_index: json_u32_field(entry, "user_turn_index"),
            user_turn_revision: json_u32_field(entry, "user_turn_revision"),
            replacement_for_user_turn_index: json_u32_field(
                entry,
                "replacement_for_user_turn_index",
            ),
        });
        return;
    }

    config.bus.send(AppEvent::LogEntry {
        session_id: Some(child_thread_id.to_string()),
        level: entry
            .get("level")
            .and_then(|v| v.as_str())
            .unwrap_or("info")
            .to_string(),
        source: source.to_string(),
        content: content.to_string(),
        turn: None,
    });
}

fn emit_codex_subagent_transcript_updates(
    config: &DrainConfig<'_>,
    stats: &mut LoopStats,
    child_thread_id: &str,
) {
    let child_thread_id = child_thread_id.trim();
    if child_thread_id.is_empty() {
        return;
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()));
    let Some(entries) =
        crate::web_gateway::external_session_entries_from_home(&home, "codex", child_thread_id)
    else {
        return;
    };

    let offset = stats
        .codex_subagent_transcript_offsets
        .entry(child_thread_id.to_string())
        .or_insert(0);
    if *offset > entries.len() {
        *offset = 0;
    }
    for entry in entries.iter().skip(*offset) {
        emit_codex_subagent_transcript_entry(config, child_thread_id, entry);
    }
    *offset = entries.len();
}

fn codex_subagent_thread_ids(
    receiver_thread_ids: &[String],
    agents: &[external_agent::SubAgentState],
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut ids = Vec::new();
    for id in receiver_thread_ids
        .iter()
        .map(String::as_str)
        .chain(agents.iter().map(|state| state.thread_id.as_str()))
    {
        let id = id.trim();
        if !id.is_empty() && seen.insert(id.to_string()) {
            ids.push(id.to_string());
        }
    }
    ids
}

fn short_external_session_id(session_id: &str) -> String {
    session_id.chars().take(8).collect()
}

fn collab_agent_tool_preview(
    tool: &str,
    receiver_thread_ids: &[String],
    prompt: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    let receivers: Vec<String> = receiver_thread_ids
        .iter()
        .map(|id| short_external_session_id(id))
        .collect();
    if !receivers.is_empty() {
        parts.push(format!("target {}", receivers.join(", ")));
    }
    if let Some(prompt) = prompt.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(prompt.chars().take(120).collect());
    }
    if parts.is_empty() {
        tool.to_string()
    } else {
        format!("{}: {}", tool, parts.join(" - "))
    }
}

async fn drain_external_child_turn(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    bus_rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
    config: &DrainConfig<'_>,
    stats: &mut LoopStats,
    diff_tracker: &mut ExternalDiffDeltaTracker,
    pending_runtime_steers: &mut std::collections::VecDeque<PendingRuntimeSteer>,
    child_thread_id: String,
    conversation_kind: &str,
) {
    slog(config.session_log, |l| {
        l.info(&format!(
            "Draining Codex {} conversation {}",
            conversation_kind, child_thread_id
        ))
    });

    let child_session_id = Some(child_thread_id.clone());
    let child_config = DrainConfig {
        bus: config.bus,
        session_id: child_session_id.clone(),
        alias_session_id: None,
        autonomy: config.autonomy.clone(),
        session_log: config.session_log,
        project_root: config.project_root,
        log_dir: config.log_dir,
        approval_registry: config.approval_registry,
        json_approval: config.json_approval.clone(),
        agent_source: config.agent_source.clone(),
        suppress_agent_started: config.suppress_agent_started,
        headless: config.headless,
        context_injection: config.context_injection,
    };

    match drain_external_agent_events(
        agent,
        event_rx,
        bus_rx,
        &child_config,
        stats,
        diff_tracker,
        pending_runtime_steers,
    )
    .await
    {
        DrainOutcome::TurnCompleted { message, .. } => {
            if let Some(message) = message {
                child_config.bus.send(AppEvent::LogEntry {
                    session_id: child_config.session_id.clone(),
                    level: "info".to_string(),
                    source: "Codex".to_string(),
                    content: message,
                    turn: None,
                });
            }
            child_config.bus.send(AppEvent::LogEntry {
                session_id: child_config.session_id.clone(),
                level: "info".to_string(),
                source: "Codex".to_string(),
                content: format!(
                    "Round complete: {} conversation ready for follow-up",
                    conversation_kind
                ),
                turn: None,
            });
        }
        DrainOutcome::Interrupted { reason } => {
            child_config.bus.send(AppEvent::LogEntry {
                session_id: child_config.session_id.clone(),
                level: "warn".to_string(),
                source: "Codex".to_string(),
                content: format!(
                    "Agent interrupted: {} conversation stopped: {}",
                    conversation_kind, reason
                ),
                turn: None,
            });
        }
        DrainOutcome::Terminated { reason, exit_code } => {
            slog(config.session_log, |l| {
                l.warn(&format!(
                    "Codex terminated during {} conversation: {} (exit code: {:?})",
                    conversation_kind, reason, exit_code
                ))
            });
        }
        DrainOutcome::ChannelClosed => {
            slog(config.session_log, |l| {
                l.warn(&format!(
                    "Codex {} conversation event channel closed",
                    conversation_kind
                ))
            });
        }
    }
}

fn provider_request_item_count(raw: &serde_json::Value) -> Option<usize> {
    for key in ["input", "messages", "contents"] {
        if let Some(items) = raw.get(key).and_then(|v| v.as_array()) {
            return Some(items.len());
        }
    }
    None
}

/// Drain external agent events until a turn completes, the agent terminates,
/// or the channel closes.
///
/// This is the unified event loop shared by both the presence path
/// (`run_with_presence`) and the non-presence path (`run_external_agent_mode`).
///
/// Also subscribes to the event bus for `AppEvent::InterruptRequested` and
/// forwards it to the external agent via `ExternalAgent::interrupt_turn()`.
/// Backends that don't support interruption return a typed error we log and
/// continue waiting for — the caller can escalate to `shutdown()` if needed.
async fn drain_external_agent_events(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    bus_rx: &mut tokio::sync::broadcast::Receiver<AppEvent>,
    config: &DrainConfig<'_>,
    stats: &mut LoopStats,
    diff_tracker: &mut ExternalDiffDeltaTracker,
    pending_runtime_steers: &mut std::collections::VecDeque<PendingRuntimeSteer>,
) -> DrainOutcome {
    use std::sync::atomic::Ordering;

    let approval_counter = std::sync::atomic::AtomicU64::new(1);
    let mut turns_in_round = 0usize;
    let local_session_id = config.session_id.clone();
    let alias_session_id = config.alias_session_id.clone();
    // Track whether we've been asked to interrupt this drain cycle. When the
    // agent finally emits TurnCompleted / Terminated we convert that into a
    // DrainOutcome::Interrupted + Interrupted event so the caller can choose
    // not to wait for a follow-up.
    let mut interrupt_pending = false;
    // Last `DiffUpdated` content hash we wrote to the session log. Codex
    // re-fires `turn/diff/updated` on every internal state change (patch
    // apply, exec, approval, turn recompute), so within one drain we commonly
    // see 2-4 identical emissions per real file write. We dedupe on the
    // unified-diff bytes: if nothing changed, don't spam session.jsonl.
    let mut last_diff_hash: Option<u64> = None;
    let mut context_snapshot_state = ExternalContextSnapshotState::default();
    let mut tool_output_limiter = ExternalToolOutputLimiter::default();
    let mut tool_previews: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut context_snapshot_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + EXTERNAL_CONTEXT_SNAPSHOT_INTERVAL,
        EXTERNAL_CONTEXT_SNAPSHOT_INTERVAL,
    );
    context_snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let post_turn_sleep = tokio::time::sleep(EXTERNAL_POST_TURN_DRAIN_GRACE);
    tokio::pin!(post_turn_sleep);
    let mut post_turn_sleep_active = false;
    let mut pending_turn_completion: Option<(Option<String>, usize)> = None;

    // Background watcher: if an interrupt arrives while an approval handler
    // below is blocked on `rx.await`, we need to drain the approval registry
    // from outside the main select! so the waiting handler unblocks. Draining
    // from the main select! wouldn't help — we can't re-enter select! until
    // the handler returns.
    //
    // The watcher only drains the *native* registry. The caller-facing bus_rx
    // receives the same InterruptRequested event (broadcast fans out) and the
    // main select! handles the actual `interrupt_turn()` call once the inner
    // approval handler has unblocked and returned.
    let watcher_handle = {
        let mut watcher_rx = config.bus.subscribe();
        let registry = config.approval_registry.clone();
        let watcher_session_id = local_session_id.clone();
        let watcher_alias_session_id = alias_session_id.clone();
        tokio::spawn(async move {
            loop {
                match watcher_rx.recv().await {
                    Ok(AppEvent::InterruptRequested { session_id })
                        if event_targets_session_or_alias(
                            &session_id,
                            &watcher_session_id,
                            &watcher_alias_session_id,
                        ) =>
                    {
                        let pending: Vec<_> = {
                            let mut reg = registry.lock().unwrap();
                            reg.drain().collect()
                        };
                        for (_, sender) in pending {
                            let _ = sender.send(event::ApprovalResponse::Deny);
                        }
                        // Stay alive — a second interrupt could arrive after
                        // a follow-up turn starts new approvals.
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };
    // Abort the watcher when this drain returns. Use a guard so drop runs on
    // any exit (normal return, panic, early return from each match arm).
    struct DrainWatcherGuard {
        handle: Option<tokio::task::JoinHandle<()>>,
    }
    impl Drop for DrainWatcherGuard {
        fn drop(&mut self) {
            if let Some(h) = self.handle.take() {
                h.abort();
            }
        }
    }
    let _watcher_guard = DrainWatcherGuard {
        handle: Some(watcher_handle),
    };

    loop {
        let event = tokio::select! {
            biased;
            bus_event = bus_rx.recv() => {
                match bus_event {
                    Ok(AppEvent::InterruptRequested { session_id })
                        if event_targets_session_or_alias(
                            &session_id,
                            &local_session_id,
                            &alias_session_id,
                        ) =>
                    {
                        interrupt_pending = true;
                        // Approval registry is drained by the background
                        // watcher task above (so inner `rx.await` sites
                        // unblock even when select! is occupied). Here we
                        // only need to forward the interrupt to the backend.
                        // For backends that don't support mid-turn cancel
                        // (Claude Code) we log a warning and keep waiting —
                        // the next interrupt could escalate to shutdown, but
                        // that's a caller policy decision.
                        match agent.interrupt_turn().await {
                            Ok(()) => {
                                config.bus.send(AppEvent::PresenceLog {
                                    message: format!("Interrupt sent to {}", agent.name()),
                                    level: None,
                                    turn: None,
                                });
                            }
                            Err(e) => {
                                config.bus.send(AppEvent::PresenceLog {
                                    message: format!(
                                        "Interrupt not supported or failed for {}: {}",
                                        agent.name(), e
                                    ),
                                    level: Some(types::LogLevel::Warn),
                                    turn: None,
                                });
                                slog(config.session_log, |l| {
                                    l.warn(&format!(
                                        "Interrupt failed for {}: {}", agent.name(), e
                                    ))
                                });
                            }
                        }
                        continue;
                    }
                    Ok(AppEvent::SteerRequested {
                        session_id,
                        text,
                        id,
                    }) if event_targets_session_or_alias(
                        &session_id,
                        &local_session_id,
                        &alias_session_id,
                    ) => {
                        // Try native mid-turn steering first. On success the
                        // backend/runtime has accepted the steer for the
                        // active turn, but it may only surface to the model at
                        // the backend's next checkpoint. We keep tracking it
                        // until the adapter observes the echoed user message.
                        // On failure (unsupported or no active turn), fall
                        // back to queuing onto context_injection — the drain-between-turns path in
                        // `run_external_agent_mode` / `run_with_presence`
                        // will flush it as a follow-up prefix on the next
                        // user message and emit SteerDelivered at that point.
                        match agent.steer_turn(&text).await {
                            Ok(()) => {
                                pending_runtime_steers.push_back(PendingRuntimeSteer {
                                    session_id: local_session_id.clone(),
                                    id: id.clone(),
                                    text: text.clone(),
                                });
                                let reason = format!(
                                    "{} accepted the steer; waiting for the next runtime checkpoint",
                                    agent.name()
                                );
                                slog(config.session_log, |l| {
                                    l.info(&format!("Steer accepted by {}", agent.name()))
                                });
                                config.bus.send(AppEvent::SteerAccepted {
                                    session_id: local_session_id.clone(),
                                    id,
                                    reason,
                                });
                            }
                            Err(e) => {
                                let reason = format!(
                                    "{} doesn't support mid-turn steering ({}); queued as follow-up",
                                    agent.name(), e
                                );
                                if let Ok(mut q) = config.context_injection.lock() {
                                    q.push(event::ContextInjection::text_with_steer_id(
                                        text.clone(),
                                        id.clone(),
                                    ));
                                }
                                slog(config.session_log, |l| l.info(&reason));
                                config.bus.send(AppEvent::SteerQueued {
                                    session_id: local_session_id.clone(),
                                    id,
                                    reason,
                                });
                            }
                        }
                        continue;
                    }
                    Ok(AppEvent::CodexThreadActionRequested {
                        session_id,
                        action,
                        params,
                    }) if event_targets_session_or_alias(
                        &session_id,
                        &local_session_id,
                        &alias_session_id,
                    ) => {
                        if action == "undo" {
                            let message =
                                "/undo is only available between turns for this session"
                                    .to_string();
                            config.bus.send(AppEvent::CodexThreadActionResult {
                                session_id: local_session_id.clone(),
                                action,
                                success: false,
                                message,
                            });
                            continue;
                        }
                        handle_external_thread_action(agent, action, params, session_id, config)
                            .await;
                        continue;
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Bus closed means the session is shutting down;
                        // fall through to let the agent channel drain.
                        continue;
                    }
                }
            }
            _ = context_snapshot_tick.tick() => {
                emit_external_context_snapshot_if_changed(
                    agent,
                    config,
                    external_context_snapshot_turn(stats),
                    &mut context_snapshot_state,
                )
                .await;
                continue;
            }
            maybe_event = event_rx.recv() => {
                match maybe_event {
                    Some(e) => e,
                    None if pending_turn_completion.is_some() => {
                        let (message, turns_in_round) = pending_turn_completion
                            .take()
                            .expect("checked pending turn completion");
                        return DrainOutcome::TurnCompleted {
                            message,
                            turns_in_round,
                        };
                    }
                    None => return DrainOutcome::ChannelClosed,
                }
            }
            _ = &mut post_turn_sleep, if post_turn_sleep_active => {
                let (message, turns_in_round) = pending_turn_completion
                    .take()
                    .expect("post-turn sleep active only while completion is pending");
                return DrainOutcome::TurnCompleted {
                    message,
                    turns_in_round,
                };
            }
        };

        match event {
            external_agent::AgentEvent::MessageDelta { text } => {
                config.bus.send(AppEvent::ModelResponseDelta {
                    session_id: config.session_id.clone(),
                    text,
                });
            }
            external_agent::AgentEvent::Message { text } => {
                stats.last_response = Some(text.clone());
                config.bus.send(AppEvent::ModelResponse {
                    session_id: config.session_id.clone(),
                    turn: stats.turns,
                    content: text,
                    usage: provider::TokenUsage::default(),
                    reasoning: None,
                    source: config.agent_source.clone(),
                });
            }
            external_agent::AgentEvent::UserMessage { text } => {
                if let Some(pos) = pending_runtime_steers.iter().position(|pending| {
                    pending_runtime_steer_targets_session(pending, &local_session_id)
                        && (pending.text == text || pending.text.trim() == text.trim())
                }) {
                    let Some(pending) = pending_runtime_steers.remove(pos) else {
                        continue;
                    };
                    slog(config.session_log, |l| {
                        l.info(&format!("Steer observed in {} conversation", agent.name()))
                    });
                    config.bus.send(AppEvent::SteerDelivered {
                        session_id: pending.session_id.or_else(|| local_session_id.clone()),
                        id: pending.id,
                        mid_turn: true,
                    });
                }
            }
            external_agent::AgentEvent::Reasoning { text } => {
                // Surface reasoning via ModelResponse with empty content +
                // reasoning set.  WASM renders this at "detail" verbosity
                // (visible in Verbose + Debug, hidden in Normal) via the
                // existing reasoning_summary path in app_state.rs.
                config.bus.send(AppEvent::ModelResponse {
                    session_id: config.session_id.clone(),
                    turn: stats.turns,
                    content: String::new(),
                    usage: provider::TokenUsage::default(),
                    reasoning: Some(text),
                    source: config.agent_source.clone(),
                });
            }
            external_agent::AgentEvent::PlanUpdate { entries } => {
                let mut md = String::from("**Plan**\n");
                for (content, _priority, status) in &entries {
                    let marker = match status.as_str() {
                        "completed" => "[x]",
                        "inprogress" => "[-]",
                        _ => "[ ]",
                    };
                    md.push_str(&format!("- {} {}\n", marker, content));
                }
                config.bus.send(AppEvent::ModelResponse {
                    session_id: config.session_id.clone(),
                    turn: stats.turns,
                    content: md,
                    usage: provider::TokenUsage::default(),
                    reasoning: None,
                    source: config.agent_source.clone(),
                });
            }
            external_agent::AgentEvent::Usage { usage } => {
                stats.usage.prompt_tokens = usage.prompt_tokens;
                stats.usage.completion_tokens = usage.completion_tokens;
                stats.usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
                stats.usage.cached_tokens = usage.cached_tokens;
                config.bus.send(AppEvent::UsageSnapshot {
                    session_id: config.session_id.clone(),
                    main: frontend::ModelUsageSnapshot {
                        provider: usage.provider,
                        model: usage.model,
                        tokens_used: usage.tokens_used,
                        context_window: usage.context_window,
                        usage_pct: usage.usage_pct,
                        prompt_tokens: usage.prompt_tokens,
                        completion_tokens: usage.completion_tokens,
                        cached_tokens: usage.cached_tokens,
                    },
                    presence: None,
                });
            }
            external_agent::AgentEvent::Log { level, message } => {
                slog(config.session_log, |l| match level.as_str() {
                    "warn" => l.warn(&message),
                    "error" => l.error(&message),
                    _ => l.info(&message),
                });
                config.bus.send(AppEvent::LogEntry {
                    session_id: config.session_id.clone(),
                    level,
                    source: config
                        .agent_source
                        .clone()
                        .unwrap_or_else(|| "worker".to_string()),
                    content: message,
                    turn: None,
                });
            }
            external_agent::AgentEvent::SubAgentToolCall {
                item_id,
                tool,
                status,
                sender_thread_id,
                receiver_thread_ids,
                prompt,
                model,
                reasoning_effort,
                agents,
            } => {
                let prompt_ref = prompt.as_deref();
                let subagent_thread_ids = codex_subagent_thread_ids(&receiver_thread_ids, &agents);
                if status == "inProgress" {
                    turns_in_round += 1;
                    if !config.suppress_agent_started {
                        stats.turns += 1;
                        config.bus.send(AppEvent::AgentStarted {
                            session_id: config.session_id.clone(),
                            turn: stats.turns,
                            commands_preview: collab_agent_tool_preview(
                                &tool,
                                &receiver_thread_ids,
                                prompt_ref,
                            ),
                            source: config.agent_source.clone(),
                        });
                    }
                }

                for child_thread_id in &subagent_thread_ids {
                    let child_thread_id = child_thread_id.trim();
                    if child_thread_id.is_empty() || child_thread_id == sender_thread_id.trim() {
                        continue;
                    }
                    let sender_thread_id = sender_thread_id.trim();
                    if !sender_thread_id.is_empty() {
                        stats
                            .codex_subagent_parent_threads
                            .entry(child_thread_id.to_string())
                            .or_insert_with(|| sender_thread_id.to_string());
                    }
                    if stats
                        .codex_subagent_sessions
                        .insert(child_thread_id.to_string())
                    {
                        emit_codex_subagent_started(
                            config,
                            sender_thread_id,
                            child_thread_id,
                            prompt_ref,
                            model.as_deref(),
                            reasoning_effort.as_deref(),
                        );
                    }
                    emit_codex_subagent_transcript_updates(config, stats, child_thread_id);
                }

                if status == "failed" {
                    let item_id = item_id.trim();
                    let item_suffix = if item_id.is_empty() {
                        String::new()
                    } else {
                        format!(" ({item_id})")
                    };
                    let content = format!(
                        "Codex subagent tool {}{} failed{}",
                        tool,
                        item_suffix,
                        prompt_ref
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(|p| format!(": {p}"))
                            .unwrap_or_default()
                    );
                    slog(config.session_log, |l| l.warn(&content));
                    config.bus.send(AppEvent::LogEntry {
                        session_id: config.session_id.clone(),
                        level: "warn".to_string(),
                        source: external_agent_log_source(config.agent_source.as_deref()),
                        content,
                        turn: None,
                    });
                }

                for state in &agents {
                    emit_codex_subagent_state(config, state);
                    emit_codex_subagent_terminal(config, stats, state);
                }

                emit_external_context_snapshot_if_changed(
                    agent,
                    config,
                    external_context_snapshot_turn(stats),
                    &mut context_snapshot_state,
                )
                .await;
            }
            external_agent::AgentEvent::ToolStarted {
                item_id,
                preview,
                tool_name,
            } => {
                turns_in_round += 1;
                if let Some(preview_text) = external_tool_preview_text(&tool_name, &preview) {
                    if !item_id.is_empty() {
                        tool_previews.insert(item_id, preview_text);
                    }
                }
                if !config.suppress_agent_started {
                    stats.turns += 1;
                    let preview_text = format!("{}: {}", tool_name, preview);
                    config.bus.send(AppEvent::AgentStarted {
                        session_id: config.session_id.clone(),
                        turn: stats.turns,
                        commands_preview: preview_text,
                        source: config.agent_source.clone(),
                    });
                }
                emit_external_context_snapshot_if_changed(
                    agent,
                    config,
                    external_context_snapshot_turn(stats),
                    &mut context_snapshot_state,
                )
                .await;
            }
            external_agent::AgentEvent::ToolOutputDelta { item_id, text } => {
                // Gemini CLI strips images from ACP, sending "[Image: image/png]".
                // Substitute with the latest screenshot from disk so the Activity
                // tab can render it.
                let stdout = if text.contains("[Image: image/png]")
                    || text.contains("[Image: image/jpeg]")
                {
                    substitute_screenshot_from_disk(&text, config.log_dir)
                } else {
                    text
                };
                if let Some(stdout) = tool_output_limiter.filter(&item_id, stdout) {
                    let output_id = event::next_agent_output_id();
                    slog(config.session_log, |l| {
                        l.agent_output_with_id(
                            &stdout,
                            "",
                            config.agent_source.as_deref(),
                            Some(&output_id),
                        )
                    });
                    config.bus.send(AppEvent::AgentOutput {
                        session_id: config.session_id.clone(),
                        stdout,
                        stderr: String::new(),
                        source: config.agent_source.clone(),
                        output_id: Some(output_id),
                    });
                }
            }
            external_agent::AgentEvent::ToolCompleted { item_id, status } => {
                tool_output_limiter.complete(&item_id);
                let tool_preview = tool_previews.remove(&item_id);
                // Success: nothing to emit.  The tool command was already
                // shown via AgentStarted at start, and any output streamed
                // via ToolOutputDelta → AgentOutput.  A completion marker
                // adds noise without new information.
                //
                // Failure: emit a warn so the user sees the error.
                // Cancelled: silent.
                match &status {
                    external_agent::ToolCompletionStatus::Failed { message } => {
                        let content = external_tool_failure_content(
                            &item_id,
                            message,
                            tool_preview.as_deref(),
                        );
                        slog(config.session_log, |l| l.warn(&content));
                        config.bus.send(AppEvent::LogEntry {
                            session_id: config.session_id.clone(),
                            level: "warn".to_string(),
                            source: external_agent_log_source(config.agent_source.as_deref()),
                            content,
                            turn: None,
                        });
                    }
                    external_agent::ToolCompletionStatus::Success
                    | external_agent::ToolCompletionStatus::Cancelled => {}
                }
            }
            external_agent::AgentEvent::ApprovalRequest {
                request_id,
                command,
                category,
            } => {
                emit_external_context_snapshot_if_changed(
                    agent,
                    config,
                    external_context_snapshot_turn(stats),
                    &mut context_snapshot_state,
                )
                .await;
                let cat = match category {
                    external_agent::ApprovalCategory::CommandExecution => {
                        autonomy::ActionCategory::CommandExec
                    }
                    external_agent::ApprovalCategory::FileChange => {
                        autonomy::ActionCategory::FileWrite
                    }
                };
                let needs = { config.autonomy.read().await.needs_approval(cat) };
                if !needs {
                    config.bus.send(AppEvent::AutoApproved {
                        preview: command.clone(),
                    });
                    slog(config.session_log, |l| l.auto_approved(&command));
                    if let Err(e) = agent
                        .resolve_approval(&request_id, external_agent::ApprovalDecision::Accept)
                        .await
                    {
                        slog(config.session_log, |l| {
                            l.warn(&format!("Failed to auto-approve: {}", e))
                        });
                    }
                } else if config.headless && config.json_approval.is_none() {
                    slog(config.session_log, |l| {
                        l.warn(&format!("Headless auto-deny: {}", command))
                    });
                    config.bus.send(AppEvent::ApprovalResolved {
                        session_id: config.session_id.clone(),
                        id: 0,
                        action: "deny".to_string(),
                    });
                    let _ = agent
                        .resolve_approval(&request_id, external_agent::ApprovalDecision::Decline)
                        .await;
                } else {
                    let id = approval_counter.fetch_add(1, Ordering::Relaxed);
                    config.bus.send(AppEvent::ApprovalRequired {
                        session_id: config.session_id.clone(),
                        id,
                        command_preview: command.clone(),
                        category: cat,
                    });

                    let rx = if let Some(ref slot) = config.json_approval {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            let mut guard = slot.lock().unwrap();
                            *guard = Some((id, tx));
                        }
                        rx
                    } else {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            config.approval_registry.lock().unwrap().insert(id, tx);
                        }
                        rx
                    };

                    match rx.await {
                        Ok(response) => {
                            let (decision, action_str) = match response {
                                event::ApprovalResponse::Approve => {
                                    (external_agent::ApprovalDecision::Accept, "approve")
                                }
                                event::ApprovalResponse::ApproveAll => (
                                    external_agent::ApprovalDecision::AcceptForSession,
                                    "approve_all",
                                ),
                                event::ApprovalResponse::Deny => {
                                    (external_agent::ApprovalDecision::Decline, "deny")
                                }
                                event::ApprovalResponse::Skip => {
                                    (external_agent::ApprovalDecision::Cancel, "skip")
                                }
                            };
                            config.bus.send(AppEvent::ApprovalResolved {
                                session_id: config.session_id.clone(),
                                id,
                                action: action_str.to_string(),
                            });
                            slog(config.session_log, |l| l.approval_resolved(id, action_str));
                            if let Err(e) = agent.resolve_approval(&request_id, decision).await {
                                slog(config.session_log, |l| {
                                    l.warn(&format!("Failed to resolve approval: {}", e))
                                });
                            }
                        }
                        Err(_) => {
                            slog(config.session_log, |l| {
                                l.warn("Approval channel closed, denying")
                            });
                            let _ = agent
                                .resolve_approval(
                                    &request_id,
                                    external_agent::ApprovalDecision::Decline,
                                )
                                .await;
                        }
                    }
                }
            }
            external_agent::AgentEvent::FileApprovalRequest {
                request_id,
                path,
                diff,
            } => {
                let cat = autonomy::ActionCategory::FileWrite;
                let needs = { config.autonomy.read().await.needs_approval(cat) };
                let preview = format!("file change: {}", path);

                if !needs {
                    config.bus.send(AppEvent::AutoApproved {
                        preview: preview.clone(),
                    });
                    slog(config.session_log, |l| l.auto_approved(&preview));
                    if let Err(e) = agent
                        .resolve_approval(&request_id, external_agent::ApprovalDecision::Accept)
                        .await
                    {
                        slog(config.session_log, |l| {
                            l.warn(&format!("Failed to auto-approve file change: {}", e))
                        });
                    }
                } else if config.headless && config.json_approval.is_none() {
                    slog(config.session_log, |l| {
                        l.warn(&format!("Headless auto-deny: {}", preview))
                    });
                    config.bus.send(AppEvent::ApprovalResolved {
                        session_id: config.session_id.clone(),
                        id: 0,
                        action: "deny".to_string(),
                    });
                    let _ = agent
                        .resolve_approval(&request_id, external_agent::ApprovalDecision::Decline)
                        .await;
                } else {
                    let id = approval_counter.fetch_add(1, Ordering::Relaxed);
                    config.bus.send(AppEvent::ApprovalRequired {
                        session_id: config.session_id.clone(),
                        id,
                        command_preview: format!("{}\n{}", preview, diff),
                        category: cat,
                    });

                    let rx = if let Some(ref slot) = config.json_approval {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            let mut guard = slot.lock().unwrap();
                            *guard = Some((id, tx));
                        }
                        rx
                    } else {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            config.approval_registry.lock().unwrap().insert(id, tx);
                        }
                        rx
                    };

                    match rx.await {
                        Ok(response) => {
                            let (decision, action_str) = match response {
                                event::ApprovalResponse::Approve => {
                                    (external_agent::ApprovalDecision::Accept, "approve")
                                }
                                event::ApprovalResponse::ApproveAll => (
                                    external_agent::ApprovalDecision::AcceptForSession,
                                    "approve_all",
                                ),
                                event::ApprovalResponse::Deny => {
                                    (external_agent::ApprovalDecision::Decline, "deny")
                                }
                                event::ApprovalResponse::Skip => {
                                    (external_agent::ApprovalDecision::Cancel, "skip")
                                }
                            };
                            config.bus.send(AppEvent::ApprovalResolved {
                                session_id: config.session_id.clone(),
                                id,
                                action: action_str.to_string(),
                            });
                            slog(config.session_log, |l| l.approval_resolved(id, action_str));
                            if let Err(e) = agent.resolve_approval(&request_id, decision).await {
                                slog(config.session_log, |l| {
                                    l.warn(&format!("Failed to resolve file approval: {}", e))
                                });
                            }
                        }
                        Err(_) => {
                            slog(config.session_log, |l| {
                                l.warn("File approval channel closed, denying")
                            });
                            let _ = agent
                                .resolve_approval(
                                    &request_id,
                                    external_agent::ApprovalDecision::Decline,
                                )
                                .await;
                        }
                    }
                }
            }
            external_agent::AgentEvent::DiffUpdated {
                files_changed,
                unified_diff,
            } => {
                let hash = {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};
                    let mut h = DefaultHasher::new();
                    unified_diff.hash(&mut h);
                    h.finish()
                };
                if last_diff_hash == Some(hash) {
                    // Identical to the previous emission — skip.
                } else {
                    last_diff_hash = Some(hash);
                    let Some(delta) =
                        diff_tracker.delta(config.project_root, &files_changed, &unified_diff)
                    else {
                        continue;
                    };
                    // Prefer the file paths from the unified diff header
                    // (`+++ b/<path>`) because `files_changed` from Codex is
                    // frequently empty in practice. Fall back to the agent's
                    // own list if parsing the diff yields nothing.
                    let parsed_files = parse_diff_file_paths(&delta.unified_diff);
                    let files = if parsed_files.is_empty() {
                        delta.files_changed
                    } else {
                        parsed_files
                    };
                    let header = if files.is_empty() {
                        "External agent diff".to_string()
                    } else if files.len() == 1 {
                        format!("External agent diff: {}", files[0])
                    } else {
                        format!(
                            "External agent diff: {} files ({})",
                            files.len(),
                            files.join(", ")
                        )
                    };
                    let diff_content = format!(
                        "# intendant-project-root: {}\n{}",
                        config.project_root.display(),
                        delta.unified_diff
                    );
                    slog(config.session_log, |l| {
                        l.info(&format!("{}\n{}", header, diff_content));
                    });
                    if !delta.unified_diff.trim().is_empty() {
                        config.bus.send(AppEvent::LogEntry {
                            session_id: config.session_id.clone(),
                            level: "info".to_string(),
                            source: "Diff".to_string(),
                            content: diff_content,
                            turn: None,
                        });
                    }
                }
            }
            external_agent::AgentEvent::TurnCompleted { message } => {
                if let Some(ref msg) = message {
                    stats.last_response = Some(msg.clone());
                }
                emit_external_context_snapshot_if_changed(
                    agent,
                    config,
                    external_context_snapshot_turn(stats),
                    &mut context_snapshot_state,
                )
                .await;
                if interrupt_pending {
                    let reason = message
                        .clone()
                        .unwrap_or_else(|| "user requested".to_string());
                    config.bus.send(AppEvent::Interrupted {
                        session_id: config.session_id.clone(),
                        reason: "user requested".into(),
                    });
                    return DrainOutcome::Interrupted { reason };
                }
                let delivered = flush_pending_runtime_steers_for_session(
                    config.bus,
                    pending_runtime_steers,
                    &local_session_id,
                );
                if delivered > 0 {
                    slog(config.session_log, |l| {
                        l.info(&format!(
                            "Marked {} accepted {} steer(s) delivered at turn completion",
                            delivered,
                            agent.name()
                        ))
                    });
                }
                pending_turn_completion = Some((message, turns_in_round));
                post_turn_sleep_active = true;
                post_turn_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + EXTERNAL_POST_TURN_DRAIN_GRACE);
                continue;
            }
            external_agent::AgentEvent::Terminated { reason, exit_code } => {
                if interrupt_pending {
                    config.bus.send(AppEvent::Interrupted {
                        session_id: config.session_id.clone(),
                        reason: "user requested".into(),
                    });
                    return DrainOutcome::Interrupted {
                        reason: format!("terminated after interrupt: {}", reason),
                    };
                }
                return DrainOutcome::Terminated { reason, exit_code };
            }
        }
    }
}

/// Drain queued steer items from `context_injection` and merge them into a
/// follow-up user message bound for an external agent.
///
/// Only drains items whose `steer_id` is `Some(_)` — those are the entries
/// that the steer fallback path pushed. Other queue sources (display
/// takeover, presence annotations) are left in place for the native
/// drain-between-turns path used by the internal agent loop.
///
/// For each drained item, emits `AppEvent::SteerDelivered { mid_turn: false }`
/// so the dashboard can retire its pending-steer UI row. The returned
/// string interleaves queued steers (prefixed with `[User]`) above the
/// caller's `followup` text — the result is sent as a single external agent
/// message so the agent sees both in the same turn's input.
///
/// When `followup` is empty the return is `None`, meaning "nothing to send"
/// — this lets callers avoid an empty `send_message` when the follow-up
/// was purely a delivery of queued steers (the external agent loop does
/// not distinguish "steer only" from "user message", so we skip the send
/// and wait for the next follow-up).
fn drain_steer_queue_as_followup(
    context_injection: &event::ContextInjectionQueue,
    followup: &str,
    bus: &EventBus,
    session_id: Option<&str>,
) -> Option<String> {
    let mut prefix_lines: Vec<String> = Vec::new();
    if let Ok(mut q) = context_injection.lock() {
        // Partition: keep non-steer entries, pull out steer entries.
        let mut kept = Vec::with_capacity(q.len());
        for inj in q.drain(..) {
            if inj.steer_id.is_some() {
                prefix_lines.push(format!("[User] {}", inj.text));
                let id = inj.steer_id.clone().unwrap_or_default();
                bus.send(AppEvent::SteerDelivered {
                    session_id: session_id.map(str::to_string),
                    id,
                    mid_turn: false,
                });
            } else {
                kept.push(inj);
            }
        }
        *q = kept;
    }
    if prefix_lines.is_empty() && followup.is_empty() {
        return None;
    }
    if prefix_lines.is_empty() {
        Some(followup.to_string())
    } else if followup.is_empty() {
        Some(prefix_lines.join("\n"))
    } else {
        Some(format!("{}\n{}", prefix_lines.join("\n"), followup))
    }
}

fn emit_follow_up_status(
    bus: &EventBus,
    session_id: Option<&str>,
    id: &Option<String>,
    text: Option<&str>,
    status: &str,
    reason: Option<&str>,
) {
    let Some(id) = id.as_deref().map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    bus.send(AppEvent::FollowUpStatus {
        session_id: session_id.map(str::to_string),
        id: id.to_string(),
        text: text.map(str::to_string),
        status: status.to_string(),
        reason: reason.map(str::to_string),
    });
}

fn codex_subagent_parent_threads_from_log(log_dir: &std::path::Path) -> HashMap<String, String> {
    let path = log_dir.join("session.jsonl");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };

    let mut parents = HashMap::new();
    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if entry.get("event").and_then(|v| v.as_str()) != Some("session_relationship") {
            continue;
        }
        let Some(data) = entry.get("data") else {
            continue;
        };
        let relationship = data
            .get("relationship")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if relationship != "subagent" {
            continue;
        }
        let parent = data
            .get("parent_session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let child = data
            .get("child_session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if parent.is_empty() || child.is_empty() || parent == child {
            continue;
        }
        parents.insert(child.to_string(), parent.to_string());
    }
    parents
}

/// Configuration for `run_daemon_loop`.
struct DaemonConfig {
    bus: EventBus,
    project_root: PathBuf,
    autonomy: SharedAutonomy,
    shared_external_agent: Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    shared_codex_config: control_plane::SharedCodexConfig,
    shared_gemini_config: control_plane::SharedGeminiConfig,
    frame_registry: Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    web_port: Option<u16>,
    flags_direct: bool,
    /// Optional shared session state for headless mode (cleared between tasks).
    shared_session: Option<web_gateway::SharedActiveSession>,
}

/// Daemon loop shared by the TUI post-exit path and the headless web-gateway path.
///
/// Waits for `StartTask` and `SetExternalAgent` control messages from the web
/// UI, spawning agent tasks in the background. Exits when the bus closes.
///
/// Ctrl+C is handled by the global signal handler installed in `main`, which
/// writes `mark_interrupted` to the session meta and calls `exit(130)` — we
/// deliberately do not also listen for it here because racing two handlers
/// risked the loop `break`ing before the meta update ran.
async fn run_daemon_loop(config: DaemonConfig) {
    session_supervisor::SessionSupervisor::new(session_supervisor::SessionSupervisorConfig {
        bus: config.bus,
        project_root: config.project_root,
        autonomy: config.autonomy,
        shared_external_agent: config.shared_external_agent,
        shared_codex_config: config.shared_codex_config,
        shared_gemini_config: config.shared_gemini_config,
        frame_registry: config.frame_registry,
        web_port: config.web_port,
        flags_direct: config.flags_direct,
        shared_session: config.shared_session,
    })
    .run()
    .await;
}

const SAFETY_CAP: usize = 500;
const MIN_BUDGET_TOKENS: u64 = 4096;
const BUDGET_WARNING_THRESHOLD: f64 = 0.85;
const EXTERNAL_POST_TURN_DRAIN_GRACE: Duration = Duration::from_millis(750);

/// Why the agent loop exited after a round.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoopExitReason {
    /// Agent sent an explicit done signal.
    DoneSignal,
    /// Task completed (no JSON, no commands, etc.).
    TaskComplete,
    /// Context budget exhausted.
    BudgetExhausted,
    /// Hit the safety cap of 500 turns.
    SafetyCapReached,
    /// User denied a command.
    Denied,
    /// An error occurred.
    Error,
    /// User requested interruption mid-turn.
    Interrupted,
}

#[derive(Debug, Clone, Default)]
struct LoopStats {
    turns: usize,
    rounds: usize,
    usage: provider::TokenUsage,
    codex_subagent_sessions: std::collections::HashSet<String>,
    codex_subagent_parent_threads: std::collections::HashMap<String, String>,
    codex_subagent_rounds: std::collections::HashMap<String, usize>,
    codex_subagent_terminal_sessions: std::collections::HashSet<String>,
    codex_subagent_transcript_offsets: std::collections::HashMap<String, usize>,
    /// Last model response content (for sub-agent result summaries).
    last_response: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct UserAttachments {
    items: Vec<external_agent::AgentAttachment>,
}

impl UserAttachments {
    fn from_items(items: Vec<external_agent::AgentAttachment>) -> Self {
        Self { items }
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn len(&self) -> usize {
        self.items.len()
    }

    fn conversation_images(&self) -> Vec<conversation::ImageData> {
        self.items
            .iter()
            .filter_map(|att| match att {
                external_agent::AgentAttachment::Image(img) => Some(conversation::ImageData {
                    media_type: img.mime_type.clone(),
                    data: img.base64.clone(),
                }),
                external_agent::AgentAttachment::File(_) => None,
            })
            .collect()
    }

    fn text_with_file_prelude(&self, text: &str) -> String {
        let files: Vec<&external_agent::AgentFileAttachment> = self
            .items
            .iter()
            .filter_map(|att| match att {
                external_agent::AgentAttachment::File(file) => Some(file),
                external_agent::AgentAttachment::Image(_) => None,
            })
            .collect();
        let prelude = external_agent::format_file_attachments_prelude(&files);
        if prelude.is_empty() {
            text.to_string()
        } else {
            format!("{}{}", prelude, text)
        }
    }
}

#[derive(Debug, Clone, Default)]
struct FollowUpMessage {
    text: String,
    attachments: UserAttachments,
    steer_id: Option<String>,
    follow_up_id: Option<String>,
    edit_user_turn_index: Option<u32>,
    edit_user_turn_revision: Option<u32>,
    target_session_id: Option<String>,
}

impl FollowUpMessage {
    fn text(text: String) -> Self {
        Self {
            text,
            attachments: UserAttachments::default(),
            steer_id: None,
            follow_up_id: None,
            edit_user_turn_index: None,
            edit_user_turn_revision: None,
            target_session_id: None,
        }
    }

    fn with_attachments(text: String, attachments: UserAttachments) -> Self {
        Self {
            text,
            attachments,
            steer_id: None,
            follow_up_id: None,
            edit_user_turn_index: None,
            edit_user_turn_revision: None,
            target_session_id: None,
        }
    }

    fn steer(text: String, attachments: UserAttachments, steer_id: String) -> Self {
        Self {
            text,
            attachments,
            steer_id: Some(steer_id),
            follow_up_id: None,
            edit_user_turn_index: None,
            edit_user_turn_revision: None,
            target_session_id: None,
        }
    }

    fn edit_user_message(
        text: String,
        attachments: UserAttachments,
        user_turn_index: u32,
        user_turn_revision: u32,
    ) -> Self {
        Self {
            text,
            attachments,
            steer_id: None,
            follow_up_id: None,
            edit_user_turn_index: Some(user_turn_index),
            edit_user_turn_revision: Some(user_turn_revision),
            target_session_id: None,
        }
    }

    fn for_target(mut self, target_session_id: Option<String>) -> Self {
        self.target_session_id = target_session_id;
        self
    }

    fn with_follow_up_id(mut self, follow_up_id: Option<String>) -> Self {
        self.follow_up_id = follow_up_id;
        self
    }
}

type FollowUpReceiver = tokio::sync::mpsc::Receiver<FollowUpMessage>;

/// CLI flags parsed from command-line arguments.
struct CliFlags {
    task: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    verbose: bool,
    no_tui: bool,
    mcp: bool,
    autonomy: AutonomyLevel,
    log_file: Option<String>,
    /// --continue / -c: resume the most recent session for this project.
    continue_last: bool,
    /// --resume / -r [id]: resume a specific session by ID or path.
    resume_id: Option<String>,
    control_socket: bool,
    /// --json: Emit JSONL events to stdout (implies --no-tui).
    json_output: bool,
    /// --sandbox: Enable Landlock filesystem sandboxing for the runtime.
    #[allow(dead_code)]
    sandbox: bool,
    /// --direct: Force single-agent mode (skip orchestrator/sub-agent delegation).
    /// Does NOT disable the TUI — use --no-tui for headless output.
    direct: bool,
    /// --no-presence: Disable the presence layer (direct agent interaction).
    no_presence: bool,
    /// --web [PORT]: Serve TUI via web (xterm.js + optional voice).
    web: bool,
    web_port: u16,
    /// --tls: Serve the `--web` dashboard over HTTPS/WSS. Off by default
    /// (plain HTTP). ORs with `[server.tls] enabled` in intendant.toml.
    /// With no cert/key override, a self-signed cert is minted at startup
    /// (SAN = bind IP + localhost, optional config hostname).
    tls: bool,
    /// --tls-cert <PATH>: PEM cert (chain) overriding the auto self-signed
    /// cert. Must be paired with `--tls-key`. Implies `--tls`.
    tls_cert: Option<String>,
    /// --tls-key <PATH>: PEM private key matching `--tls-cert`.
    tls_key: Option<String>,
    /// --transcription: Enable user speech transcription.
    transcription: bool,
    /// --record-display <ID>: Record an existing X11 display (repeatable).
    record_displays: Vec<u32>,

    /// --agent <BACKEND>: Use external agent backend (codex, claude-code).
    agent_backend: Option<external_agent::AgentBackend>,

    /// --no-web: Disable web gateway (on by default).
    no_web: bool,

    /// --advertise-url <URL>: WebSocket URL to advertise in this daemon's
    /// Agent Card (repeatable). Each occurrence appends one URL in the
    /// preference order they're given. When non-empty, the entire list
    /// replaces both the `[server.advertise]` config value and the
    /// auto-detected single URL — operator at the CLI wins.
    advertise_urls: Vec<String>,
}

fn print_help() {
    println!("intendant - AI agent runtime with process lifecycle management");
    println!();
    println!("USAGE:");
    println!("    intendant [OPTIONS] [TASK]");
    println!("    echo \"task\" | intendant [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("    --provider <NAME>     API provider (openai, anthropic, or gemini)");
    println!("    --model <NAME>        Model name to use");
    println!("    --autonomy <LEVEL>    Autonomy level: low, medium, high, full");
    println!("    --log-file <DIR>      Override session log directory (default: ~/.intendant/logs/<uuid>/)");
    println!("    --continue, -c        Resume the most recent session for this project");
    println!("    --resume, -r [ID]     Resume a specific session by ID, prefix, or path");
    println!("    --no-tui              Disable TUI, run headless");
    println!("    --mcp                 Run as MCP server on stdio (replaces TUI)");
    println!("    --verbose, -v         Enable verbose output");
    println!("    --control-socket      Enable Unix control socket");
    println!("    --json                Emit JSONL events to stdout (implies --no-tui)");
    println!("    --sandbox             Enable Landlock filesystem sandboxing for the runtime");
    println!("    --direct              Force single-agent mode (skip orchestrator/sub-agent delegation)");
    println!("    --no-presence         Disable the presence layer (direct agent interaction)");
    println!("    --web [PORT]          Web dashboard (default: on, port 8765; idle starts daemon/no TUI)");
    println!(
        "    --tls                 Serve the web dashboard over HTTPS/WSS (auto self-signed cert)"
    );
    println!("    --tls-cert <PATH>     PEM cert overriding the self-signed cert (with --tls-key; implies --tls)");
    println!("    --tls-key <PATH>      PEM private key matching --tls-cert");
    println!("    --no-web              Disable web dashboard; use terminal TUI when interactive");
    println!("    --transcription       Enable user speech transcription");
    println!(
        "    --record-display <ID> Record an existing X11 display (e.g. 50 for :50, repeatable)"
    );
    println!("    --agent <BACKEND>     Use external agent backend (codex, claude-code)");
    println!("    --advertise-url <URL> WebSocket URL to advertise to peers in this daemon's");
    println!("                          Agent Card (repeatable, preference order). Overrides");
    println!("                          [server.advertise] in intendant.toml when given.");
    println!("                          Example: --advertise-url ws://192.168.1.42:8765/ws");
    println!(
        "                                   --advertise-url wss://node.tail-abcd.ts.net:8443/ws"
    );
    println!("    --help, -h            Show this help message");
    println!();
    println!("SESSION LOGS:");
    println!(
        "    Logs are always written to ~/.intendant/logs/<timestamp>/ (override with --log-file)."
    );
    println!("    The log directory contains:");
    println!("      session.jsonl           Structured JSONL event log (one JSON object per line)");
    println!("      turns/turn_NNN_*.txt    Full model responses, agent I/O per turn");
    println!("      summary.json            Post-session summary");
    println!();
    println!("    AI agents can grep session.jsonl by event type, turn number, or level,");
    println!("    then read specific turn files for full content.");
    println!();
    println!("ENVIRONMENT:");
    println!("    OPENAI_API_KEY        OpenAI API key (for openai provider)");
    println!("    ANTHROPIC_API_KEY     Anthropic API key (for anthropic provider)");
    println!("    GEMINI_API_KEY        Google AI API key (for gemini provider)");
    println!("    PROVIDER              Default provider (openai, anthropic, or gemini)");
    println!("    MODEL_NAME            Default model name");
    println!("    STRUCTURED_OUTPUT     Enable JSON structured output (true/false)");
    println!("    REASONING_EFFORT      Reasoning effort: low, medium, high");
    println!("    REASONING_SUMMARY     Reasoning summary: auto, concise, detailed");
}

/// Try binding to ports starting from `preferred`, returning the bound listener.
/// Avoids TOCTOU by keeping the listener alive instead of probing and releasing.
///
/// Binds dual-stack (IPv6 with `IPV6_V6ONLY=false`) so the listener
/// accepts both IPv6 and IPv4 connections. Without this, macOS
/// defaults `V6ONLY=true` on IPv6 sockets and an IPv4-only bind
/// would mismatch [`web_gateway::resolve_advertise_urls`], which
/// enumerates every routable interface (v4 and v6) into the Agent
/// Card. Federation code that picks a card URL verbatim — notably
/// slice 3b's `relay_advertise_url` — would then inject an
/// unreachable IPv6 ICE-TCP candidate and the browser would fail
/// to form a pair. Dual-stack keeps every advertised URL
/// truthful.
///
/// Falls back to IPv4-only if an IPv6 socket can't be created or
/// configured (containerized envs with no IPv6 stack, hardened
/// sandboxes that block V6ONLY toggling, etc). On those hosts
/// `routable_local_addrs` won't find any IPv6 interfaces either,
/// so the card's URL list stays consistent with the bind.
async fn find_available_port(
    preferred: u16,
) -> Result<(u16, tokio::net::TcpListener), CallerError> {
    for offset in 0..20u16 {
        let port = preferred.checked_add(offset).unwrap_or(preferred);
        match bind_dual_stack_or_v4(port).await {
            Ok(listener) => return Ok((port, listener)),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(e) => {
                return Err(CallerError::Config(format!(
                    "Failed to bind web gateway port: {}",
                    e
                )));
            }
        }
    }
    Err(CallerError::Config(format!(
        "No available port found in range {}-{}",
        preferred,
        preferred + 19
    )))
}

/// Bind a TCP listener on `port`, preferring IPv6 dual-stack.
/// See [`find_available_port`] for why dual-stack matters.
///
/// Uses `socket2` directly because `tokio::net::TcpSocket` doesn't
/// expose `IPV6_V6ONLY`. The constructed `std::net::TcpListener` is
/// set non-blocking and handed to tokio via `from_std`, which is the
/// same path tokio's own `TcpSocket::listen` takes under the hood.
///
/// Sets `SO_REUSEADDR` so a restart lands on the same port even
/// when the previous daemon's sockets are still in `TIME_WAIT`.
/// Without this, the Intendant.app wrapper's IPv4 probe (which
/// does set `SO_REUSEADDR`) says 8765 is free — the backend then
/// fails to bind it and slides to 8766, the WKWebView's HTTP poll
/// keeps hitting 8765, and the UI shows "Failed to connect to
/// backend on port 8765" even though the backend is healthy on
/// the next port. Matching the wrapper's assumption keeps the
/// port stable across restarts.
async fn bind_dual_stack_or_v4(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::SocketAddr;
    if let Ok(socket) = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)) {
        // Flip V6ONLY off so the listener accepts IPv4 too. If the
        // kernel doesn't support the toggle (hardened sandboxes),
        // fall through to the IPv4 fallback path.
        if socket.set_only_v6(false).is_ok() {
            // Best-effort: SO_REUSEADDR isn't load-bearing for
            // correctness (ignore Err), but without it a quick
            // restart races the kernel's TIME_WAIT window.
            let _ = socket.set_reuse_address(true);
            let v6_wildcard: SocketAddr = format!("[::]:{port}")
                .parse()
                .expect("IPv6 wildcard literal parses");
            // Propagate bind errors (AddrInUse / EACCES / etc) so the
            // caller's loop can walk to the next port or fail loudly.
            // Don't silently fall back to IPv4 here — an in-use IPv6
            // port is in use for IPv4 too on a dual-stack host.
            socket.bind(&v6_wildcard.into())?;
            socket.listen(1024)?;
            // tokio::net::TcpListener::from_std requires the underlying
            // socket to be in non-blocking mode.
            socket.set_nonblocking(true)?;
            let std_listener: std::net::TcpListener = socket.into();
            return tokio::net::TcpListener::from_std(std_listener);
        }
    }
    // IPv4 fallback for hosts without an IPv6 stack. Same TIME_WAIT
    // reasoning as the v6 path above — set SO_REUSEADDR via socket2
    // rather than going through tokio's bind (which doesn't expose it).
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    let _ = socket.set_reuse_address(true);
    let v4_wildcard: SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .expect("IPv4 wildcard literal parses");
    socket.bind(&v4_wildcard.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    let std_listener: std::net::TcpListener = socket.into();
    tokio::net::TcpListener::from_std(std_listener)
}

/// Build the optional TLS acceptor for the `--web` dashboard.
///
/// TLS is enabled when either the CLI `--tls` flag is set or
/// `[server.tls] enabled = true` is in intendant.toml. When enabled, the
/// cert source is resolved in priority order:
///   1. Explicit PEM files — CLI `--tls-cert`/`--tls-key` first, else
///      `[server.tls] cert`/`key`. Both halves of a pair must be present.
///   2. Otherwise a self-signed cert minted by `rcgen`, with the listener
///      bind IP plus `localhost` (and optional `[server.tls] hostname`) in
///      the SAN list.
///
/// Returns `Ok(None)` when TLS is off (the default), `Ok(Some(acceptor))`
/// when on and the cert built, or `Err` when enabled but misconfigured
/// (mismatched cert/key pair, unreadable/invalid PEM, cert-gen failure) —
/// surfaced loudly at startup rather than silently serving plain HTTP.
fn build_web_tls_acceptor(
    flags: &CliFlags,
    server_cfg: &project::ServerTlsConfig,
    bind_addr: Option<std::net::SocketAddr>,
) -> Result<Option<tokio_rustls::TlsAcceptor>, CallerError> {
    let enabled = flags.tls || server_cfg.enabled;
    if !enabled {
        return Ok(None);
    }

    // Resolve an explicit cert/key pair: CLI overrides config. A
    // half-specified pair (only cert or only key) is a configuration
    // error rather than a silent fallback to self-signed.
    let cert_path = flags.tls_cert.clone().or_else(|| server_cfg.cert.clone());
    let key_path = flags.tls_key.clone().or_else(|| server_cfg.key.clone());
    let source = match (cert_path, key_path) {
        (Some(c), Some(k)) => web_tls::TlsCertSource::Files {
            cert_path: c.into(),
            key_path: k.into(),
        },
        (Some(_), None) | (None, Some(_)) => {
            return Err(CallerError::Config(
                "TLS cert/key must be supplied together (got only one of --tls-cert/--tls-key \
                 or [server.tls] cert/key)"
                    .to_string(),
            ));
        }
        (None, None) => web_tls::TlsCertSource::SelfSigned {
            bind_ip: bind_addr.map(|a| a.ip()),
            hostname: server_cfg.hostname.clone(),
        },
    };

    let acceptor = web_tls::build_acceptor(&source)
        .map_err(|e| CallerError::Config(format!("TLS setup failed: {e}")))?;
    Ok(Some(acceptor))
}

fn parse_cli_flags() -> Result<CliFlags, CallerError> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut flags = CliFlags {
        task: None,
        provider: None,
        model: None,
        verbose: false,
        no_tui: false,
        mcp: false,
        autonomy: AutonomyLevel::Medium,
        log_file: None,
        continue_last: false,
        resume_id: None,
        control_socket: false,
        json_output: false,
        sandbox: false,
        direct: false,
        no_presence: false,
        web: false,
        web_port: web_gateway::DEFAULT_PORT,
        tls: false,
        tls_cert: None,
        tls_key: None,
        transcription: false,
        record_displays: Vec::new(),

        agent_backend: None,

        no_web: false,

        advertise_urls: Vec::new(),
    };

    let mut task_parts = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--provider" => {
                if i + 1 < args.len() {
                    flags.provider = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --provider".to_string(),
                    ));
                }
            }
            "--model" => {
                if i + 1 < args.len() {
                    flags.model = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    return Err(CallerError::Config("Missing value for --model".to_string()));
                }
            }
            "--verbose" | "-v" => {
                flags.verbose = true;
                i += 1;
            }
            "--no-tui" => {
                flags.no_tui = true;
                i += 1;
            }
            "--autonomy" => {
                if i + 1 < args.len() {
                    flags.autonomy = AutonomyLevel::from_str_loose(&args[i + 1]);
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --autonomy".to_string(),
                    ));
                }
            }
            "--log-file" => {
                if i + 1 < args.len() {
                    flags.log_file = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --log-file".to_string(),
                    ));
                }
            }
            "--continue" | "-c" => {
                flags.continue_last = true;
                i += 1;
            }
            "--resume" | "-r" => {
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    flags.resume_id = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    // --resume without argument acts like --continue
                    flags.continue_last = true;
                    i += 1;
                }
            }
            "--mcp" => {
                flags.mcp = true;
                i += 1;
            }
            "--json" => {
                flags.json_output = true;
                flags.no_tui = true; // --json implies --no-tui
                i += 1;
            }
            "--sandbox" => {
                flags.sandbox = true;
                i += 1;
            }
            "--control-socket" => {
                flags.control_socket = true;
                i += 1;
            }
            "--direct" => {
                flags.direct = true;
                i += 1;
            }
            "--no-presence" => {
                flags.no_presence = true;
                i += 1;
            }
            "--no-web" => {
                flags.no_web = true;
                i += 1;
            }
            "--web" => {
                flags.web = true;
                // --web enables the dashboard. Idle web startup uses the
                // daemon/no-terminal-TUI path; a task still runs through the
                // normal frontend selection below.
                // Optional port argument (next arg if it's numeric)
                if i + 1 < args.len() && args[i + 1].parse::<u16>().is_ok() {
                    flags.web_port = args[i + 1].parse().unwrap();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--tls" => {
                // Serve the dashboard over HTTPS/WSS. Auto self-signed
                // cert unless --tls-cert/--tls-key are also given.
                flags.tls = true;
                i += 1;
            }
            "--tls-cert" => {
                if i + 1 < args.len() {
                    flags.tls_cert = Some(args[i + 1].clone());
                    flags.tls = true; // a cert override implies TLS
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --tls-cert".to_string(),
                    ));
                }
            }
            "--tls-key" => {
                if i + 1 < args.len() {
                    flags.tls_key = Some(args[i + 1].clone());
                    flags.tls = true; // a key override implies TLS
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --tls-key".to_string(),
                    ));
                }
            }
            "--transcription" => {
                flags.transcription = true;
                i += 1;
            }
            "--agent" => {
                if i + 1 < args.len() {
                    let backend = external_agent::AgentBackend::from_str_loose(&args[i + 1])
                        .ok_or_else(|| {
                            CallerError::Config(format!(
                                "Unknown agent backend: '{}'. Valid options: codex, claude-code",
                                args[i + 1]
                            ))
                        })?;
                    flags.agent_backend = Some(backend);
                    i += 2;
                } else {
                    return Err(CallerError::Config("Missing value for --agent".to_string()));
                }
            }
            "--advertise-url" => {
                // Repeatable: every occurrence appends one URL in the
                // order given. The full list replaces config + auto-
                // detection when non-empty.
                if i + 1 < args.len() {
                    flags.advertise_urls.push(args[i + 1].clone());
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --advertise-url".to_string(),
                    ));
                }
            }
            "--record-display" => {
                if i + 1 >= args.len() {
                    return Err(CallerError::Config(
                        "--record-display requires a display ID (e.g. 50 for :50)".to_string(),
                    ));
                }
                let raw = args[i + 1].trim_start_matches(':');
                let id: u32 = raw.parse().map_err(|_| {
                    CallerError::Config(format!(
                        "--record-display: '{}' is not a valid display ID",
                        args[i + 1]
                    ))
                })?;
                flags.record_displays.push(id);
                i += 2;
            }
            other => {
                if other.starts_with('-') {
                    return Err(CallerError::Config(format!(
                        "Unknown CLI flag: {}. Use --help to see valid options.",
                        other
                    )));
                }
                task_parts.push(other.to_string());
                i += 1;
            }
        }
    }

    if !task_parts.is_empty() {
        flags.task = Some(task_parts.join(" "));
    }

    Ok(flags)
}

fn should_start_idle_web_daemon(use_web: bool, flags: &CliFlags) -> bool {
    use_web
        && !flags.mcp
        && flags
            .task
            .as_ref()
            .map(|task| task.trim().is_empty())
            .unwrap_or(true)
}

fn extract_json(text: &str) -> Option<&str> {
    // Try to find JSON in ```json code fences
    if let Some(start) = text.find("```json") {
        let json_start = start + 7;
        if let Some(end) = text[json_start..].find("```") {
            let candidate = text[json_start..json_start + end].trim();
            if candidate.starts_with('{') {
                return Some(candidate);
            }
        }
    }

    // Try generic code fences
    if let Some(start) = text.find("```") {
        let after_fence = start + 3;
        let json_start = if let Some(nl) = text[after_fence..].find('\n') {
            after_fence + nl + 1
        } else {
            after_fence
        };
        if let Some(end) = text[json_start..].find("```") {
            let candidate = text[json_start..json_start + end].trim();
            if candidate.starts_with('{') {
                return Some(candidate);
            }
        }
    }

    // Try bare JSON - find first { and last }
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            if end > start {
                let candidate = &text[start..=end];
                if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

/// Parse a `BRIEF: ...` line from the model's last response.
/// Returns `(brief_text, was_explicit)` — `was_explicit` is false when falling back.
fn parse_brief(text: &str) -> (String, bool) {
    // Look for explicit BRIEF: marker (scan from end for last occurrence)
    for line in text.lines().rev() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("BRIEF:") {
            let brief = rest.trim();
            if !brief.is_empty() {
                return (brief.to_string(), true);
            }
        }
    }
    // Fallback: extract first 1-2 sentences from the text
    (extract_brief_from_text(text), false)
}

/// Extract a short brief from freeform text by taking the first 1-2 sentences.
fn extract_brief_from_text(text: &str) -> String {
    let cleaned = text.trim();
    if cleaned.is_empty() {
        return "Task completed.".to_string();
    }
    // Skip markdown headers and blank lines to find the first content line(s)
    let mut sentences = String::new();
    let mut sentence_count = 0;
    for line in cleaned.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("```")
            || trimmed.starts_with("BRIEF:")
        {
            if sentence_count > 0 {
                break; // Stop at first blank/header after content
            }
            continue;
        }
        // Strip markdown formatting
        let plain = trimmed
            .trim_start_matches("- ")
            .trim_start_matches("* ")
            .trim_start_matches("> ");
        if !sentences.is_empty() {
            sentences.push(' ');
        }
        sentences.push_str(plain);
        sentence_count += 1;
        if sentence_count >= 2 || sentences.len() > 200 {
            break;
        }
    }
    if sentences.is_empty() {
        return "Task completed.".to_string();
    }
    // Truncate if still too long
    if sentences.len() > 200 {
        if let Some(pos) = sentences[..200].rfind(". ") {
            sentences.truncate(pos + 1);
        } else {
            sentences.truncate(200);
            sentences.push_str("...");
        }
    }
    sentences
}

/// Returns (json_string, had_context_directives).
/// Empty json_string means no commands to execute.
fn apply_context_directives(json_str: &str, conversation: &mut Conversation) -> (String, bool) {
    let mut value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return (json_str.to_string(), false),
    };

    let mut had_context = false;

    if let Some(context) = value.get("context").cloned() {
        had_context = true;

        // Apply drop_turns
        if let Some(drops) = context.get("drop_turns").and_then(|d| d.as_array()) {
            let indices: Vec<usize> = drops
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as usize))
                .collect();
            conversation.drop_turns(&indices);
        }

        // Apply summarize
        if let Some(summarize) = context.get("summarize") {
            if let (Some(turns), Some(summary)) = (
                summarize.get("turns").and_then(|t| t.as_array()),
                summarize.get("summary").and_then(|s| s.as_str()),
            ) {
                let indices: Vec<usize> = turns
                    .iter()
                    .filter_map(|v| v.as_u64().map(|n| n as usize))
                    .collect();
                conversation.summarize_turns(&indices, summary);
            }
        }

        // Strip context field before passing to agent
        if let Some(obj) = value.as_object_mut() {
            obj.remove("context");
        }
    }

    // Check if there are commands; if not, return empty to signal no commands
    let has_commands = value
        .get("commands")
        .and_then(|c| c.as_array())
        .is_some_and(|a| !a.is_empty());

    if !has_commands {
        return (String::new(), had_context);
    }

    (
        serde_json::to_string(&value).unwrap_or_else(|_| json_str.to_string()),
        had_context,
    )
}

fn inject_project_context(json_str: &str, project: &Project) -> String {
    let mut value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return json_str.to_string(),
    };

    if let Some(commands) = value.get_mut("commands").and_then(|c| c.as_array_mut()) {
        let memory_file = project.memory_path().to_string_lossy().to_string();

        for cmd in commands.iter_mut() {
            if let Some("storeMemory" | "recallMemory") =
                cmd.get("function").and_then(|f| f.as_str())
            {
                if cmd.get("memory_file").is_none() {
                    cmd["memory_file"] = serde_json::Value::String(memory_file.clone());
                }
            }
        }
    }

    serde_json::to_string(&value).unwrap_or_else(|_| json_str.to_string())
}

fn has_ask_human_command(json_str: &str) -> bool {
    let parsed: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return false,
    };

    parsed
        .get("commands")
        .and_then(|v| v.as_array())
        .map(|commands| {
            commands
                .iter()
                .any(|cmd| cmd.get("function").and_then(|v| v.as_str()) == Some("askHuman"))
        })
        .unwrap_or(false)
}

/// Extract the question text from an askHuman command in a batch JSON string.
fn extract_ask_human_question(json_str: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;
    parsed
        .get("commands")
        .and_then(|v| v.as_array())
        .and_then(|commands| {
            commands.iter().find_map(|cmd| {
                if cmd.get("function").and_then(|v| v.as_str()) == Some("askHuman") {
                    cmd.get("question")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
        })
}

fn has_capture_screen_command(json_str: &str) -> bool {
    let parsed: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return false,
    };

    parsed
        .get("commands")
        .and_then(|v| v.as_array())
        .map(|commands| {
            commands
                .iter()
                .any(|cmd| cmd.get("function").and_then(|v| v.as_str()) == Some("captureScreen"))
        })
        .unwrap_or(false)
}

fn has_exec_command(json_str: &str) -> bool {
    let parsed: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return false,
    };

    parsed
        .get("commands")
        .and_then(|v| v.as_array())
        .map(|commands| {
            commands.iter().any(|cmd| {
                matches!(
                    cmd.get("function").and_then(|v| v.as_str()),
                    Some("execAsAgent" | "execPty")
                )
            })
        })
        .unwrap_or(false)
}

/// Try to encode a captureScreen result as base64 image data.
/// Returns `Some(vec![ImageData])` on success, `None` on any failure.
/// Replace "[Image: image/png]" placeholders in Gemini agent output with
/// the actual screenshot data from disk, formatted as MCP JSON so the Activity
/// tab's format_agent_output can extract and lazy-load the images.
fn substitute_screenshot_from_disk(text: &str, log_dir: &std::path::Path) -> String {
    let screenshots_dir = log_dir.join("screenshots");
    // Find the latest cu_screenshot_*.png by modification time
    let latest = std::fs::read_dir(&screenshots_dir)
        .ok()
        .and_then(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .map(|n| n.starts_with("cu_screenshot_") && n.ends_with(".png"))
                        .unwrap_or(false)
                })
                .max_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()))
        });

    let Some(entry) = latest else {
        return text.to_string();
    };

    let Ok(png_bytes) = std::fs::read(entry.path()) else {
        return text.to_string();
    };

    let base64_data =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png_bytes);

    // Build MCP-style JSON with both text and image content blocks
    let text_part = text
        .replace("[Image: image/png]", "")
        .replace("[Image: image/jpeg]", "");
    let text_part = text_part.trim();

    let mut content = Vec::new();
    if !text_part.is_empty() {
        content.push(serde_json::json!({"text": text_part, "type": "text"}));
    }
    content.push(serde_json::json!({
        "data": base64_data,
        "type": "image",
        "mimeType": "image/png",
    }));

    serde_json::json!({"content": content}).to_string()
}

fn encode_screenshot(result_text: &str) -> Option<Vec<conversation::ImageData>> {
    let parsed: serde_json::Value = serde_json::from_str(result_text).ok()?;
    if parsed.get("success").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let path_str = parsed.get("screenshot_path").and_then(|v| v.as_str())?;
    let bytes = std::fs::read(path_str).ok()?;
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(vec![conversation::ImageData {
        media_type: "image/png".to_string(),
        data: encoded,
    }])
}

/// Auto-launch Xvfb when no working display exists and the batch needs one.
///
/// Detection flow:
/// 1. Already launched (`xvfb_guard` is `Some`)? → skip
/// 2. Current DISPLAY accessible? Yes → skip
/// 3. Batch contains `captureScreen` or any `execAsAgent`? No → skip
/// 4. Launch Xvfb, store guard, set DISPLAY
/// 5. On failure → log warning, let commands fail naturally
///
/// Format raw agent JSON into a human-readable preview for the Activity tab.
pub(crate) fn format_commands_preview(json_str: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
        if let Some(cmds) = parsed.get("commands").and_then(|v| v.as_array()) {
            let parts: Vec<String> = cmds
                .iter()
                .filter_map(|cmd| {
                    let func = cmd.get("function").and_then(|v| v.as_str()).unwrap_or("?");
                    match func {
                        "execAsAgent" => cmd
                            .get("command")
                            .and_then(|v| v.as_str())
                            .map(|c| format!("exec: {}", c)),
                        "inspectPath" => cmd
                            .get("path")
                            .and_then(|v| v.as_str())
                            .map(|p| format!("inspect: {}", p)),
                        "editFile" | "writeFile" => cmd
                            .get("file_path")
                            .and_then(|v| v.as_str())
                            .map(|p| format!("{}: {}", func, p)),
                        "spawn_live_audio" => Some(format!(
                            "spawn_live_audio ({})",
                            cmd.get("provider").and_then(|v| v.as_str()).unwrap_or("?")
                        )),
                        _ => Some(func.to_string()),
                    }
                })
                .collect();
            if !parts.is_empty() {
                return parts.join(" | ");
            }
        }
    }
    json_str.to_string()
}

/// We launch on execAsAgent (not just captureScreen) because GUI applications
/// started in early turns must share the same display that captureScreen will
/// later capture. Launching only on captureScreen is too late — the app would
/// already be running on a different (or no) display.
async fn maybe_auto_launch_xvfb(
    json_str: &str,
    xvfb_guard: &mut Option<vision::XvfbGuard>,
    provider_name: &str,
    session_log: &SharedSessionLog,
    bus: &EventBus,
) {
    if xvfb_guard.is_some() {
        return;
    }
    if !has_capture_screen_command(json_str) && !has_exec_command(json_str) {
        return;
    }
    // If a display is already accessible (e.g. DISPLAY was set before launch,
    // or on macOS where the native display is always available), skip Xvfb.
    // Don't emit DisplayReady — no DisplaySession exists, so the web dashboard
    // can't connect via WebRTC. Recording uses x11grab/avfoundation directly.
    if vision::is_display_accessible() {
        let default_display = if cfg!(target_os = "macos") { 0 } else { 99 };
        let display_id = std::env::var("DISPLAY")
            .ok()
            .and_then(|d| d.trim_start_matches(':').parse::<u32>().ok())
            .unwrap_or(default_display);
        let (width, height) = query_display_resolution(display_id);
        slog(session_log, |l| {
            l.info(&format!(
                "Using existing display :{} ({}x{}) — no web slot (no DisplaySession)",
                display_id, width, height
            ))
        });
        return;
    }
    let config = vision::display_config_for_provider(provider_name);
    let trigger = if has_capture_screen_command(json_str) {
        "captureScreen"
    } else {
        "execAsAgent (display needed)"
    };
    let virtual_id = match config.target {
        computer_use::DisplayTarget::Virtual { id } => id,
        _ => return,
    };
    slog(session_log, |l| {
        l.info(&format!(
            "Auto-launching Xvfb :{} at {}x{} for {}",
            virtual_id, config.width, config.height, trigger
        ))
    });
    match vision::launch_display(&config).await {
        Ok(guard) => {
            // Phase 1: no DisplayReady for virtual displays — no DisplaySession means no web slot.
            // The agent uses this display for CU via X11 tools directly.
            slog(session_log, |l| {
                l.info(&format!(
                    "Xvfb :{} launched (no web slot in phase 1)",
                    virtual_id
                ))
            });
            *xvfb_guard = Some(guard);
        }
        Err(e) => {
            slog(session_log, |l| {
                l.warn(&format!("Failed to auto-launch Xvfb: {}", e))
            });
        }
    }
}

/// Query the resolution of the native display via system_profiler.
/// Returns the logical (point) resolution, not device pixels.
/// Uses CoreGraphics via swift, which returns logical resolution directly
/// (e.g. 1339x837 on a Retina display, not the 2x device pixel size).
/// Falls back to system_profiler, then a default.
#[cfg(target_os = "macos")]
pub(crate) fn query_display_resolution(_display_id: u32) -> (u32, u32) {
    // Primary method: CoreGraphics (works in VMs where system_profiler is empty)
    if let Ok(out) = std::process::Command::new("swift")
        .args(["-e", "import CoreGraphics; let d = CGMainDisplayID(); print(\"\\(CGDisplayPixelsWide(d))x\\(CGDisplayPixelsHigh(d))\")"])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let parts: Vec<&str> = text.split('x').collect();
        if parts.len() == 2 {
            if let (Ok(w), Ok(h)) = (parts[0].parse(), parts[1].parse()) {
                return (w, h);
            }
        }
    }
    // Fallback: system_profiler (may be empty in VMs)
    if let Ok(out) = std::process::Command::new("system_profiler")
        .arg("SPDisplaysDataType")
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("Resolution:") {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 4 {
                    if let (Ok(w), Ok(h)) = (parts[1].parse::<u32>(), parts[3].parse::<u32>()) {
                        let is_retina = trimmed.to_lowercase().contains("retina");
                        if is_retina {
                            return (w / 2, h / 2);
                        }
                        return (w, h);
                    }
                }
            }
        }
    }
    (1920, 1080)
}

/// Query the resolution of an existing X11 display via xdpyinfo.
/// Returns (width, height) or a default of (1280, 720) if detection fails.
#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
pub(crate) fn query_display_resolution(display_id: u32) -> (u32, u32) {
    let output = std::process::Command::new("xdpyinfo")
        .arg("-display")
        .arg(format!(":{}", display_id))
        .output();
    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("dimensions:") {
                // "dimensions:    1280x720 pixels (338x190 millimeters)"
                if let Some(dims) = trimmed.split_whitespace().nth(1) {
                    let parts: Vec<&str> = dims.split('x').collect();
                    if parts.len() == 2 {
                        if let (Ok(w), Ok(h)) = (parts[0].parse(), parts[1].parse()) {
                            return (w, h);
                        }
                    }
                }
            }
        }
    }
    (1280, 720)
}

/// No X11 / `xdpyinfo` on Windows. Return the same conservative default
/// the X11 path falls back to; Tier-1's DXGI backend will report the real
/// resolution via the display enumeration path instead.
#[cfg(target_os = "windows")]
pub(crate) fn query_display_resolution(_display_id: u32) -> (u32, u32) {
    (1280, 720)
}

/// Start recording external displays (--record-display) directly on the registry.
/// Does NOT emit DisplayReady — external displays have no DisplaySession, so the
/// web dashboard can't connect. Recording uses x11grab independently.
async fn start_external_display_recordings(
    displays: &[u32],
    registry: &std::sync::Arc<tokio::sync::RwLock<recording::RecordingRegistry>>,
    bus: &EventBus,
) {
    for &id in displays {
        let (width, height) = query_display_resolution(id);
        eprintln!("Recording external display :{} ({}x{})", id, width, height);
        let mut reg = registry.write().await;
        if !reg.is_enabled() {
            eprintln!("Recording not enabled in config — skipping :{}", id);
            continue;
        }
        if !recording::is_ffmpeg_available() {
            eprintln!("ffmpeg not available — skipping :{}", id);
            continue;
        }
        match reg.start_external_display(id, width, height).await {
            Ok(stream_name) => {
                bus.send(AppEvent::RecordingStarted { stream_name });
            }
            Err(e) => eprintln!("Failed to start recording for :{}: {}", id, e),
        }
    }
}

/// Format a human-readable command preview from raw JSON (for approval display).
fn format_command_preview(json_str: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
        if let Some(commands) = parsed.get("commands").and_then(|c| c.as_array()) {
            let summaries: Vec<String> = commands
                .iter()
                .map(|cmd| {
                    let func = cmd.get("function").and_then(|f| f.as_str()).unwrap_or("?");
                    match func {
                        "execAsAgent" => {
                            let command =
                                cmd.get("command").and_then(|c| c.as_str()).unwrap_or("?");
                            format!("exec: {}", command)
                        }
                        "writeFile" | "editFile" => {
                            let path = cmd.get("file_path").and_then(|p| p.as_str()).unwrap_or("?");
                            format!("{}: {}", func, path)
                        }
                        "inspectPath" => {
                            let path = cmd.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                            format!("inspect: {}", path)
                        }
                        "browse" => {
                            let url = cmd.get("url").and_then(|u| u.as_str()).unwrap_or("?");
                            format!("browse: {}", url)
                        }
                        _ => func.to_string(),
                    }
                })
                .collect();
            if !summaries.is_empty() {
                return summaries.join(" | ");
            }
        }
    }
    // Fallback: full raw JSON (UI handles collapsing)
    json_str.to_string()
}

fn normalize_command_batch(json_str: &str) -> String {
    let mut value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return json_str.to_string(),
    };

    let Some(commands) = value.get_mut("commands").and_then(|c| c.as_array_mut()) else {
        return json_str.to_string();
    };

    for cmd in commands {
        if cmd.get("function").and_then(|f| f.as_str()) == Some("writeFile") {
            cmd["function"] = serde_json::Value::String("editFile".to_string());
            if cmd.get("operation").is_none() {
                cmd["operation"] = serde_json::Value::String("write".to_string());
            }
        }
    }

    serde_json::to_string(&value).unwrap_or_else(|_| json_str.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `build_local_advertised_auth` with the default config (all
    /// `[server.auth]` fields unset) produces `AuthRequirements::none()`
    /// — the conservative default that doesn't advertise any auth.
    /// Doesn't touch the cert dir at all; safe to run with no LAN
    /// setup.
    #[test]
    fn build_local_advertised_auth_defaults_to_none() {
        let server_auth = project::ServerAuthConfig::default();
        let cert_dir = std::path::PathBuf::from("/nonexistent");
        let auth = build_local_advertised_auth(&server_auth, &cert_dir).unwrap();
        assert_eq!(auth, peer::AuthRequirements::none());
    }

    #[test]
    fn fork_session_name_from_params_trims_blank_names() {
        assert_eq!(
            fork_session_name_from_params(&serde_json::json!({ "name": "  Forked work  " })),
            Some("Forked work".to_string())
        );
        assert_eq!(
            fork_session_name_from_params(&serde_json::json!({ "name": "   " })),
            None
        );
        assert_eq!(fork_session_name_from_params(&serde_json::json!({})), None);
    }

    #[test]
    fn side_session_prompt_from_params_accepts_prompt_aliases() {
        assert_eq!(
            side_session_prompt_from_params(&serde_json::json!({ "prompt": "  quick question  " })),
            Some("quick question".to_string())
        );
        assert_eq!(
            side_session_prompt_from_params(&serde_json::json!({ "task": "check this" })),
            Some("check this".to_string())
        );
        assert_eq!(
            side_session_prompt_from_params(&serde_json::json!("inline prompt")),
            Some("inline prompt".to_string())
        );
        assert_eq!(
            side_session_prompt_from_params(&serde_json::json!({ "prompt": "   " })),
            None
        );
    }

    #[test]
    fn codex_subagent_thread_ids_uses_late_agent_state_ids() {
        let agents = vec![
            external_agent::SubAgentState {
                thread_id: " child-from-state ".to_string(),
                status: "completed".to_string(),
                message: None,
            },
            external_agent::SubAgentState {
                thread_id: "child-from-receiver".to_string(),
                status: "running".to_string(),
                message: None,
            },
        ];
        assert_eq!(
            codex_subagent_thread_ids(
                &[
                    " child-from-receiver ".to_string(),
                    String::new(),
                    "child-from-receiver".to_string(),
                ],
                &agents,
            ),
            vec![
                "child-from-receiver".to_string(),
                "child-from-state".to_string()
            ]
        );
    }

    #[test]
    fn codex_subagent_terminal_reason_includes_final_message() {
        let state = external_agent::SubAgentState {
            thread_id: "child".to_string(),
            status: "completed".to_string(),
            message: Some("done".to_string()),
        };
        assert_eq!(
            codex_subagent_terminal_reason(&state).as_deref(),
            Some("Codex subagent completed: done")
        );

        let running = external_agent::SubAgentState {
            thread_id: "child".to_string(),
            status: "running".to_string(),
            message: None,
        };
        assert!(codex_subagent_terminal_reason(&running).is_none());
    }

    #[test]
    fn side_thread_ids_from_message_extracts_parent_child() {
        assert_eq!(
            side_thread_ids_from_message(
                "side conversation started in thread child-123 from parent parent-456"
            ),
            Some(("parent-456".to_string(), "child-123".to_string()))
        );
        assert_eq!(
            side_thread_ids_from_message("forked into thread child"),
            None
        );
    }

    #[test]
    fn side_rewind_first_turn_for_undo_stays_inside_side_boundary() {
        assert_eq!(
            side_rewind_first_turn_for_undo(3, 1, "side-child").unwrap(),
            3
        );
        assert_eq!(
            side_rewind_first_turn_for_undo(3, 3, "side-child").unwrap(),
            1
        );

        let zero = side_rewind_first_turn_for_undo(3, 0, "side-child").unwrap_err();
        assert!(zero.contains("at least 1"), "got: {zero}");

        let beyond = side_rewind_first_turn_for_undo(3, 4, "side-child").unwrap_err();
        assert!(beyond.contains("after the /side boundary"), "got: {beyond}");
    }

    #[test]
    fn parent_rewind_first_turn_for_undo_tracks_active_turn_count() {
        assert_eq!(parent_rewind_first_turn_for_undo(3, 1).unwrap(), 3);
        assert_eq!(parent_rewind_first_turn_for_undo(3, 2).unwrap(), 2);

        let zero = parent_rewind_first_turn_for_undo(3, 0).unwrap_err();
        assert!(zero.contains("at least 1"), "got: {zero}");

        let beyond = parent_rewind_first_turn_for_undo(3, 4).unwrap_err();
        assert!(beyond.contains("only 3 user turn"), "got: {beyond}");
    }

    #[test]
    fn user_turn_revision_state_rejects_stale_replacement() {
        let mut state = UserTurnRevisionState::default();
        let (turn, revision) = state.record_next_turn();
        assert_eq!((turn, revision), (1, 1));
        assert!(state.validate_expected_revision(1, Some(1)).is_ok());

        state.rewind_from_turn(1);
        let (replacement_turn, replacement_revision) = state.record_next_turn();
        assert_eq!((replacement_turn, replacement_revision), (1, 2));

        let stale = state.validate_expected_revision(1, Some(1)).unwrap_err();
        assert!(stale.contains("stale"), "got: {stale}");
        assert!(state.validate_expected_revision(1, Some(2)).is_ok());
    }

    #[test]
    fn codex_injected_user_text_filters_subagent_notifications() {
        assert!(is_codex_injected_user_text_for_main(
            "<subagent_notification>\n{\"agent_path\":\"child\"}\n</subagent_notification>"
        ));
        assert!(!is_codex_injected_user_text_for_main(
            "please inspect subagent_notification handling"
        ));
    }

    #[test]
    fn thread_action_params_with_thread_id_targets_clicked_window() {
        let params = thread_action_params_with_thread_id(
            "fork",
            serde_json::json!({ "name": "Parent fork" }),
            Some("parent-thread"),
        );
        assert_eq!(params["threadId"], "parent-thread");
        assert_eq!(params["name"], "Parent fork");

        let explicit = thread_action_params_with_thread_id(
            "fork",
            serde_json::json!({ "threadId": "explicit-thread" }),
            Some("parent-thread"),
        );
        assert_eq!(explicit["threadId"], "explicit-thread");

        let side_prompt = thread_action_params_with_thread_id(
            "side",
            serde_json::json!("quick check"),
            Some("parent-thread"),
        );
        assert_eq!(side_prompt["threadId"], "parent-thread");
        assert_eq!(side_prompt["prompt"], "quick check");
    }

    /// `advertised_transport = "mutual-tls"` advertises plain mTLS.
    /// Doesn't read the cert dir (no fingerprint to compute).
    #[test]
    fn build_local_advertised_auth_mutual_tls_no_cert_lookup() {
        let server_auth = project::ServerAuthConfig {
            bearer_token: None,
            advertised_transport: "mutual-tls".to_string(),
        };
        let cert_dir = std::path::PathBuf::from("/nonexistent");
        let auth = build_local_advertised_auth(&server_auth, &cert_dir).unwrap();
        assert!(matches!(auth.transport, peer::TransportAuth::MutualTls));
        assert!(auth.application.is_none());
    }

    /// `advertised_transport = "pin-self-cert"` reads the LAN cert
    /// dir, computes the fingerprint, embeds it in PinnedMutualTls.
    /// Uses `lan::certs::ensure_certs` to populate a tempdir.
    /// `lan::certs` is now pure-Rust and compiles everywhere, so this
    /// applies on all platforms.
    #[test]
    fn build_local_advertised_auth_pin_self_cert_reads_cert_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        lan::certs::ensure_certs(tmp.path(), "10.0.0.1", "test", false).unwrap();
        let expected_fp = lan::certs::read_server_cert_fingerprint(tmp.path()).unwrap();

        let server_auth = project::ServerAuthConfig {
            bearer_token: None,
            advertised_transport: "pin-self-cert".to_string(),
        };
        let auth = build_local_advertised_auth(&server_auth, tmp.path()).unwrap();
        match &auth.transport {
            peer::TransportAuth::PinnedMutualTls {
                server_cert_fingerprints,
            } => {
                assert_eq!(server_cert_fingerprints, &vec![expected_fp]);
            }
            other => panic!("expected PinnedMutualTls, got {other:?}"),
        }
    }

    /// `advertised_transport = "pin-self-cert"` with no cert in
    /// the dir errors with a clear message that points the
    /// operator at `intendant lan setup`.
    #[test]
    fn build_local_advertised_auth_pin_self_cert_errors_without_cert() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server_auth = project::ServerAuthConfig {
            bearer_token: None,
            advertised_transport: "pin-self-cert".to_string(),
        };
        let err = build_local_advertised_auth(&server_auth, tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("server.crt"), "msg: {msg}");
        assert!(msg.contains("intendant lan setup"), "msg: {msg}");
    }

    /// Unrecognized `advertised_transport` value errors loudly at
    /// startup so the operator notices the typo (vs. silent fall
    /// back to "none" which would surprise them).
    #[test]
    fn build_local_advertised_auth_rejects_invalid_transport_value() {
        let server_auth = project::ServerAuthConfig {
            bearer_token: None,
            advertised_transport: "definitely-not-valid".to_string(),
        };
        let cert_dir = std::path::PathBuf::from("/nonexistent");
        let err = build_local_advertised_auth(&server_auth, &cert_dir).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("definitely-not-valid"), "msg: {msg}");
        assert!(msg.contains("none"), "msg: {msg}");
        assert!(msg.contains("mutual-tls"), "msg: {msg}");
        assert!(msg.contains("pin-self-cert"), "msg: {msg}");
    }

    /// `bearer_token` set produces `application = Some(Bearer)`
    /// regardless of the transport value. The `hint` field
    /// documents where the token comes from so connecting peers
    /// can give operators a useful "configure me" message.
    #[test]
    fn build_local_advertised_auth_bearer_token_sets_application() {
        let server_auth = project::ServerAuthConfig {
            bearer_token: Some("secret".to_string()),
            advertised_transport: "none".to_string(),
        };
        let cert_dir = std::path::PathBuf::from("/nonexistent");
        let auth = build_local_advertised_auth(&server_auth, &cert_dir).unwrap();
        match &auth.application {
            Some(peer::ApplicationAuth::Bearer { hint, rotation_url }) => {
                assert!(hint.is_some(), "hint should document the source");
                assert!(hint.as_ref().unwrap().contains("[server.auth]"));
                assert!(
                    rotation_url.is_none(),
                    "rotation_url unset until rotation lands"
                );
            }
            other => panic!("expected Bearer application auth, got {other:?}"),
        }
    }

    /// Combination: `pin-self-cert` + `bearer_token` produces the
    /// full defense-in-depth advertise (PinnedMutualTls transport +
    /// Bearer application). The expected configuration for WAN-
    /// exposed daemons that want both wire-layer and app-layer auth.
    /// `lan::certs` is now pure-Rust and compiles everywhere, so this
    /// applies on all platforms.
    #[test]
    fn build_local_advertised_auth_full_defense_in_depth() {
        let tmp = tempfile::TempDir::new().unwrap();
        lan::certs::ensure_certs(tmp.path(), "10.0.0.99", "wan-test", false).unwrap();

        let server_auth = project::ServerAuthConfig {
            bearer_token: Some("wan-secret".to_string()),
            advertised_transport: "pin-self-cert".to_string(),
        };
        let auth = build_local_advertised_auth(&server_auth, tmp.path()).unwrap();
        assert!(matches!(
            auth.transport,
            peer::TransportAuth::PinnedMutualTls { .. }
        ));
        assert!(matches!(
            auth.application,
            Some(peer::ApplicationAuth::Bearer { .. })
        ));
    }

    #[tokio::test]
    async fn resolve_attachments_includes_uploaded_files_and_images() {
        use std::io::Write as _;

        fn upload_tempfile(bytes: &[u8]) -> tempfile::NamedTempFile {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            file.write_all(bytes).unwrap();
            file.flush().unwrap();
            file
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("session");
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();

        let file_upload = upload_store::commit_upload(
            upload_tempfile(b"a,b\n1,2\n"),
            "data.csv",
            "text/csv",
            8,
            upload_store::UploadDestination::Workspace,
            &session_dir,
            "sess-1",
            &project_root,
        )
        .unwrap();
        let image_upload = upload_store::commit_upload(
            upload_tempfile(b"not-really-a-png"),
            "screen.png",
            "image/png",
            16,
            upload_store::UploadDestination::Task,
            &session_dir,
            "sess-1",
            &project_root,
        )
        .unwrap();

        let registry = Arc::new(tokio::sync::RwLock::new(frames::FrameRegistry::new(
            &session_dir,
        )));
        let ids = vec![
            format!("upload:{}", file_upload.id),
            format!("upload:{}", image_upload.id),
        ];
        let attachments = resolve_attachments(&ids, &registry, &session_dir, &project_root).await;

        assert_eq!(attachments.len(), 2);
        match &attachments[0] {
            external_agent::AgentAttachment::File(file) => {
                assert_eq!(file.name, "data.csv");
                assert_eq!(file.mime_type, "text/csv");
                assert_eq!(file.size, 8);
                assert!(file
                    .local_path
                    .starts_with(project_root.join("workspace_files")));
            }
            other => panic!("expected file upload attachment, got {other:?}"),
        }
        match &attachments[1] {
            external_agent::AgentAttachment::Image(image) => {
                assert_eq!(image.mime_type, "image/png");
                assert_eq!(image.local_path.as_ref(), Some(&image_upload.path));
                assert!(!image.base64.is_empty());
            }
            other => panic!("expected image upload attachment, got {other:?}"),
        }
    }

    #[test]
    fn parse_diff_file_paths_new_file() {
        let diff = "\
diff --git a/foo.rs b/foo.rs
new file mode 100644
index 0000000..abc
--- /dev/null
+++ b/foo.rs
@@ -0,0 +1,2 @@
+hello
+world
";
        let files = parse_diff_file_paths(diff);
        assert_eq!(files, vec!["foo.rs".to_string()]);
    }

    #[test]
    fn parse_diff_file_paths_absolute_with_double_slash() {
        // Codex in practice writes `b//home/user/...` for absolute paths.
        // The stripped form must preserve the leading `/`.
        let diff = "\
diff --git a//home/user/proj/x.py b//home/user/proj/x.py
new file mode 100644
--- /dev/null
+++ b//home/user/proj/x.py
@@ -0,0 +1 @@
+pass
";
        let files = parse_diff_file_paths(diff);
        assert_eq!(files, vec!["/home/user/proj/x.py".to_string()]);
    }

    #[test]
    fn parse_diff_file_paths_deleted_file() {
        // Pure deletion: `+++ /dev/null`, so we must pick up the `a/` side.
        let diff = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-removed line
";
        let files = parse_diff_file_paths(diff);
        assert_eq!(files, vec!["gone.txt".to_string()]);
    }

    #[test]
    fn parse_diff_file_paths_multiple_and_dedup() {
        let diff = "\
--- a/one.rs
+++ b/one.rs
@@ -1 +1 @@
-a
+b
--- a/two.rs
+++ b/two.rs
@@ -1 +1 @@
-x
+y
";
        let files = parse_diff_file_paths(diff);
        assert_eq!(files, vec!["one.rs".to_string(), "two.rs".to_string()]);
    }

    #[test]
    fn split_unified_diff_by_file_keeps_file_blocks() {
        let diff = "\
diff --git a/one.rs b/one.rs
--- a/one.rs
+++ b/one.rs
@@ -1 +1 @@
-a
+b
diff --git a/two.rs b/two.rs
--- a/two.rs
+++ b/two.rs
@@ -1 +1 @@
-x
+y
";
        let blocks = split_unified_diff_by_file(diff);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, "one.rs");
        assert!(blocks[0].1.contains("diff --git a/one.rs b/one.rs"));
        assert!(!blocks[0].1.contains("diff --git a/two.rs b/two.rs"));
        assert_eq!(blocks[1].0, "two.rs");
        assert!(blocks[1].1.contains("diff --git a/two.rs b/two.rs"));
    }

    #[test]
    fn resolve_diff_file_path_allows_project_and_tmp_absolute_paths() {
        let project_root = Path::new("/work/project");
        assert_eq!(
            resolve_diff_file_path(project_root, "/work/project/src/main.rs").unwrap(),
            PathBuf::from("/work/project/src/main.rs")
        );
        assert_eq!(
            resolve_diff_file_path(project_root, "/tmp/intendant-edit.txt").unwrap(),
            PathBuf::from("/tmp/intendant-edit.txt")
        );
        assert!(resolve_diff_file_path(project_root, "/etc/passwd").is_none());
        assert!(resolve_diff_file_path(project_root, "../outside.txt").is_none());
    }

    #[test]
    fn parse_session_diff_file_paths_reads_persisted_diff_logs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let jsonl = r#"{"event":"info","message":"External agent diff: one.rs\n--- a/one.rs\n+++ b/one.rs\n@@ -1 +1 @@\n-a\n+b\n"}"#;
        std::fs::write(tmp.path().join("session.jsonl"), format!("{jsonl}\n")).unwrap();

        let files = parse_session_diff_file_paths(tmp.path());
        assert_eq!(files, vec!["one.rs".to_string()]);
    }

    #[test]
    fn external_diff_delta_tracker_can_seed_resumed_session_state() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
        let log_dir = tmp.path().join("session");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(project_root.join("tracked.txt"), "old logged state\n").unwrap();
        let jsonl = r#"{"event":"info","message":"External agent diff: tracked.txt\n--- a/tracked.txt\n+++ b/tracked.txt\n@@ -0,0 +1 @@\n+old logged state\n"}"#;
        std::fs::write(log_dir.join("session.jsonl"), format!("{jsonl}\n")).unwrap();

        let mut tracker = ExternalDiffDeltaTracker::default();
        tracker.seed_from_session_log(&project_root, &log_dir);

        std::fs::write(
            project_root.join("tracked.txt"),
            "old logged state\nnew resumed edit\n",
        )
        .unwrap();
        let cumulative_after_resume = "\
diff --git a/tracked.txt b/tracked.txt
--- /dev/null
+++ b/tracked.txt
@@ -0,0 +1,2 @@
+old logged state
+new resumed edit
";
        let delta = tracker
            .delta(&project_root, &[], cumulative_after_resume)
            .unwrap();
        assert_eq!(delta.files_changed, vec!["tracked.txt".to_string()]);
        assert!(delta.unified_diff.contains("+new resumed edit"));
        assert!(!delta.unified_diff.contains("+old logged state"));
    }

    #[test]
    fn external_diff_delta_tracker_emits_per_event_changes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project_root = tmp.path();
        let mut tracker = ExternalDiffDeltaTracker::default();

        let smoke_delete = "\
diff --git a/activity-diff-smoke.txt b/activity-diff-smoke.txt
deleted file mode 100644
--- a/activity-diff-smoke.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-old one
-old two
";
        let first = tracker.delta(project_root, &[], smoke_delete).unwrap();
        assert_eq!(
            first.files_changed,
            vec!["activity-diff-smoke.txt".to_string()]
        );
        assert!(first.unified_diff.contains("activity-diff-smoke.txt"));
        assert!(first.unified_diff.contains("-old one"));

        std::fs::write(
            project_root.join("activity-diff-live-check.md"),
            "# Activity Diff Live Check\n\n- first event\n",
        )
        .unwrap();
        let cumulative_after_create = format!(
            "{}{}",
            smoke_delete,
            "\
diff --git a/activity-diff-live-check.md b/activity-diff-live-check.md
new file mode 100644
--- /dev/null
+++ b/activity-diff-live-check.md
@@ -0,0 +1,3 @@
+# Activity Diff Live Check
+
+- first event
"
        );
        let second = tracker
            .delta(project_root, &[], &cumulative_after_create)
            .unwrap();
        assert_eq!(
            second.files_changed,
            vec!["activity-diff-live-check.md".to_string()]
        );
        assert!(!second.unified_diff.contains("activity-diff-smoke.txt"));
        assert!(second.unified_diff.contains("activity-diff-live-check.md"));
        assert!(second.unified_diff.contains("+- first event"));

        std::fs::write(
            project_root.join("activity-diff-live-check.md"),
            "# Activity Diff Live Check\n\n- first event\n- second event\n",
        )
        .unwrap();
        let cumulative_after_modify = format!(
            "{}{}",
            smoke_delete,
            "\
diff --git a/activity-diff-live-check.md b/activity-diff-live-check.md
new file mode 100644
--- /dev/null
+++ b/activity-diff-live-check.md
@@ -0,0 +1,4 @@
+# Activity Diff Live Check
+
+- first event
+- second event
"
        );
        let third = tracker
            .delta(project_root, &[], &cumulative_after_modify)
            .unwrap();
        assert_eq!(
            third.files_changed,
            vec!["activity-diff-live-check.md".to_string()]
        );
        assert!(!third.unified_diff.contains("activity-diff-smoke.txt"));
        assert!(third
            .unified_diff
            .contains("--- a/activity-diff-live-check.md"));
        assert!(third.unified_diff.contains("+- second event"));
        assert!(!third.unified_diff.contains("+@"));
    }

    #[test]
    fn extract_json_from_json_fence() {
        let text = r#"Here is the command:
```json
{"commands": [{"function": "execAsAgent", "nonce": 1}]}
```
Done."#;
        let json = extract_json(text).unwrap();
        assert!(json.starts_with('{'));
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(parsed["commands"].is_array());
    }

    #[test]
    fn extract_json_from_generic_fence() {
        let text = r#"Result:
```
{"commands": []}
```"#;
        let json = extract_json(text).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(parsed["commands"].is_array());
    }

    #[test]
    fn extract_json_bare() {
        let text = r#"I'll run this: {"commands": [{"function": "inspectPath", "nonce": 1, "path": "/tmp"}]} end"#;
        let json = extract_json(text).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["commands"][0]["function"], "inspectPath");
    }

    #[test]
    fn extract_json_no_json() {
        let text = "This is just plain text with no JSON.";
        assert!(extract_json(text).is_none());
    }

    #[test]
    fn extract_json_invalid_bare_json() {
        let text = "Some text with {broken json} here";
        assert!(extract_json(text).is_none());
    }

    #[test]
    fn extract_json_nested_braces() {
        let text = r#"```json
{"commands": [{"function": "execAsAgent", "command": "echo {hello}", "nonce": 1}]}
```"#;
        let json = extract_json(text).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["commands"][0]["command"], "echo {hello}");
    }

    #[test]
    fn extract_json_prefers_json_fence() {
        let text = r#"```json
{"source": "json_fence"}
```
Also: {"source": "bare"}"#;
        let json = extract_json(text).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["source"], "json_fence");
    }

    #[test]
    fn extract_json_empty_fence() {
        let text = "```json\n```";
        // Empty fence - no JSON starting with {
        assert!(extract_json(text).is_none());
    }

    #[test]
    fn extract_json_fence_with_whitespace() {
        let text = "```json\n  {\"key\": \"value\"}  \n```";
        let json = extract_json(text).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn parse_brief_found() {
        let text =
            "I did a bunch of work.\n\nBRIEF: Implemented the login feature and added tests.";
        let (brief, explicit) = parse_brief(text);
        assert_eq!(brief, "Implemented the login feature and added tests.");
        assert!(explicit);
    }

    #[test]
    fn parse_brief_not_found_uses_fallback() {
        let text = "I did a bunch of work. No brief marker here.";
        let (brief, explicit) = parse_brief(text);
        assert_eq!(brief, "I did a bunch of work. No brief marker here.");
        assert!(!explicit);
    }

    #[test]
    fn parse_brief_empty_value_uses_fallback() {
        let text = "Some output\nBRIEF:   \nMore text";
        let (brief, explicit) = parse_brief(text);
        assert_eq!(brief, "Some output");
        assert!(!explicit);
    }

    #[test]
    fn parse_brief_last_occurrence() {
        let text = "BRIEF: first\nsome text\nBRIEF: second and final";
        let (brief, explicit) = parse_brief(text);
        assert_eq!(brief, "second and final");
        assert!(explicit);
    }

    #[test]
    fn parse_brief_fallback_skips_headers() {
        let text = "# Summary\n\nThis is the main finding. It was significant.";
        let (brief, explicit) = parse_brief(text);
        assert_eq!(brief, "This is the main finding. It was significant.");
        assert!(!explicit);
    }

    #[test]
    fn parse_brief_fallback_empty_text() {
        let (brief, explicit) = parse_brief("");
        assert_eq!(brief, "Task completed.");
        assert!(!explicit);
    }

    #[test]
    fn apply_context_directives_drop_turns() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.add_assistant("a1".to_string());
        conv.add_user("u2".to_string());
        conv.add_assistant("a2".to_string());
        conv.add_user("u3".to_string());
        conv.add_assistant("a3".to_string());

        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}],"context":{"drop_turns":[1,2]}}"#;
        let (result, had_context) = apply_context_directives(json, &mut conv);

        // Messages 1,2 dropped (u1, a1)
        assert_eq!(conv.len(), 5);
        assert!(had_context);
        // context field stripped
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed.get("context").is_none());
        assert!(parsed.get("commands").is_some());
    }

    #[test]
    fn apply_context_directives_summarize() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.add_assistant("a1".to_string());
        conv.add_user("u2".to_string());
        conv.add_assistant("a2".to_string());
        conv.add_user("u3".to_string());
        conv.add_assistant("a3".to_string());

        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}],"context":{"summarize":{"turns":[1,2,3,4],"summary":"Setup phase"}}}"#;
        let (result, had_context) = apply_context_directives(json, &mut conv);

        assert_eq!(conv.len(), 4); // sys + summary + u3 + a3
        assert!(conv.messages()[1].content.contains("Setup phase"));
        assert!(had_context);
        assert!(!result.is_empty());
    }

    #[test]
    fn apply_context_directives_context_only() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.add_assistant("a1".to_string());
        conv.add_user("u2".to_string());
        conv.add_assistant("a2".to_string());
        conv.add_user("u3".to_string());
        conv.add_assistant("a3".to_string());

        let json = r#"{"commands":[],"context":{"drop_turns":[1,2]}}"#;
        let (result, had_context) = apply_context_directives(json, &mut conv);
        assert!(result.is_empty()); // no commands
        assert!(had_context); // but context was applied
    }

    #[test]
    fn apply_context_directives_no_context() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.add_assistant("a1".to_string());

        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#;
        let (result, had_context) = apply_context_directives(json, &mut conv);
        assert_eq!(conv.len(), 3); // unchanged
        assert!(!had_context);
        assert!(!result.is_empty());
    }

    #[test]
    fn apply_context_directives_empty_commands_no_context() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.add_assistant("a1".to_string());

        let json = r#"{"commands":[]}"#;
        let (result, had_context) = apply_context_directives(json, &mut conv);
        assert!(result.is_empty()); // no commands
        assert!(!had_context); // no context directives — signals task complete
    }

    #[test]
    fn done_signal_detected() {
        let json = r#"{"commands":[],"done":true,"message":"All tasks completed"}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(parsed
            .get("done")
            .and_then(|d| d.as_bool())
            .unwrap_or(false));
        assert_eq!(
            parsed.get("message").and_then(|m| m.as_str()),
            Some("All tasks completed")
        );
    }

    #[test]
    fn done_signal_without_message() {
        let json = r#"{"commands":[],"done":true}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(parsed
            .get("done")
            .and_then(|d| d.as_bool())
            .unwrap_or(false));
        assert!(parsed.get("message").and_then(|m| m.as_str()).is_none());
    }

    #[test]
    fn no_done_signal_continues() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(!parsed
            .get("done")
            .and_then(|d| d.as_bool())
            .unwrap_or(false));
    }

    #[test]
    fn inject_project_context_adds_memory_file() {
        let root = std::path::PathBuf::from("/tmp/proj");
        let project = Project {
            root: root.clone(),
            config: project::ProjectConfig::default(),
        };
        let input = r#"{"commands":[{"function":"storeMemory","nonce":1,"memory_key":"test","memory_summary":"hello"}]}"#;
        let result = inject_project_context(input, &project);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        // Build the expected path the same platform-aware way production does
        // (via `PathBuf::join`) instead of hardcoding '/'-joined POSIX text,
        // so the assertion holds on Windows (separator '\\') too.
        let expected = root
            .join(".intendant")
            .join("memory.json")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            parsed["commands"][0]["memory_file"].as_str().unwrap(),
            expected
        );
    }

    #[test]
    fn inject_project_context_preserves_existing() {
        let project = Project {
            root: std::path::PathBuf::from("/tmp/proj"),
            config: project::ProjectConfig::default(),
        };
        let input = r#"{"commands":[{"function":"storeMemory","nonce":1,"memory_file":"/custom/path.json"}]}"#;
        let result = inject_project_context(input, &project);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            parsed["commands"][0]["memory_file"].as_str().unwrap(),
            "/custom/path.json"
        );
    }

    #[test]
    fn inject_project_context_ignores_unrelated() {
        let project = Project {
            root: std::path::PathBuf::from("/tmp/proj"),
            config: project::ProjectConfig::default(),
        };
        let input = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#;
        let result = inject_project_context(input, &project);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["commands"][0].get("memory_file").is_none());
        assert!(parsed["commands"][0].get("project_dir").is_none());
    }

    #[test]
    fn budget_constants_are_sane() {
        assert!(SAFETY_CAP > 0);
        assert!(MIN_BUDGET_TOKENS > 0);
        assert!(BUDGET_WARNING_THRESHOLD > 0.0 && BUDGET_WARNING_THRESHOLD < 1.0);
    }

    #[test]
    fn is_simple_task_short() {
        assert!(is_simple_task("list files in /tmp"));
        assert!(is_simple_task("what is 2+2"));
        assert!(is_simple_task("echo hello"));
    }

    #[test]
    fn is_simple_task_complex_keywords() {
        assert!(!is_simple_task(
            "research the database schema and document findings"
        ));
        assert!(!is_simple_task("implement a new authentication system"));
        assert!(!is_simple_task("refactor the payment module"));
        assert!(!is_simple_task("build and deploy the application"));
        assert!(!is_simple_task("investigate why the tests are failing"));
    }

    #[test]
    fn is_simple_task_long() {
        let long_task = "x".repeat(150);
        assert!(!is_simple_task(&long_task));
    }

    #[test]
    fn is_simple_task_multiline() {
        assert!(!is_simple_task("line1\nline2\nline3\nline4"));
    }

    fn cli_flags_for_tests() -> CliFlags {
        CliFlags {
            task: None,
            provider: None,
            model: None,
            verbose: false,
            no_tui: false,
            mcp: false,
            autonomy: AutonomyLevel::Medium,
            log_file: None,
            continue_last: false,
            resume_id: None,
            control_socket: false,
            json_output: false,
            sandbox: false,
            direct: false,
            no_presence: false,
            web: false,
            web_port: web_gateway::DEFAULT_PORT,
            tls: false,
            tls_cert: None,
            tls_key: None,
            transcription: false,
            record_displays: Vec::new(),
            agent_backend: None,
            no_web: false,
            advertise_urls: Vec::new(),
        }
    }

    #[test]
    fn idle_web_defaults_to_daemon_without_no_tui() {
        let flags = cli_flags_for_tests();
        assert!(should_start_idle_web_daemon(true, &flags));
    }

    #[test]
    fn idle_web_daemon_requires_web_and_no_task() {
        let mut flags = cli_flags_for_tests();
        assert!(!should_start_idle_web_daemon(false, &flags));

        flags.task = Some("do the thing".to_string());
        assert!(!should_start_idle_web_daemon(true, &flags));
    }

    #[test]
    fn parse_cli_flags_empty() {
        // Can't easily test parse_cli_flags since it reads env::args(),
        // but we can test the struct defaults
        let flags = CliFlags {
            task: None,
            provider: None,
            model: None,
            verbose: false,
            no_tui: false,
            mcp: false,
            autonomy: AutonomyLevel::Medium,
            log_file: None,
            continue_last: false,
            resume_id: None,
            control_socket: false,
            json_output: false,
            sandbox: false,
            direct: false,
            no_presence: false,
            web: false,
            web_port: web_gateway::DEFAULT_PORT,
            tls: false,
            tls_cert: None,
            tls_key: None,
            transcription: false,
            record_displays: Vec::new(),

            agent_backend: None,

            no_web: false,

            advertise_urls: Vec::new(),
        };
        assert!(!flags.verbose);
        assert!(!flags.no_tui);
        assert!(!flags.mcp);
        assert!(!flags.continue_last);
        assert!(flags.resume_id.is_none());
        assert!(!flags.sandbox);
        assert!(!flags.json_output);
        assert!(!flags.direct);
        assert!(!flags.no_presence);
        assert!(!flags.web);
        assert!(!flags.no_web);
        assert!(!flags.transcription);
        assert_eq!(flags.web_port, 8765);
        assert_eq!(flags.autonomy, AutonomyLevel::Medium);
    }

    #[test]
    fn cli_web_flag() {
        let flags = CliFlags {
            task: None,
            provider: None,
            model: None,
            verbose: false,
            no_tui: false,
            mcp: false,
            autonomy: AutonomyLevel::Medium,
            log_file: None,
            continue_last: false,
            resume_id: None,
            control_socket: false,
            json_output: false,
            sandbox: false,
            direct: false,
            no_presence: false,
            web: true,
            web_port: web_gateway::DEFAULT_PORT,
            tls: false,
            tls_cert: None,
            tls_key: None,
            transcription: false,
            record_displays: Vec::new(),

            agent_backend: None,

            no_web: false,

            advertise_urls: Vec::new(),
        };
        assert!(flags.web);
        assert_eq!(flags.web_port, web_gateway::DEFAULT_PORT);
    }

    #[test]
    fn cli_web_with_port() {
        let flags = CliFlags {
            task: None,
            provider: None,
            model: None,
            verbose: false,
            no_tui: false,
            mcp: false,
            autonomy: AutonomyLevel::Medium,
            log_file: None,
            continue_last: false,
            resume_id: None,
            control_socket: false,
            json_output: false,
            sandbox: false,
            direct: false,
            no_presence: false,
            web: true,
            web_port: 9000,
            tls: false,
            tls_cert: None,
            tls_key: None,
            transcription: false,
            record_displays: Vec::new(),

            agent_backend: None,

            no_web: false,

            advertise_urls: Vec::new(),
        };
        assert!(flags.web);
        assert_eq!(flags.web_port, 9000);
    }

    #[test]
    fn format_command_preview_exec() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls -la /tmp"}]}"#;
        let preview = format_command_preview(json);
        assert!(preview.contains("exec: ls -la /tmp"));
    }

    #[test]
    fn format_command_preview_write_file() {
        let json = r#"{"commands":[{"function":"writeFile","nonce":1,"file_path":"/home/user/test.rs","content":"fn main(){}"}]}"#;
        let preview = format_command_preview(json);
        assert!(preview.contains("writeFile: /home/user/test.rs"));
    }

    #[test]
    fn format_command_preview_multiple() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"cargo build"},{"function":"inspectPath","nonce":2,"path":"/tmp"}]}"#;
        let preview = format_command_preview(json);
        assert!(preview.contains("exec: cargo build"));
        assert!(preview.contains("inspect: /tmp"));
        assert!(preview.contains(" | "));
    }

    #[test]
    fn format_command_preview_inspect() {
        let json = r#"{"commands":[{"function":"inspectPath","nonce":1,"path":"/tmp/dir"}]}"#;
        let preview = format_command_preview(json);
        assert!(preview.contains("inspect: /tmp/dir"));
    }

    #[test]
    fn format_command_preview_browse() {
        let json = r#"{"commands":[{"function":"browse","nonce":1,"url":"https://example.com"}]}"#;
        let preview = format_command_preview(json);
        assert!(preview.contains("browse: https://example.com"));
    }

    #[test]
    fn format_command_preview_invalid_json() {
        let json = "not json at all";
        let preview = format_command_preview(json);
        assert_eq!(preview, "not json at all");
    }

    #[test]
    fn has_ask_human_command_true() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1},{"function":"askHuman","nonce":2}]}"#;
        assert!(has_ask_human_command(json));
    }

    #[test]
    fn has_ask_human_command_false() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1}]}"#;
        assert!(!has_ask_human_command(json));
    }

    #[test]
    fn has_ask_human_command_invalid_json() {
        assert!(!has_ask_human_command("not json"));
    }

    #[test]
    fn has_capture_screen_command_true() {
        let json = r#"{"commands":[{"function":"captureScreen","nonce":1}]}"#;
        assert!(has_capture_screen_command(json));
    }

    #[test]
    fn has_capture_screen_command_false() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1}]}"#;
        assert!(!has_capture_screen_command(json));
    }

    #[test]
    fn has_capture_screen_command_mixed_batch() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1},{"function":"captureScreen","nonce":2}]}"#;
        assert!(has_capture_screen_command(json));
    }

    #[test]
    fn has_capture_screen_command_invalid_json() {
        assert!(!has_capture_screen_command("not json"));
    }

    #[test]
    fn encode_screenshot_valid() {
        let dir = tempfile::tempdir().unwrap();
        let img_path = dir.path().join("test.png");
        std::fs::write(&img_path, b"\x89PNG\r\n\x1a\n").unwrap();
        let json = serde_json::json!({
            "success": true,
            "screenshot_path": img_path.to_str().unwrap(),
        });
        let result = encode_screenshot(&json.to_string());
        assert!(result.is_some());
        let images = result.unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");
        assert!(!images[0].data.is_empty());
    }

    #[test]
    fn encode_screenshot_missing_file() {
        let json = r#"{"success":true,"screenshot_path":"/tmp/nonexistent_screenshot_12345.png"}"#;
        assert!(encode_screenshot(json).is_none());
    }

    #[test]
    fn encode_screenshot_success_false() {
        let json = r#"{"success":false,"screenshot_path":"/tmp/whatever.png"}"#;
        assert!(encode_screenshot(json).is_none());
    }

    #[test]
    fn encode_screenshot_invalid_json() {
        assert!(encode_screenshot("not json").is_none());
    }

    #[test]
    fn encode_screenshot_missing_path_field() {
        let json = r#"{"success":true}"#;
        assert!(encode_screenshot(json).is_none());
    }

    #[test]
    fn has_exec_command_true() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#;
        assert!(has_exec_command(json));
    }

    #[test]
    fn has_exec_command_pty() {
        let json = r#"{"commands":[{"function":"execPty","nonce":1,"command":"ls"}]}"#;
        assert!(has_exec_command(json));
    }

    #[test]
    fn has_exec_command_false_for_non_exec() {
        let json = r#"{"commands":[{"function":"captureScreen","nonce":1}]}"#;
        assert!(!has_exec_command(json));
    }

    #[test]
    fn has_exec_command_invalid_json() {
        assert!(!has_exec_command("not json"));
    }

    // --- assemble_batch_from_tool_calls tests ---

    #[test]
    fn assemble_batch_single_exec() {
        let calls = vec![provider::ToolCall {
            id: "call_1".to_string(),
            call_id: "call_1".to_string(),
            name: "exec_command".to_string(),
            arguments: r#"{"nonce":1,"command":"ls -la"}"#.to_string(),
        }];
        let result = assemble_batch_from_tool_calls(&calls);
        assert!(!result.is_done);
        assert!(result.context_directives.is_none());
        assert!(result.agent_input_json.is_some());

        let input: serde_json::Value =
            serde_json::from_str(result.agent_input_json.as_ref().unwrap()).unwrap();
        assert_eq!(input["commands"][0]["function"], "execAsAgent");
        assert_eq!(input["commands"][0]["command"], "ls -la");
        assert_eq!(input["commands"][0]["nonce"], 1);
        assert_eq!(result.nonce_to_call_id.get(&1), Some(&"call_1".to_string()));
    }

    #[test]
    fn assemble_batch_signal_done() {
        let calls = vec![provider::ToolCall {
            id: "call_1".to_string(),
            call_id: "call_1".to_string(),
            name: "signal_done".to_string(),
            arguments: r#"{"message":"All tasks completed"}"#.to_string(),
        }];
        let result = assemble_batch_from_tool_calls(&calls);
        assert!(result.is_done);
        assert_eq!(result.done_message.as_deref(), Some("All tasks completed"));
        assert!(result.agent_input_json.is_none());
    }

    #[test]
    fn assemble_batch_signal_done_no_message() {
        let calls = vec![provider::ToolCall {
            id: "call_1".to_string(),
            call_id: "call_1".to_string(),
            name: "signal_done".to_string(),
            arguments: r#"{}"#.to_string(),
        }];
        let result = assemble_batch_from_tool_calls(&calls);
        assert!(result.is_done);
        assert!(result.done_message.is_none());
    }

    #[test]
    fn assemble_batch_manage_context() {
        let calls = vec![provider::ToolCall {
            id: "call_1".to_string(),
            call_id: "call_1".to_string(),
            name: "manage_context".to_string(),
            arguments: r#"{"drop_turns":[1,2]}"#.to_string(),
        }];
        let result = assemble_batch_from_tool_calls(&calls);
        assert!(!result.is_done);
        assert!(result.agent_input_json.is_none());
        assert!(result.context_directives.is_some());
        let ctx = result.context_directives.unwrap();
        assert_eq!(ctx["drop_turns"][0], 1);
        assert_eq!(ctx["drop_turns"][1], 2);
    }

    #[test]
    fn assemble_batch_mixed_tools() {
        let calls = vec![
            provider::ToolCall {
                id: "call_1".to_string(),
                call_id: "call_1".to_string(),
                name: "exec_command".to_string(),
                arguments: r#"{"nonce":10,"command":"echo hello"}"#.to_string(),
            },
            provider::ToolCall {
                id: "call_2".to_string(),
                call_id: "call_2".to_string(),
                name: "inspect_path".to_string(),
                arguments: r#"{"nonce":11,"path":"/tmp"}"#.to_string(),
            },
            provider::ToolCall {
                id: "call_3".to_string(),
                call_id: "call_3".to_string(),
                name: "manage_context".to_string(),
                arguments: r#"{"drop_turns":[3]}"#.to_string(),
            },
        ];
        let result = assemble_batch_from_tool_calls(&calls);
        assert!(!result.is_done);
        assert!(result.context_directives.is_some());
        assert!(result.agent_input_json.is_some());

        let input: serde_json::Value =
            serde_json::from_str(result.agent_input_json.as_ref().unwrap()).unwrap();
        let commands = input["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0]["function"], "execAsAgent");
        assert_eq!(commands[1]["function"], "inspectPath");
        assert_eq!(result.nonce_to_call_id.len(), 2);
        assert_eq!(result.call_id_names.len(), 3);
    }

    #[test]
    fn assemble_batch_unknown_tool_ignored() {
        let calls = vec![provider::ToolCall {
            id: "call_1".to_string(),
            call_id: "call_1".to_string(),
            name: "nonexistent_tool".to_string(),
            arguments: r#"{"nonce":1}"#.to_string(),
        }];
        let result = assemble_batch_from_tool_calls(&calls);
        assert!(result.agent_input_json.is_none());
    }

    #[test]
    fn assemble_batch_duplicate_nonce_emits_error() {
        let calls = vec![
            provider::ToolCall {
                id: "call_1".to_string(),
                call_id: "call_1".to_string(),
                name: "exec_command".to_string(),
                arguments: r#"{"nonce":1,"command":"echo a"}"#.to_string(),
            },
            provider::ToolCall {
                id: "call_2".to_string(),
                call_id: "call_2".to_string(),
                name: "inspect_path".to_string(),
                arguments: r#"{"nonce":1,"path":"/tmp"}"#.to_string(),
            },
        ];
        let result = assemble_batch_from_tool_calls(&calls);
        assert_eq!(result.precomputed_results.len(), 1);
        assert!(result.precomputed_results[0]
            .2
            .contains("duplicate nonce 1"));
    }

    #[test]
    fn assemble_batch_tool_name_mapping() {
        // Verify all tool names map correctly
        let tool_pairs = vec![
            ("exec_command", "execAsAgent"),
            ("capture_screen", "captureScreen"),
            ("inspect_path", "inspectPath"),
            ("edit_file", "editFile"),
            ("browse_url", "browse"),
            ("ask_human", "askHuman"),
            ("exec_pty", "execPty"),
            ("store_memory", "storeMemory"),
            ("recall_memory", "recallMemory"),
        ];
        for (tool_name, expected_func) in tool_pairs {
            let calls = vec![provider::ToolCall {
                id: "call_1".to_string(),
                call_id: "call_1".to_string(),
                name: tool_name.to_string(),
                arguments: r#"{"nonce":1,"command":"test","status_type":"stdout","path":"/tmp","file_path":"/tmp/f","operation":"write","url":"http://x","question":"?","memory_key":"k","memory_summary":"s","memory_query":"q"}"#.to_string(),
            }];
            let result = assemble_batch_from_tool_calls(&calls);
            let input: serde_json::Value =
                serde_json::from_str(result.agent_input_json.as_ref().unwrap()).unwrap();
            assert_eq!(
                input["commands"][0]["function"].as_str().unwrap(),
                expected_func,
                "Tool {} should map to function {}",
                tool_name,
                expected_func
            );
        }
    }

    // --- map_results_to_tool_responses tests ---

    #[test]
    fn map_results_single_exec() {
        let stdout = "{\"type\":\"status\",\"nonce\":1,\"status\":\"r\",\"pid\":0,\"exit_code\":0}\n{\"type\":\"status\",\"nonce\":1,\"status\":\"c\",\"pid\":1234,\"exit_code\":0}\n";
        let stderr = "";
        let mut nonce_map = std::collections::HashMap::new();
        nonce_map.insert(1u64, "call_1".to_string());
        let call_ids = vec![("call_1".to_string(), "exec_command".to_string())];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "call_1");
        assert!(results[0].2.contains("1c0"));
    }

    #[test]
    fn map_results_with_result_output() {
        let stdout = "{\"type\":\"status\",\"nonce\":5,\"status\":\"c\",\"pid\":0,\"exit_code\":0}\n{\"type\":\"result\",\"nonce\":5,\"data\":\"{\\\"content\\\":\\\"hello\\\",\\\"total_size\\\":5}\"}\n";
        let stderr = "";
        let mut nonce_map = std::collections::HashMap::new();
        nonce_map.insert(5u64, "call_1".to_string());
        let call_ids = vec![("call_1".to_string(), "inspect_path".to_string())];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 1);
        assert!(results[0].2.contains("5c0"));
        assert!(results[0].2.contains("\"content\":\"hello\""));
    }

    #[test]
    fn map_results_with_stderr() {
        let stdout =
            "{\"type\":\"status\",\"nonce\":1,\"status\":\"c\",\"pid\":0,\"exit_code\":1}\n";
        let stderr = "command not found";
        let mut nonce_map = std::collections::HashMap::new();
        nonce_map.insert(1u64, "call_1".to_string());
        let call_ids = vec![("call_1".to_string(), "exec_command".to_string())];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 1);
        assert!(results[0].2.contains("1c1"));
        assert!(results[0].2.contains("stderr: command not found"));
    }

    #[test]
    fn map_results_signal_done() {
        let stdout = "";
        let stderr = "";
        let nonce_map = std::collections::HashMap::new();
        let call_ids = vec![("call_1".to_string(), "signal_done".to_string())];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].2, "OK");
    }

    #[test]
    fn map_results_manage_context() {
        let stdout = "";
        let stderr = "";
        let nonce_map = std::collections::HashMap::new();
        let call_ids = vec![("call_1".to_string(), "manage_context".to_string())];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].2, "OK");
    }

    #[test]
    fn map_results_multiple_tools() {
        let stdout = "{\"type\":\"status\",\"nonce\":10,\"status\":\"r\",\"pid\":0,\"exit_code\":0}\n{\"type\":\"status\",\"nonce\":10,\"status\":\"c\",\"pid\":0,\"exit_code\":0}\n{\"type\":\"status\",\"nonce\":11,\"status\":\"c\",\"pid\":0,\"exit_code\":0}\n{\"type\":\"result\",\"nonce\":11,\"data\":\"{\\\"exists\\\":true}\"}\n";
        let stderr = "";
        let mut nonce_map = std::collections::HashMap::new();
        nonce_map.insert(10u64, "call_1".to_string());
        nonce_map.insert(11u64, "call_2".to_string());
        let call_ids = vec![
            ("call_1".to_string(), "exec_command".to_string()),
            ("call_2".to_string(), "inspect_path".to_string()),
        ];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 2);
        // exec_command should have its status
        assert!(results[0].2.contains("10c0"));
        // inspect_path should have result data
        assert!(results[1].2.contains("\"exists\":true"));
    }

    #[test]
    fn map_results_empty_output() {
        let stdout = "";
        let stderr = "";
        let mut nonce_map = std::collections::HashMap::new();
        nonce_map.insert(1u64, "call_1".to_string());
        let call_ids = vec![("call_1".to_string(), "exec_command".to_string())];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].2, "OK");
    }

    #[test]
    fn external_tool_output_limiter_caps_each_tool() {
        let mut limiter = ExternalToolOutputLimiter::default();
        let first = "a".repeat(EXTERNAL_TOOL_OUTPUT_ACTIVITY_LIMIT - 2);
        let out = limiter.filter("item-1", first.clone()).unwrap();
        assert_eq!(out, first);

        let out = limiter.filter("item-1", "bcdef".to_string()).unwrap();
        assert!(out.starts_with("bc"));
        assert!(out.contains("output truncated by Intendant"));
        assert!(
            limiter.filter("item-1", "more".to_string()).is_none(),
            "further output after truncation should be suppressed"
        );
    }

    #[test]
    fn external_tool_output_limiter_resets_on_completion() {
        let mut limiter = ExternalToolOutputLimiter::default();
        let oversized = "a".repeat(EXTERNAL_TOOL_OUTPUT_ACTIVITY_LIMIT + 10);
        let out = limiter.filter("item-1", oversized).unwrap();
        assert!(out.contains("output truncated by Intendant"));

        limiter.complete("item-1");

        let out = limiter.filter("item-1", "fresh".to_string()).unwrap();
        assert_eq!(out, "fresh");
    }

    #[test]
    fn external_tool_failure_content_includes_item_and_preview() {
        let content = external_tool_failure_content(
            "call-1",
            "command exited 1",
            Some("command: rg missing static/app.html"),
        );

        assert_eq!(
            content,
            "Command failed (call-1): command exited 1\nCommand: rg missing static/app.html"
        );
    }

    #[test]
    fn external_tool_failure_content_omits_empty_preview() {
        let content = external_tool_failure_content("call-1", "unknown error", Some("  "));

        assert_eq!(content, "Tool failed (call-1): unknown error");
    }

    #[test]
    fn external_agent_log_source_prefers_backend_source() {
        assert_eq!(external_agent_log_source(Some("Codex")), "Codex");
        assert_eq!(external_agent_log_source(Some("  ")), "worker");
        assert_eq!(external_agent_log_source(None), "worker");
    }

    #[test]
    fn external_tool_preview_text_combines_tool_name_and_preview() {
        assert_eq!(
            external_tool_preview_text("command", "rg needle file").as_deref(),
            Some("command: rg needle file")
        );
        assert_eq!(
            external_tool_preview_text("", "rg needle file").as_deref(),
            Some("rg needle file")
        );
        assert_eq!(external_tool_preview_text("", ""), None);
    }

    // ── Steer fallback plumbing ──
    //
    // The full `drain_external_agent_events` loop is integration-heavy
    // (needs a backend + event channel); we unit-test the smaller helpers
    // that encapsulate the fallback policy. The end-to-end flow is covered
    // indirectly by the dispatcher / Codex tests.

    #[tokio::test]
    async fn drain_steer_queue_as_followup_prefixes_single_queued_item() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let queue = event::ContextInjectionQueue::default();
        queue
            .lock()
            .unwrap()
            .push(event::ContextInjection::text_with_steer_id(
                "switch to Python".into(),
                "steer-1".into(),
            ));

        let merged = drain_steer_queue_as_followup(&queue, "original follow-up", &bus, None)
            .expect("should produce a message");

        assert_eq!(merged, "[User] switch to Python\noriginal follow-up");
        // Queue drained.
        assert!(queue.lock().unwrap().is_empty());

        // SteerDelivered emitted for the drained item.
        let ev = rx.try_recv().expect("SteerDelivered event");
        match ev {
            AppEvent::SteerDelivered { id, mid_turn, .. } => {
                assert_eq!(id, "steer-1");
                assert!(!mid_turn, "queued fallback should report mid_turn=false");
            }
            other => panic!("expected SteerDelivered, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn drain_steer_queue_as_followup_ignores_non_steer_entries() {
        // A ContextInjection without a steer_id (e.g. from display
        // takeover) must be left alone — the native agent loop still owns
        // draining those.
        let bus = EventBus::new();
        let queue = event::ContextInjectionQueue::default();
        queue
            .lock()
            .unwrap()
            .push(event::ContextInjection::text("display grant".into()));

        let merged = drain_steer_queue_as_followup(&queue, "follow-up", &bus, None)
            .expect("should produce a message");
        assert_eq!(merged, "follow-up");
        assert_eq!(queue.lock().unwrap().len(), 1, "non-steer entry preserved");
    }

    #[tokio::test]
    async fn drain_steer_queue_as_followup_no_queue_no_followup_is_none() {
        // Empty queue + empty followup => Some(None) (caller will skip the
        // send). Verifies the "steer only + empty follow-up" degenerate
        // case doesn't produce an empty agent message.
        let bus = EventBus::new();
        let queue = event::ContextInjectionQueue::default();
        assert!(drain_steer_queue_as_followup(&queue, "", &bus, None).is_none());
    }

    #[tokio::test]
    async fn drain_steer_queue_as_followup_combines_multiple_items() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let queue = event::ContextInjectionQueue::default();
        {
            let mut q = queue.lock().unwrap();
            q.push(event::ContextInjection::text_with_steer_id(
                "first".into(),
                "s1".into(),
            ));
            q.push(event::ContextInjection::text_with_steer_id(
                "second".into(),
                "s2".into(),
            ));
        }

        let merged = drain_steer_queue_as_followup(&queue, "main", &bus, None).expect("merged");
        assert_eq!(merged, "[User] first\n[User] second\nmain");

        let mut delivered_ids: Vec<String> = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::SteerDelivered { id, mid_turn, .. } = ev {
                assert!(!mid_turn);
                delivered_ids.push(id);
            }
        }
        assert_eq!(delivered_ids, vec!["s1".to_string(), "s2".to_string()]);
    }

    #[test]
    fn flush_pending_runtime_steers_delivers_only_matching_session() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut pending = std::collections::VecDeque::from([
            PendingRuntimeSteer {
                session_id: Some("parent".to_string()),
                id: "steer-parent".to_string(),
                text: "parent steer".to_string(),
            },
            PendingRuntimeSteer {
                session_id: Some("side".to_string()),
                id: "steer-side".to_string(),
                text: "side steer".to_string(),
            },
        ]);

        let delivered =
            flush_pending_runtime_steers_for_session(&bus, &mut pending, &Some("side".into()));

        assert_eq!(delivered, 1);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "steer-parent");

        let ev = rx.try_recv().expect("SteerDelivered event");
        match ev {
            AppEvent::SteerDelivered {
                session_id,
                id,
                mid_turn,
            } => {
                assert_eq!(session_id.as_deref(), Some("side"));
                assert_eq!(id, "steer-side");
                assert!(mid_turn);
            }
            other => panic!("expected SteerDelivered, got {:?}", other),
        }
    }
}

const PROGRESS_INTERVAL: usize = 5;

#[allow(clippy::too_many_arguments)]
async fn run_agent_loop(
    provider: &dyn provider::ChatProvider,
    conversation: &mut Conversation,
    project: &Project,
    sub_agent_mode: Option<&(String, sub_agent::SubAgentRole)>,
    bus: &EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: &std::path::Path,
    mcp_mgr: Option<&mcp_client::McpClientManager>,
    json_approval: Option<&JsonApprovalSlot>,
    approval_registry: &event::ApprovalRegistry,
    context_injection: &event::ContextInjectionQueue,
    mut xvfb_guard: &mut Option<vision::XvfbGuard>,
    // When true, askHuman is unavailable and approvals without a json_approval
    // slot are auto-denied (headless non-JSON mode).
    headless: bool,
) -> Result<(LoopStats, LoopExitReason), CallerError> {
    let mut budget_warning_shown = false;
    let mut empty_command_streak = 0usize;
    let mut cu_action_counter = 0u64;
    let mut loop_stats = LoopStats::default();
    let mut seen_sub_agent_results: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut exit_reason = LoopExitReason::TaskComplete;

    // Discard stale System injections from before this task started
    // (e.g. display take/release events that happened while idle), but
    // PRESERVE User injections — those come from the dashboard's annotation
    // Send button and may have been queued while the agent was idle. We owe
    // the user the courtesy of actually delivering what they sent.
    if let Ok(mut q) = context_injection.lock() {
        q.retain(|inj| inj.source == event::InjectionSource::User);
    }

    // Cancellation plumbing: a watcher task flips the token when it sees
    // AppEvent::InterruptRequested on the bus, and drains the approval
    // registry so any in-flight `rx.await` inside the approval handler
    // unblocks immediately. The loop checks the token at its boundaries
    // and wraps the streaming API call in tokio::select! so an interrupt
    // mid-stream drops the response cleanly.
    //
    // The same watcher also handles AppEvent::SteerRequested: it pushes
    // the steer text onto the shared `context_injection` queue (tagged as
    // a user injection so it survives inter-task drains) and emits
    // `SteerAccepted`. The native agent loop drains `context_injection` at
    // the top of every turn and emits `SteerDelivered` at that point, so
    // queued steers are distinguishable from actual model-context delivery.
    // We keep the watcher alive across multiple steers — unlike the interrupt
    // branch which exits after cancelling.
    let local_session_id = session_log_id(&session_log);
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let cancel_watcher_handle = {
        let watcher_token = cancel_token.clone();
        let watcher_registry = approval_registry.clone();
        let watcher_injection = context_injection.clone();
        let watcher_bus = bus.clone();
        let watcher_session_id = local_session_id.clone();
        let mut bus_rx = bus.subscribe();
        tokio::spawn(async move {
            loop {
                match bus_rx.recv().await {
                    Ok(AppEvent::InterruptRequested { session_id })
                        if event_targets_session(&session_id, &watcher_session_id) =>
                    {
                        // Drain pending approvals with Deny so their
                        // receivers unblock and the loop can reach its
                        // cancellation-check boundary.
                        let pending: Vec<_> = {
                            let mut reg = watcher_registry.lock().unwrap();
                            reg.drain().collect()
                        };
                        for (_, sender) in pending {
                            let _ = sender.send(event::ApprovalResponse::Deny);
                        }
                        watcher_token.cancel();
                        break;
                    }
                    Ok(AppEvent::SteerRequested {
                        session_id,
                        text,
                        id,
                    }) if event_targets_session(&session_id, &watcher_session_id) => {
                        // Queue the steer for the next turn's drain. The
                        // native loop has no separate "mid-turn inject"
                        // hook — model calls are atomic — so acceptance and
                        // delivery are separate UI states.
                        if let Ok(mut q) = watcher_injection.lock() {
                            q.push(event::ContextInjection::text_with_steer_id(
                                text,
                                id.clone(),
                            ));
                        }
                        watcher_bus.send(AppEvent::SteerAccepted {
                            session_id: watcher_session_id.clone(),
                            id,
                            reason: "Queued for the next model checkpoint".to_string(),
                        });
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    // Guard that aborts the watcher and drains approvals exactly once on
    // any exit (interrupt OR normal completion). We cancel the watcher on
    // drop so it stops listening, and we proactively resolve any pending
    // approvals with Deny if the exit path was interrupt-driven.
    struct InterruptGuard {
        watcher: Option<tokio::task::JoinHandle<()>>,
    }
    impl Drop for InterruptGuard {
        fn drop(&mut self) {
            if let Some(h) = self.watcher.take() {
                h.abort();
            }
        }
    }
    let _guard = InterruptGuard {
        watcher: Some(cancel_watcher_handle),
    };

    for turn in 1..=SAFETY_CAP {
        // Interrupt check at loop boundary.
        if cancel_token.is_cancelled() {
            // Drain and deny any pending approvals so their receivers unblock.
            let pending: Vec<_> = {
                let mut reg = approval_registry.lock().unwrap();
                reg.drain().collect()
            };
            for (_, sender) in pending {
                let _ = sender.send(event::ApprovalResponse::Deny);
            }
            bus.send(AppEvent::Interrupted {
                session_id: local_session_id.clone(),
                reason: "user requested".into(),
            });
            slog(&session_log, |l| l.info("Agent loop interrupted"));
            return Ok((loop_stats, LoopExitReason::Interrupted));
        }
        // Check budget before sending
        if conversation.remaining_budget() <= MIN_BUDGET_TOKENS {
            let remaining = conversation.remaining_budget();
            slog(&session_log, |l| {
                l.warn(&format!(
                    "Budget exhausted ({} tokens remaining)",
                    remaining
                ))
            });
            bus.send(AppEvent::BudgetExhausted { remaining });
            exit_reason = LoopExitReason::BudgetExhausted;
            break;
        }

        // Drain context injection queue (display takeover messages, presence
        // interjections, steer fallbacks, etc.). Steer entries (tagged with
        // `steer_id`) are surfaced as `[User]` so the model reads them as
        // user direction; everything else uses the `[System]` prefix it has
        // always used.
        if let Ok(mut q) = context_injection.lock() {
            for inj in q.drain(..) {
                let prefix = if inj.steer_id.is_some() {
                    "User"
                } else {
                    "System"
                };
                let text = format!("[{}] {}", prefix, inj.text);
                if inj.images.is_empty() {
                    conversation.add_user(text.clone());
                } else {
                    conversation.add_user_with_images(text.clone(), inj.images);
                }
                slog(&session_log, |l| {
                    l.info(&format!("Context injected: {}", inj.text))
                });
                if let Some(id) = inj.steer_id {
                    bus.send(AppEvent::SteerDelivered {
                        session_id: local_session_id.clone(),
                        id,
                        mid_turn: false,
                    });
                }
            }
        }

        conversation.increment_turn();
        let budget_pct = conversation.usage_fraction() * 100.0;
        let remaining = conversation.remaining_budget();

        slog(&session_log, |l| l.turn_start(turn, budget_pct, remaining));

        bus.send(AppEvent::TurnStarted {
            session_id: local_session_id.clone(),
            turn,
            budget_pct,
            remaining,
        });

        // When CU is enabled, the OpenAI computer tool rejects multiple images.
        // Strip all but the most recent screenshot before each API call so the
        // logged context matches the payload sent to the model.
        if provider.cu_enabled() {
            conversation.strip_old_images();
        }

        // Log the full messages array being sent to the API
        slog(&session_log, |l| {
            if let Ok(json) = serde_json::to_string_pretty(conversation.messages()) {
                l.messages_input(&json);
            }
        });
        match provider.request_snapshot(conversation.messages(), true) {
            Ok((context_format, raw_context)) => {
                bus.send(AppEvent::ContextSnapshot {
                    session_id: local_session_id.clone(),
                    source: "native".to_string(),
                    label: "Internal agent request payload".to_string(),
                    turn: Some(turn),
                    format: context_format,
                    token_count: conversation.last_usage().map(|u| u.total_tokens),
                    context_window: Some(conversation.context_window()),
                    item_count: provider_request_item_count(&raw_context),
                    raw: raw_context,
                });
            }
            Err(e) => {
                slog(&session_log, |l| {
                    l.warn(&format!(
                        "Failed to build provider request context snapshot: {}",
                        e
                    ))
                });
            }
        }

        // Streaming API call — wrapped in select! so an interrupt cancels
        // mid-stream without waiting for the provider to finish. The
        // interrupt branch returns `None` so the surrounding block can
        // handle drain-and-exit identically to the top-of-loop check.
        let response_opt: Option<provider::ChatResponse> = {
            const STREAM_RETRIES: u32 = 3;
            let mut last_stream_err = None;
            let mut resp = None;
            let mut was_cancelled = false;
            for attempt in 0..=STREAM_RETRIES {
                let stream_bus = bus.clone();
                let stream_session_id = local_session_id.clone();
                let on_stream_event = move |event: crate::provider::StreamEvent| {
                    if let crate::provider::StreamEvent::Delta(ref text) = event {
                        stream_bus.send(AppEvent::ModelResponseDelta {
                            session_id: stream_session_id.clone(),
                            text: text.clone(),
                        });
                    }
                };
                let stream_fut = provider.chat_stream(conversation.messages(), &on_stream_event);
                let outcome = tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        was_cancelled = true;
                        break;
                    }
                    r = stream_fut => r,
                };
                match outcome {
                    Ok(r) => {
                        resp = Some(r);
                        break;
                    }
                    Err(e) => {
                        let is_stream_error = e.to_string().contains("Stream error");
                        if is_stream_error && attempt < STREAM_RETRIES {
                            slog(&session_log, |l| {
                                l.warn(&format!(
                                    "Stream error (attempt {}/{}), retrying: {}",
                                    attempt + 1,
                                    STREAM_RETRIES + 1,
                                    e
                                ))
                            });
                            let delay = std::time::Duration::from_millis(
                                1000 * 2u64.pow(attempt) + (turn as u64 % 500),
                            );
                            // Retries are also interruptible — don't sit in
                            // a sleep while the user is trying to cancel.
                            tokio::select! {
                                biased;
                                _ = cancel_token.cancelled() => {
                                    was_cancelled = true;
                                    break;
                                }
                                _ = tokio::time::sleep(delay) => {}
                            }
                            last_stream_err = Some(e);
                            continue;
                        }
                        slog(&session_log, |l| l.error(&format!("API error: {}", e)));
                        bus.send(AppEvent::LoopError(e.to_string()));
                        return Err(e);
                    }
                }
            }
            if was_cancelled {
                None
            } else {
                match resp {
                    Some(r) => Some(r),
                    None => {
                        let e = last_stream_err.unwrap_or_else(|| {
                            CallerError::Provider("Stream failed after retries".to_string())
                        });
                        slog(&session_log, |l| l.error(&format!("API error: {}", e)));
                        bus.send(AppEvent::LoopError(e.to_string()));
                        return Err(e);
                    }
                }
            }
        };

        // Cancelled mid-stream → drain approvals and exit via Interrupted.
        let response = match response_opt {
            Some(r) => r,
            None => {
                let pending: Vec<_> = {
                    let mut reg = approval_registry.lock().unwrap();
                    reg.drain().collect()
                };
                for (_, sender) in pending {
                    let _ = sender.send(event::ApprovalResponse::Deny);
                }
                bus.send(AppEvent::Interrupted {
                    session_id: local_session_id.clone(),
                    reason: "user requested".into(),
                });
                slog(&session_log, |l| {
                    l.info("Agent loop interrupted mid-stream")
                });
                return Ok((loop_stats, LoopExitReason::Interrupted));
            }
        };
        conversation.set_usage(response.usage.clone());

        // Auto-compact when context usage exceeds 90%
        if conversation.auto_compact() {
            slog(&session_log, |l| {
                l.info(&format!("Auto-compacted conversation at turn {}", turn))
            });
            bus.send(AppEvent::ContextManagement { turn });
        }

        loop_stats.turns = turn;
        loop_stats.usage.prompt_tokens += response.usage.prompt_tokens;
        loop_stats.usage.completion_tokens += response.usage.completion_tokens;
        loop_stats.usage.total_tokens += response.usage.total_tokens;
        if !response.content.is_empty() {
            loop_stats.last_response = Some(response.content.clone());
        }

        // Store assistant message — with or without tool calls
        let has_tool_calls = !response.tool_calls.is_empty();
        let has_cu_calls = !response.cu_calls.is_empty();
        if has_tool_calls || has_cu_calls {
            let refs: Vec<conversation::ToolCallRef> = response
                .tool_calls
                .iter()
                .map(|tc| conversation::ToolCallRef {
                    id: tc.id.clone(),
                    call_id: tc.call_id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                })
                .collect();
            conversation.add_assistant_tool_calls(
                response.content.clone(),
                refs,
                response.raw_output.clone(),
            );
        } else {
            conversation.add_assistant(response.content.clone());
        }

        // Log the full model response (no truncation)
        slog(&session_log, |l| {
            l.model_response(
                &response.content,
                response.usage.prompt_tokens,
                response.usage.completion_tokens,
                response.usage.total_tokens,
                response.usage.cached_tokens,
                None,
            )
        });

        // Log reasoning content if available
        if response.reasoning_summary.is_some() || response.reasoning_content.is_some() {
            slog(&session_log, |l| {
                l.reasoning_content(
                    response.reasoning_summary.as_deref(),
                    response.reasoning_content.as_deref(),
                )
            });
        }

        // Check budget warning
        if !budget_warning_shown && conversation.usage_fraction() >= BUDGET_WARNING_THRESHOLD {
            let pct = conversation.usage_fraction() * 100.0;
            let remaining = conversation.remaining_budget();
            slog(&session_log, |l| {
                l.warn(&format!(
                    "Budget warning: {:.0}% used, {} remaining",
                    pct, remaining
                ))
            });
            bus.send(AppEvent::BudgetWarning { pct, remaining });
            budget_warning_shown = true;
        }

        // Write sub-agent progress periodically
        if let Some((id, _role)) = sub_agent_mode {
            if turn % PROGRESS_INTERVAL == 0 {
                if let Ok(progress_path) = env::var("INTENDANT_PROGRESS_FILE") {
                    let last_action = response.content.chars().take(500).collect::<String>();
                    let progress = sub_agent::SubAgentProgress {
                        id: id.clone(),
                        turn,
                        status: "running".to_string(),
                        last_action,
                        question: None,
                    };
                    let _ =
                        sub_agent::write_progress(std::path::Path::new(&progress_path), &progress);
                }
            }
        }

        // For CU-only turns, synthesize a content summary from the actions
        let display_content = if response.content.is_empty() && has_cu_calls {
            let descs: Vec<String> = response
                .cu_calls
                .iter()
                .flat_map(|cu| {
                    cu.actions.iter().map(|a| match a {
                        computer_use::CuAction::Click { x, y, .. } => format!("click({},{})", x, y),
                        computer_use::CuAction::DoubleClick { x, y, .. } => {
                            format!("double_click({},{})", x, y)
                        }
                        computer_use::CuAction::Type { text } => {
                            format!("type(\"{}\")", &text[..text.len().min(30)])
                        }
                        computer_use::CuAction::Key { key } => format!("key({})", key),
                        computer_use::CuAction::Scroll { x, y, .. } => {
                            format!("scroll({},{})", x, y)
                        }
                        computer_use::CuAction::Screenshot => "screenshot".to_string(),
                        computer_use::CuAction::Wait { .. } => "wait".to_string(),
                        _ => format!("{:?}", a),
                    })
                })
                .collect();
            descs.join(" → ")
        } else {
            response.content.clone()
        };

        bus.send(AppEvent::ModelResponse {
            session_id: local_session_id.clone(),
            turn,
            content: display_content,
            usage: response.usage.clone(),
            reasoning: response.reasoning_summary.clone(),
            source: None,
        });

        // ====== TOOL CALL PATH vs TEXT EXTRACTION PATH ======
        if has_tool_calls {
            // --- Native tool call path ---
            let batch = assemble_batch_from_tool_calls(&response.tool_calls);

            for (call_id, tool_name, result_text) in &batch.precomputed_results {
                conversation.add_tool_result(call_id, tool_name, result_text);
            }

            // Apply context directives from manage_context tool call
            if let Some(ref ctx) = batch.context_directives {
                if let Some(drops) = ctx.get("drop_turns").and_then(|d| d.as_array()) {
                    let indices: Vec<usize> = drops
                        .iter()
                        .filter_map(|v| v.as_u64().map(|n| n as usize))
                        .collect();
                    conversation.drop_turns(&indices);
                }
                if let Some(summarize) = ctx.get("summarize") {
                    if let (Some(turns), Some(summary)) = (
                        summarize.get("turns").and_then(|t| t.as_array()),
                        summarize.get("summary").and_then(|s| s.as_str()),
                    ) {
                        let indices: Vec<usize> = turns
                            .iter()
                            .filter_map(|v| v.as_u64().map(|n| n as usize))
                            .collect();
                        conversation.summarize_turns(&indices, summary);
                    }
                }
                slog(&session_log, |l| {
                    l.debug("Context directives applied (tool call)")
                });
            }

            // Check done signal
            if batch.is_done {
                slog(&session_log, |l| {
                    l.info(&format!(
                        "Done signal received (tool call): {}",
                        batch.done_message.as_deref().unwrap_or("(no message)")
                    ))
                });
                // Send tool results for all calls including signal_done
                for (call_id, tool_name, _) in map_results_to_tool_responses(
                    "",
                    "",
                    &batch.nonce_to_call_id,
                    &batch.call_id_names,
                ) {
                    conversation.add_tool_result(&call_id, &tool_name, "OK");
                }
                bus.send(AppEvent::DoneSignal {
                    message: batch.done_message.clone(),
                });
                exit_reason = LoopExitReason::DoneSignal;
                break;
            }

            // Process MCP tool calls (if any)
            if !batch.mcp_calls.is_empty() {
                if let Some(mgr) = mcp_mgr {
                    for (call_id, tool_name, args_json) in &batch.mcp_calls {
                        let args: serde_json::Value =
                            serde_json::from_str(args_json).unwrap_or_default();
                        let result = mgr.call_tool(tool_name, args).await;
                        let output = match result {
                            Ok(text) => text,
                            Err(e) => format!("MCP tool error: {}", e),
                        };
                        conversation.add_tool_result(call_id, tool_name, &output);
                    }
                } else {
                    for (call_id, tool_name, _) in &batch.mcp_calls {
                        conversation.add_tool_result(
                            call_id,
                            tool_name,
                            "Error: MCP client not configured",
                        );
                    }
                }
            }

            // Process invoke_skill tool calls (if any)
            for (call_id, skill_name, arguments) in &batch.skill_invocations {
                let discovered = skills::discover_skills(Some(&project.root));
                match discovered.iter().find(|s| s.config.name == *skill_name) {
                    Some(skill) => {
                        let body = skills::load_skill_body(skill, arguments);
                        slog(&session_log, |l| {
                            l.info(&format!(
                                "Invoked skill '{}' (args: {})",
                                skill_name,
                                if arguments.is_empty() {
                                    "(none)"
                                } else {
                                    arguments
                                }
                            ))
                        });
                        conversation.add_tool_result(
                            call_id,
                            "invoke_skill",
                            &format!(
                                "Skill '{}' loaded. Follow these instructions:\n\n{}",
                                skill_name, body
                            ),
                        );
                    }
                    None => {
                        let available: Vec<&str> =
                            discovered.iter().map(|s| s.config.name.as_str()).collect();
                        conversation.add_tool_result(
                            call_id,
                            "invoke_skill",
                            &format!(
                                "Error: skill '{}' not found. Available: {}",
                                skill_name,
                                if available.is_empty() {
                                    "(none)".to_string()
                                } else {
                                    available.join(", ")
                                }
                            ),
                        );
                    }
                }
            }

            // Handle live audio spawn requests (blocking)
            for (call_id, session_id, args) in &batch.live_audio_spawns {
                let spec_result =
                    serde_json::from_value::<live_audio_types::LiveAudioSpec>(args.clone());
                match spec_result {
                    Ok(mut spec) => {
                        let system_prompt = prompts::build_live_audio_prompt(
                            &spec.playbook,
                            &spec.response_schema,
                            Some(&project.root),
                        );
                        spec.playbook = system_prompt;

                        let api_key_var = match spec.provider {
                            live_audio_types::LiveAudioProvider::Gemini => "GEMINI_API_KEY",
                            live_audio_types::LiveAudioProvider::OpenAI => "OPENAI_API_KEY",
                        };
                        let api_key = match std::env::var(api_key_var) {
                            Ok(k) => k,
                            Err(_) => {
                                conversation.add_tool_result(
                                    call_id,
                                    "spawn_live_audio",
                                    &format!("Error: {} not set", api_key_var),
                                );
                                continue;
                            }
                        };

                        // Vortex shared-memory probe is POSIX-only
                        // (`shm_open`). On Windows the Vortex bridge isn't
                        // available, so the probe is compiled out and the
                        // code falls through to the regular audio bridge.
                        #[cfg(unix)]
                        let vortex_shm_available = unsafe {
                            let fd = libc::shm_open(
                                b"/vortex-audio\0".as_ptr() as *const libc::c_char,
                                libc::O_RDONLY,
                                0,
                            );
                            if fd >= 0 {
                                libc::close(fd);
                                true
                            } else {
                                false
                            }
                        };
                        #[cfg(not(unix))]
                        let vortex_shm_available = false;
                        let mut bridge = if vortex_shm_available {
                            audio_routing::create_vortex_bridge("shm")
                        } else {
                            match audio_routing::create_bridge(session_id).await {
                                Ok(b) => b,
                                Err(e) => {
                                    conversation.add_tool_result(
                                        call_id,
                                        "spawn_live_audio",
                                        &format!("Error creating audio bridge: {}", e),
                                    );
                                    continue;
                                }
                            }
                        };

                        if bridge.vortex_socket_path().is_none() {
                            if let Err(e) = audio_routing::set_as_default(&mut bridge).await {
                                slog(&session_log, |l| {
                                    l.warn(&format!("Could not set audio bridge as default: {}", e))
                                });
                            }
                        }

                        slog(&session_log, |l| {
                            l.info(&format!(
                                "Live audio session '{}' starting ({:?})",
                                session_id, spec.provider
                            ))
                        });

                        let result =
                            live_audio::run_session(&spec, &api_key, &bridge, log_dir, Some(bus))
                                .await;

                        drop(bridge);

                        match result {
                            Ok(la_result) => {
                                let result_json = serde_json::to_string_pretty(&la_result)
                                    .unwrap_or_else(|_| format!("{:?}", la_result));
                                conversation.add_tool_result(
                                    call_id,
                                    "spawn_live_audio",
                                    &result_json,
                                );
                            }
                            Err(e) => {
                                conversation.add_tool_result(
                                    call_id,
                                    "spawn_live_audio",
                                    &format!("Error: {}", e),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        conversation.add_tool_result(
                            call_id,
                            "spawn_live_audio",
                            &format!("Error parsing LiveAudioSpec: {}", e),
                        );
                    }
                }
            }

            if batch.agent_input_json.is_none() && !batch.precomputed_results.is_empty() {
                continue;
            }

            // If no runtime commands, just respond to tool calls with context update
            let Some(ref json_str) = batch.agent_input_json else {
                empty_command_streak = 0;
                // Respond to manage_context, MCP, or empty batch
                for (call_id, tool_name) in &batch.call_id_names {
                    if !mcp_client::McpClientManager::is_mcp_tool(tool_name) {
                        conversation.add_tool_result(call_id, tool_name, "OK — context updated.");
                    }
                }
                continue;
            };
            empty_command_streak = 0;

            // Inject project context and normalize
            let json_str = normalize_command_batch(&inject_project_context(json_str, project));

            // Headless askHuman check — skip unless JSON mode (which handles it via stdin)
            if headless && json_approval.is_none() && has_ask_human_command(&json_str) {
                slog(&session_log, |l| {
                    l.warn("askHuman requested in headless mode; prompting model to continue")
                });
                for (call_id, tool_name) in &batch.call_id_names {
                    conversation.add_tool_result(
                        call_id,
                        tool_name,
                        "askHuman is unavailable in headless mode. Proceed with assumptions.",
                    );
                }
                continue;
            }
            // In JSON mode, emit the question so the outbound broadcaster prints it
            if json_approval.is_some() {
                if let Some(question) = extract_ask_human_question(&json_str) {
                    bus.send(AppEvent::HumanQuestionDetected { question });
                }
            }

            // Autonomy / approval check (same as text path)
            let needs_approval = {
                let classifications = autonomy::classify_batch(&json_str);
                let autonomy_state = autonomy.read().await;
                let mut need: Option<(autonomy::ActionCategory, bool)> = None;
                for (_idx, categories) in &classifications {
                    for &cat in categories {
                        if cat == autonomy::ActionCategory::HumanInput {
                            continue;
                        }
                        let rule = autonomy_state.rules.rule_for(cat);
                        if matches!(rule, autonomy::ApprovalRule::Deny) {
                            if need.is_none_or(|(prev, _)| cat.severity() > prev.severity()) {
                                need = Some((cat, true));
                            }
                        } else if autonomy_state.needs_approval(cat) {
                            if need.is_none_or(|(prev, was_deny)| {
                                !was_deny && cat.severity() > prev.severity()
                            }) {
                                need = Some((cat, false));
                            }
                        }
                    }
                }
                need
            };

            let mut should_skip = false;
            if let Some((cat, denied_by_policy)) = needs_approval {
                let preview = format_command_preview(&json_str);

                // Dedup: skip approval for retries of already-approved commands
                if !denied_by_policy && autonomy.read().await.was_command_approved(&preview) {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "dedup-auto-approved")
                    });
                } else {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "waiting")
                    });

                    if denied_by_policy {
                        slog(&session_log, |l| {
                            l.approval(&cat.to_string(), &preview, "denied-policy")
                        });
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: format!("Denied by policy ({})", cat),
                            summary: None,
                        });
                        return Ok((loop_stats, LoopExitReason::Denied));
                    }

                    if let Some(slot) = json_approval {
                        // JSON mode: emit approval event and wait for stdin response
                        bus.send(AppEvent::ApprovalRequired {
                            session_id: local_session_id.clone(),
                            id: turn as u64,
                            command_preview: preview.clone(),
                            category: cat,
                        });
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            let mut guard = slot.lock().unwrap();
                            *guard = Some((turn as u64, tx));
                        }
                        match rx.await {
                            Ok(event::ApprovalResponse::Approve) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approved")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "approve".to_string(),
                                });
                                // Record approved command for dedup
                                autonomy.write().await.record_approved_command(&preview);
                                // Session-grant: first DisplayControl approval unlocks the session
                                if cat == autonomy::ActionCategory::DisplayControl {
                                    let mut state = autonomy.write().await;
                                    if !state.user_display_granted {
                                        state.user_display_granted = true;
                                        std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
                                        bus.send(AppEvent::UserDisplayGranted { display_id: 0 });
                                    }
                                }
                            }
                            Ok(event::ApprovalResponse::ApproveAll) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approve-all")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "approve_all".to_string(),
                                });
                                let mut state = autonomy.write().await;
                                state.level = AutonomyLevel::Full;
                                // Session-grant: DisplayControl approval also unlocks user display
                                if cat == autonomy::ActionCategory::DisplayControl
                                    && !state.user_display_granted
                                {
                                    state.user_display_granted = true;
                                    std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
                                    bus.send(AppEvent::UserDisplayGranted { display_id: 0 });
                                }
                            }
                            Ok(event::ApprovalResponse::Skip) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "skipped")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "skip".to_string(),
                                });
                                should_skip = true;
                            }
                            Ok(event::ApprovalResponse::Deny) | Err(_) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "denied")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "deny".to_string(),
                                });
                                bus.send(AppEvent::TaskComplete {
                                    session_id: local_session_id.clone(),
                                    reason: "Denied by user".to_string(),
                                    summary: None,
                                });
                                return Ok((loop_stats, LoopExitReason::Denied));
                            }
                        }
                    } else if headless {
                        slog(&session_log, |l| {
                            l.approval(&cat.to_string(), &preview, "denied-no-approver")
                        });
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: format!("Approval required in headless mode ({})", cat),
                            summary: None,
                        });
                        return Ok((loop_stats, LoopExitReason::Denied));
                    } else {
                        // Interactive mode (TUI/MCP): approval via registry
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        approval_registry.lock().unwrap().insert(turn as u64, tx);
                        bus.send(AppEvent::ApprovalRequired {
                            session_id: local_session_id.clone(),
                            id: turn as u64,
                            command_preview: preview.clone(),
                            category: cat,
                        });
                        match rx.await {
                            Ok(event::ApprovalResponse::Approve) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approved")
                                });
                            }
                            Ok(event::ApprovalResponse::ApproveAll) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approve-all")
                                });
                                let mut state = autonomy.write().await;
                                state.level = AutonomyLevel::Full;
                            }
                            Ok(event::ApprovalResponse::Skip) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "skipped")
                                });
                                should_skip = true;
                            }
                            Ok(event::ApprovalResponse::Deny) | Err(_) => {
                                // Distinguish a real user deny from an interrupt
                                // that caused the watcher to drain the registry
                                // with Deny as a synthetic response. Interrupt
                                // takes precedence so the phase/exit reason
                                // reflects what actually happened.
                                if cancel_token.is_cancelled() {
                                    bus.send(AppEvent::Interrupted {
                                        session_id: local_session_id.clone(),
                                        reason: "user requested".into(),
                                    });
                                    slog(&session_log, |l| {
                                        l.info("Agent loop interrupted during approval wait")
                                    });
                                    return Ok((loop_stats, LoopExitReason::Interrupted));
                                }
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "denied")
                                });
                                bus.send(AppEvent::TaskComplete {
                                    session_id: local_session_id.clone(),
                                    reason: "Denied by user".to_string(),
                                    summary: None,
                                });
                                return Ok((loop_stats, LoopExitReason::Denied));
                            }
                        }
                    }
                } // close dedup else block
            } else {
                // Commands auto-approved — log for visibility at Normal verbosity
                let preview = format_command_preview(&json_str);
                if !preview.is_empty() {
                    bus.send(AppEvent::AutoApproved {
                        preview: preview.clone(),
                    });
                }
            }

            if should_skip {
                for (call_id, tool_name) in &batch.call_id_names {
                    conversation.add_tool_result(call_id, tool_name, "Command skipped by user.");
                }
                continue;
            }

            // Run agent
            slog(&session_log, |l| l.agent_input(&json_str));
            maybe_auto_launch_xvfb(
                &json_str,
                &mut xvfb_guard,
                provider.name(),
                &session_log,
                bus,
            )
            .await;
            let preview = format_commands_preview(&json_str);
            bus.send(AppEvent::AgentStarted {
                session_id: local_session_id.clone(),
                turn,
                commands_preview: preview.clone(),
                source: None,
            });

            let output = agent_runner::run_agent(&json_str, log_dir).await?;
            let output_id = event::next_agent_output_id();

            // Log agent output
            slog(&session_log, |l| {
                l.agent_output_with_id(&output.stdout, &output.stderr, None, Some(&output_id))
            });

            bus.send(AppEvent::AgentOutput {
                session_id: local_session_id.clone(),
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                source: None,
                output_id: Some(output_id),
            });

            // Map results back to individual tool responses
            let tool_results = map_results_to_tool_responses(
                &output.stdout,
                &output.stderr,
                &batch.nonce_to_call_id,
                &batch.call_id_names,
            );
            let budget = conversation.budget_summary();
            for (call_id, tool_name, result_text) in &tool_results {
                let text = format!("{}\n\n{}", result_text, budget);
                if tool_name == "capture_screen" {
                    if let Some(images) = encode_screenshot(result_text) {
                        conversation.add_tool_result_with_images(call_id, tool_name, &text, images);
                        continue;
                    }
                }
                conversation.add_tool_result(call_id, tool_name, &text);
            }

            // Process CU calls alongside function tool calls
            if has_cu_calls {
                execute_cu_calls(
                    &response.cu_calls,
                    conversation,
                    provider.cu_display(),
                    log_dir,
                    &mut cu_action_counter,
                    &session_log,
                )
                .await;
            }
        } else if has_cu_calls {
            // CU-only turn (no function tool calls)
            execute_cu_calls(
                &response.cu_calls,
                conversation,
                provider.cu_display(),
                log_dir,
                &mut cu_action_counter,
                &session_log,
            )
            .await;
        } else {
            // --- Legacy text extraction path ---

            // Extract JSON from response
            let json_str = match extract_json(&response.content) {
                Some(json) => json.to_string(),
                None => {
                    slog(&session_log, |l| {
                        l.info("No JSON found in response — task complete")
                    });
                    let brief: String = response.content.chars().take(500).collect();
                    bus.send(AppEvent::TaskComplete {
                        session_id: local_session_id.clone(),
                        reason: "Task complete".to_string(),
                        summary: if brief.is_empty() {
                            None
                        } else {
                            Some(brief.clone())
                        },
                    });
                    exit_reason = LoopExitReason::TaskComplete;
                    break;
                }
            };

            slog(&session_log, |l| l.json_extracted(&json_str));

            bus.send(AppEvent::JsonExtracted {
                preview: json_str.chars().take(100).collect(),
            });

            // Check for explicit done signal (used in structured output / JSON mode)
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json_str) {
                if parsed
                    .get("done")
                    .and_then(|d| d.as_bool())
                    .unwrap_or(false)
                {
                    let message = parsed
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(String::from);
                    slog(&session_log, |l| {
                        l.info(&format!(
                            "Done signal received: {}",
                            message.as_deref().unwrap_or("(no message)")
                        ))
                    });
                    bus.send(AppEvent::DoneSignal {
                        message: message.clone(),
                    });
                    exit_reason = LoopExitReason::DoneSignal;
                    break;
                }
            }

            // Apply context directives (drop_turns, summarize) before sending to agent
            let (json_str, had_context) = apply_context_directives(&json_str, conversation);

            if had_context {
                slog(&session_log, |l| l.debug("Context directives applied"));
            }

            // No commands to execute
            if json_str.is_empty() {
                if had_context {
                    empty_command_streak = 0;
                    slog(&session_log, |l| {
                        l.debug(&format!("Turn {}: context management only", turn))
                    });
                    bus.send(AppEvent::ContextManagement { turn });
                    conversation.add_user("Context updated.".to_string());
                    continue;
                } else {
                    empty_command_streak += 1;
                    if empty_command_streak >= 2 {
                        slog(&session_log, |l| {
                            l.info("No commands across consecutive turns — task complete")
                        });
                        let brief: String = response.content.chars().take(500).collect();
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: "Task complete".to_string(),
                            summary: if brief.is_empty() {
                                None
                            } else {
                                Some(brief.clone())
                            },
                        });
                        exit_reason = LoopExitReason::TaskComplete;
                        break;
                    }
                    slog(&session_log, |l| {
                        l.warn(
                            "No commands and no context directives — requesting explicit done signal",
                        )
                    });
                    conversation.add_user(
                        "No commands were produced. If the task is complete, respond with JSON containing done=true. Otherwise provide commands.".to_string(),
                    );
                    continue;
                }
            }
            empty_command_streak = 0;

            // Inject project context (memory_file) into commands and normalize aliases.
            let json_str = normalize_command_batch(&inject_project_context(&json_str, project));

            // In headless mode there is no askHuman input panel — skip unless JSON mode.
            if headless && json_approval.is_none() && has_ask_human_command(&json_str) {
                slog(&session_log, |l| {
                    l.warn("askHuman requested in headless mode; prompting model to continue")
                });
                conversation.add_user(
                    "askHuman is unavailable in headless mode (--no-tui or non-interactive stdin). \
Proceed with explicit assumptions and continue without additional questions."
                        .to_string(),
                );
                continue;
            }
            // In JSON mode, emit the question so the outbound broadcaster prints it
            if json_approval.is_some() {
                if let Some(question) = extract_ask_human_question(&json_str) {
                    bus.send(AppEvent::HumanQuestionDetected { question });
                }
            }

            // Check autonomy / approval for commands
            let needs_approval = {
                let classifications = autonomy::classify_batch(&json_str);
                let autonomy_state = autonomy.read().await;
                let mut need: Option<(autonomy::ActionCategory, bool)> = None;
                for (_idx, categories) in &classifications {
                    for &cat in categories {
                        if cat == autonomy::ActionCategory::HumanInput {
                            continue;
                        }
                        let rule = autonomy_state.rules.rule_for(cat);
                        if matches!(rule, autonomy::ApprovalRule::Deny) {
                            if need.is_none_or(|(prev, _)| cat.severity() > prev.severity()) {
                                need = Some((cat, true));
                            }
                        } else if autonomy_state.needs_approval(cat) {
                            if need.is_none_or(|(prev, was_deny)| {
                                !was_deny && cat.severity() > prev.severity()
                            }) {
                                need = Some((cat, false));
                            }
                        }
                    }
                }
                need
            };

            let mut should_skip = false;
            if let Some((cat, denied_by_policy)) = needs_approval {
                let preview = format_command_preview(&json_str);

                // Dedup: skip approval for retries of already-approved commands
                if !denied_by_policy && autonomy.read().await.was_command_approved(&preview) {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "dedup-auto-approved")
                    });
                } else {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "waiting")
                    });

                    if denied_by_policy {
                        slog(&session_log, |l| {
                            l.approval(&cat.to_string(), &preview, "denied-policy")
                        });
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: format!("Denied by policy ({})", cat),
                            summary: None,
                        });
                        return Ok((loop_stats, LoopExitReason::Denied));
                    }

                    if let Some(slot) = json_approval {
                        // JSON mode: emit approval event and wait for stdin response
                        bus.send(AppEvent::ApprovalRequired {
                            session_id: local_session_id.clone(),
                            id: turn as u64,
                            command_preview: preview.clone(),
                            category: cat,
                        });
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            let mut guard = slot.lock().unwrap();
                            *guard = Some((turn as u64, tx));
                        }
                        match rx.await {
                            Ok(event::ApprovalResponse::Approve) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approved")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "approve".to_string(),
                                });
                                // Record approved command for dedup
                                autonomy.write().await.record_approved_command(&preview);
                                // Session-grant: first DisplayControl approval unlocks the session
                                if cat == autonomy::ActionCategory::DisplayControl {
                                    let mut state = autonomy.write().await;
                                    if !state.user_display_granted {
                                        state.user_display_granted = true;
                                        std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
                                        bus.send(AppEvent::UserDisplayGranted { display_id: 0 });
                                    }
                                }
                            }
                            Ok(event::ApprovalResponse::ApproveAll) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approve-all")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "approve_all".to_string(),
                                });
                                let mut state = autonomy.write().await;
                                state.level = AutonomyLevel::Full;
                                // Session-grant: DisplayControl approval also unlocks user display
                                if cat == autonomy::ActionCategory::DisplayControl
                                    && !state.user_display_granted
                                {
                                    state.user_display_granted = true;
                                    std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
                                    bus.send(AppEvent::UserDisplayGranted { display_id: 0 });
                                }
                            }
                            Ok(event::ApprovalResponse::Skip) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "skipped")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "skip".to_string(),
                                });
                                should_skip = true;
                            }
                            Ok(event::ApprovalResponse::Deny) | Err(_) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "denied")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "deny".to_string(),
                                });
                                bus.send(AppEvent::TaskComplete {
                                    session_id: local_session_id.clone(),
                                    reason: "Denied by user".to_string(),
                                    summary: None,
                                });
                                return Ok((loop_stats, LoopExitReason::Denied));
                            }
                        }
                    } else if headless {
                        slog(&session_log, |l| {
                            l.approval(&cat.to_string(), &preview, "denied-no-approver")
                        });
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: format!("Approval required in headless mode ({})", cat),
                            summary: None,
                        });
                        return Ok((loop_stats, LoopExitReason::Denied));
                    } else {
                        // Interactive mode (TUI/MCP): approval via registry
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        approval_registry.lock().unwrap().insert(turn as u64, tx);
                        bus.send(AppEvent::ApprovalRequired {
                            session_id: local_session_id.clone(),
                            id: turn as u64,
                            command_preview: preview.clone(),
                            category: cat,
                        });
                        match rx.await {
                            Ok(event::ApprovalResponse::Approve) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approved")
                                });
                            }
                            Ok(event::ApprovalResponse::ApproveAll) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approve-all")
                                });
                                let mut state = autonomy.write().await;
                                state.level = AutonomyLevel::Full;
                            }
                            Ok(event::ApprovalResponse::Skip) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "skipped")
                                });
                                should_skip = true;
                            }
                            Ok(event::ApprovalResponse::Deny) | Err(_) => {
                                // Distinguish a real user deny from an interrupt
                                // that caused the watcher to drain the registry
                                // with Deny as a synthetic response. Interrupt
                                // takes precedence so the phase/exit reason
                                // reflects what actually happened.
                                if cancel_token.is_cancelled() {
                                    bus.send(AppEvent::Interrupted {
                                        session_id: local_session_id.clone(),
                                        reason: "user requested".into(),
                                    });
                                    slog(&session_log, |l| {
                                        l.info("Agent loop interrupted during approval wait")
                                    });
                                    return Ok((loop_stats, LoopExitReason::Interrupted));
                                }
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "denied")
                                });
                                bus.send(AppEvent::TaskComplete {
                                    session_id: local_session_id.clone(),
                                    reason: "Denied by user".to_string(),
                                    summary: None,
                                });
                                return Ok((loop_stats, LoopExitReason::Denied));
                            }
                        }
                    }
                } // close dedup else block
            } else {
                // Commands auto-approved — log for visibility at Normal verbosity
                let preview = format_command_preview(&json_str);
                if !preview.is_empty() {
                    bus.send(AppEvent::AutoApproved {
                        preview: preview.clone(),
                    });
                }
            }

            if should_skip {
                conversation.add_user("Command skipped by user.".to_string());
                continue;
            }

            // Log the full JSON being sent to the agent
            slog(&session_log, |l| l.agent_input(&json_str));
            maybe_auto_launch_xvfb(
                &json_str,
                &mut xvfb_guard,
                provider.name(),
                &session_log,
                bus,
            )
            .await;

            let preview = format_commands_preview(&json_str);
            bus.send(AppEvent::AgentStarted {
                session_id: local_session_id.clone(),
                turn,
                commands_preview: preview.clone(),
                source: None,
            });

            let output = agent_runner::run_agent(&json_str, log_dir).await?;
            let output_id = event::next_agent_output_id();

            // Log agent output
            slog(&session_log, |l| {
                l.agent_output_with_id(&output.stdout, &output.stderr, None, Some(&output_id))
            });

            bus.send(AppEvent::AgentOutput {
                session_id: local_session_id.clone(),
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                source: None,
                output_id: Some(output_id),
            });

            // Check for completed sub-agent results
            let sub_agent_dir = project.sub_agent_dir();
            if sub_agent_dir.exists() {
                let results = sub_agent::scan_completed_results(&sub_agent_dir);
                for result in &results {
                    let key = format!("{}::{}", result.id, result.summary);
                    if !seen_sub_agent_results.insert(key) {
                        continue;
                    }
                    let msg = sub_agent::format_result_message(result);
                    slog(&session_log, |l| {
                        l.info(&format!("Sub-agent result: {}", msg))
                    });
                    bus.send(AppEvent::SubAgentResult {
                        formatted: msg.clone(),
                    });
                }
            }

            // Format agent output as next user message, include budget summary
            let mut user_msg = format!("Agent output:\n{}", output.stdout);
            if !output.stderr.is_empty() {
                user_msg.push_str(&format!("\nStderr:\n{}", output.stderr));
            }
            user_msg.push_str(&format!("\n\n{}", conversation.budget_summary()));
            conversation.add_user(user_msg);
        } // end tool_calls vs text branch

        // Auto-save conversation for resume capability
        let conv_path = log_dir.join("conversation.jsonl");
        if let Err(e) = conversation.save_to_file(&conv_path) {
            slog(&session_log, |l| {
                l.debug(&format!("Failed to save conversation: {}", e))
            });
        }

        if turn == SAFETY_CAP {
            slog(&session_log, |l| {
                l.warn(&format!("Safety cap ({}) reached", SAFETY_CAP))
            });
            bus.send(AppEvent::SafetyCapReached);
            exit_reason = LoopExitReason::SafetyCapReached;
        }
    }

    slog(&session_log, |l| l.info("Agent loop finished"));
    Ok((loop_stats, exit_reason))
}

/// Wraps `run_agent_loop` in a multi-round loop that waits for follow-up messages
/// between rounds. The session continues until the user closes the channel,
/// budget is exhausted, safety cap is reached, or a non-recoverable exit occurs.
async fn run_round_loop(
    provider: &dyn provider::ChatProvider,
    conversation: &mut Conversation,
    project: &Project,
    sub_agent_mode: Option<&(String, sub_agent::SubAgentRole)>,
    bus: &EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: &std::path::Path,
    mcp_mgr: Option<&mcp_client::McpClientManager>,
    follow_up_rx: &mut FollowUpReceiver,
    json_approval: Option<&JsonApprovalSlot>,
    approval_registry: &event::ApprovalRegistry,
    context_injection: &event::ContextInjectionQueue,
    headless: bool,
) -> Result<LoopStats, CallerError> {
    let mut round = 1usize;
    let mut cumulative_stats = LoopStats::default();
    let mut xvfb_guard: Option<vision::XvfbGuard> = None;
    let local_session_id = session_log_id(&session_log);

    loop {
        let (stats, exit_reason) = run_agent_loop(
            provider,
            conversation,
            project,
            sub_agent_mode,
            bus,
            autonomy.clone(),
            session_log.clone(),
            log_dir,
            mcp_mgr,
            json_approval,
            approval_registry,
            context_injection,
            &mut xvfb_guard,
            headless,
        )
        .await?;

        cumulative_stats.turns += stats.turns;
        cumulative_stats.usage.prompt_tokens += stats.usage.prompt_tokens;
        cumulative_stats.usage.completion_tokens += stats.usage.completion_tokens;
        cumulative_stats.usage.total_tokens += stats.usage.total_tokens;
        cumulative_stats.rounds = round;

        // Sub-agent mode: never wait for follow-up
        if sub_agent_mode.is_some() {
            break;
        }

        // Only wait for follow-up on recoverable exits
        match exit_reason {
            LoopExitReason::DoneSignal | LoopExitReason::TaskComplete => {
                // Emit RoundComplete event. Snapshot the native conversation
                // message count so a conversation-rollback request can
                // truncate the tail back to this point.
                let turns_in_round = stats.turns;
                let native_message_count = Some(conversation.messages().len() as u32);
                bus.send(AppEvent::RoundComplete {
                    session_id: local_session_id.clone(),
                    round,
                    turns_in_round,
                    native_message_count,
                });

                // Wait for follow-up message
                match follow_up_rx.recv().await {
                    Some(message) => {
                        round += 1;
                        let followup_text =
                            message.attachments.text_with_file_prelude(&message.text);
                        let followup_images = message.attachments.conversation_images();
                        slog(&session_log, |l| {
                            l.info(&format!(
                                "Round {} follow-up: {}{}",
                                round,
                                &message.text,
                                if message.attachments.is_empty() {
                                    String::new()
                                } else {
                                    format!(" ({} attachment(s))", message.attachments.len())
                                }
                            ))
                        });
                        if followup_images.is_empty() {
                            conversation.add_user(followup_text);
                        } else {
                            conversation.add_user_with_images(followup_text, followup_images);
                        }
                        if let Some(id) = message.steer_id {
                            bus.send(AppEvent::SteerDelivered {
                                session_id: local_session_id.clone(),
                                id,
                                mid_turn: false,
                            });
                        }
                        emit_follow_up_status(
                            bus,
                            local_session_id.as_deref(),
                            &message.follow_up_id,
                            Some(&message.text),
                            "delivered",
                            None,
                        );
                    }
                    None => {
                        // Channel closed — user quit or sender dropped
                        break;
                    }
                }
            }
            LoopExitReason::BudgetExhausted
            | LoopExitReason::SafetyCapReached
            | LoopExitReason::Denied
            | LoopExitReason::Error
            | LoopExitReason::Interrupted => {
                break;
            }
        }
    }

    Ok(cumulative_stats)
}

fn get_task_from_flags_or_env(flags: &CliFlags) -> Result<String, CallerError> {
    if let Some(ref task) = flags.task {
        return Ok(task.clone());
    }
    if let Ok(task) = env::var("INTENDANT_TASK") {
        return Ok(task);
    }
    print!("Enter task: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// Legacy get_task for sub-agent mode (doesn't use CliFlags).
fn get_task() -> Result<String, CallerError> {
    if env::args().len() > 1 {
        Ok(env::args().skip(1).collect::<Vec<_>>().join(" "))
    } else if let Ok(task) = env::var("INTENDANT_TASK") {
        Ok(task)
    } else {
        print!("Enter task: ");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        Ok(line.trim().to_string())
    }
}

async fn run_sub_agent_mode(
    provider: Box<dyn provider::ChatProvider>,
    id: String,
    role: sub_agent::SubAgentRole,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
) -> Result<LoopStats, CallerError> {
    let project = Project::detect()?;
    let system_prompt = if provider.use_tools() {
        prompts::resolve_system_prompt_for_tools(&role, Some(&project.root))?
    } else {
        prompts::resolve_system_prompt(&role, Some(&project.root))?
    };
    let task = get_task()?;

    if task.is_empty() {
        return Err(CallerError::Config("No task provided".to_string()));
    }

    slog(&session_log, |l| {
        l.write_meta_with_role(Some(&project.root), None, Some(role.as_str()));
        l.info(&format!("Sub-agent mode: {} (role: {})", id, role.as_str()));
        l.info(&format!(
            "Provider: {} (context window: {})",
            provider.name(),
            provider.context_window()
        ));
    });
    println!("Running as sub-agent: {} (role: {})", id, role.as_str());
    println!(
        "Provider: {} (context window: {})",
        provider.name(),
        provider.context_window()
    );

    let mut conversation = Conversation::new(system_prompt, provider.context_window());

    // Inject project root so the model knows which directory to work in
    conversation.add_user(format!(
        "Working directory: {}\nThis is the project you should examine and modify. \
All relative paths and commands execute from this directory.",
        project.root.display()
    ));
    conversation.add_assistant(
        "Understood. I will work within the specified project directory.".to_string(),
    );

    // Inject INTENDANT.md instructions
    if let Some(instructions) = prompts::load_project_instructions(Some(&project.root)) {
        conversation.add_user(instructions);
        conversation
            .add_assistant("Acknowledged. I will follow the project instructions.".to_string());
    }

    // Inject knowledge if inherited
    if env::var("INTENDANT_INHERIT_MEMORY").is_ok() && project.config.memory.enabled {
        if let Ok(kstore) = knowledge::load(&project.memory_path()) {
            let refs: Vec<&_> = kstore.entries.iter().collect();
            let msg = knowledge::format_for_injection(&refs);
            if !msg.is_empty() {
                conversation.add_user(msg);
                conversation.add_assistant(
                    "Acknowledged. I have loaded the project knowledge.".to_string(),
                );
            }
        }
    }

    conversation.add_user(task.clone());
    slog(&session_log, |l| l.info(&format!("Task: {}", task)));
    println!("Task: {}", task);
    println!("---");

    let autonomy = autonomy::shared_autonomy(AutonomyState::new(
        AutonomyLevel::Full, // sub-agents run fully autonomous
        autonomy::ApprovalConfig::default(),
    ));

    let sub_agent_info = (id.clone(), role);
    let session_log_for_summary = session_log.clone();
    let sub_agent_bus = EventBus::new();
    let sub_agent_registry = event::ApprovalRegistry::default();
    let result = run_agent_loop(
        provider.as_ref(),
        &mut conversation,
        &project,
        Some(&sub_agent_info),
        &sub_agent_bus,
        autonomy,
        session_log,
        &log_dir,
        None, // no MCP client for sub-agents
        None, // no JSON approval for sub-agents
        &sub_agent_registry,
        &event::ContextInjectionQueue::default(),
        &mut None, // sub-agents get their own display if needed
        true,      // headless (sub-agents have no interactive UI)
    )
    .await;

    // Map (LoopStats, LoopExitReason) → LoopStats for sub-agent callers
    let result = result.map(|(stats, _reason)| stats);

    // Update session status before writing result file
    match &result {
        Ok(stats) => slog(&session_log_for_summary, |l| {
            l.write_summary_with_rounds(&task, "completed", stats.turns, Some(stats.rounds))
        }),
        Err(e) => slog(&session_log_for_summary, |l| {
            l.write_summary(&task, &format!("error: {}", e), 0)
        }),
    }

    // Write result file
    if let Ok(result_path) = env::var("INTENDANT_RESULT_FILE") {
        let (status, summary, brief, usage) = match &result {
            Ok(stats) => {
                let full = stats
                    .last_response
                    .clone()
                    .unwrap_or_else(|| "Task completed successfully".to_string());
                let (brief, was_explicit) = parse_brief(&full);
                if was_explicit {
                    slog(&session_log_for_summary, |l| {
                        l.debug(&format!("Task brief (model): {}", brief))
                    });
                } else {
                    slog(&session_log_for_summary, |l| {
                        l.debug(&format!(
                            "Task brief (fallback — model omitted BRIEF: line): {}",
                            brief
                        ))
                    });
                }
                (
                    sub_agent::SubAgentStatus::Completed,
                    full,
                    brief,
                    stats.usage.clone(),
                )
            }
            Err(e) => (
                sub_agent::SubAgentStatus::Failed(e.to_string()),
                format!("Task failed: {}", e),
                format!("Task failed: {}", e),
                provider::TokenUsage::default(),
            ),
        };

        let agent_result = sub_agent::SubAgentResult {
            id,
            status,
            summary,
            brief,
            findings: vec![],
            artifacts: vec![],
            usage,
        };
        let _ = sub_agent::write_result(std::path::Path::new(&result_path), &agent_result);
    }

    result
}

/// RAII guard that increments the presence-pause ref-count on construction
/// and decrements it on drop. Lets a direct-mode task pause server-side
/// narration for its own duration without clobbering pause contributions
/// from other sources (e.g. browser voice's PresenceConnected ref-count).
struct PresencePauseGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl PresencePauseGuard {
    fn new(counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self { counter }
    }
}

impl Drop for PresencePauseGuard {
    fn drop(&mut self) {
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Run with the presence layer mediating between user and agent loop.
///
/// The presence layer runs in its own background task, handling user input
/// and narrating agent events via `PresenceLayer::run()`. This function
/// dispatches task envelopes produced by presence to the actual agent loop.
#[allow(clippy::too_many_arguments)]
async fn run_with_presence(
    task: Option<String>,
    project: Project,
    bus: EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
    user_rx: tokio::sync::mpsc::Receiver<String>,
    response_tx: tokio::sync::mpsc::Sender<String>,
    presence_event_rx: tokio::sync::mpsc::Receiver<presence::PresenceEvent>,
    agent_state: Arc<Mutex<presence::AgentStateSnapshot>>,
    _force_direct: bool,
    presence_paused: Arc<std::sync::atomic::AtomicUsize>,
    task_tx: tokio::sync::mpsc::Sender<presence::TaskEnvelope>,
    mut task_rx: tokio::sync::mpsc::Receiver<presence::TaskEnvelope>,
    approval_registry: event::ApprovalRegistry,
    frame_registry: Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    context_injection: event::ContextInjectionQueue,
    session_registry: display::SharedSessionRegistry,
    agent_backend_override: Option<external_agent::AgentBackend>,
    shared_external_agent: Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    shared_codex_config: control_plane::SharedCodexConfig,
    shared_gemini_config: control_plane::SharedGeminiConfig,
    web_port: Option<u16>,
) -> Result<LoopStats, CallerError> {
    // 1. Try to create presence provider. Degrade to silent mode on failure so
    //    an external-agent-only run (e.g. codex with no API keys configured)
    //    still starts. The main task loop below doesn't depend on the presence
    //    LLM — it only needs `task_rx` alive.
    let presence_provider_opt = match provider::select_presence_provider(
        project.config.presence.provider.as_deref(),
        project.config.presence.model.as_deref(),
    ) {
        Ok(p) => Some(p),
        Err(e) => {
            bus.send(AppEvent::PresenceLog {
                message: format!(
                    "Presence LLM unavailable ({}). Running without narration — \
                     dashboard chat and tasks will dispatch directly to the worker.",
                    e
                ),
                level: Some(types::LogLevel::Warn),
                turn: None,
            });
            None
        }
    };

    let fallback_task_tx = task_tx.clone();

    if let Some(presence_provider) = presence_provider_opt {
        bus.send(AppEvent::PresenceUsageUpdate {
            total_tokens: 0,
            context_window: project.config.presence.context_window,
            usage_pct: 0.0,
            provider: presence_provider.name().to_string(),
            model: presence_provider.model().to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
        });

        let presence_prompt = prompts::resolve_presence_prompt(Some(&project.root));
        let context_window = project.config.presence.context_window;
        let mut presence = presence::PresenceLayer::new(
            presence_provider,
            presence_prompt,
            context_window,
            bus.clone(),
            task_tx,
            presence_event_rx,
            agent_state.clone(),
            project.memory_path(),
            log_dir.clone(),
            project.root.clone(),
            presence_paused.clone(),
            context_injection.clone(),
        );

        // Send initial task to presence (if provided), with a timeout so a
        // slow or misconfigured presence provider doesn't freeze the TUI.
        let mut presence_failed_task: Option<String> = None;
        if let Some(ref task_str) = task {
            let input = format!("The user wants: {}", task_str);
            match tokio::time::timeout(
                tokio::time::Duration::from_secs(30),
                presence.process_user_input(&input),
            )
            .await
            {
                Ok(Ok(response)) if !response.is_empty() => {
                    let _ = response_tx.send(response).await;
                }
                Ok(Err(e)) => {
                    bus.send(AppEvent::PresenceLog {
                        message: format!(
                            "Presence provider error: {}. Use --no-presence or --direct to bypass. \
                             Submitting task directly.",
                            e
                        ),
                        level: Some(types::LogLevel::Warn),
                        turn: None,
                    });
                    presence_failed_task = Some(task_str.clone());
                }
                Err(_) => {
                    bus.send(AppEvent::PresenceLog {
                        message: "Presence provider timed out (30s). Use --no-presence or --direct to bypass. \
                             Submitting task directly."
                            .to_string(),
                        level: Some(types::LogLevel::Warn),
                        turn: None,
                    });
                    presence_failed_task = Some(task_str.clone());
                }
                _ => {}
            }
        }

        if let Some(failed_task) = presence_failed_task {
            let envelope = presence::TaskEnvelope {
                task: failed_task,
                force_direct: true,
                context_hints: vec![],
                reference_frame_ids: vec![],
                display_target: None,
                attachment_frame_ids: vec![],
                steer_id: None,
            };
            let _ = fallback_task_tx.send(envelope).await;
        }
        drop(fallback_task_tx);

        // Spawn presence.run() for user input + event narration.
        let _presence_handle = tokio::spawn(async move {
            presence.run(user_rx, response_tx).await;
        });
    } else {
        // Silent mode: no presence LLM. Inject the initial task directly and
        // forward subsequent user text from the dashboard chat into task_tx
        // as force_direct envelopes. presence_event_rx and response_tx are
        // dropped at scope exit — no consumer for them without a PresenceLayer.
        let _ = presence_event_rx;
        let _ = response_tx;
        let _ = agent_state;
        let _ = context_injection;

        if let Some(task_str) = task.as_ref() {
            let envelope = presence::TaskEnvelope {
                task: task_str.clone(),
                force_direct: true,
                context_hints: vec![],
                reference_frame_ids: vec![],
                display_target: None,
                attachment_frame_ids: vec![],
                steer_id: None,
            };
            let _ = fallback_task_tx.send(envelope).await;
        }
        // Keep task_tx alive for the forwarder below; drop the extra clone.
        drop(fallback_task_tx);

        let forwarder_tx = task_tx;
        let mut user_rx = user_rx;
        tokio::spawn(async move {
            while let Some(text) = user_rx.recv().await {
                let envelope = presence::TaskEnvelope {
                    task: text,
                    force_direct: true,
                    context_hints: vec![],
                    reference_frame_ids: vec![],
                    display_target: None,
                    attachment_frame_ids: vec![],
                    steer_id: None,
                };
                if forwarder_tx.send(envelope).await.is_err() {
                    break;
                }
            }
        });
    }

    // 8. Persistent server conversation across all presence tasks.
    //    First task initializes the conversation; subsequent tasks inject new
    //    user messages into the same conversation. This preserves the server
    //    model's context across the entire presence session.
    let mut cumulative_stats = LoopStats::default();
    let project_root = project.root.clone();

    // Resolve external agent backend: CLI override > web UI selection > config default > None.
    let initial_agent_backend = resolve_agent_backend_from_config(agent_backend_override, &project);
    // Seed the shared state so the web UI reflects the initial selection.
    {
        let mut guard = shared_external_agent.write().await;
        if guard.is_none() {
            *guard = initial_agent_backend.clone();
        }
    }

    // Conversation, provider, project — created on first task, reused thereafter.
    let mut persistent_conv: Option<Conversation> = None;
    let mut persistent_provider: Option<Box<dyn provider::ChatProvider>> = None;
    let mut persistent_project: Option<Project> = None;
    // External agent + thread — created on first task, reused for subsequent messages.
    let mut persistent_agent: Option<Box<dyn external_agent::ExternalAgent>> = None;
    let mut persistent_thread: Option<external_agent::AgentThread> = None;
    let mut persistent_event_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    > = None;
    let mut persistent_diff_tracker = ExternalDiffDeltaTracker::default();
    let mut persistent_pending_runtime_steers: std::collections::VecDeque<PendingRuntimeSteer> =
        std::collections::VecDeque::new();
    // Track which backend the persistent agent was created for, so we can reset
    // when the web UI changes the selection between tasks.
    let mut persistent_agent_backend: Option<external_agent::AgentBackend> = None;
    // Track the Codex runtime config the persistent agent was born with.
    // Codex locks sandbox / approval policy / model at `thread/start`, so
    // these can't change mid-thread — if any field differs from the current
    // `shared_codex_config` when a new task arrives, we tear the agent down
    // and build a fresh one. Only meaningful when the backend is Codex.
    let mut persistent_codex_config: Option<control_plane::CodexRuntimeConfig> = None;
    // Same idea for Gemini, but even more strict: Gemini latches every knob
    // at process spawn (not at session/new), so a changed --approval-mode
    // or --sandbox flag forces a kill + respawn of the gemini CLI process
    // on the next task. Only meaningful when the backend is GeminiCli.
    let mut persistent_gemini_config: Option<control_plane::GeminiRuntimeConfig> = None;

    // Side channel for thread actions (Codex slash commands) dispatched from
    // the dashboard / MCP between tasks. We subscribe to the bus here (not
    // just inside the drain) so actions still fire when the loop is idle,
    // waiting for the next task.
    let local_session_id = session_log_id(&session_log);
    let mut outer_bus_rx = bus.subscribe();
    // Turn controls (steer / interrupt) need to be subscribed before the
    // turn-start RPC. Otherwise an immediate follow-up can land during the
    // handoff and miss the running-turn drain entirely.
    let mut turn_bus_rx = bus.subscribe();

    // Outer loop: either a task envelope arrives (run the agent), a thread
    // action arrives (invoke it on the persistent agent), or the task
    // channel closes (exit cleanly).
    enum OuterSignal {
        Task(presence::TaskEnvelope),
        ThreadAction {
            session_id: Option<String>,
            op: String,
            params: serde_json::Value,
        },
        /// Gemini thread action — carried separately from Codex's so we can
        /// pick the right result event (`GeminiThreadActionResult` vs
        /// `CodexThreadActionResult`) downstream.
        GeminiThreadAction {
            op: String,
            params: serde_json::Value,
        },
        /// Conversation-rollback request from the web gateway. Fired
        /// when the user POSTs `/api/session/current/rollback` with
        /// `revert_conversation: true`. The gateway only sends this
        /// when the agent is idle (guarded by `ensure_idle`), so
        /// handling it between tasks is safe.
        ConversationRollback {
            round_id: u64,
            target_native_message_count: Option<u32>,
            turns_to_drop: u32,
        },
        Done,
    }

    loop {
        let signal = tokio::select! {
            biased;
            env = task_rx.recv() => match env {
                Some(e) => OuterSignal::Task(e),
                None => OuterSignal::Done,
            },
            msg = outer_bus_rx.recv() => match msg {
                Ok(AppEvent::CodexThreadActionRequested {
                    session_id,
                    action,
                    params,
                }) if event_targets_session(&session_id, &local_session_id) => {
                    OuterSignal::ThreadAction {
                        session_id,
                        op: action,
                        params,
                    }
                }
                Ok(AppEvent::GeminiThreadActionRequested { action, params }) => {
                    OuterSignal::GeminiThreadAction { op: action, params }
                }
                Ok(AppEvent::ConversationRollbackRequested {
                    round_id,
                    target_native_message_count,
                    turns_to_drop,
                }) => OuterSignal::ConversationRollback {
                    round_id,
                    target_native_message_count,
                    turns_to_drop,
                },
                Ok(AppEvent::InterruptRequested { session_id })
                    if event_targets_session(&session_id, &local_session_id) =>
                {
                    // Drop idle interrupts so an old Stop action cannot
                    // interrupt the next task that happens to start later.
                    turn_bus_rx = bus.subscribe();
                    continue;
                }
                // Any other bus event: skip, keep selecting. Lagged /
                // Closed also fall through — task_rx close is the
                // authoritative "we're done" signal.
                _ => continue,
            },
        };
        let envelope = match signal {
            OuterSignal::Task(e) => e,
            OuterSignal::Done => break,
            OuterSignal::ThreadAction {
                session_id,
                op,
                params,
            } => {
                let mut action_params = params;
                // `/new` is a daemon-side operation (not a Codex RPC): clear
                // the persistent agent so the next task creates a fresh
                // thread. Handled here — not inside dispatch_thread_action
                // — because the Box<dyn ExternalAgent> lives in this loop.
                let result = if op == "new" {
                    persistent_agent = None;
                    persistent_thread = None;
                    persistent_event_rx = None;
                    persistent_codex_config = None;
                    persistent_diff_tracker = ExternalDiffDeltaTracker::default();
                    Ok("agent torn down; next task will start a fresh thread".to_string())
                } else if let Some(ref mut agent) = persistent_agent {
                    let target_thread_id = session_id
                        .as_deref()
                        .filter(|id| Some(*id) != local_session_id.as_deref())
                        .or_else(|| {
                            persistent_thread
                                .as_ref()
                                .map(|thread| thread.thread_id.as_str())
                        });
                    action_params =
                        thread_action_params_with_thread_id(&op, action_params, target_thread_id);
                    agent
                        .thread_action(&op, &action_params)
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    Err("no active agent — start a task first".to_string())
                };
                let (success, message) = match result {
                    Ok(msg) => (true, msg),
                    Err(e) => (false, e),
                };
                let result_session_id = session_id.or_else(|| local_session_id.clone());
                slog(&session_log, |l| {
                    l.info(&format!(
                        "Codex thread action /{}: {} — {}",
                        op,
                        if success { "ok" } else { "FAILED" },
                        message
                    ))
                });
                bus.send(AppEvent::CodexThreadActionResult {
                    session_id: result_session_id.clone(),
                    action: op.clone(),
                    success,
                    message: message.clone(),
                });
                if success && op == "fork" {
                    if let Some(child_id) = forked_thread_id_from_message(&message) {
                        emit_codex_fork_session_name(&bus, &child_id, &action_params);
                        emit_session_relationship(
                            &bus,
                            result_session_id.as_deref(),
                            &child_id,
                            "fork",
                            false,
                        );
                        bus.send(AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
                            source: "codex".to_string(),
                            session_id: child_id.clone(),
                            resume_id: Some(child_id),
                            project_root: Some(project_root.to_string_lossy().to_string()),
                            task: None,
                            direct: Some(true),
                        }));
                    }
                }
                if success && op == "side" {
                    if let Some((parent_thread_id, child_thread_id)) =
                        side_thread_ids_from_message(&message)
                    {
                        let side_prompt = side_session_prompt_from_params(&action_params);
                        if let (Some(agent), Some(event_rx)) =
                            (persistent_agent.as_mut(), persistent_event_rx.as_mut())
                        {
                            let drain_config = DrainConfig {
                                bus: &bus,
                                session_id: session_log_id(&session_log),
                                alias_session_id: None,
                                autonomy: autonomy.clone(),
                                session_log: &session_log,
                                project_root: &project.root,
                                log_dir: &log_dir,
                                approval_registry: &approval_registry,
                                json_approval: None,
                                agent_source: Some("Codex".to_string()),
                                suppress_agent_started: true,
                                headless: false,
                                context_injection: &context_injection,
                            };
                            emit_side_session_started(
                                &drain_config,
                                &parent_thread_id,
                                &child_thread_id,
                                side_prompt.as_deref(),
                            );
                            // `turn_bus_rx` was subscribed before the
                            // `/side` request was broadcast, so it may still
                            // contain the triggering CodexThreadActionRequested
                            // event. Use a fresh receiver for the child drain
                            // to avoid dispatching `/side` a second time.
                            let mut side_bus_rx = bus.subscribe();
                            drain_external_child_turn(
                                agent,
                                event_rx,
                                &mut side_bus_rx,
                                &drain_config,
                                &mut cumulative_stats,
                                &mut persistent_diff_tracker,
                                &mut persistent_pending_runtime_steers,
                                child_thread_id,
                                "side",
                            )
                            .await;
                        } else {
                            slog(&session_log, |l| {
                                l.warn("Codex side conversation started but no event receiver is available")
                            });
                        }
                    }
                }
                turn_bus_rx = bus.subscribe();
                continue;
            }
            OuterSignal::GeminiThreadAction { op, params: _ } => {
                // Gemini currently has only one daemon-side action: `/new`.
                // The CLI doesn't expose mid-session RPCs via ACP that map
                // to Codex's /compact, /fork, /undo — if that changes we'll
                // extend the trait's `thread_action` for Gemini.
                let result = if op == "new" {
                    persistent_agent = None;
                    persistent_thread = None;
                    persistent_event_rx = None;
                    persistent_gemini_config = None;
                    Ok("agent torn down; next task will spawn a fresh Gemini process".to_string())
                } else {
                    Err(format!("gemini thread action /{} not supported", op))
                };
                let (success, message) = match result {
                    Ok(msg) => (true, msg),
                    Err(e) => (false, e),
                };
                slog(&session_log, |l| {
                    l.info(&format!(
                        "Gemini thread action /{}: {} — {}",
                        op,
                        if success { "ok" } else { "FAILED" },
                        message
                    ))
                });
                bus.send(AppEvent::GeminiThreadActionResult {
                    action: op,
                    success,
                    message,
                });
                turn_bus_rx = bus.subscribe();
                continue;
            }
            OuterSignal::ConversationRollback {
                round_id,
                target_native_message_count,
                turns_to_drop,
            } => {
                // Three possible states:
                //   1. External agent active (Codex / CC / Gemini)
                //   2. Native agent active (persistent_conv is Some)
                //   3. Neither — nothing to roll back from
                //
                // For external agents we try `rollback_turns` first; on
                // the default "not supported" error we fall back to a
                // session reset (shut down, clear persistent state; the
                // next task will re-initialize from scratch).
                if let Some(ref mut agent) = persistent_agent {
                    let backend_name = agent.name().to_ascii_lowercase().replace(' ', "-");
                    match agent.rollback_turns(turns_to_drop).await {
                        Ok(()) => {
                            bus.send(AppEvent::ConversationRolledBack {
                                round_id,
                                turns_removed: turns_to_drop,
                                backend: backend_name,
                                method: "truncated".into(),
                            });
                        }
                        Err(e) => {
                            // Fall back to a session reset: shut the
                            // agent down, drop persistent handles, and
                            // let the next task re-initialize. This
                            // loses conversation context — the only
                            // honest behavior when the protocol doesn't
                            // expose rollback.
                            slog(&session_log, |l| {
                                l.warn(&format!(
                                    "Conversation rollback via protocol failed ({}); falling back to session reset",
                                    e
                                ))
                            });
                            let _ = agent.shutdown().await;
                            persistent_agent = None;
                            persistent_thread = None;
                            persistent_event_rx = None;
                            persistent_codex_config = None;
                            persistent_gemini_config = None;
                            bus.send(AppEvent::ConversationRolledBack {
                                round_id,
                                turns_removed: turns_to_drop,
                                backend: backend_name,
                                method: "session-reset".into(),
                            });
                        }
                    }
                } else if let Some(ref mut conv) = persistent_conv {
                    // Native path: truncate the messages array to the
                    // recorded length. If the round didn't store a
                    // native_message_count (e.g. an external-agent
                    // round), we can't truncate meaningfully; log and
                    // emit a 0-turn event so the dashboard clears the
                    // pending state.
                    let removed = match target_native_message_count {
                        Some(n) => conv.truncate_to(n as usize),
                        None => 0,
                    };
                    bus.send(AppEvent::ConversationRolledBack {
                        round_id,
                        turns_removed: removed as u32,
                        backend: "native".into(),
                        method: "truncated".into(),
                    });
                } else {
                    // No conversation to revert — emit completion
                    // anyway so the dashboard doesn't wait forever.
                    bus.send(AppEvent::ConversationRolledBack {
                        round_id,
                        turns_removed: 0,
                        backend: "native".into(),
                        method: "truncated".into(),
                    });
                }
                turn_bus_rx = bus.subscribe();
                continue;
            }
        };
        // Backend-side dispatch log: emitted at task acceptance, replacing the
        // legacy TUI-side log so headless and dashboard-direct tasks both reach
        // external consumers regardless of which frontend is running.
        emit_task_dispatched_log(
            &bus,
            &session_log,
            &envelope.task,
            envelope.attachment_frame_ids.len(),
        );

        // Pause server-side presence narration for direct-mode tasks — no
        // narration, no hallucinated side-tasks, no 400 errors from Gemini.
        // Programmatic clients (WebSocket with direct:true) don't need it.
        // Uses fetch_add/fetch_sub so it composes with browser voice's
        // ref-count (PresenceConnected += 1, PresenceDisconnected -= 1) —
        // each pause source is one independent reason to mute narration.
        let _direct_pause = if envelope.force_direct {
            Some(PresencePauseGuard::new(presence_paused.clone()))
        } else {
            None
        };

        slog(&session_log, |l| {
            l.debug(&format!(
                "{}task: {}",
                if envelope.force_direct {
                    "Direct "
                } else {
                    "Presence dispatched "
                },
                envelope.task
            ));
        });

        // Resolve frame context_hints → images
        let frame_images = resolve_frame_hints(&envelope.context_hints, &frame_registry).await;

        // Resolve user-attached frames → images. These come from the dashboard's
        // "Attach" buttons (annotation toolbar / Video tab) and are appended to
        // the first user message of the agent conversation, in addition to
        // anything from `context_hints`.
        let attachment_images =
            resolve_frame_ids(&envelope.attachment_frame_ids, &frame_registry).await;
        if !attachment_images.is_empty() {
            slog(&session_log, |l| {
                l.debug(&format!(
                    "Task has {} user attachment(s)",
                    attachment_images.len()
                ))
            });
        }

        // ── CU-first routing: all tasks go to fast CU model first ──
        let mut task_for_agent: Option<String> = None;

        slog(&session_log, |l| {
            l.debug(&format!(
                "CU-first routing: force_direct={}, task={}",
                envelope.force_direct,
                &envelope.task[..envelope.task.len().min(60)]
            ))
        });

        if !envelope.force_direct {
            // Auto-attach latest display frame(s) if none were explicitly provided
            let mut reference_images =
                resolve_frame_ids(&envelope.reference_frame_ids, &frame_registry).await;
            if reference_images.is_empty() {
                reference_images = auto_attach_display_frames(&frame_registry).await;
            }

            // Combine context-hint frames with user attachments so the CU
            // model also sees what the user pointed at when issuing the task.
            let mut cu_context_images = frame_images.clone();
            cu_context_images.extend(attachment_images.iter().cloned());

            match try_cu_first(
                &project_root,
                &reference_images,
                &cu_context_images,
                &envelope.task,
                &session_log,
                &log_dir,
                &bus,
                &session_registry,
            )
            .await
            {
                Some(Ok(CuTaskResult::Completed(stats))) => {
                    cumulative_stats.turns += stats.turns;
                    bus.send(AppEvent::PresenceLog {
                        message: format!("CU task complete ({} turns)", stats.turns),
                        level: None,
                        turn: None,
                    });
                    continue; // done
                }
                Some(Ok(CuTaskResult::Escalate { task })) => {
                    slog(&session_log, |l| {
                        l.info(&format!(
                            "CU escalated to agent: {}",
                            &task[..task.len().min(80)]
                        ))
                    });
                    bus.send(AppEvent::PresenceLog {
                        message: format!("Escalating to agent: {}", &task[..task.len().min(80)]),
                        level: None,
                        turn: None,
                    });
                    task_for_agent = Some(task);
                }
                Some(Err(e)) => {
                    slog(&session_log, |l| {
                        l.cu_task_error(&e.to_string(), Some("main agent"))
                    });
                    task_for_agent = Some(envelope.task.clone());
                }
                None => {
                    // No CU available (no display, no provider) — go to agent directly
                    task_for_agent = Some(envelope.task.clone());
                }
            }
        } else {
            task_for_agent = Some(envelope.task.clone());
        }

        // ── Regular agent path (for escalated or non-CU tasks) ──
        let task_text = task_for_agent.unwrap_or_else(|| envelope.task.clone());

        // Re-read the agent backend each task: the web UI may have changed it.
        let agent_backend = shared_external_agent.read().await.clone();
        // Snapshot the current Codex + Gemini runtime configs. Both backends
        // latch their per-session config at spawn/thread-start — a toggle in
        // the UI takes effect on the NEXT task by forcing an agent rebuild.
        let current_codex_config = shared_codex_config.read().await.clone();
        let current_gemini_config = shared_gemini_config.read().await.clone();

        // Teardown conditions:
        //  - backend changed (any agent)
        //  - backend is Codex and any of the Codex-locked fields differ
        //  - backend is Gemini and any of the Gemini-locked fields differ
        let codex_config_changed =
            matches!(agent_backend, Some(external_agent::AgentBackend::Codex))
                && persistent_codex_config
                    .as_ref()
                    .is_some_and(|prev| !codex_runtime_config_equal(prev, &current_codex_config));
        let gemini_config_changed =
            matches!(agent_backend, Some(external_agent::AgentBackend::GeminiCli))
                && persistent_gemini_config
                    .as_ref()
                    .is_some_and(|prev| !gemini_runtime_config_equal(prev, &current_gemini_config));

        if persistent_agent.is_some()
            && (agent_backend != persistent_agent_backend
                || codex_config_changed
                || gemini_config_changed)
        {
            if codex_config_changed {
                slog(&session_log, |l| {
                    l.info("Codex config changed; rebuilding agent for next task")
                });
            }
            if gemini_config_changed {
                slog(&session_log, |l| {
                    l.info("Gemini config changed; rebuilding agent for next task")
                });
            }
            persistent_agent = None;
            persistent_thread = None;
            persistent_event_rx = None;
            persistent_codex_config = None;
            persistent_gemini_config = None;
            persistent_diff_tracker = ExternalDiffDeltaTracker::default();
            persistent_pending_runtime_steers.clear();
        }

        if let Some(ref backend) = agent_backend {
            // ── External agent path ──
            // The external agent manages its own conversation; we keep the
            // agent + thread alive across tasks dispatched by presence.
            if persistent_agent.is_none() {
                let mut proj = match Project::from_root(project_root.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("Project error: {}", e),
                            level: Some(types::LogLevel::Error),
                            turn: None,
                        });
                        continue;
                    }
                };
                // Apply the live runtime config on top of what was loaded
                // from TOML. The control plane writes TOML synchronously on
                // each change, so normally the two agree — but there's no
                // ordering guarantee between the save and the next
                // `from_root`, and `shared_codex_config` is always the
                // authoritative "what the user just chose" source.
                if matches!(backend, external_agent::AgentBackend::Codex) {
                    let cx = &mut proj.config.agent.codex;
                    cx.command = current_codex_config.command.clone();
                    cx.sandbox = current_codex_config.sandbox.clone();
                    cx.approval_policy = current_codex_config.approval_policy.clone();
                    cx.model = current_codex_config.model.clone();
                    cx.reasoning_effort = current_codex_config.reasoning_effort.clone();
                    cx.web_search = current_codex_config.web_search;
                    cx.network_access = current_codex_config.network_access;
                    cx.writable_roots = current_codex_config.writable_roots.clone();
                }
                if matches!(backend, external_agent::AgentBackend::GeminiCli) {
                    let gm = &mut proj.config.agent.gemini_cli;
                    gm.model = current_gemini_config.model.clone();
                    gm.approval_mode = current_gemini_config.approval_mode.clone();
                    gm.sandbox = current_gemini_config.sandbox;
                    gm.extensions = current_gemini_config.extensions.clone();
                    gm.allowed_mcp_servers = current_gemini_config.allowed_mcp_servers.clone();
                    gm.include_directories = current_gemini_config.include_directories.clone();
                    gm.debug = current_gemini_config.debug;
                }
                let (agent, thread, event_rx) =
                    match create_external_agent(backend, &proj, &session_log, web_port, None).await
                    {
                        Ok(result) => result,
                        Err(e) => {
                            bus.send(AppEvent::PresenceLog {
                                message: format!("External agent error: {}", e),
                                level: Some(types::LogLevel::Error),
                                turn: None,
                            });
                            continue;
                        }
                    };
                slog(&session_log, |l| {
                    l.debug(&format!(
                        "Mode: external agent ({}) via presence, thread: {}",
                        backend, thread.thread_id
                    ))
                });
                persistent_agent = Some(agent);
                persistent_thread = Some(thread);
                persistent_event_rx = Some(event_rx);
                persistent_diff_tracker = ExternalDiffDeltaTracker::default();
                persistent_pending_runtime_steers.clear();
                persistent_agent_backend = agent_backend.clone();
                // Remember the Codex config this agent was spawned with so
                // we can detect drift at the next task and rebuild.
                persistent_codex_config =
                    if matches!(agent_backend, Some(external_agent::AgentBackend::Codex)) {
                        Some(current_codex_config.clone())
                    } else {
                        None
                    };
                persistent_gemini_config =
                    if matches!(agent_backend, Some(external_agent::AgentBackend::GeminiCli)) {
                        Some(current_gemini_config.clone())
                    } else {
                        None
                    };
            }

            // Send the task as a new turn in the existing thread, with any
            // user-attached frames passed as image inputs (Codex `LocalImage`,
            // Gemini ACP `Image` content block).
            //
            // Merge in any steer items queued by the fallback path (backend
            // returned Err from `steer_turn`). They prepend as `[User]`
            // lines so the agent sees them in the same turn's input.
            let agent = persistent_agent.as_mut().unwrap();
            let thread = persistent_thread.as_ref().unwrap();
            let merged_text = drain_steer_queue_as_followup(
                &context_injection,
                &task_text,
                &bus,
                session_log_id(&session_log).as_deref(),
            )
            .unwrap_or_else(|| task_text.clone());
            persistent_diff_tracker.seed_from_session_log(&project.root, &log_dir);
            let send_result = if envelope.attachment_frame_ids.is_empty() {
                agent.send_message(thread, &merged_text).await
            } else {
                // Mixed attachments: frame ids (auto-prefixed "frame:" or bare)
                // and upload ids (always "upload:<id>"). Grab the session dir
                // from the live session log so upload lookups work after a
                // session rotation.
                let session_dir = session_log
                    .lock()
                    .ok()
                    .map(|l| l.dir().to_path_buf())
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let attachments = resolve_attachments(
                    &envelope.attachment_frame_ids,
                    &frame_registry,
                    &session_dir,
                    &project.root,
                )
                .await;
                agent
                    .send_message_with_attachments(thread, &merged_text, &attachments)
                    .await
            };
            if let Err(e) = send_result {
                bus.send(AppEvent::PresenceLog {
                    message: format!("External agent send error: {}", e),
                    level: Some(types::LogLevel::Error),
                    turn: None,
                });
                turn_bus_rx = bus.subscribe();
                continue;
            }
            if let Some(id) = envelope.steer_id.as_deref() {
                bus.send(AppEvent::SteerDelivered {
                    session_id: session_log_id(&session_log),
                    id: id.to_string(),
                    mid_turn: false,
                });
            }

            // Drain events until this turn completes
            let event_rx = persistent_event_rx.as_mut().unwrap();
            let drain_config = DrainConfig {
                bus: &bus,
                session_id: session_log_id(&session_log),
                alias_session_id: None,
                autonomy: autonomy.clone(),
                session_log: &session_log,
                project_root: &project.root,
                log_dir: &log_dir,
                approval_registry: &approval_registry,
                json_approval: None,
                agent_source: Some(backend.to_string()),
                suppress_agent_started: true,
                headless: false,
                context_injection: &context_injection,
            };
            match drain_external_agent_events(
                agent,
                event_rx,
                &mut turn_bus_rx,
                &drain_config,
                &mut cumulative_stats,
                &mut persistent_diff_tracker,
                &mut persistent_pending_runtime_steers,
            )
            .await
            {
                DrainOutcome::TurnCompleted {
                    message,
                    turns_in_round,
                } => {
                    cumulative_stats.turns += 1;
                    cumulative_stats.rounds += 1;
                    bus.send(AppEvent::DoneSignal {
                        message: message.clone(),
                    });
                    // External-agent rounds: no native conversation to snapshot.
                    // Conversation rollback will use the backend's native
                    // mechanism (Codex thread/rollback) or fall back to a
                    // session reset (CC, Gemini).
                    bus.send(AppEvent::RoundComplete {
                        session_id: session_log_id(&session_log),
                        round: cumulative_stats.rounds,
                        turns_in_round,
                        native_message_count: None,
                    });
                }
                DrainOutcome::Interrupted { reason } => {
                    // Interrupt acknowledged by the agent. The Interrupted
                    // event was already emitted inside the drain — we just
                    // surface a presence log and close out the round.
                    bus.send(AppEvent::PresenceLog {
                        message: format!("External agent interrupted: {}", reason),
                        level: None,
                        turn: None,
                    });
                    cumulative_stats.rounds += 1;
                }
                DrainOutcome::Terminated { reason, .. } => {
                    bus.send(AppEvent::PresenceLog {
                        message: format!("External agent terminated: {}", reason),
                        level: Some(types::LogLevel::Error),
                        turn: None,
                    });
                    // Agent is gone — clear persistent state so next task re-initializes
                    persistent_agent = None;
                    persistent_thread = None;
                    persistent_event_rx = None;
                    persistent_diff_tracker = ExternalDiffDeltaTracker::default();
                }
                DrainOutcome::ChannelClosed => {
                    // Channel closed unexpectedly
                    persistent_agent = None;
                    persistent_thread = None;
                    persistent_event_rx = None;
                    persistent_diff_tracker = ExternalDiffDeltaTracker::default();
                }
            }
            turn_bus_rx = bus.subscribe();
        } else {
            // ── Native agent path ──
            if persistent_conv.is_none() {
                // ── First task: full initialization ──
                let proj = match Project::from_root(project_root.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("Project error: {}", e),
                            level: Some(types::LogLevel::Error),
                            turn: None,
                        });
                        continue;
                    }
                };

                // CU tasks are handled by the ephemeral path above; this is the
                // persistent conversation path for regular coding tasks.
                let mut task_provider = match provider::select_provider() {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("Provider error: {}", e),
                            level: Some(types::LogLevel::Error),
                            turn: None,
                        });
                        continue;
                    }
                };
                task_provider.set_cu_enabled(true);

                slog(&session_log, |l| {
                    l.info(&format!(
                        "Mode: direct (provider: {}, context: {})",
                        task_provider.name(),
                        task_provider.context_window()
                    ));
                });

                let role = sub_agent::SubAgentRole::Custom("direct".to_string());
                let system_prompt = if task_provider.use_tools() {
                    prompts::resolve_system_prompt_for_tools(&role, Some(&proj.root))?
                } else {
                    prompts::resolve_system_prompt(&role, Some(&proj.root))?
                };

                let mut conv = Conversation::new(system_prompt, task_provider.context_window());
                setup_fresh_conversation_no_task(&mut conv, &proj);

                // Frame directory awareness
                let frames_dir = log_dir.join("frames");
                conv.add_user(format!(
                    "[System] Video frames from the user's camera are stored at: {}\n\
                     Each frame is a JPEG named by frame ID (e.g., cam0-f00001.jpg).\n\
                     When you receive frame references, you can read them from this path.",
                    frames_dir.display()
                ));
                conv.add_assistant("Understood.".to_string());

                // Add task with optional frame images. Combine context-hint
                // frames (from `frames:` hints) with user-attached frames
                // (from the dashboard's "Attach" buttons) — they're both
                // image content the model should see alongside the task.
                let mut combined_images = frame_images;
                combined_images.extend(attachment_images.iter().cloned());
                if combined_images.is_empty() {
                    conv.add_user(task_text.clone());
                } else {
                    conv.add_user_with_images(task_text.clone(), combined_images);
                }

                persistent_project = Some(proj);
                persistent_provider = Some(task_provider);
                persistent_conv = Some(conv);
            } else {
                // ── Subsequent task: inject into existing conversation ──
                let conv = persistent_conv.as_mut().unwrap();

                let resolved = conv.resolve_dangling_tool_calls();
                if resolved > 0 {
                    slog(&session_log, |l| {
                        l.info(&format!(
                            "Resolved {} dangling tool call(s) from previous round",
                            resolved
                        ))
                    });
                }

                let mut combined_images = frame_images;
                combined_images.extend(attachment_images.iter().cloned());
                if combined_images.is_empty() {
                    conv.add_user(format!("[New Task] {}", task_text));
                } else {
                    conv.add_user_with_images(format!("[New Task] {}", task_text), combined_images);
                }
            }

            if let Some(id) = envelope.steer_id.as_deref() {
                bus.send(AppEvent::SteerDelivered {
                    session_id: session_log_id(&session_log),
                    id: id.to_string(),
                    mid_turn: false,
                });
            }

            // Run one round (agent loop until done/budget/error)
            let (follow_up_tx, mut follow_up_rx) = tokio::sync::mpsc::channel::<FollowUpMessage>(1);
            drop(follow_up_tx); // single-round per task dispatch

            let result = run_round_loop(
                persistent_provider.as_ref().unwrap().as_ref(),
                persistent_conv.as_mut().unwrap(),
                persistent_project.as_ref().unwrap(),
                None, // not sub-agent
                &bus,
                autonomy.clone(),
                session_log.clone(),
                &log_dir,
                None, // no MCP
                &mut follow_up_rx,
                None, // no JSON approval
                &approval_registry,
                &context_injection, // shared with presence
                false,              // not headless
            )
            .await;

            match result {
                Ok(stats) => {
                    cumulative_stats.turns += stats.turns;
                    cumulative_stats.rounds += stats.rounds;
                    cumulative_stats.usage.prompt_tokens += stats.usage.prompt_tokens;
                    cumulative_stats.usage.completion_tokens += stats.usage.completion_tokens;
                    cumulative_stats.usage.total_tokens += stats.usage.total_tokens;
                }
                Err(e) => {
                    // Log error but DON'T discard conversation — it persists
                    bus.send(AppEvent::PresenceLog {
                        message: format!("Task error: {}", e),
                        level: Some(types::LogLevel::Error),
                        turn: None,
                    });
                }
            }
        }
    }

    Ok(cumulative_stats)
}

/// Tail the orchestrator's session JSONL from `offset`, emitting new entries
/// to the TUI as orchestrator log entries. Returns the new offset.
fn tail_orchestrator_log(
    log_path: &Path,
    offset: u64,
    bus: &EventBus,
    session_log: &SharedSessionLog,
) -> u64 {
    use std::io::{BufRead, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(log_path) else {
        return offset;
    };
    let meta = file.metadata().ok();
    let file_len = meta.map(|m| m.len()).unwrap_or(0);
    if file_len <= offset {
        return offset;
    }
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return offset;
    }
    let reader = std::io::BufReader::new(&file);
    let mut new_offset = offset;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        new_offset += line.len() as u64 + 1; // +1 for newline
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let event = entry["event"].as_str().unwrap_or("");
        let level = entry["level"].as_str().unwrap_or("info");
        let message = entry["message"].as_str().unwrap_or("");
        let turn = entry["turn"].as_u64().map(|t| t as usize);

        // Skip noisy/redundant events
        match event {
            "session_start" | "session_end" | "messages_input" => continue,
            _ => {}
        }

        // Map orchestrator log level to TUI LogLevel
        let tui_level = match level {
            "debug" => crate::types::LogLevel::Debug,
            "warn" => crate::types::LogLevel::Warn,
            "error" => crate::types::LogLevel::Error,
            _ => crate::types::LogLevel::Detail,
        };

        // Format the log line with orchestrator context
        let content = match event {
            "turn_start" => {
                let budget = entry["data"]["budget_pct"].as_f64().unwrap_or(0.0);
                format!("Turn {} — budget {:.0}%", turn.unwrap_or(0), budget * 100.0)
            }
            "model_response" => {
                let data = &entry["data"];
                let tokens = data["tokens"]["total"].as_u64().unwrap_or(0);
                let content_len = data["content_length"].as_u64().unwrap_or(0);
                if content_len > 0 {
                    let preview: String = message.chars().take(200).collect();
                    format!("Model ({} tokens): {}", tokens, preview)
                } else {
                    format!("Model ({} tokens, tool calls)", tokens)
                }
            }
            "reasoning" => {
                if message.is_empty() {
                    continue;
                }
                format!("Reasoning: {}", message)
            }
            "agent_input" => {
                let funcs = entry["data"]["functions"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                format!("Agent: {}", funcs)
            }
            "agent_output" => {
                let preview: String = message.chars().take(300).collect();
                if preview.is_empty() {
                    continue;
                }
                format!("Output: {}", preview)
            }
            "info" | "debug" | "warn" | "error" => {
                if message.is_empty() {
                    continue;
                }
                message.to_string()
            }
            _ => {
                if message.is_empty() {
                    continue;
                }
                format!("{}: {}", event, message)
            }
        };

        let prefixed = format!("[orch] {}", content);

        slog(session_log, |l| {
            l.debug(&prefixed);
        });

        bus.send(AppEvent::OrchestratorLog {
            message: prefixed.clone(),
            level: tui_level,
        });
    }
    new_offset
}

async fn run_user_mode(
    _provider: Box<dyn provider::ChatProvider>,
    task: String,
    project: Project,
    bus: EventBus,
    _autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
) -> Result<LoopStats, CallerError> {
    slog(&session_log, |l| {
        l.info("Mode: user (orchestrator subprocess)");
    });
    bus.send(AppEvent::OrchestratorProgress {
        turn: 0,
        status: "spawning".to_string(),
        last_action: String::new(),
    });

    // Build orchestrator spec
    let caller_path = user_mode::get_caller_path();
    let spec = user_mode::spawn_orchestrator_spec(&task, &project, &caller_path);

    // Create directories for result/progress files
    if let Some(parent) = spec.result_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Build and spawn the orchestrator subprocess
    let spawn_cmd = sub_agent::build_spawn_command(&spec, &caller_path);
    slog(&session_log, |l| {
        l.info(&format!("Spawning orchestrator: {}", spawn_cmd));
    });

    let mut child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&spawn_cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| CallerError::SubAgent(format!("Failed to spawn orchestrator: {}", e)))?;

    // Capture stderr in a background task — extract orchestrator session log path
    let stderr = child.stderr.take();
    let session_log_stderr = session_log.clone();
    let orch_session_log_path: Arc<std::sync::Mutex<Option<PathBuf>>> =
        Arc::new(std::sync::Mutex::new(None));
    let orch_log_path_writer = orch_session_log_path.clone();
    let stderr_handle = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            use tokio::io::AsyncBufReadExt;
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Extract session log path from "Session log: <path>"
                if line.starts_with("Session log: ") {
                    let path = PathBuf::from(line.trim_start_matches("Session log: ").trim());
                    *orch_log_path_writer
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(path);
                }
                slog(&session_log_stderr, |l| {
                    l.debug(&format!("orchestrator stderr: {}", line));
                });
                eprintln!("orchestrator: {}", line);
            }
        }
    });

    // Monitor loop: poll progress file + tail orchestrator session log
    let mut last_progress_turn: usize = 0;
    let mut orch_log_offset: u64 = 0;
    let mut orch_log_file: Option<PathBuf> = None;
    let poll_interval = tokio::time::Duration::from_millis(500);
    let mut poll_timer = tokio::time::interval(poll_interval);
    poll_timer.tick().await; // consume the immediate first tick

    let exit_status = loop {
        tokio::select! {
            status = child.wait() => {
                break status.map_err(|e| CallerError::SubAgent(format!("Orchestrator wait error: {}", e)))?;
            }
            _ = poll_timer.tick() => {
                // Check progress file
                if let Ok(progress) = sub_agent::read_progress(&spec.progress_file) {
                    if progress.turn > last_progress_turn {
                        last_progress_turn = progress.turn;
                        let user_msg = user_mode::format_progress_for_user(&progress);
                        slog(&session_log, |l| {
                            l.info(&format!("Orchestrator progress: {}", user_msg));
                        });
                        bus.send(AppEvent::OrchestratorProgress {
                            turn: progress.turn,
                            status: progress.status.clone(),
                            last_action: progress.last_action.clone(),
                        });
                    }
                }

                // Tail orchestrator session log for detailed events
                if orch_log_file.is_none() {
                    orch_log_file = orch_session_log_path
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                }
                if let Some(ref log_path) = orch_log_file {
                    orch_log_offset = tail_orchestrator_log(
                        log_path, orch_log_offset, &bus, &session_log,
                    );
                }
            }
        }
    };

    // Final tail to catch any remaining log entries written before exit
    if let Some(ref log_path) = orch_log_file {
        tail_orchestrator_log(log_path, orch_log_offset, &bus, &session_log);
    }

    // Wait for stderr task to finish
    let _ = stderr_handle.await;

    slog(&session_log, |l| {
        l.info(&format!("Orchestrator exited with status: {}", exit_status));
    });

    // Read result from result file, or synthesize a failure
    let mut loop_stats = LoopStats::default();
    let result = if spec.result_file.exists() {
        match sub_agent::read_result(&spec.result_file) {
            Ok(r) => r,
            Err(e) => sub_agent::SubAgentResult {
                id: spec.id.clone(),
                status: sub_agent::SubAgentStatus::Failed(format!("Result parse error: {}", e)),
                summary: "Orchestrator finished but result could not be parsed".to_string(),
                brief: "Orchestrator result could not be parsed.".to_string(),
                findings: vec![],
                artifacts: vec![],
                usage: provider::TokenUsage::default(),
            },
        }
    } else {
        sub_agent::SubAgentResult {
            id: spec.id.clone(),
            status: sub_agent::SubAgentStatus::Failed(format!("exit code: {}", exit_status)),
            summary: "Orchestrator exited without writing a result file".to_string(),
            brief: "Orchestrator exited without a result.".to_string(),
            findings: vec![],
            artifacts: vec![],
            usage: provider::TokenUsage::default(),
        }
    };

    loop_stats.usage = result.usage.clone();
    loop_stats.turns = last_progress_turn;

    let result_msg = sub_agent::format_result_message(&result);
    slog(&session_log, |l| {
        l.info(&format!("Orchestrator result: {}", result_msg));
    });
    slog(&session_log, |l| {
        l.debug(&format!("Task brief (orchestrator): {}", result.brief));
    });
    bus.send(AppEvent::SubAgentResult {
        formatted: result_msg.clone(),
    });

    let reason = match &result.status {
        sub_agent::SubAgentStatus::Completed => "Task complete".to_string(),
        sub_agent::SubAgentStatus::Failed(reason) => format!("Orchestrator failed: {}", reason),
    };
    bus.send(AppEvent::TaskComplete {
        session_id: session_log_id(&session_log),
        reason: reason.clone(),
        summary: Some(result.brief.clone()),
    });

    Ok(loop_stats)
}

async fn run_direct_mode(
    mut provider: Box<dyn provider::ChatProvider>,
    task: String,

    project: Project,
    bus: EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
    mcp_mgr: Option<mcp_client::McpClientManager>,
    mut follow_up_rx: FollowUpReceiver,
    json_approval: Option<JsonApprovalSlot>,
    approval_registry: event::ApprovalRegistry,
    context_injection: event::ContextInjectionQueue,
    headless: bool,
    attachments: UserAttachments,
) -> Result<LoopStats, CallerError> {
    let role = sub_agent::SubAgentRole::Custom("direct".to_string());
    let system_prompt = if provider.use_tools() {
        prompts::resolve_system_prompt_for_tools(&role, Some(&project.root))?
    } else {
        prompts::resolve_system_prompt(&role, Some(&project.root))?
    };

    slog(&session_log, |l| {
        l.info(&format!(
            "Mode: direct (provider: {}, context: {})",
            provider.name(),
            provider.context_window()
        ));
    });
    if headless {
        println!(
            "Provider: {} (context window: {})",
            provider.name(),
            provider.context_window()
        );
    }

    // Try to resume from saved conversation if it exists in this session dir
    let conv_path = log_dir.join("conversation.jsonl");
    let attachment_images = attachments.conversation_images();
    let mut conversation = if conv_path.exists() {
        match Conversation::load_from_file(&conv_path, provider.context_window()) {
            Ok(mut conv) => {
                slog(&session_log, |l| {
                    l.info(&format!(
                        "Resumed conversation ({} messages, turn {})",
                        conv.len(),
                        conv.turn()
                    ))
                });
                // Append the new task as a continuation message
                let resume_msg = attachments
                    .text_with_file_prelude(&format!("[Session resumed] Continue with: {}", task));
                if attachment_images.is_empty() {
                    conv.add_user(resume_msg);
                } else {
                    conv.add_user_with_images(resume_msg, attachment_images.clone());
                }
                conv
            }
            Err(e) => {
                slog(&session_log, |l| {
                    l.warn(&format!(
                        "Failed to load conversation, starting fresh: {}",
                        e
                    ))
                });
                let mut conv = Conversation::new(system_prompt, provider.context_window());
                setup_fresh_conversation_with_attachments(
                    &mut conv,
                    &project,
                    &attachments.text_with_file_prelude(&task),
                    attachment_images.clone(),
                );
                conv
            }
        }
    } else {
        let mut conv = Conversation::new(system_prompt, provider.context_window());
        setup_fresh_conversation_with_attachments(
            &mut conv,
            &project,
            &attachments.text_with_file_prelude(&task),
            attachment_images.clone(),
        );
        conv
    };

    // Register MCP tools so providers include them in API requests
    if let Some(ref mgr) = mcp_mgr {
        tools::register_extra_tools(mgr.all_tools());
    }

    // Enable native CU on the main provider. The "computer" tool type
    // requires no display dimensions — the model infers from screenshots.
    provider.set_cu_enabled(true);

    if headless {
        println!("Task: {}", task);
        println!("---");
    }

    run_round_loop(
        provider.as_ref(),
        &mut conversation,
        &project,
        None,
        &bus,
        autonomy,
        session_log,
        &log_dir,
        mcp_mgr.as_ref(),
        &mut follow_up_rx,
        json_approval.as_ref(),
        &approval_registry,
        &context_injection,
        headless,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_external_agent_mode(
    backend: external_agent::AgentBackend,
    task: String,
    project: Project,
    bus: EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    _log_dir: PathBuf,
    mut follow_up_rx: FollowUpReceiver,
    json_approval: Option<JsonApprovalSlot>,
    approval_registry: event::ApprovalRegistry,
    context_injection: event::ContextInjectionQueue,
    headless: bool,
    web_port: Option<u16>,
    attachments: UserAttachments,
    resume_session: Option<String>,
    control_session_id: Option<String>,
    emit_session_started_after_identity: bool,
) -> Result<LoopStats, CallerError> {
    slog(&session_log, |l| {
        l.info(&format!("Mode: external agent ({})", backend));
    });
    if headless {
        println!("External agent: {}", backend);
        if task.trim().is_empty() {
            println!("Attached session; waiting for input");
        } else {
            println!("Task: {}", task);
        }
        println!("---");
    }

    // Construct, initialize, and start a thread for the external agent
    let resumed_external_session = resume_session.clone();
    let intendant_session_id = control_session_id.or_else(|| session_log_id(&session_log));
    let (mut agent, thread, mut event_rx) =
        match create_external_agent(&backend, &project, &session_log, web_port, resume_session)
            .await
        {
            Ok(started) => started,
            Err(e) => {
                if emit_session_started_after_identity {
                    if let Some(session_id) = intendant_session_id.clone() {
                        bus.send(AppEvent::SessionStarted {
                            session_id,
                            task: if task.trim().is_empty() {
                                None
                            } else {
                                Some(task.clone())
                            },
                        });
                    }
                }
                return Err(e);
            }
        };
    let backend_session_id = thread.thread_id.clone();
    let live_session_id = if backend.thread_id_is_canonical(&backend_session_id) {
        Some(backend_session_id.clone())
    } else {
        intendant_session_id.clone()
    };
    if let Some(session_id) = intendant_session_id.clone() {
        bus.send(AppEvent::SessionIdentity {
            session_id,
            source: backend.as_short_str().to_string(),
            backend_session_id: backend_session_id.clone(),
        });
    }
    if emit_session_started_after_identity {
        if let Some(session_id) = live_session_id.clone() {
            bus.send(AppEvent::SessionStarted {
                session_id,
                task: if task.trim().is_empty() {
                    None
                } else {
                    Some(task.clone())
                },
            });
        }
    }

    // Event loop
    let mut user_turn_revisions = match (
        &backend,
        resumed_external_session.as_deref(),
        backend_session_id.as_str(),
    ) {
        (external_agent::AgentBackend::Codex, Some(_), session_id) => {
            codex_user_turn_state_from_history(session_id).unwrap_or_default()
        }
        _ => UserTurnRevisionState::default(),
    };
    let mut round = user_turn_revisions.active_count() as usize;
    let mut stats = LoopStats::default();
    if backend == external_agent::AgentBackend::Codex {
        stats.codex_subagent_parent_threads = codex_subagent_parent_threads_from_log(&_log_dir);
        for child_id in stats.codex_subagent_parent_threads.keys().cloned() {
            stats.codex_subagent_rounds.entry(child_id).or_insert(0);
        }
    }
    let mut diff_tracker = ExternalDiffDeltaTracker::default();
    let mut pending_runtime_steers: std::collections::VecDeque<PendingRuntimeSteer> =
        std::collections::VecDeque::new();
    let mut open_side_threads: HashMap<String, String> = HashMap::new();
    let mut side_rounds: HashMap<String, usize> = HashMap::new();
    let mut side_turn_revisions: HashMap<String, UserTurnRevisionState> = HashMap::new();
    let mut next_turn = if task.trim().is_empty() {
        None
    } else {
        Some(FollowUpMessage::with_attachments(task, attachments))
    };

    let drain_config = DrainConfig {
        bus: &bus,
        session_id: live_session_id.clone(),
        alias_session_id: if intendant_session_id != live_session_id {
            intendant_session_id.clone()
        } else {
            None
        },
        autonomy: autonomy.clone(),
        session_log: &session_log,
        project_root: &project.root,
        log_dir: &_log_dir,
        approval_registry: &approval_registry,
        json_approval: json_approval.as_ref(),
        agent_source: Some(backend.to_string()),
        suppress_agent_started: false,
        headless,
        context_injection: &context_injection,
    };
    let mut idle_bus_rx = bus.subscribe();
    // Subscribe while idle so a steer sent immediately after the prompt
    // (before turn/start returns) is still available to the turn drain.
    let mut turn_bus_rx = bus.subscribe();

    'outer: loop {
        let followup = match next_turn.take() {
            Some(turn) => turn,
            None => loop {
                tokio::select! {
                    maybe_followup = follow_up_rx.recv() => {
                        match maybe_followup {
                            Some(followup) => break followup,
                            None => {
                                slog(&session_log, |l| {
                                    l.info("Follow-up channel closed, exiting")
                                });
                                break 'outer;
                            }
                        }
                    }
                    bus_event = idle_bus_rx.recv() => {
                        match bus_event {
                            Ok(AppEvent::SteerRequested {
                                session_id,
                                text,
                                id,
                            }) if event_targets_external_session_or_side(
                                &session_id,
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                break FollowUpMessage::steer(
                                    text,
                                    UserAttachments::default(),
                                    id,
                                )
                                .for_target(session_id);
                            }
                            Ok(AppEvent::CodexThreadActionRequested {
                                session_id,
                                action,
                                params,
                            }) if event_targets_external_session_or_side(
                                &session_id,
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                if let Some(side_thread_id) = session_id
                                    .as_deref()
                                    .filter(|id| open_side_threads.contains_key(*id))
                                    .map(str::to_string)
                                {
                                    if action == "undo" {
                                        handle_side_undo_thread_action(
                                            &mut agent,
                                            &mut side_rounds,
                                            &mut side_turn_revisions,
                                            &side_thread_id,
                                            params,
                                            &drain_config,
                                        )
                                        .await;
                                        turn_bus_rx = bus.subscribe();
                                        continue;
                                    }
                                }
                                if action == "undo" {
                                    handle_parent_undo_thread_action(
                                        &mut agent,
                                        &mut round,
                                        &mut user_turn_revisions,
                                        params,
                                        &drain_config,
                                    )
                                    .await;
                                    turn_bus_rx = bus.subscribe();
                                    continue;
                                }
                                let effect = handle_external_thread_action(
                                    &mut agent,
                                    action,
                                    params,
                                    session_id,
                                    &drain_config,
                                )
                                .await;
                                if let ExternalThreadActionEffect::SideTurnStarted {
                                    parent_thread_id,
                                    child_thread_id,
                                    prompt,
                                } = effect
                                {
                                    open_side_threads.insert(
                                        child_thread_id.clone(),
                                        parent_thread_id.clone(),
                                    );
                                    side_rounds.entry(child_thread_id.clone()).or_insert(1);
                                    side_turn_revisions
                                        .entry(child_thread_id.clone())
                                        .or_insert_with(|| {
                                            let mut state = UserTurnRevisionState::default();
                                            state.record_next_turn();
                                            state
                                        });
                                    emit_side_session_started(
                                        &drain_config,
                                        &parent_thread_id,
                                        &child_thread_id,
                                        prompt.as_deref(),
                                    );
                                    // `turn_bus_rx` can still have the
                                    // triggering `/side` event queued because
                                    // the idle loop consumed it through a
                                    // separate receiver. A fresh receiver keeps
                                    // the side drain from replaying the action.
                                    let mut side_bus_rx = bus.subscribe();
                                    drain_external_child_turn(
                                        &mut agent,
                                        &mut event_rx,
                                        &mut side_bus_rx,
                                        &drain_config,
                                        &mut stats,
                                        &mut diff_tracker,
                                        &mut pending_runtime_steers,
                                        child_thread_id,
                                        "side",
                                    )
                                    .await;
                                } else if let ExternalThreadActionEffect::SideTurnClosed {
                                    child_thread_id,
                                } = effect
                                {
                                    open_side_threads.remove(&child_thread_id);
                                    side_rounds.remove(&child_thread_id);
                                    side_turn_revisions.remove(&child_thread_id);
                                }
                                turn_bus_rx = bus.subscribe();
                            }
                            Ok(AppEvent::InterruptRequested { session_id })
                                if event_targets_external_session_or_side(
                                    &session_id,
                                    &live_session_id,
                                    &drain_config.alias_session_id,
                                    &open_side_threads,
                                ) =>
                            {
                                // Ignore idle interrupts and reset the turn
                                // receiver so the next task does not inherit
                                // a stale Stop request.
                                turn_bus_rx = bus.subscribe();
                            }
                            Ok(_) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                slog(&session_log, |l| l.info("Event bus closed, exiting"));
                                break 'outer;
                            }
                        }
                    }
                }
            },
        };
        let turn_text = followup.text;
        let attachments = followup.attachments;
        let steer_id = followup.steer_id;
        let follow_up_id = followup.follow_up_id;
        let edit_user_turn_index = followup.edit_user_turn_index;
        let edit_user_turn_revision = followup.edit_user_turn_revision;
        let target_session_id = followup.target_session_id.clone();

        if let Some(side_thread_id) = target_session_id
            .as_deref()
            .filter(|id| open_side_threads.contains_key(*id))
            .map(str::to_string)
        {
            let mut replacement_for_user_turn_index = None;
            if let Some(user_turn_index) = edit_user_turn_index {
                if !agent.supports_user_message_rewind() {
                    let message = format!("{} does not support user-message rewind", agent.name());
                    slog(&session_log, |l| l.warn(&message));
                    bus.send(AppEvent::LoopError(message));
                    continue;
                }
                let current_side_round = *side_rounds.entry(side_thread_id.clone()).or_insert(1);
                let revisions = side_turn_revisions
                    .entry(side_thread_id.clone())
                    .or_default();
                revisions.seed_active_turns_to(current_side_round as u32);
                if let Err(message) =
                    revisions.validate_expected_revision(user_turn_index, edit_user_turn_revision)
                {
                    slog(&session_log, |l| l.warn(&message));
                    bus.send(AppEvent::LoopError(message));
                    continue;
                }
                match rollback_side_thread_from_turn(
                    &mut agent,
                    &mut side_rounds,
                    &mut side_turn_revisions,
                    &side_thread_id,
                    user_turn_index,
                    &drain_config,
                )
                .await
                {
                    Ok(turns_to_drop) => {
                        replacement_for_user_turn_index = Some(user_turn_index);
                        let message = format!(
                            "Edited side user turn {}; rolled back {} turn{}",
                            user_turn_index,
                            turns_to_drop,
                            if turns_to_drop == 1 { "" } else { "s" }
                        );
                        slog(&session_log, |l| l.info(&message));
                    }
                    Err(message) => {
                        slog(&session_log, |l| l.warn(&message));
                        bus.send(AppEvent::LoopError(message));
                        continue;
                    }
                }
            }

            let side_round = side_rounds.entry(side_thread_id.clone()).or_insert(0);
            *side_round += 1;
            let user_turn_revision = side_turn_revisions
                .entry(side_thread_id.clone())
                .or_default()
                .record_active_turn(*side_round as u32);
            emit_user_message_log(
                &bus,
                &session_log,
                Some(&side_thread_id),
                Some(*side_round as u32),
                Some(user_turn_revision),
                replacement_for_user_turn_index,
                &turn_text,
            );
            let merged = drain_steer_queue_as_followup(
                &context_injection,
                &turn_text,
                &bus,
                Some(&side_thread_id),
            )
            .unwrap_or_else(|| turn_text.clone());
            let side_thread = external_agent::AgentThread {
                thread_id: side_thread_id.clone(),
            };
            let send_result = if attachments.is_empty() {
                agent.send_message(&side_thread, &merged).await
            } else {
                agent
                    .send_message_with_attachments(&side_thread, &merged, &attachments.items)
                    .await
            };
            if let Err(e) = send_result {
                emit_follow_up_status(
                    &bus,
                    Some(&side_thread_id),
                    &follow_up_id,
                    Some(&turn_text),
                    "failed",
                    Some("failed to send side follow-up"),
                );
                bus.send(AppEvent::LoopError(format!(
                    "Failed to send side follow-up: {}",
                    e
                )));
                continue;
            }
            emit_follow_up_status(
                &bus,
                Some(&side_thread_id),
                &follow_up_id,
                Some(&turn_text),
                "delivered",
                None,
            );
            if let Some(id) = steer_id {
                bus.send(AppEvent::SteerDelivered {
                    session_id: Some(side_thread_id.clone()),
                    id,
                    mid_turn: false,
                });
            }
            let mut side_bus_rx = bus.subscribe();
            drain_external_child_turn(
                &mut agent,
                &mut event_rx,
                &mut side_bus_rx,
                &drain_config,
                &mut stats,
                &mut diff_tracker,
                &mut pending_runtime_steers,
                side_thread_id,
                "side",
            )
            .await;
            turn_bus_rx = bus.subscribe();
            continue;
        }

        if let Some(subagent_thread_id) = target_session_id
            .as_deref()
            .filter(|id| stats.codex_subagent_parent_threads.contains_key(*id))
            .map(str::to_string)
        {
            if edit_user_turn_index.is_some() {
                let message = format!(
                    "User-message rewind is not supported for Codex subagent session {}",
                    subagent_thread_id.chars().take(8).collect::<String>()
                );
                slog(&session_log, |l| l.warn(&message));
                bus.send(AppEvent::LoopError(message));
                continue;
            }

            let subagent_round = stats
                .codex_subagent_rounds
                .entry(subagent_thread_id.clone())
                .or_insert(0);
            *subagent_round += 1;
            emit_user_message_log(
                &bus,
                &session_log,
                Some(&subagent_thread_id),
                Some(*subagent_round as u32),
                None,
                None,
                &turn_text,
            );
            let merged = drain_steer_queue_as_followup(
                &context_injection,
                &turn_text,
                &bus,
                Some(&subagent_thread_id),
            )
            .unwrap_or_else(|| turn_text.clone());
            let subagent_thread = external_agent::AgentThread {
                thread_id: subagent_thread_id.clone(),
            };
            let parent_thread_id = stats
                .codex_subagent_parent_threads
                .get(&subagent_thread_id)
                .cloned()
                .unwrap_or_else(|| thread.thread_id.clone());
            let send_result = if attachments.is_empty() {
                agent.send_message(&subagent_thread, &merged).await
            } else {
                agent
                    .send_message_with_attachments(&subagent_thread, &merged, &attachments.items)
                    .await
            };
            if let Err(e) = send_result {
                let _ = agent.activate_thread(&parent_thread_id).await;
                emit_follow_up_status(
                    &bus,
                    Some(&subagent_thread_id),
                    &follow_up_id,
                    Some(&turn_text),
                    "failed",
                    Some("failed to send subagent follow-up"),
                );
                bus.send(AppEvent::LoopError(format!(
                    "Failed to send subagent follow-up: {}",
                    e
                )));
                continue;
            }
            emit_follow_up_status(
                &bus,
                Some(&subagent_thread_id),
                &follow_up_id,
                Some(&turn_text),
                "delivered",
                None,
            );
            if let Some(id) = steer_id {
                bus.send(AppEvent::SteerDelivered {
                    session_id: Some(subagent_thread_id.clone()),
                    id,
                    mid_turn: false,
                });
            }
            let mut subagent_bus_rx = bus.subscribe();
            drain_external_child_turn(
                &mut agent,
                &mut event_rx,
                &mut subagent_bus_rx,
                &drain_config,
                &mut stats,
                &mut diff_tracker,
                &mut pending_runtime_steers,
                subagent_thread_id,
                "subagent",
            )
            .await;
            if let Err(e) = agent.activate_thread(&parent_thread_id).await {
                let message = format!("Failed to restore Codex parent thread: {}", e);
                slog(&session_log, |l| l.warn(&message));
                bus.send(AppEvent::LoopError(message));
            }
            turn_bus_rx = bus.subscribe();
            continue;
        }

        let mut replacement_for_user_turn_index = None;
        if let Some(user_turn_index) = edit_user_turn_index {
            if !agent.supports_user_message_rewind() {
                let message = format!("{} does not support user-message rewind", agent.name());
                slog(&session_log, |l| l.warn(&message));
                bus.send(AppEvent::LoopError(message));
                continue;
            }
            if user_turn_index == 0 || user_turn_index as usize > round {
                let message = format!(
                    "Cannot edit user turn {} in {} session {}; current user turn count is {}",
                    user_turn_index,
                    backend,
                    live_session_id
                        .as_deref()
                        .map(|sid| sid.chars().take(8).collect::<String>())
                        .unwrap_or_else(|| "unknown".to_string()),
                    round
                );
                slog(&session_log, |l| l.warn(&message));
                bus.send(AppEvent::LoopError(message));
                continue;
            }
            if let Err(message) = user_turn_revisions
                .validate_expected_revision(user_turn_index, edit_user_turn_revision)
            {
                slog(&session_log, |l| l.warn(&message));
                bus.send(AppEvent::LoopError(message));
                continue;
            }
            let turns_to_drop = round as u32 - user_turn_index + 1;
            match agent.rollback_turns(turns_to_drop).await {
                Ok(()) => {
                    user_turn_revisions.rewind_from_turn(user_turn_index);
                    round = user_turn_index.saturating_sub(1) as usize;
                    replacement_for_user_turn_index = Some(user_turn_index);
                    let message = format!(
                        "Edited user turn {}; rolled back {} turn{}",
                        user_turn_index,
                        turns_to_drop,
                        if turns_to_drop == 1 { "" } else { "s" }
                    );
                    slog(&session_log, |l| l.info(&message));
                    bus.send(AppEvent::UserMessageRewind {
                        session_id: live_session_id.clone(),
                        user_turn_index,
                        turns_removed: turns_to_drop,
                    });
                }
                Err(e) => {
                    let message = format!(
                        "Cannot edit user turn {} in {} session: {}",
                        user_turn_index, backend, e
                    );
                    slog(&session_log, |l| l.warn(&message));
                    bus.send(AppEvent::LoopError(message));
                    continue;
                }
            }
        }

        round += 1;
        let user_turn_revision = user_turn_revisions.record_active_turn(round as u32);
        stats.turns = 0;
        let attachment_count = attachments.len();
        emit_user_message_log(
            &bus,
            &session_log,
            live_session_id.as_deref(),
            Some(round as u32),
            Some(user_turn_revision),
            replacement_for_user_turn_index,
            &turn_text,
        );
        let merged = drain_steer_queue_as_followup(
            &context_injection,
            &turn_text,
            &bus,
            live_session_id.as_deref(),
        )
        .unwrap_or_else(|| turn_text.clone());
        slog(&session_log, |l| {
            if round == 1 {
                l.info(&format!(
                    "Initial task sent to external agent{}",
                    if attachment_count == 0 {
                        String::new()
                    } else {
                        format!(" with {} attachment(s)", attachment_count)
                    }
                ));
            } else {
                l.info(&format!(
                    "Follow-up round {}: {}{}",
                    round,
                    merged,
                    if attachment_count == 0 {
                        String::new()
                    } else {
                        format!(" ({} attachment(s))", attachment_count)
                    }
                ));
            }
        });
        diff_tracker.seed_from_session_log(&project.root, &_log_dir);
        let send_result = if attachments.is_empty() {
            agent.send_message(&thread, &merged).await
        } else {
            agent
                .send_message_with_attachments(&thread, &merged, &attachments.items)
                .await
        };
        if let Err(e) = send_result {
            emit_follow_up_status(
                &bus,
                live_session_id.as_deref(),
                &follow_up_id,
                Some(&turn_text),
                "failed",
                Some("failed to send follow-up"),
            );
            if round == 1 {
                return Err(e);
            }
            bus.send(AppEvent::LoopError(format!(
                "Failed to send follow-up: {}",
                e
            )));
            break;
        }
        emit_follow_up_status(
            &bus,
            live_session_id.as_deref(),
            &follow_up_id,
            Some(&turn_text),
            "delivered",
            None,
        );
        if let Some(id) = steer_id {
            bus.send(AppEvent::SteerDelivered {
                session_id: live_session_id.clone(),
                id,
                mid_turn: false,
            });
        }

        match drain_external_agent_events(
            &mut agent,
            &mut event_rx,
            &mut turn_bus_rx,
            &drain_config,
            &mut stats,
            &mut diff_tracker,
            &mut pending_runtime_steers,
        )
        .await
        {
            DrainOutcome::TurnCompleted {
                message,
                turns_in_round,
            } => {
                stats.rounds = round;

                bus.send(AppEvent::DoneSignal {
                    message: message.clone(),
                });
                bus.send(AppEvent::RoundComplete {
                    session_id: live_session_id.clone(),
                    round,
                    turns_in_round,
                    native_message_count: None,
                });
                slog(&session_log, |l| l.round_complete(round, turns_in_round));
            }
            DrainOutcome::Interrupted { reason } => {
                // User-requested interrupt. Emit RoundComplete so the
                // dashboard updates, log it, and wait for the next
                // follow-up or channel close — the interrupt *is* the
                // terminal event for this round.
                stats.rounds = round;
                slog(&session_log, |l| {
                    l.info(&format!("External agent interrupted: {}", reason))
                });
                bus.send(AppEvent::RoundComplete {
                    session_id: live_session_id.clone(),
                    round,
                    turns_in_round: stats.turns,
                    native_message_count: None,
                });
            }
            DrainOutcome::Terminated { reason, exit_code } => {
                stats.rounds = round;
                slog(&session_log, |l| {
                    l.info(&format!(
                        "External agent terminated: {} (exit code: {:?})",
                        reason, exit_code
                    ));
                });
                bus.send(AppEvent::TaskComplete {
                    session_id: live_session_id.clone(),
                    reason: reason.clone(),
                    summary: stats.last_response.clone(),
                });
                break;
            }
            DrainOutcome::ChannelClosed => {
                slog(&session_log, |l| {
                    l.info("External agent event channel closed")
                });
                break;
            }
        }
        turn_bus_rx = bus.subscribe();
    }

    if let Err(e) = agent.shutdown().await {
        slog(&session_log, |l| {
            l.warn(&format!("Agent shutdown error: {}", e))
        });
    }

    Ok(stats)
}

/// Set up a fresh conversation with project context, memory, and skills (without a task).
/// Used by both `setup_fresh_conversation` and the persistent presence conversation.
fn setup_fresh_conversation_no_task(conv: &mut Conversation, project: &Project) {
    // Inject project root so the model knows which directory to work in
    conv.add_user(format!(
        "Working directory: {}\nThis is the project you should examine and modify. \
All relative paths and commands execute from this directory.",
        project.root.display()
    ));
    conv.add_assistant(
        "Understood. I will work within the specified project directory.".to_string(),
    );

    // Inject INTENDANT.md instructions
    if let Some(instructions) = prompts::load_project_instructions(Some(&project.root)) {
        conv.add_user(instructions);
        conv.add_assistant("Acknowledged. I will follow the project instructions.".to_string());
    }

    // Inject knowledge
    if project.config.memory.enabled {
        if let Ok(kstore) = knowledge::load(&project.memory_path()) {
            let refs: Vec<&_> = kstore.entries.iter().collect();
            let msg = knowledge::format_for_injection(&refs);
            if !msg.is_empty() {
                conv.add_user(msg);
                conv.add_assistant(
                    "Acknowledged. I have loaded the project knowledge.".to_string(),
                );
            }
        }
    }

    // Inject skill catalog
    let discovered_skills = skills::discover_skills(Some(&project.root));
    if !discovered_skills.is_empty() {
        let catalog = skills::format_skill_catalog(&discovered_skills);
        conv.add_user(catalog);
        conv.add_assistant("Acknowledged. I see the available skills.".to_string());
    }
}

/// Set up a fresh conversation with project context, memory, skills, and task.
fn setup_fresh_conversation(conv: &mut Conversation, project: &Project, task: &str) {
    setup_fresh_conversation_no_task(conv, project);
    conv.add_user(task.to_string());
}

/// Set up a fresh conversation with project context, memory, skills, task, and
/// optional user-attached images.  When images are present, they are added to
/// the same user message as the task so the model sees them as inline context.
fn setup_fresh_conversation_with_attachments(
    conv: &mut Conversation,
    project: &Project,
    task: &str,
    images: Vec<conversation::ImageData>,
) {
    setup_fresh_conversation_no_task(conv, project);
    if images.is_empty() {
        conv.add_user(task.to_string());
    } else {
        conv.add_user_with_images(task.to_string(), images);
    }
}

/// Resolve `frames:` context hints into HQ images from the frame registry.
async fn resolve_frame_hints(
    hints: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> Vec<conversation::ImageData> {
    let mut images = Vec::new();
    for hint in hints {
        if let Some(frame_list) = hint.strip_prefix("frames:") {
            let reg = registry.read().await;
            for fid in frame_list
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                match reg.read_hq(fid) {
                    Ok(data) => {
                        use base64::Engine;
                        images.push(conversation::ImageData {
                            media_type: "image/jpeg".to_string(),
                            data: base64::engine::general_purpose::STANDARD.encode(&data),
                        });
                    }
                    Err(_) => {
                        // Frame not found — skip silently
                    }
                }
            }
        }
    }
    images
}

/// Resolve explicit frame IDs into HQ images from the frame registry.
async fn resolve_frame_ids(
    frame_ids: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> Vec<conversation::ImageData> {
    if frame_ids.is_empty() {
        return Vec::new();
    }
    let mut images = Vec::new();
    let reg = registry.read().await;
    for fid in frame_ids {
        match reg.read_hq(fid) {
            Ok(data) => {
                use base64::Engine;
                images.push(conversation::ImageData {
                    media_type: "image/jpeg".to_string(),
                    data: base64::engine::general_purpose::STANDARD.encode(&data),
                });
            }
            Err(_) => {
                // Frame not found — skip silently
            }
        }
    }
    images
}

/// Resolve frame IDs into `AgentImageAttachment`s for an external agent.
///
/// Captures the on-disk path so backends like Codex can pass `LocalImage`
/// (file reference) instead of inline base64 in JSON-RPC.
async fn resolve_frame_attachments(
    frame_ids: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> Vec<external_agent::AgentImageAttachment> {
    if frame_ids.is_empty() {
        return Vec::new();
    }
    let mut atts = Vec::new();
    let reg = registry.read().await;
    for fid in frame_ids {
        let Ok(data) = reg.read_hq(fid) else { continue };
        use base64::Engine;
        let base64 = base64::engine::general_purpose::STANDARD.encode(&data);
        let path = reg.path_for(fid);
        atts.push(external_agent::AgentImageAttachment::from_frame_path(
            path,
            base64,
            "image/jpeg".to_string(),
        ));
    }
    atts
}

/// Resolve a mixed list of attachment identifiers (frames from the live
/// frame registry, uploads from the on-disk store) into the unified
/// `AgentAttachment` shape the backends consume.
///
/// Identifier convention:
/// - `"frame:<id>"` or plain `<id>` — a frame registry entry. Plain ids
///   remain supported for backward compatibility with the existing
///   dashboard path that submits frame ids directly.
/// - `"upload:<id>"` — an upload store descriptor. Images load base64
///   inline (for Gemini ACP); files pass through as `AgentAttachment::File`
///   and the backend's default handling prepends a prelude pointing at the
///   on-disk path.
///
/// Order is preserved from the input list so the prelude reads the files
/// in the order the user selected them.
async fn resolve_attachments(
    ids: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    session_dir: &std::path::Path,
    project_root: &std::path::Path,
) -> Vec<external_agent::AgentAttachment> {
    if ids.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<external_agent::AgentAttachment> = Vec::with_capacity(ids.len());
    for raw in ids {
        if let Some(upload_id) = raw.strip_prefix("upload:") {
            let Some(d) = upload_store::find_upload(upload_id, session_dir, project_root) else {
                continue;
            };
            if d.is_image() {
                // Load the bytes eagerly so Gemini ACP can base64-encode
                // inline. Codex prefers the path.
                let (base64, mime) = match std::fs::read(&d.path) {
                    Ok(bytes) => {
                        use base64::Engine;
                        (
                            base64::engine::general_purpose::STANDARD.encode(&bytes),
                            d.mime.clone(),
                        )
                    }
                    Err(_) => continue,
                };
                out.push(external_agent::AgentAttachment::Image(
                    external_agent::AgentImageAttachment::from_frame_path(
                        d.path.clone(),
                        base64,
                        mime,
                    ),
                ));
            } else {
                out.push(external_agent::AgentAttachment::File(
                    external_agent::AgentFileAttachment {
                        local_path: d.path.clone(),
                        name: d.name.clone(),
                        mime_type: d.mime.clone(),
                        size: d.size,
                    },
                ));
            }
            continue;
        }
        // Frame resolution: accept both "frame:<id>" and bare ids for
        // backward compatibility with dashboards that predate the upload
        // feature.
        let fid = raw.strip_prefix("frame:").unwrap_or(raw);
        let (data, path) = {
            let reg = registry.read().await;
            let Ok(data) = reg.read_hq(fid) else {
                continue;
            };
            (data, reg.path_for(fid))
        };
        use base64::Engine;
        let base64 = base64::engine::general_purpose::STANDARD.encode(&data);
        out.push(external_agent::AgentAttachment::Image(
            external_agent::AgentImageAttachment::from_frame_path(
                path,
                base64,
                "image/jpeg".to_string(),
            ),
        ));
    }
    out
}

/// Auto-attach the latest display frame(s) from the frame registry.
async fn auto_attach_display_frames(
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> Vec<conversation::ImageData> {
    let reg = registry.read().await;
    let mut images = Vec::new();
    for stream in reg.active_streams() {
        if stream.starts_with("display_") {
            if let Some(frame_id) = reg.latest(Some(&stream)) {
                if let Ok(data) = reg.read_hq(frame_id) {
                    use base64::Engine;
                    images.push(conversation::ImageData {
                        media_type: "image/jpeg".to_string(),
                        data: base64::engine::general_purpose::STANDARD.encode(&data),
                    });
                }
            }
        }
    }
    images
}

/// Take a fresh screenshot of the user's display for CU-first routing.
/// Tries DisplaySession first (works on Wayland), falls back to platform tools.
async fn capture_display_screenshot(
    log_dir: &std::path::Path,
    session_registry: &display::SharedSessionRegistry,
) -> Option<conversation::ImageData> {
    // Try DisplaySession first — works on Wayland and any display with a session
    if let Some(session) = session_registry.read().await.get(0) {
        if let Ok(png_bytes) = session.screenshot().await {
            let screenshot_path = log_dir.join("cu_reference.png");
            std::fs::write(&screenshot_path, &png_bytes).ok()?;
            use base64::Engine;
            return Some(conversation::ImageData {
                media_type: "image/png".to_string(),
                data: base64::engine::general_purpose::STANDARD.encode(&png_bytes),
            });
        }
    }

    // Fallback: platform-native screenshot tools
    let screenshot_path = log_dir.join("cu_reference.png");
    let ok = if cfg!(target_os = "macos") {
        tokio::process::Command::new("screencapture")
            .args(["-x", &screenshot_path.to_string_lossy()])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
        tokio::process::Command::new("import")
            .args([
                "-window",
                "root",
                "-display",
                &display,
                &screenshot_path.to_string_lossy(),
            ])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !ok {
        return None;
    }
    let data = std::fs::read(&screenshot_path).ok()?;
    use base64::Engine;
    Some(conversation::ImageData {
        media_type: "image/png".to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(&data),
    })
}

// Try the CU-first path: send task to the fast CU model.
/// Returns None if CU is not available (no display, no provider).
#[allow(clippy::too_many_arguments)]
async fn try_cu_first(
    project_root: &std::path::Path,
    reference_images: &[conversation::ImageData],
    frame_images: &[conversation::ImageData],
    task: &str,
    session_log: &SharedSessionLog,
    log_dir: &std::path::Path,
    bus: &event::EventBus,
    session_registry: &display::SharedSessionRegistry,
) -> Option<Result<CuTaskResult, CallerError>> {
    slog(session_log, |l| {
        l.info(&format!(
            "try_cu_first: ref_images={}, frame_images={}, task={}",
            reference_images.len(),
            frame_images.len(),
            &task[..task.len().min(60)]
        ))
    });

    let reference_images = if reference_images.is_empty() {
        // No frames from browser streaming — try a fresh screenshot if user display
        // is granted, so CU-first can work without the Stream button being active.
        if std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_ok() {
            slog(session_log, |l| {
                l.info("try_cu_first: no registry frames, taking fresh screenshot")
            });
            match capture_display_screenshot(log_dir, session_registry).await {
                Some(img) => vec![img],
                None => {
                    slog(session_log, |l| {
                        l.info("try_cu_first: fresh screenshot failed, returning None")
                    });
                    return None;
                }
            }
        } else {
            slog(session_log, |l| {
                l.info("try_cu_first: no display images and no display grant, returning None")
            });
            return None;
        }
    } else {
        reference_images.to_vec()
    };

    let proj = Project::from_root(project_root.to_path_buf()).ok()?;
    let mut cu_provider = match provider::select_cu_provider(&proj.config.computer_use) {
        Ok(p) => {
            if !p.cu_enabled() {
                slog(session_log, |l| {
                    l.warn("CU provider selected but cu_enabled=false, skipping CU-first")
                });
                return None;
            }
            p
        }
        Err(_) => return None,
    };

    // Override cu_display with the actual display dimensions. The default
    // from select_cu_provider is sized for virtual displays (e.g. 768x1024).
    // On macOS or when targeting the user's real display, the actual resolution
    // may differ (e.g. 1512x949), causing coordinate mismatches.
    if std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_ok() {
        let display_id = std::env::var("DISPLAY")
            .ok()
            .and_then(|d| d.trim_start_matches(':').parse::<u32>().ok())
            .unwrap_or(0);
        let (w, h) = query_display_resolution(display_id);
        if w > 0 && h > 0 {
            slog(session_log, |l| {
                l.info(&format!(
                    "CU display override: {}x{} (actual user display)",
                    w, h
                ))
            });
            cu_provider.set_cu_display((w, h));
        }
    }

    slog(session_log, |l| {
        l.info(&format!(
            "CU-first: {} (provider: {}, model: {})",
            &task[..task.len().min(80)],
            cu_provider.name(),
            cu_provider.model()
        ))
    });
    bus.send(event::AppEvent::PresenceLog {
        message: format!("Trying CU: {}", &task[..task.len().min(80)]),
        level: None,
        turn: None,
    });

    Some(
        run_cu_task(
            cu_provider.as_ref(),
            task,
            reference_images.to_vec(),
            frame_images.to_vec(),
            session_log,
            log_dir,
            bus,
            &proj.config.computer_use,
            None, // auto-resolve display target
        )
        .await,
    )
}

/// Spawn a listener that reacts to display grant/revoke events.
/// On grant: create a DisplaySession (Wayland) and emit DisplayReady.
/// On revoke: stop the session and remove it from the registry.
pub fn spawn_user_display_listener(
    bus: EventBus,
    session_registry: display::SharedSessionRegistry,
    frame_registry: Option<std::sync::Arc<tokio::sync::RwLock<frames::FrameRegistry>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(AppEvent::UserDisplayGranted { display_id }) => {
                    activate_user_display(
                        &bus,
                        &session_registry,
                        frame_registry.clone(),
                        display_id,
                    )
                    .await;
                }
                Ok(AppEvent::UserDisplayRevoked { display_id, .. }) => {
                    deactivate_user_display(&session_registry, display_id).await;
                }
                Ok(AppEvent::DisplayCaptureLost {
                    display_id,
                    ref reason,
                }) => {
                    // Capture backend stopped unexpectedly (portal session
                    // ended, backend crashed, etc.).  Remove the session from
                    // the registry so a re-grant creates a fresh one.
                    eprintln!(
                        "[user_display] Capture lost for display {}: {}",
                        display_id, reason,
                    );
                    if let Some(session) = session_registry.write().await.remove(display_id) {
                        session.stop().await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                _ => {}
            }
        }
    })
}

/// Tear down a user display session on revoke.
///
/// Registry removal is the only part that has to complete before the
/// caller returns — once the session is out of the registry, no new
/// offer can find it. `session.stop()` then tears down the capture,
/// encoder, and clipboard tasks, which can take many seconds (each
/// awaits a thread join). We run that in the background so the
/// caller — `spawn_user_display_listener`'s `rx.recv()` loop — can
/// pick up the next event (e.g. a follow-up `UserDisplayGranted`
/// from a user who toggled off and back on) without waiting for the
/// old session's threads to exit. Before this, a toggle-off-then-on
/// cycle serialized behind `session.stop().await` — "turn on, wait
/// 20+s, turn on is instant" mapped exactly to "the old stop finally
/// finished and the listener got to the new grant".
async fn deactivate_user_display(
    session_registry: &display::SharedSessionRegistry,
    display_id: u32,
) {
    if let Some(session) = session_registry.write().await.remove(display_id) {
        eprintln!(
            "[user_display] Stopping display session for :{}",
            display_id
        );
        tokio::spawn(async move {
            session.stop().await;
        });
    }
}

/// Handle user display grant: create a `DisplaySession` and emit
/// `DisplayReady` for the selected user display.
///
/// `target_display_id` is the intendant-stable display ID (0 = primary).
/// This wires the user's display into the same lifecycle as virtual displays —
/// the recording listener starts ffmpeg and the web dashboard shows a display slot.
async fn activate_user_display(
    bus: &EventBus,
    session_registry: &display::SharedSessionRegistry,
    frame_registry: Option<std::sync::Arc<tokio::sync::RwLock<frames::FrameRegistry>>>,
    target_display_id: u32,
) {
    let display_id: u32 = target_display_id;
    let (width, height) = query_display_resolution(display_id);

    // On Wayland: create a DisplaySession with WaylandBackend.
    // Detect Wayland even when WAYLAND_DISPLAY isn't in our env (e.g. started
    // from a tty/ssh session while a graphical session is active).
    #[cfg(target_os = "linux")]
    if std::env::var("WAYLAND_DISPLAY").is_ok() || detect_wayland_socket().is_some() {
        if let Some(socket) = detect_wayland_socket() {
            if std::env::var("WAYLAND_DISPLAY").is_err() {
                eprintln!(
                    "[user_display] WAYLAND_DISPLAY not set, detected socket: {}",
                    socket
                );
                std::env::set_var("WAYLAND_DISPLAY", &socket);
            }
            if std::env::var("XDG_RUNTIME_DIR").is_err() {
                let uid = unsafe { libc::getuid() };
                let runtime_dir = format!("/run/user/{}", uid);
                std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);
            }
        }
        eprintln!("[user_display] Requesting Wayland screen capture via XDG portal...");
        eprintln!(
            "[user_display] A screen-sharing dialog should appear on the display — \
             approve it to enable video capture"
        );
        let backend = display::wayland::WaylandBackend::new();
        let session = display::DisplaySession::new(display_id, Arc::new(backend));
        // The portal dialog requires user interaction on the physical display.
        // If the user is accessing intendant remotely (web dashboard, SSH) they
        // may never see the dialog, so emit a status event for the dashboard to
        // surface a banner — and apply a generous timeout to avoid hanging
        // forever, falling through to X11 capture if the user never approves.
        bus.send(AppEvent::DisplayApprovalPending {
            display_id,
            backend: "wayland",
        });
        const WAYLAND_PORTAL_APPROVAL_TIMEOUT_SECS: u64 = 300;
        match tokio::time::timeout(
            std::time::Duration::from_secs(WAYLAND_PORTAL_APPROVAL_TIMEOUT_SECS),
            session.start(30, frame_registry.clone(), Some(bus.clone())),
        )
        .await
        {
            Ok(Ok(())) => {
                // Use the backend's resolution (from portal), not xdpyinfo.
                let (width, height) = session.resolution();
                let session = Arc::new(session);
                session.spawn_metrics_logger(Some(bus.clone()));
                session_registry.write().await.insert(display_id, session);
                bus.send(AppEvent::DisplayReady {
                    display_id,
                    width,
                    height,
                });
                return;
            }
            Ok(Err(e)) => {
                eprintln!("[user_display] Wayland display session failed: {}", e);
            }
            Err(_) => {
                eprintln!(
                    "[user_display] Wayland portal timed out after \
                     {WAYLAND_PORTAL_APPROVAL_TIMEOUT_SECS}s \
                     (screen-sharing dialog was not approved), trying X11"
                );
            }
        }
    }

    // X11: detect display and create a DisplaySession with X11Backend.
    #[cfg(target_os = "linux")]
    {
        let has_x11 = std::env::var("DISPLAY").is_ok() || vision::detect_x11_display().is_some();
        if has_x11 {
            // Ensure DISPLAY is set for downstream tools (xdotool, import, etc.)
            if std::env::var("DISPLAY").is_err() {
                if let Some(d) = vision::detect_x11_display() {
                    std::env::set_var("DISPLAY", &d);
                }
            }
            // If a specific display was requested, look it up from xrandr
            // enumeration and use X11Backend::with_display() for the
            // matching X display string (e.g. ":0", ":1").
            let backend = if target_display_id != 0 {
                let displays = display::enumerate_displays().await;
                if let Some(info) = displays.iter().find(|d| d.id == target_display_id) {
                    eprintln!(
                        "[user_display] X11: requested display_id={}, matched '{}'",
                        target_display_id, info.name,
                    );
                    // X11 monitors share the same DISPLAY string -- the
                    // root window spans all monitors.  The enumerated
                    // displays from xrandr are sub-regions of the same
                    // root.  We still create a standard backend capturing
                    // the root window; the per-monitor distinction is used
                    // for coordinate mapping in the CU layer.
                    display::x11::X11Backend::new()
                        .map_err(|e| eprintln!("[user_display] X11 backend failed: {}", e))
                } else {
                    eprintln!(
                        "[user_display] X11: display_id={} not found, falling back to default",
                        target_display_id,
                    );
                    display::x11::X11Backend::new()
                        .map_err(|e| eprintln!("[user_display] X11 backend failed: {}", e))
                }
            } else {
                display::x11::X11Backend::new()
                    .map_err(|e| eprintln!("[user_display] X11 backend failed: {}", e))
            };
            if let Ok(backend) = backend {
                let session = display::DisplaySession::new(display_id, Arc::new(backend));
                if let Err(e) = session
                    .start(30, frame_registry.clone(), Some(bus.clone()))
                    .await
                {
                    eprintln!("[user_display] X11 display session failed: {}", e);
                } else {
                    let (width, height) = session.resolution();
                    let session = Arc::new(session);
                    session.spawn_metrics_logger(Some(bus.clone()));
                    session_registry.write().await.insert(display_id, session);
                    bus.send(AppEvent::DisplayReady {
                        display_id,
                        width,
                        height,
                    });
                    return;
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        // If a specific display was requested, resolve its platform_id (CGDisplayID)
        // from the enumerated list; otherwise use the default (first available).
        let backend = if target_display_id != 0 {
            let displays = display::enumerate_displays().await;
            if let Some(info) = displays.iter().find(|d| d.id == target_display_id) {
                display::macos::MacOSBackend::with_display_id(info.platform_id as u32)
            } else {
                eprintln!(
                    "[user_display] display_id {} not found, falling back to primary",
                    target_display_id
                );
                display::macos::MacOSBackend::new()
            }
        } else {
            display::macos::MacOSBackend::new()
        };
        let session = display::DisplaySession::new(display_id, Arc::new(backend));
        if let Err(e) = session.start(30, frame_registry, Some(bus.clone())).await {
            eprintln!("[user_display] macOS display session failed: {}", e);
        } else {
            let (width, height) = session.resolution();
            let session = Arc::new(session);
            session.spawn_metrics_logger(Some(bus.clone()));
            session_registry.write().await.insert(display_id, session);
            bus.send(AppEvent::DisplayReady {
                display_id,
                width,
                height,
            });
            return;
        }
    }

    #[cfg(target_os = "windows")]
    {
        // If a specific display was requested, resolve its platform_id (DXGI
        // output ordinal) from the enumerated list; otherwise capture the
        // primary output. Mirrors the macOS arm.
        let backend = if target_display_id != 0 {
            let displays = display::enumerate_displays().await;
            if let Some(info) = displays.iter().find(|d| d.id == target_display_id) {
                display::windows::WindowsBackend::with_output_index(info.platform_id as u32)
            } else {
                eprintln!(
                    "[user_display] display_id {} not found, falling back to primary",
                    target_display_id
                );
                display::windows::WindowsBackend::new()
            }
        } else {
            display::windows::WindowsBackend::new()
        };
        let session = display::DisplaySession::new(display_id, Arc::new(backend));
        if let Err(e) = session.start(30, frame_registry, Some(bus.clone())).await {
            eprintln!("[user_display] Windows display session failed: {}", e);
        } else {
            let (width, height) = session.resolution();
            let session = Arc::new(session);
            session.spawn_metrics_logger(Some(bus.clone()));
            session_registry.write().await.insert(display_id, session);
            bus.send(AppEvent::DisplayReady {
                display_id,
                width,
                height,
            });
            return;
        }
    }

    #[allow(unreachable_code)]
    {
        eprintln!("[user_display] No supported display backend detected");
    }
}

/// Auto-register the Windows desktop as an active display at web-daemon
/// startup, so the dashboard's Video tab streams it on connect — no grant
/// click and no running agent required.
///
/// On macOS and Linux the screen is shared behind a consent gate (TCC, the
/// Wayland portal dialog) or a virtual display is launched on demand, so
/// those platforms keep activating the user display only on an explicit
/// grant. Windows has no such per-session consent step: in the headless /
/// RDP server scenario the existing desktop *is* the always-on stream, and
/// the OS-level capture permission is implicit. We therefore mirror the
/// macOS *end state* (a live `DisplaySession` already in the registry, so a
/// fresh browser connect replays `display_ready` and auto-streams) by
/// activating display 0 up front, reusing the same [`activate_user_display`]
/// machinery — which on Windows captures the existing desktop via
/// `WindowsBackend` (DXGI Desktop Duplication), NOT a virtual Xvfb display.
///
/// The autonomy grant flag and `INTENDANT_USER_DISPLAY_GRANTED` env are set
/// to match a real grant, so the dashboard's "your display" toggle, CU
/// display targeting, and agent subprocesses all observe a consistent
/// "granted" state. Activation degrades gracefully — if the capture backend
/// can't start (no interactive desktop, etc.) `activate_user_display` logs
/// and returns without registering, leaving the dashboard at "No displays
/// active" rather than failing startup.
#[cfg(target_os = "windows")]
async fn auto_activate_windows_user_display(
    bus: &EventBus,
    session_registry: &display::SharedSessionRegistry,
    frame_registry: Option<std::sync::Arc<tokio::sync::RwLock<frames::FrameRegistry>>>,
    autonomy: &SharedAutonomy,
) {
    eprintln!("[user_display] Windows: auto-registering desktop as active display (display 0)");
    {
        let mut guard = autonomy.write().await;
        guard.user_display_granted = true;
    }
    std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
    activate_user_display(bus, session_registry, frame_registry, 0).await;
}

/// Detect a Wayland compositor socket even when WAYLAND_DISPLAY is not set.
/// Checks /run/user/<uid>/ for wayland-* sockets.
#[cfg(target_os = "linux")]
fn detect_wayland_socket() -> Option<String> {
    let uid = unsafe { libc::getuid() };
    let runtime_dir = format!("/run/user/{}", uid);
    let entries = std::fs::read_dir(&runtime_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Match "wayland-0", "wayland-1", etc. but not ".lock" files
        if name.starts_with("wayland-") && !name.ends_with(".lock") {
            if entry.file_type().ok().is_some_and(|ft| {
                use std::os::unix::fs::FileTypeExt;
                ft.is_socket() || ft.is_file()
            }) {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Parse a display target string from the presence model into a `DisplayTarget`.
///
/// Accepts "user_session" for the user's display, or ":<N>" / "<N>" for virtual.
fn parse_display_target_str(s: &str) -> computer_use::DisplayTarget {
    match s.trim() {
        "user_session" | "user" | ":0" | "0" => computer_use::DisplayTarget::UserSession,
        other => {
            let num_str = other.trim_start_matches(':');
            if let Ok(id) = num_str.parse::<u32>() {
                if id == 0 {
                    computer_use::DisplayTarget::UserSession
                } else {
                    computer_use::DisplayTarget::Virtual { id }
                }
            } else {
                // Unrecognized — fall back to auto-resolve
                resolve_cu_display_target()
            }
        }
    }
}

/// Resolve the display target for CU actions.
///
/// If user display access is granted (env var set) and the current DISPLAY
/// is `:0` (or unset, indicating no virtual display was launched), returns
/// `UserSession`. Otherwise returns `Virtual` with the current display ID.
/// On macOS, always returns `UserSession` when DISPLAY is unset (no Xvfb).
fn resolve_cu_display_target() -> computer_use::DisplayTarget {
    let display_id: Option<u32> = std::env::var("DISPLAY")
        .ok()
        .and_then(|d| d.trim_start_matches(':').parse().ok());

    let user_granted = std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_ok();

    match display_id {
        // DISPLAY is :0 and user granted → target user session
        Some(0) if user_granted => computer_use::DisplayTarget::UserSession,
        // DISPLAY is set to a virtual display
        Some(id) => computer_use::DisplayTarget::Virtual { id },
        // No DISPLAY set — if user granted, target their session; else default virtual
        None if user_granted => computer_use::DisplayTarget::UserSession,
        // macOS has no Xvfb — native display is always the target
        None if cfg!(target_os = "macos") => computer_use::DisplayTarget::UserSession,
        None => computer_use::DisplayTarget::Virtual { id: 99 },
    }
}

/// Maximum turns for an ephemeral CU task before giving up.
const CU_TASK_MAX_TURNS: usize = 20;

/// Result of an ephemeral CU task.
enum CuTaskResult {
    /// Task completed by the CU agent.
    Completed(LoopStats),
    /// CU agent determined this isn't a display task; escalate to the full agent.
    Escalate { task: String },
}

/// Run an ephemeral computer-use task with minimal context.
///
/// Creates a lightweight conversation (no project context, skills, or knowledge),
/// runs the CU model for a few turns until the task is done, and returns.
#[allow(clippy::too_many_arguments)]
async fn run_cu_task(
    provider: &dyn provider::ChatProvider,
    task: &str,
    reference_images: Vec<conversation::ImageData>,
    context_images: Vec<conversation::ImageData>,
    session_log: &SharedSessionLog,
    log_dir: &std::path::Path,
    bus: &event::EventBus,
    cu_config: &project::ComputerUseConfig,
    target_override: Option<computer_use::DisplayTarget>,
) -> Result<CuTaskResult, CallerError> {
    let mut stats = LoopStats::default();
    let mut cu_counter = 0u64;
    let backend = computer_use::DisplayBackend::from_config(&cu_config.backend);

    let display_target = target_override.unwrap_or_else(resolve_cu_display_target);

    // CU-first system prompt: handle display tasks or escalate
    let system_prompt =
        "You are a fast computer-use agent. You can see and interact with a desktop display.\n\n\
        ROUTING:\n\
        - If the task involves the display (clicking, typing, scrolling, pressing buttons, \
          opening apps, interacting with GUI elements), handle it with your computer use tools.\n\
        - If the task is NOT about the display (coding, file editing, research, shell commands, \
          git, debugging, questions), call escalate_to_agent with the task description.\n\
        - If no display screenshot is provided below, call escalate_to_agent immediately.\n\n\
        WHEN HANDLING DISPLAY TASKS:\n\
        1. Examine the screenshot to identify target elements\n\
        2. Perform the required actions\n\
        3. Take a verification screenshot\n\
        4. Respond with DONE and a one-sentence summary\n\n\
        RULES:\n\
        - Perform ONLY the requested task, nothing else.\n\
        - Once done, STOP. Do not take additional actions.\n\
        - Be precise with coordinates. Act efficiently."
            .to_string();

    // No display frames at all → escalate immediately without API call
    if reference_images.is_empty() && context_images.is_empty() {
        slog(session_log, |l| {
            l.info("CU: no display frames available, escalating")
        });
        return Ok(CuTaskResult::Escalate {
            task: task.to_string(),
        });
    }

    let ref_image_count = reference_images.len();
    let mut conv = Conversation::new(system_prompt, provider.context_window());

    // Inject reference frames
    if !reference_images.is_empty() {
        conv.add_user_with_images(
            "The user was looking at this screen when they made their request:".to_string(),
            reference_images,
        );
        conv.add_assistant(
            "I can see the reference screen. I'll compare this with the current state.".to_string(),
        );
    }

    // Inject context images
    if !context_images.is_empty() {
        conv.add_user_with_images("Additional context:".to_string(), context_images);
        conv.add_assistant("Noted.".to_string());
    }

    // Add the task
    conv.add_user(task.to_string());

    slog(session_log, |l| {
        l.cu_task_start(
            task,
            provider.name(),
            provider.model(),
            provider.cu_enabled(),
            provider.cu_display(),
            ref_image_count,
        )
    });

    for turn in 1..=CU_TASK_MAX_TURNS {
        stats.turns = turn;

        slog(session_log, |l| {
            l.info(&format!("CU turn {} starting", turn))
        });

        let response = provider
            .chat_stream(conv.messages(), &|event| {
                if let provider::StreamEvent::Delta(ref delta) = event {
                    bus.send(AppEvent::PresenceLog {
                        message: format!("[CU] {}", delta),
                        level: None,
                        turn: Some(turn),
                    });
                }
            })
            .await?;

        conv.set_usage(response.usage.clone());

        // Log structured CU turn
        {
            let mut actions_desc: Vec<String> = response
                .cu_calls
                .iter()
                .flat_map(|cu| cu.actions.iter().map(|a| format!("{:?}", a)))
                .collect();
            for tc in &response.tool_calls {
                actions_desc.push(format!(
                    "{}({})",
                    tc.name,
                    &tc.arguments[..tc.arguments.len().min(100)]
                ));
            }
            slog(session_log, |l| {
                l.cu_turn(
                    turn,
                    response.content.len(),
                    response.cu_calls.len(),
                    response.tool_calls.len(),
                    response.usage.prompt_tokens,
                    response.usage.completion_tokens,
                    &actions_desc,
                )
            });
        }
        if !response.content.is_empty() {
            slog(session_log, |l| {
                l.info(&format!(
                    "CU turn {} text: {}",
                    turn,
                    &response.content[..response.content.len().min(500)]
                ))
            });
        }
        // Check for escalation before processing anything else
        if let Some(esc_call) = response
            .tool_calls
            .iter()
            .find(|tc| tc.name == "escalate_to_agent")
        {
            let args: serde_json::Value =
                serde_json::from_str(&esc_call.arguments).unwrap_or_default();
            let escalated_task = args["task"].as_str().unwrap_or(task).to_string();
            slog(session_log, |l| {
                l.cu_task_error("escalated", Some(&escalated_task))
            });
            return Ok(CuTaskResult::Escalate {
                task: escalated_task,
            });
        }

        // Handle unrecognized function tool calls: return error results so the
        // model knows these tools are not available in CU mode.
        let non_escalate_tools: Vec<_> = response
            .tool_calls
            .iter()
            .filter(|tc| tc.name != "escalate_to_agent")
            .collect();
        if !non_escalate_tools.is_empty() {
            let refs: Vec<conversation::ToolCallRef> = non_escalate_tools
                .iter()
                .map(|tc| conversation::ToolCallRef {
                    id: tc.id.clone(),
                    call_id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                })
                .collect();
            conv.add_assistant_tool_calls(
                response.content.clone(),
                refs,
                response.raw_output.clone(),
            );
            for tc in &non_escalate_tools {
                slog(session_log, |l| {
                    l.warn(&format!(
                        "CU turn {}: unrecognized tool '{}' — returning error result",
                        turn, tc.name
                    ))
                });
                conv.add_tool_result(
                    &tc.id,
                    &tc.name,
                    &format!(
                        "Error: tool '{}' is not available in computer-use mode. \
                         Use your native computer use actions (click, type, scroll, screenshot) \
                         or call escalate_to_agent to hand off to the coding agent.",
                        tc.name
                    ),
                );
            }
            continue; // let model see the error results
        }

        // Check for task completion
        let content_lower = response.content.to_lowercase();
        let is_done = content_lower.contains("done")
            && response.cu_calls.is_empty()
            && response.tool_calls.is_empty();

        // Store assistant message
        if !response.cu_calls.is_empty() {
            // CU calls: store as assistant with tool call refs
            let refs: Vec<conversation::ToolCallRef> = response
                .cu_calls
                .iter()
                .map(|cu| conversation::ToolCallRef {
                    id: cu.call_id.clone(),
                    call_id: cu.call_id.clone(),
                    name: "computer".to_string(),
                    arguments: String::new(),
                })
                .collect();
            conv.add_assistant_tool_calls(
                response.content.clone(),
                refs,
                response.raw_output.clone(),
            );
        } else {
            conv.add_assistant(response.content.clone());
        }

        if is_done {
            let summary = &response.content[..response.content.len().min(200)];
            slog(session_log, |l| l.cu_task_complete(turn, true, summary));
            break;
        }

        // Execute CU calls
        if !response.cu_calls.is_empty() {
            for cu_call in &response.cu_calls {
                slog(session_log, |l| {
                    l.info(&format!(
                        "CU turn {}: {} action(s)",
                        turn,
                        cu_call.actions.len()
                    ))
                });

                let results = computer_use::execute_actions(
                    &cu_call.actions,
                    display_target,
                    backend,
                    log_dir,
                    &mut cu_counter,
                    &None,
                    None,
                )
                .await;

                let last_screenshot = results.iter().rev().find_map(|r| r.screenshot.as_ref());
                let output = if results.iter().all(|r| r.success) {
                    "Actions executed successfully.".to_string()
                } else {
                    let errors: Vec<&str> =
                        results.iter().filter_map(|r| r.error.as_deref()).collect();
                    format!("Some actions failed: {}", errors.join("; "))
                };

                if let Some(screenshot) = last_screenshot {
                    let images = vec![conversation::ImageData {
                        media_type: "image/png".to_string(),
                        data: screenshot.base64_png.clone(),
                    }];
                    conv.add_cu_result(&cu_call.call_id, &output, images);
                } else {
                    conv.add_cu_result(&cu_call.call_id, &output, vec![]);
                }
            }
            continue; // next turn — let model see the results
        }

        // No CU calls and not done — model may be thinking or confused
        if response.cu_calls.is_empty() && response.tool_calls.is_empty() && !is_done {
            slog(session_log, |l| {
                l.cu_task_error(
                    &format!("CU turn {}: no actions returned (text-only response)", turn),
                    None,
                )
            });
        }
        if turn >= CU_TASK_MAX_TURNS {
            slog(session_log, |l| {
                l.cu_task_error("CU task hit max turns", None)
            });
        }
    }

    Ok(CuTaskResult::Completed(stats))
}

/// Execute native computer-use tool calls via the xdotool executor
/// and add results (with screenshots) to the conversation.
#[allow(clippy::too_many_arguments)]
async fn execute_cu_calls(
    cu_calls: &[computer_use::CuToolCall],
    conversation: &mut conversation::Conversation,
    cu_display: Option<(u32, u32)>,
    log_dir: &std::path::Path,
    counter: &mut u64,
    session_log: &SharedSessionLog,
) {
    let display_target = if cu_display.is_some() {
        resolve_cu_display_target()
    } else {
        // No CU display configured — default to virtual :99
        computer_use::DisplayTarget::Virtual { id: 99 }
    };

    for cu_call in cu_calls {
        // Build human-readable description of CU actions
        let action_descs: Vec<String> = cu_call
            .actions
            .iter()
            .map(|a| match a {
                computer_use::CuAction::Click { x, y, button } => {
                    format!("click({},{} {:?})", x, y, button)
                }
                computer_use::CuAction::DoubleClick { x, y, .. } => {
                    format!("double_click({},{})", x, y)
                }
                computer_use::CuAction::Type { text } => {
                    format!("type(\"{}\")", &text[..text.len().min(50)])
                }
                computer_use::CuAction::Key { key } => format!("key({})", key),
                computer_use::CuAction::Scroll {
                    x,
                    y,
                    direction,
                    amount,
                } => format!("scroll({},{} {:?} {})", x, y, direction, amount),
                computer_use::CuAction::MoveMouse { x, y } => format!("move({},{})", x, y),
                computer_use::CuAction::Drag {
                    start_x,
                    start_y,
                    end_x,
                    end_y,
                } => format!("drag({},{}->{},{})", start_x, start_y, end_x, end_y),
                computer_use::CuAction::Screenshot => "screenshot".to_string(),
                computer_use::CuAction::Wait { ms } => format!("wait({}ms)", ms),
            })
            .collect();
        let desc = action_descs.join(" → ");
        slog(session_log, |l| l.info(&format!("CU: {}", desc)));

        let backend = computer_use::DisplayBackend::detect();
        let results = computer_use::execute_actions(
            &cu_call.actions,
            display_target,
            backend,
            log_dir,
            counter,
            &None,
            None,
        )
        .await;

        // Find the last screenshot from results
        let last_screenshot = results.iter().rev().find_map(|r| r.screenshot.as_ref());
        let output = if results.iter().all(|r| r.success) {
            "Actions executed successfully.".to_string()
        } else {
            let errors: Vec<&str> = results.iter().filter_map(|r| r.error.as_deref()).collect();
            format!("Some actions failed: {}", errors.join("; "))
        };

        if let Some(screenshot) = last_screenshot {
            let images = vec![conversation::ImageData {
                media_type: "image/png".to_string(),
                data: screenshot.base64_png.clone(),
            }];
            conversation.add_cu_result(&cu_call.call_id, &output, images);
        } else {
            conversation.add_cu_result(&cu_call.call_id, &output, vec![]);
        }
    }
}

fn is_simple_task(task: &str) -> bool {
    // A simple task is a single line with no complex indicators
    let lines: Vec<&str> = task.lines().collect();
    if lines.len() > 3 {
        return false;
    }

    let lower = task.to_lowercase();
    let complex_indicators = [
        "research",
        "investigate",
        "implement",
        "build",
        "refactor",
        "migrate",
        "deploy",
        "set up",
        "analyze",
        "compare",
        "design",
        "create a",
    ];

    for indicator in &complex_indicators {
        if lower.contains(indicator) {
            return false;
        }
    }

    // Short tasks are simple
    task.len() < 100
}

fn configure_sandbox_env(flags: &CliFlags, project: &Project, log_dir: &std::path::Path) {
    let enabled = flags.sandbox || project.config.sandbox.enabled;
    if !enabled {
        env::remove_var("INTENDANT_SANDBOX_WRITE_PATHS");
        return;
    }

    let mut sandbox_cfg = sandbox::SandboxConfig::default_for_project(&project.root, log_dir);
    for p in &project.config.sandbox.extra_write_paths {
        let extra = if std::path::Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            project.root.join(p)
        };
        sandbox_cfg.write_paths.push(extra);
    }
    sandbox_cfg.write_paths.sort();
    sandbox_cfg.write_paths.dedup();

    let write_paths: Vec<String> = sandbox_cfg
        .write_paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    env::set_var("INTENDANT_SANDBOX_WRITE_PATHS", write_paths.join(":"));
}

#[tokio::main]
async fn main() -> Result<(), CallerError> {
    // Install the process-wide rustls `CryptoProvider`. **Required
    // by rustls 0.23+**: without this, the first DTLS handshake
    // (typically when the WebRTC driver answers a federated peer's
    // offer — see `display::webrtc::driver`) panics with
    //   "Could not automatically determine the process-level
    //    CryptoProvider from Rustls crate features."
    // The panic kills the worker thread, the in-flight encoder is
    // torn down, and every subsequent offer also panics. Tests
    // call this via the `ensure_rustls_crypto_provider` helper in
    // `display::webrtc::tests`; production never installed it,
    // which surfaced during the 4d.3 E2E smoke test.
    //
    // `install_default()` returns `Err(Arc<CryptoProvider>)` if a
    // provider is already installed (idempotent); we ignore that.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Panic hook: handle broken pipe gracefully and persist panic info
    // to the active session's log directory for post-mortem auditing.
    {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Broken pipe from println!/write! — exit cleanly
            let is_broken_pipe = if let Some(s) = info.payload().downcast_ref::<String>() {
                s.contains("Broken pipe")
            } else if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.contains("Broken pipe")
            } else {
                false
            };
            if is_broken_pipe {
                std::process::exit(0);
            }

            // Write panic info to the session log directory if available.
            // This makes panics discoverable by audit agents alongside
            // session.jsonl and transcript files — no need to hunt for
            // app-backend.log or stderr captures.
            if let Some(dir) = PANIC_LOG_DIR.get() {
                let panic_path = dir.join("panic.log");
                let msg = format!(
                    "{}\n\nBacktrace:\n{:?}\n",
                    info,
                    std::backtrace::Backtrace::force_capture(),
                );
                let _ = std::fs::write(&panic_path, &msg);
            }

            default_hook(info);
        }));
    }

    // Ensure platform tool directories (Homebrew etc.) are in PATH.
    platform::ensure_tool_paths();

    // Intercept `intendant lan <action>` before the main runtime setup —
    // the lan subcommand is a pure system-setup path with no project,
    // no .env, no provider selection. The subcommand's cert machinery
    // (OpenSSL + nginx) is deferred on Windows (Tier-0), so there it
    // reports unsupported and exits rather than calling the gated path.
    if env::args().nth(1).as_deref() == Some("lan") {
        #[cfg(not(target_os = "windows"))]
        {
            let argv: Vec<String> = env::args().skip(2).collect();
            return match lan::run(argv).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
        }
        #[cfg(target_os = "windows")]
        {
            eprintln!("error: `intendant lan` is not supported on Windows yet");
            std::process::exit(1);
        }
    }

    // Load .env: cwd (+ parents) first, then project root, then ~/.config/intendant/
    dotenvy::dotenv().ok();
    let mut project = Project::detect()?;
    dotenvy::from_path(project.root.join(".env")).ok();
    if let Some(config_dir) = dirs::config_dir() {
        dotenvy::from_path(config_dir.join("intendant").join(".env")).ok();
    }

    // Override env vars from CLI flags before provider selection
    let flags = parse_cli_flags()?;
    if let Some(ref p) = flags.provider {
        env::set_var("PROVIDER", p);
    }
    if let Some(ref m) = flags.model {
        env::set_var("MODEL_NAME", m);
    }
    // Apply project model config when CLI/env did not override.
    if env::var("MODEL_CONTEXT_WINDOW").is_err() {
        if let Some(ctx) = project.config.model.context_window {
            env::set_var("MODEL_CONTEXT_WINDOW", ctx.to_string());
        }
    }
    if env::var("MAX_OUTPUT_TOKENS").is_err() {
        if let Some(max_out) = project.config.model.max_output_tokens {
            env::set_var("MAX_OUTPUT_TOKENS", max_out.to_string());
        }
    }
    if let Some(max_parallel) = project.config.orchestrator.max_parallel_agents {
        env::set_var("INTENDANT_MAX_PARALLEL_AGENTS", max_parallel.to_string());
    }

    // Create or resume session log.
    let _is_resume = flags.continue_last || flags.resume_id.is_some();
    let log_dir = if let Some(ref session_id) = flags.resume_id {
        // --resume <id>: find a specific session by ID or path
        session_log::SessionLog::find_session_by_id(session_id).ok_or_else(|| {
            CallerError::Config(format!(
                "Resume requested, but session '{}' was not found",
                session_id
            ))
        })?
    } else if flags.continue_last {
        // --continue: find the most recent session for this project
        session_log::SessionLog::find_latest_session(&project.root)
            .map(|(_, dir)| dir)
            .ok_or_else(|| {
                CallerError::Config(
                    "Continue requested, but no existing session was found for this project"
                        .to_string(),
                )
            })?
    } else {
        session_log::SessionLog::resolve_path(flags.log_file.as_deref())
    };
    let session_log: SharedSessionLog = match session_log::SessionLog::open(log_dir.clone()) {
        Ok(log) => {
            eprintln!("Session log: {}/session.jsonl", log.dir().display());
            eprintln!("Session ID: {}", log.session_id());
            // Register session dir for the panic hook
            let _ = PANIC_LOG_DIR.set(log.dir().to_path_buf());
            Arc::new(Mutex::new(log))
        }
        Err(e) => {
            eprintln!(
                "Warning: Could not create session log at {}: {}",
                log_dir.display(),
                e
            );
            // Fallback to /tmp
            let fallback = PathBuf::from("/tmp/intendant_session");
            let log = session_log::SessionLog::open(fallback)
                .map_err(|e| CallerError::Config(format!("Cannot create session log: {}", e)))?;
            eprintln!(
                "Session log (fallback): {}/session.jsonl",
                log.dir().display()
            );
            Arc::new(Mutex::new(log))
        }
    };

    // Tee controller stderr/stdout into {session_dir}/daemon.log so the
    // "Download session report" button in Settings → Debug can include
    // controller-side output (eprintln!, panics, tracing) in the zip
    // alongside session.jsonl and turn files. Skipped when the
    // controller owns the real interactive TTY, because ratatui writes
    // escape sequences to stdout and cannot tolerate a pipe.
    {
        let will_use_web = !flags.no_web && !flags.mcp && !flags.json_output;
        let owns_real_tty = !will_use_web
            && !flags.no_tui
            && !flags.mcp
            && io::stdin().is_terminal()
            && io::stdout().is_terminal();
        if !owns_real_tty {
            let daemon_log_path = log_dir.join("daemon.log");
            if let Err(e) = daemon_log_tee::install(&daemon_log_path) {
                eprintln!(
                    "daemon_log_tee: could not tee stderr/stdout to {}: {}",
                    daemon_log_path.display(),
                    e
                );
            }
        }
    }

    // Create shared frame registry for video frame storage.
    let frame_registry: Arc<tokio::sync::RwLock<frames::FrameRegistry>> = Arc::new(
        tokio::sync::RwLock::new(frames::FrameRegistry::new(&log_dir)),
    );

    // Create recording registry (listener spawned after bus creation in each mode).
    if project.config.recording.enabled && !recording::is_ffmpeg_available() {
        slog(&session_log, |l| {
            l.warn("Recording enabled in intendant.toml but ffmpeg is not installed — recording will be disabled. Install with: sudo apt-get install ffmpeg")
        });
    }
    let recording_registry: Arc<tokio::sync::RwLock<recording::RecordingRegistry>> =
        Arc::new(tokio::sync::RwLock::new(recording::RecordingRegistry::new(
            &log_dir,
            project.config.recording.clone(),
        )));

    // Create shared display session registry (WebRTC display transport).
    let session_registry: display::SharedSessionRegistry =
        Arc::new(tokio::sync::RwLock::new(display::SessionRegistry::new()));

    configure_sandbox_env(&flags, &project, &log_dir);

    // CLI --transcription flag overrides config file setting
    if flags.transcription {
        project.config.transcription.enabled = true;
    }

    // Install signal handler to mark session as interrupted before exit.
    // Rust's Drop trait does not run when the process is killed by a signal,
    // so we need an explicit handler to update session_meta.json. We catch
    // both SIGTERM (external shutdown) and SIGINT (Ctrl+C in terminal or at
    // the `run_daemon_loop` prompt after TUI quit) so the session doesn't
    // linger as `"status": "running"` in ~/.intendant/logs/ forever.
    {
        let signal_session_log = session_log.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigterm =
                    signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
                let mut sigint =
                    signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
                tokio::select! {
                    _ = sigterm.recv() => {}
                    _ = sigint.recv() => {}
                }
                if let Ok(mut log) = signal_session_log.lock() {
                    log.mark_interrupted();
                }
                // Clean up control socket
                control::cleanup();
                // Restore terminal (best-effort) so the shell isn't left in raw mode
                let _ = crossterm::terminal::disable_raw_mode();
                let _ = crossterm::execute!(
                    std::io::stdout(),
                    crossterm::terminal::LeaveAlternateScreen
                );
                std::process::exit(130);
            }
        });
    }

    // Write session metadata (project root, task will be filled in later if available).
    slog(&session_log, |l| {
        l.write_meta(Some(&project.root), None);
    });

    // Web gateway is on by default unless explicitly disabled, or when running
    // in MCP/JSON modes that own stdio.
    let use_web = !flags.no_web && !flags.mcp && !flags.json_output;

    // Resolve web port via auto-discovery, keeping the listener alive (no TOCTOU).
    let (web_port, mut web_listener) = if use_web {
        let (port, listener) = find_available_port(flags.web_port).await?;
        (port, Some(listener))
    } else {
        (flags.web_port, None)
    };
    // Only expose the web port to external agents when the web gateway is actually running.
    let web_port_for_agent: Option<u16> = if use_web { Some(web_port) } else { None };

    // Build the dashboard's TLS acceptor once (cheap to clone into each
    // gateway spawn site). Off unless `--tls` / `[server.tls] enabled`.
    // A misconfiguration (bad cert/key, half-specified pair) fails startup
    // here rather than silently degrading to plain HTTP. The bind address
    // feeds the self-signed cert's SAN list.
    let web_tls_acceptor = if use_web {
        let bind_addr = web_listener.as_ref().and_then(|l| l.local_addr().ok());
        build_web_tls_acceptor(&flags, &project.config.server.tls, bind_addr)?
    } else {
        None
    };
    if web_tls_acceptor.is_some() {
        eprintln!(
            "[web_gateway] TLS enabled — dashboard is HTTPS/WSS-only on port {web_port} \
             (cleartext HTTP/WS connections are refused)"
        );
    }

    let provider_result = provider::select_provider();
    let provider = match provider_result {
        Ok(p) => {
            slog(&session_log, |l| {
                l.debug(&format!("Provider: {}", p.name()));
                l.debug(&format!("Model: {}", p.model()));
            });
            Some(p)
        }
        Err(ref e) if use_web || flags.mcp => {
            // No API keys — start the dashboard anyway.
            // Display control, session browsing, annotations, and clipping
            // all work without inference.
            eprintln!(
                "Warning: {} AI features will be unavailable. \
                 The web dashboard is starting without a model provider.",
                e
            );
            slog(&session_log, |l| {
                l.warn(&format!("No AI provider: {}", e));
            });
            None
        }
        Err(e) => return Err(e),
    };
    slog(&session_log, |l| {
        l.debug(&format!("Project root: {}", project.root.display()));
        l.debug(&format!("Autonomy: {}", flags.autonomy));
    });

    // Check if running as a sub-agent (headless, no TUI)
    if let Some((id, role)) = sub_agent::detect_sub_agent_mode() {
        let provider = provider
            .ok_or_else(|| CallerError::Config("Sub-agent mode requires an API key".to_string()))?;
        run_sub_agent_mode(provider, id, role, session_log, log_dir).await?;
        return Ok(());
    }

    // Determine whether to use TUI (needed early for task resolution).
    // Idle web/dashboard startup defaults to the daemon path: no terminal TUI,
    // and the session supervisor owns all launches. `--no-web` keeps the
    // terminal TUI available for interactive local use.
    let web_daemon_requested = should_start_idle_web_daemon(use_web, &flags);
    let use_tui = !web_daemon_requested
        && (use_web
            || (!flags.no_tui
                && !flags.mcp
                && io::stdin().is_terminal()
                && io::stdout().is_terminal()));

    // Task resolution: MCP and TUI modes allow starting without a task.
    // MCP mode must NOT call get_task_from_flags_or_env() because it would
    // print to stdout and read from stdin, both reserved for JSON-RPC.
    // TUI mode can accept a task later via the follow-up input panel.
    // Headless mode still requires a task upfront.
    let task = if web_daemon_requested {
        None
    } else if flags.mcp {
        flags.task.clone().filter(|t| !t.is_empty())
    } else if use_tui {
        flags.task.clone().filter(|t| !t.is_empty())
    } else {
        let t = get_task_from_flags_or_env(&flags)?;
        if t.is_empty() {
            return Err(CallerError::Config("No task provided".to_string()));
        }
        Some(t)
    };

    if let Some(ref t) = task {
        slog(&session_log, |l| l.info(&format!("Task: {}", t)));
    }

    // Build autonomy state from project config + CLI flags
    let autonomy_state = AutonomyState::new(flags.autonomy, project.config.approval.clone());
    let autonomy = autonomy::shared_autonomy(autonomy_state);

    if web_daemon_requested {
        let bus = EventBus::new();
        let _tick_handle = event::spawn_tick_timer(bus.clone(), 1000);
        let _recording_listener = recording::spawn_recording_listener(
            bus.subscribe(),
            recording_registry.clone(),
            bus.clone(),
            Some(session_registry.clone()),
        );
        let _user_display_listener = spawn_user_display_listener(
            bus.clone(),
            session_registry.clone(),
            Some(frame_registry.clone()),
        );
        // Windows: auto-register the existing desktop as an active display so
        // the dashboard streams it on connect (mirrors the macOS end state of
        // a live session sitting in the registry). macOS/Linux compile this
        // out and keep activating only on an explicit grant.
        #[cfg(target_os = "windows")]
        auto_activate_windows_user_display(
            &bus,
            &session_registry,
            Some(frame_registry.clone()),
            &autonomy,
        )
        .await;
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler = Some(debug::spawn_debug_screen_handler(
            bus.subscribe(),
            project.config.recording.clone(),
            web_port,
            bus.clone(),
        ));

        let (outbound_tx, _) = tokio::sync::broadcast::channel::<String>(256);
        let _outbound_broadcaster =
            event::spawn_outbound_broadcaster(bus.subscribe(), outbound_tx.clone());
        let _session_log_writer =
            event::spawn_session_log_writer(bus.subscribe(), session_log.clone());

        let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) = {
            let snapshot_dir = log_dir.join("file_snapshots");
            match file_watcher::FileWatcher::new(project.root.clone(), snapshot_dir, bus.clone()) {
                Ok(watcher) => {
                    let (fw, wh, rh) = watcher.start_shared();
                    (Some(fw), Some(wh), Some(rh))
                }
                Err(e) => {
                    eprintln!("[file_watcher] Failed to start: {}", e);
                    (None, None, None)
                }
            }
        };

        let transcriber: Option<std::sync::Arc<dyn transcription::Transcriber>> =
            if project.config.transcription.enabled {
                match transcription::WhisperTranscriber::new(&project.config.transcription) {
                    Ok(t) => Some(std::sync::Arc::new(t)),
                    Err(e) => {
                        eprintln!("Transcription init failed: {}", e);
                        None
                    }
                }
            } else {
                None
            };
        let web_config = web_gateway::build_config(
            project.config.presence.live_provider.as_deref(),
            project.config.presence.live_model.as_deref(),
            project.config.transcription.enabled,
            project.config.webrtc.to_ice_config(),
            project.config.webrtc.federation_allow_h264,
        );
        let shared_session = Arc::new(tokio::sync::RwLock::new(web_gateway::ActiveSessionState {
            daemon_session_id: session_log_id(&session_log),
            query_ctx: None,
            frame_registry: Some(frame_registry.clone()),
            session_log: None,
            recording_registry: Some(recording_registry.clone()),
            session_registry: Some(session_registry.clone()),
            snapshot_dir: Some(log_dir.join("file_snapshots")),
            project_root_for_changes: Some(project.root.clone()),
            file_watcher: shared_file_watcher.clone(),
        }));
        let mut mcp_http_state = mcp::McpAppState::new(
            "none".into(),
            "none".into(),
            autonomy.clone(),
            log_dir.clone(),
        );
        mcp_http_state.frame_registry = Some(frame_registry.clone());
        mcp_http_state.session_registry = Some(session_registry.clone());
        mcp_http_state.screenshot_dir = Some(log_dir.join("screenshots"));
        let mcp_http_server = Some(Arc::new(mcp::IntendantServer::new(
            Arc::new(tokio::sync::RwLock::new(mcp_http_state)),
            bus.clone(),
        )));
        let peer_registry = build_and_hydrate_peer_registry(&log_dir, &project.config.peers);
        let advertise_urls = resolve_advertise_urls_from_flags_and_config(&flags, &project);
        let _web_handle = web_gateway::spawn_web_gateway(
            web_listener
                .take()
                .expect("web listener must exist when use_web"),
            bus.clone(),
            outbound_tx.clone(),
            web_config,
            shared_session.clone(),
            transcriber,
            None,
            None,
            Some(project.root.clone()),
            mcp_http_server,
            Some(peer_registry),
            advertise_urls,
            project.config.server.auth.bearer_token.clone(),
            build_local_advertised_auth(
                &project.config.server.auth,
                &lan::backend::select_backend().cert_dir(),
            )?,
            web_tls_acceptor.clone(),
        );
        eprintln!("Web TUI: http://0.0.0.0:{}", web_port);

        let agent_backend =
            resolve_agent_backend_from_config(flags.agent_backend.clone(), &project);
        let shared_external_agent: Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>> =
            Arc::new(tokio::sync::RwLock::new(agent_backend));
        let shared_codex_config: control_plane::SharedCodexConfig = {
            let cfg = &project.config.agent.codex;
            Arc::new(tokio::sync::RwLock::new(
                control_plane::CodexRuntimeConfig {
                    command: cfg.command.clone(),
                    sandbox: project::normalize_sandbox_mode(&cfg.sandbox),
                    approval_policy: project::normalize_approval_policy(&cfg.approval_policy),
                    model: cfg.model.clone(),
                    reasoning_effort: project::normalize_reasoning_effort(
                        cfg.reasoning_effort.as_deref(),
                    ),
                    web_search: cfg.web_search,
                    network_access: cfg.network_access,
                    writable_roots: cfg.writable_roots.clone(),
                },
            ))
        };
        let shared_gemini_config: control_plane::SharedGeminiConfig = {
            let cfg = &project.config.agent.gemini_cli;
            Arc::new(tokio::sync::RwLock::new(
                control_plane::GeminiRuntimeConfig {
                    model: cfg.model.clone(),
                    approval_mode: project::normalize_gemini_approval_mode(&cfg.approval_mode),
                    sandbox: cfg.sandbox,
                    extensions: cfg.extensions.clone(),
                    allowed_mcp_servers: cfg.allowed_mcp_servers.clone(),
                    include_directories: cfg.include_directories.clone(),
                    debug: cfg.debug,
                },
            ))
        };
        let _control_plane_handle = control_plane::spawn(
            bus.subscribe(),
            control_plane::ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: shared_external_agent.clone(),
                codex_config: shared_codex_config.clone(),
                gemini_config: shared_gemini_config.clone(),
                bus: bus.clone(),
                project_root: Some(project.root.clone()),
            },
        );

        session_supervisor::SessionSupervisor::new(session_supervisor::SessionSupervisorConfig {
            bus,
            project_root: project.root.clone(),
            autonomy,
            shared_external_agent,
            shared_codex_config,
            shared_gemini_config,
            frame_registry,
            web_port: web_port_for_agent,
            flags_direct: flags.direct,
            shared_session: Some(shared_session),
        })
        .run()
        .await;
        return Ok(());
    }

    if flags.mcp {
        // MCP mode — speaks Model Context Protocol on stdio.
        // This is architecturally a peer of the TUI: same EventBus, same UserAction contract.
        let bus = EventBus::new();
        let event_rx = bus.subscribe();
        let human_question_path = event::shared_question_path(log_dir.join("human_question"));
        let _human_monitor =
            event::spawn_human_question_monitor(bus.clone(), human_question_path.clone());
        let _tick_handle = event::spawn_tick_timer(bus.clone(), 1000);
        let _recording_listener = recording::spawn_recording_listener(
            bus.subscribe(),
            recording_registry.clone(),
            bus.clone(),
            Some(session_registry.clone()),
        );
        let _user_display_listener = spawn_user_display_listener(
            bus.clone(),
            session_registry.clone(),
            Some(frame_registry.clone()),
        );
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler = if use_web {
            Some(debug::spawn_debug_screen_handler(
                bus.subscribe(),
                project.config.recording.clone(),
                web_port,
                bus.clone(),
            ))
        } else {
            None
        };
        let mcp_control_tx = if flags.control_socket {
            let (_control_handle, control_tx) = control::spawn_control_server(bus.clone());
            slog(&session_log, |l| {
                l.info(&format!(
                    "Control socket: {}",
                    control::socket_path().display()
                ))
            });
            Some(control_tx)
        } else {
            None
        };

        // Outbound event broadcast channel — shared by control socket, web gateway,
        // and the outbound broadcaster.  If control socket is active, reuse its
        // channel; otherwise create a standalone one when web or broadcaster needs it.
        let outbound_tx = if let Some(ref tx) = mcp_control_tx {
            tx.clone()
        } else if use_web {
            let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
            tx
        } else {
            // No control socket, no web — create a channel anyway so the
            // outbound broadcaster can still run (receivers just drop events).
            let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
            tx
        };

        // Wire outbound broadcaster: converts AppEvents to OutboundEvents on the
        // shared broadcast channel.
        let _outbound_broadcaster =
            event::spawn_outbound_broadcaster(bus.subscribe(), outbound_tx.clone());

        // Wire session log writer: persists bus events that aren't logged inline.
        let _session_log_writer =
            event::spawn_session_log_writer(bus.subscribe(), session_log.clone());

        // File watcher: observes project directory for changes, emits FileChanged events.
        let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) = {
            let snapshot_dir = log_dir.join("file_snapshots");
            match file_watcher::FileWatcher::new(project.root.clone(), snapshot_dir, bus.clone()) {
                Ok(watcher) => {
                    let (fw, wh, rh) = watcher.start_shared();
                    (Some(fw), Some(wh), Some(rh))
                }
                Err(e) => {
                    eprintln!("[file_watcher] Failed to start: {}", e);
                    (None, None, None)
                }
            }
        };

        // Web gateway (WebSocket)
        let _web_handle = if use_web {
            let broadcast_tx = outbound_tx.clone();
            let transcriber: Option<std::sync::Arc<dyn transcription::Transcriber>> =
                if project.config.transcription.enabled {
                    match transcription::WhisperTranscriber::new(&project.config.transcription) {
                        Ok(t) => Some(std::sync::Arc::new(t)),
                        Err(e) => {
                            eprintln!("Transcription init failed: {}", e);
                            None
                        }
                    }
                } else {
                    None
                };
            let config = web_gateway::build_config(
                project.config.presence.live_provider.as_deref(),
                project.config.presence.live_model.as_deref(),
                project.config.transcription.enabled,
                project.config.webrtc.to_ice_config(),
                project.config.webrtc.federation_allow_h264,
            );
            let snapshot_dir = log_dir.join("file_snapshots");
            let shared_session =
                Arc::new(tokio::sync::RwLock::new(web_gateway::ActiveSessionState {
                    daemon_session_id: session_log_id(&session_log),
                    query_ctx: None,
                    frame_registry: Some(frame_registry.clone()),
                    session_log: Some(session_log.clone()),
                    recording_registry: Some(recording_registry.clone()),
                    session_registry: Some(session_registry.clone()),
                    snapshot_dir: Some(snapshot_dir.clone()),
                    project_root_for_changes: Some(project.root.clone()),
                    file_watcher: shared_file_watcher.clone(),
                }));
            let mut mcp_http_state = mcp::McpAppState::new(
                "none".into(),
                "none".into(),
                autonomy.clone(),
                log_dir.clone(),
            );
            mcp_http_state.frame_registry = Some(frame_registry.clone());
            mcp_http_state.session_registry = Some(session_registry.clone());
            mcp_http_state.screenshot_dir = Some(log_dir.join("screenshots"));
            let mcp_http_server = Some(Arc::new(mcp::IntendantServer::new(
                Arc::new(tokio::sync::RwLock::new(mcp_http_state)),
                bus.clone(),
            )));
            let peer_registry = build_and_hydrate_peer_registry(&log_dir, &project.config.peers);
            let advertise_urls = resolve_advertise_urls_from_flags_and_config(&flags, &project);
            let handle = web_gateway::spawn_web_gateway(
                web_listener
                    .take()
                    .expect("web listener must exist when use_web"),
                bus.clone(),
                broadcast_tx,
                config,
                shared_session,
                transcriber,
                None, // MCP mode: no WebTui
                None, // No task_tx in MCP mode
                Some(project.root.clone()),
                mcp_http_server,
                Some(peer_registry),
                advertise_urls,
                project.config.server.auth.bearer_token.clone(),
                build_local_advertised_auth(
                    &project.config.server.auth,
                    &lan::backend::select_backend().cert_dir(),
                )?,
                web_tls_acceptor.clone(),
            );
            slog(&session_log, |l| {
                l.info(&format!("Web TUI: http://0.0.0.0:{}", web_port))
            });
            eprintln!("Web TUI: http://0.0.0.0:{}", web_port);
            Some(handle)
        } else {
            None
        };

        let mut mcp_app_state = mcp::McpAppState::new(
            provider
                .as_ref()
                .map(|p| p.name().to_string())
                .unwrap_or_else(|| "none".to_string()),
            provider
                .as_ref()
                .map(|p| p.model().to_string())
                .unwrap_or_else(|| "none".to_string()),
            autonomy.clone(),
            log_dir.clone(),
        );
        mcp_app_state.context_window = provider.as_ref().map(|p| p.context_window()).unwrap_or(0);
        mcp_app_state.session_id = session_log
            .lock()
            .map(|l| l.session_id().to_string())
            .unwrap_or_default();
        mcp_app_state.task_description = task.clone().unwrap_or_default();
        mcp_app_state.frame_registry = Some(frame_registry.clone());
        mcp_app_state.session_registry = Some(session_registry.clone());
        mcp_app_state.screenshot_dir = Some(log_dir.join("screenshots"));
        let mcp_state = std::sync::Arc::new(tokio::sync::RwLock::new(mcp_app_state));

        // Build a launcher closure that can spawn the agent loop on demand.
        // This captures the provider factory parameters (not the provider itself,
        // since providers are not Clone) so each start_task creates a fresh provider.
        let project_root = project.root.clone();
        let autonomy_for_launcher = autonomy.clone();
        let session_log_for_launcher = session_log.clone();
        let log_dir_for_launcher = log_dir.clone();
        let mcp_state_for_launcher = mcp_state.clone();
        #[allow(clippy::async_yields_async)]
        let launcher: mcp::TaskLauncher = Box::new(move |task_str: String, bus: EventBus| {
            let project_root = project_root.clone();
            let autonomy = autonomy_for_launcher.clone();
            let session_log = session_log_for_launcher.clone();
            let _parent_log_dir = log_dir_for_launcher.clone();
            let mcp_state = mcp_state_for_launcher.clone();
            Box::pin(async move {
                // Each MCP task gets a fresh session directory so conversations
                // don't bleed between tasks (reasoning items, tool calls, etc.).
                let task_log_dir = session_log::SessionLog::resolve_path(None);
                match session_log::SessionLog::open(task_log_dir.clone()) {
                    Ok(mut l) => {
                        l.write_meta(Some(&project_root), Some(&task_str));
                        l.info(&format!("MCP sub-task session: {}", l.session_id()));
                        // Replace the shared session log with the fresh one
                        if let Ok(mut guard) = session_log.lock() {
                            *guard = l;
                        }
                        // Notify MCP state of the new session dir so askHuman
                        // response files are written to the correct location.
                        bus.send(AppEvent::SessionDirChanged {
                            path: task_log_dir.clone(),
                        });
                    }
                    Err(e) => {
                        bus.send(AppEvent::LoopError(format!(
                            "Failed to create task session: {}",
                            e
                        )));
                        return tokio::spawn(async {});
                    }
                }
                let log_dir = task_log_dir;

                // Create a fresh provider for this task
                let provider = match provider::select_provider() {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::LoopError(format!(
                            "Failed to create provider: {}",
                            e
                        )));
                        return tokio::spawn(async {});
                    }
                };
                let project = match Project::from_root(project_root.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::LoopError(format!(
                            "Failed to load project: {}",
                            e
                        )));
                        return tokio::spawn(async {});
                    }
                };
                // Read and consume the mode override set by start_task
                let orchestrate_override = {
                    let mut s = mcp_state.write().await;
                    s.next_task_orchestrate.take()
                };
                let use_orchestration = match orchestrate_override {
                    Some(true) => true,
                    Some(false) => false,
                    None => !is_simple_task(&task_str), // auto: same heuristic as TUI
                };

                // Create follow-up channel for multi-round support
                let (follow_up_tx, follow_up_rx) = tokio::sync::mpsc::channel::<FollowUpMessage>(1);
                {
                    let mut s = mcp_state.write().await;
                    s.follow_up_tx = Some(follow_up_tx);
                }

                let approval_registry = mcp_state.read().await.approval_registry.clone();
                let bus_clone = bus.clone();
                let task_for_summary = task_str.clone();
                let session_log_summary = session_log.clone();
                let mcp_state_cleanup = mcp_state.clone();
                // Resolve external agent backend: MCP shared state > config default
                let agent_backend = resolve_agent_backend_from_config(
                    mcp_state.read().await.external_agent.clone(),
                    &project,
                );

                tokio::spawn(async move {
                    let result = if let Some(backend) = agent_backend {
                        run_external_agent_mode(
                            backend,
                            task_str,
                            project,
                            bus_clone.clone(),
                            autonomy,
                            session_log,
                            log_dir,
                            follow_up_rx,
                            None,
                            approval_registry,
                            event::ContextInjectionQueue::default(),
                            false,
                            web_port_for_agent,
                            UserAttachments::default(),
                            None,
                            None,
                            false,
                        )
                        .await
                    } else if use_orchestration {
                        run_user_mode(
                            provider,
                            task_str,
                            project,
                            bus_clone.clone(),
                            autonomy,
                            session_log,
                        )
                        .await
                    } else {
                        run_direct_mode(
                            provider,
                            task_str,
                            project,
                            bus_clone.clone(),
                            autonomy,
                            session_log,
                            log_dir,
                            None,
                            follow_up_rx,
                            None, // no JSON approval in MCP mode
                            approval_registry,
                            event::ContextInjectionQueue::default(),
                            false, // not headless — MCP has interactive approval
                            UserAttachments::default(),
                        )
                        .await
                    };

                    match result {
                        Ok(stats) => {
                            slog(&session_log_summary, |l| {
                                l.write_summary_with_rounds(
                                    &task_for_summary,
                                    "completed",
                                    stats.turns,
                                    Some(stats.rounds),
                                )
                            });
                            // Note: TaskComplete is already emitted by run_agent_loop
                            // when it breaks (done signal, no JSON, etc.)
                        }
                        Err(e) => {
                            slog(&session_log_summary, |l| {
                                l.write_summary(&task_for_summary, &format!("error: {}", e), 0)
                            });
                            bus_clone.send(AppEvent::LoopError(e.to_string()));
                        }
                    }

                    // Clean up follow-up sender so MCP knows no task is active
                    {
                        let mut s = mcp_state_cleanup.write().await;
                        s.follow_up_tx = None;
                    }
                })
            })
        });

        // Store the launcher in MCP state
        {
            let mut s = mcp_state.write().await;
            s.launcher = Some(std::sync::Arc::new(launcher));
        }

        // If a task was provided on the CLI, start it immediately
        if let Some(initial_task) = task {
            let handle = {
                let s = mcp_state.read().await;
                let launcher = s.launcher.as_ref().unwrap().clone();
                drop(s);
                (launcher)(initial_task, bus.clone()).await
            };
            let mut s = mcp_state.write().await;
            s.phase = types::Phase::Thinking;
            s.task_handle = Some(handle);
        }

        // Run the MCP server on stdio (blocks until client disconnects or quit)
        let reloaded = env::var("INTENDANT_MCP_RELOAD").is_ok();
        if reloaded {
            // Clear the flag so a subsequent reload doesn't think it's still reloading
            env::remove_var("INTENDANT_MCP_RELOAD");
            slog(&session_log, |l| {
                l.info("MCP server reloaded via exec (injecting synthetic init)");
            });
        }
        if let Err(e) = mcp::run_mcp_server(
            mcp_state,
            bus,
            event_rx,
            reloaded,
            Some(human_question_path),
            mcp_control_tx,
        )
        .await
        {
            slog(&session_log, |l| {
                l.info(&format!("MCP server ended: {}", e))
            });
        }
        if flags.control_socket {
            control::cleanup();
        }
    } else if use_tui {
        // TUI mode — task may be None (user provides it via follow-up input)

        // TUI mode
        let bus = EventBus::new();
        let event_rx = bus.subscribe();

        // Spawn background tasks.
        // In web mode, key events come from WebSocket, not the terminal.
        let _crossterm_handle = if !use_web {
            Some(tui::event::spawn_crossterm_reader(bus.clone()))
        } else {
            None
        };
        let _tick_handle = event::spawn_tick_timer(bus.clone(), 100);
        let _human_monitor = event::spawn_human_question_monitor(
            bus.clone(),
            event::shared_question_path(log_dir.join("human_question")),
        );
        let _recording_listener = recording::spawn_recording_listener(
            bus.subscribe(),
            recording_registry.clone(),
            bus.clone(),
            Some(session_registry.clone()),
        );
        let _user_display_listener = spawn_user_display_listener(
            bus.clone(),
            session_registry.clone(),
            Some(frame_registry.clone()),
        );
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler = if use_web {
            Some(debug::spawn_debug_screen_handler(
                bus.subscribe(),
                project.config.recording.clone(),
                web_port,
                bus.clone(),
            ))
        } else {
            None
        };

        // TUI is created later — just before run() — so that web mode
        // (--web) can use WebTui instead of the real terminal backend.

        // Create app state
        let mut app = tui::app::App::new(
            provider
                .as_ref()
                .map(|p| p.name().to_string())
                .unwrap_or_else(|| "none".to_string()),
            provider
                .as_ref()
                .map(|p| p.model().to_string())
                .unwrap_or_else(|| "none".to_string()),
            autonomy.clone(),
            log_dir.clone(),
        );
        app.context_window = provider.as_ref().map(|p| p.context_window()).unwrap_or(0);
        app.session_id = session_log
            .lock()
            .map(|l| l.session_id().to_string())
            .unwrap_or_default();
        app.task_description = task.clone().unwrap_or_default();
        app.project_root = Some(project.root.clone());
        app.knowledge_path = Some(project.memory_path());
        app.skills = skills::discover_skills(Some(&project.root));
        if flags.verbose {
            app.pending_verbosity = Some(types::Verbosity::Debug);
        }
        if flags.control_socket {
            let (_control_handle, control_tx) = control::spawn_control_server(bus.clone());
            app.set_control_socket(control_tx);
            app.log(
                types::LogLevel::Info,
                format!("Control socket: {}", control::socket_path().display()),
            );
        }

        // Per-connection WebTui command channel (only for web mode).
        let (web_tui_tx, web_tui_rx) = if use_web {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<tui::web::WebTuiCommand>();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Web gateway broadcast channel — shares with control socket if both enabled.
        // The actual web gateway spawn is deferred until after presence setup so we
        // can pass the WebQueryCtx (agent state, project root, etc.) for tool requests.
        let web_broadcast_tx = if use_web {
            let tx = if let Some(ref tx) = app.control_tx {
                tx.clone()
            } else {
                let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
                app.set_control_socket(tx.clone());
                tx
            };
            Some(tx)
        } else {
            None
        };

        // Wire outbound broadcaster: converts AppEvents to OutboundEvents on the
        // shared broadcast channel (control socket / web gateway).
        let _outbound_broadcaster = if let Some(ref tx) = app.control_tx {
            Some(event::spawn_outbound_broadcaster(
                bus.subscribe(),
                tx.clone(),
            ))
        } else {
            None
        };

        // Wire session log writer: persists bus events that aren't logged inline.
        let _session_log_writer =
            event::spawn_session_log_writer(bus.subscribe(), session_log.clone());

        // File watcher: observes project directory for changes, emits FileChanged events.
        let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) = {
            let snapshot_dir = log_dir.join("file_snapshots");
            match file_watcher::FileWatcher::new(project.root.clone(), snapshot_dir, bus.clone()) {
                Ok(watcher) => {
                    let (fw, wh, rh) = watcher.start_shared();
                    (Some(fw), Some(wh), Some(rh))
                }
                Err(e) => {
                    eprintln!("[file_watcher] Failed to start: {}", e);
                    (None, None, None)
                }
            }
        };

        if let Some(ref t) = task {
            app.log(types::LogLevel::Info, format!("Task: {}", t));
        }

        // Determine if presence layer should be active.
        // Note: --direct only forces single-agent mode for the worker; it does
        // NOT disable presence.  Use --no-presence to disable presence.
        let use_presence = !flags.no_presence && project.config.presence.enabled;

        // Create follow-up channel for multi-round support.
        // When there is no initial task, the follow-up channel also delivers
        // the very first task from the input panel. Owned by the task
        // dispatcher (spawned below), not the TUI — the TUI emits
        // ControlCommand on the bus, the dispatcher routes.
        let (follow_up_tx, mut follow_up_rx) = tokio::sync::mpsc::channel::<FollowUpMessage>(4);

        // If no task was provided, start in follow-up mode so the user sees
        // the input panel immediately.
        if task.is_none() {
            app.current_phase = types::Phase::WaitingFollowUp;
            app.mode = tui::app::AppMode::FollowUp;
            let mut textarea = ratatui_textarea::TextArea::default();
            textarea.set_cursor_line_style(ratatui::style::Style::default());
            app.follow_up_textarea = Some(textarea);
            app.log(
                types::LogLevel::Info,
                "Ready. Enter a task to get started.".to_string(),
            );
        }

        // If presence is active, create channels for user ↔ presence communication
        // and the shared agent state snapshot. The presence_tx sender is owned by
        // the task dispatcher (spawned below), which routes non-direct user text
        // through the presence LLM.
        let (
            presence_user_rx,
            presence_event_rx_for_task,
            presence_agent_state,
            presence_tx_for_dispatch,
        ) = if use_presence {
            let (presence_tx, presence_user_rx) = tokio::sync::mpsc::channel::<String>(4);

            // Create presence event channel: TUI forwards filtered events here
            let (presence_event_tx, presence_event_rx) =
                tokio::sync::mpsc::channel::<presence::PresenceEvent>(64);
            app.set_presence_event_sender(presence_event_tx);

            // Shared agent state: updated by TUI (via forward_to_presence), read by presence tools
            let agent_state = Arc::new(std::sync::Mutex::new(
                presence::AgentStateSnapshot::default(),
            ));
            app.set_presence_agent_state(agent_state.clone());

            app.log_sourced(
                types::LogLevel::Info,
                "Presence layer active".to_string(),
                tui::app::LogSource::Presence,
                None,
            );
            // If there's an initial task, set the phase to Thinking immediately
            // so the TUI doesn't sit at "Idle" during the presence API call.
            if task.is_some() {
                app.current_phase = types::Phase::Thinking;
            }
            (
                Some(presence_user_rx),
                Some(presence_event_rx),
                Some(agent_state),
                Some(presence_tx),
            )
        } else {
            (None, None, None, None)
        };

        // Create the shared PresenceSession for event replay and checkpoints
        let presence_session = {
            let sid = session_log
                .lock()
                .map(|l| l.session_id().to_string())
                .unwrap_or_default();
            Arc::new(Mutex::new(presence::PresenceSession::new(sid)))
        };
        app.presence_session = Some(presence_session.clone());
        app.session_log = Some(session_log.clone());

        // Task dispatch channel: browser tool calls / dashboard StartTask →
        // presence task loop (CU-first routing). Only created when presence
        // is enabled, because the channel is consumed by `run_with_presence`.
        // The sender is owned by the dispatcher (spawned below) and by the
        // presence layer (its own `submit_task` tool). In non-presence mode,
        // leaving `task_tx` as None makes the dispatcher route to
        // `follow_up_tx` instead, which is consumed by
        // `run_external_agent_mode` / `run_direct_mode`.
        let (task_tx, task_rx) = if use_presence {
            let (tx, rx) = tokio::sync::mpsc::channel::<presence::TaskEnvelope>(4);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Spawn the backend task dispatcher. It listens on the bus for
        // ControlCommand(StartTask | FollowUp) and routes to the appropriate
        // channel. Replaces the routing logic that used to live in the TUI.
        let _dispatcher_handle = task_dispatch::Dispatcher {
            presence_tx: presence_tx_for_dispatch,
            task_tx: task_tx.clone(),
            follow_up_tx: Some(follow_up_tx.clone()),
            primary_session_id: session_log
                .lock()
                .map(|log| log.session_id().to_string())
                .ok(),
        }
        .spawn(bus.clone());

        // Deferred web gateway spawn — now we have the agent state for tool queries.
        // Note: WebQueryCtx is built UNCONDITIONALLY (not gated on presence).
        // The web dashboard's annotation Send button needs the context_injection
        // queue regardless of whether the presence layer is enabled, so that
        // injections still reach the agent loop in --no-presence mode.
        // When presence is disabled, agent_state is a fresh empty snapshot
        // (no live updates), but context_injection is still wired through.
        let mut web_shared_session_for_supervisor: Option<web_gateway::SharedActiveSession> = None;
        let _web_handle = if let Some(broadcast_tx) = web_broadcast_tx {
            let query_ctx_agent_state = presence_agent_state.clone().unwrap_or_else(|| {
                Arc::new(std::sync::Mutex::new(
                    presence::AgentStateSnapshot::default(),
                ))
            });
            let query_ctx = Some(web_gateway::WebQueryCtx {
                agent_state: query_ctx_agent_state,
                project_root: project.root.clone(),
                log_dir: log_dir.clone(),
                knowledge_path: project.memory_path(),
                presence_session: Some(presence_session.clone()),
                context_injection: Some(app.context_injection.clone()),
            });
            let transcriber: Option<std::sync::Arc<dyn transcription::Transcriber>> =
                if project.config.transcription.enabled {
                    match transcription::WhisperTranscriber::new(&project.config.transcription) {
                        Ok(t) => Some(std::sync::Arc::new(t)),
                        Err(e) => {
                            app.log(
                                types::LogLevel::Warn,
                                format!("Transcription init failed: {}", e),
                            );
                            None
                        }
                    }
                } else {
                    None
                };
            let config = web_gateway::build_config(
                project.config.presence.live_provider.as_deref(),
                project.config.presence.live_model.as_deref(),
                project.config.transcription.enabled,
                project.config.webrtc.to_ice_config(),
                project.config.webrtc.federation_allow_h264,
            );
            let snapshot_dir = log_dir.join("file_snapshots");
            let shared_session =
                Arc::new(tokio::sync::RwLock::new(web_gateway::ActiveSessionState {
                    daemon_session_id: session_log_id(&session_log),
                    query_ctx,
                    frame_registry: Some(frame_registry.clone()),
                    session_log: Some(session_log.clone()),
                    recording_registry: Some(recording_registry.clone()),
                    session_registry: Some(session_registry.clone()),
                    snapshot_dir: Some(snapshot_dir.clone()),
                    project_root_for_changes: Some(project.root.clone()),
                    file_watcher: shared_file_watcher.clone(),
                }));
            web_shared_session_for_supervisor = Some(shared_session.clone());
            // Create MCP server for HTTP transport (display/CU tools for external agents)
            let mut mcp_http_state = mcp::McpAppState::new(
                "none".into(),
                "none".into(),
                autonomy.clone(),
                log_dir.clone(),
            );
            mcp_http_state.frame_registry = Some(frame_registry.clone());
            mcp_http_state.session_registry = Some(session_registry.clone());
            mcp_http_state.screenshot_dir = Some(log_dir.join("screenshots"));
            let mcp_http_server = Some(Arc::new(mcp::IntendantServer::new(
                Arc::new(tokio::sync::RwLock::new(mcp_http_state)),
                bus.clone(),
            )));
            let peer_registry = build_and_hydrate_peer_registry(&log_dir, &project.config.peers);
            // Browser-voice SubmitTask actions go via the EventBus → dispatcher
            // path (task_tx=None triggers the fallback at web_gateway.rs),
            // keeping a single routing authority.
            let advertise_urls = resolve_advertise_urls_from_flags_and_config(&flags, &project);
            let handle = web_gateway::spawn_web_gateway(
                web_listener
                    .take()
                    .expect("web listener must exist when use_web"),
                bus.clone(),
                broadcast_tx,
                config,
                shared_session,
                transcriber,
                web_tui_tx.clone(),
                None,
                Some(project.root.clone()),
                mcp_http_server,
                Some(peer_registry),
                advertise_urls,
                project.config.server.auth.bearer_token.clone(),
                build_local_advertised_auth(
                    &project.config.server.auth,
                    &lan::backend::select_backend().cert_dir(),
                )?,
                web_tls_acceptor.clone(),
            );
            app.log(
                types::LogLevel::Info,
                format!("Web TUI: http://0.0.0.0:{}", web_port),
            );
            Some(handle)
        } else {
            None
        };

        // Save for daemon loop (project is moved into the agent loop closure)
        let project_root = project.root.clone();
        // Clone frame_registry for event handlers (original may be moved into spawns)
        let frame_registry_for_events = frame_registry.clone();

        // Spawn the agent loop in a background task
        let bus_clone = bus.clone();
        let autonomy_clone = autonomy.clone();
        let session_log_clone = session_log.clone();
        let session_log_summary = session_log.clone();
        let log_dir_clone = log_dir.clone();
        let approval_registry_clone = app.approval_registry.clone();
        let context_injection_clone = app.context_injection.clone();
        let mcp_mgr = if !project.config.mcp_servers.is_empty() {
            Some(mcp_client::McpClientManager::connect_all(&project.config.mcp_servers).await)
        } else {
            None
        };
        let force_direct = flags.direct;
        // Resolve external agent backend: CLI flag > config default > None
        let agent_backend =
            resolve_agent_backend_from_config(flags.agent_backend.clone(), &project);
        // Shared state for dynamic external agent selection from the web UI.
        // Seeded with the resolved CLI/config value; updated by SetExternalAgent ControlMsg.
        let shared_external_agent: Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>> =
            Arc::new(tokio::sync::RwLock::new(agent_backend.clone()));
        // Live Codex config — seeded from TOML, updated by SetCodex* ControlMsgs.
        // The daemon loop reads this at the start of each task so a Control-tab
        // toggle takes effect on the next task without needing a restart.
        let shared_codex_config: control_plane::SharedCodexConfig = {
            let cfg = &project.config.agent.codex;
            Arc::new(tokio::sync::RwLock::new(
                control_plane::CodexRuntimeConfig {
                    command: cfg.command.clone(),
                    sandbox: project::normalize_sandbox_mode(&cfg.sandbox),
                    approval_policy: project::normalize_approval_policy(&cfg.approval_policy),
                    model: cfg.model.clone(),
                    reasoning_effort: project::normalize_reasoning_effort(
                        cfg.reasoning_effort.as_deref(),
                    ),
                    web_search: cfg.web_search,
                    network_access: cfg.network_access,
                    writable_roots: cfg.writable_roots.clone(),
                },
            ))
        };
        let shared_gemini_config: control_plane::SharedGeminiConfig = {
            let cfg = &project.config.agent.gemini_cli;
            Arc::new(tokio::sync::RwLock::new(
                control_plane::GeminiRuntimeConfig {
                    model: cfg.model.clone(),
                    approval_mode: project::normalize_gemini_approval_mode(&cfg.approval_mode),
                    sandbox: cfg.sandbox,
                    extensions: cfg.extensions.clone(),
                    allowed_mcp_servers: cfg.allowed_mcp_servers.clone(),
                    include_directories: cfg.include_directories.clone(),
                    debug: cfg.debug,
                },
            ))
        };
        let _control_plane_handle = control_plane::spawn(
            bus.subscribe(),
            control_plane::ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: shared_external_agent.clone(),
                codex_config: shared_codex_config.clone(),
                gemini_config: shared_gemini_config.clone(),
                bus: bus.clone(),
                project_root: Some(project.root.clone()),
            },
        );
        let _resume_listener_handle = if use_web {
            Some(
                session_supervisor::SessionSupervisor::new(
                    session_supervisor::SessionSupervisorConfig {
                        bus: bus.clone(),
                        project_root: project.root.clone(),
                        autonomy: autonomy.clone(),
                        shared_external_agent: shared_external_agent.clone(),
                        shared_codex_config: shared_codex_config.clone(),
                        shared_gemini_config: shared_gemini_config.clone(),
                        frame_registry: frame_registry.clone(),
                        web_port: web_port_for_agent,
                        flags_direct: flags.direct,
                        shared_session: web_shared_session_for_supervisor.clone(),
                    },
                )
                .spawn_resume_listener(),
            )
        } else {
            None
        };
        let mut loop_handle = if use_presence {
            // Presence mode: the presence layer mediates between user and agent
            let presence_user_rx = presence_user_rx.unwrap();
            let presence_event_rx = presence_event_rx_for_task.unwrap();
            let agent_state = presence_agent_state.unwrap();
            // task_tx/task_rx are Some when use_presence is true (see above).
            let task_tx = task_tx.expect("task_tx created in presence mode");
            let task_rx = task_rx.expect("task_rx created in presence mode");
            let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<String>(8);

            // Shared paused ref-count: incremented by PresenceConnected, decremented by PresenceDisconnected.
            // Server-side presence is paused when count > 0 (any browser has active voice).
            let presence_paused = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            app.set_presence_paused_flag(presence_paused.clone());

            // Forward presence responses to TUI as log entries + reset phase
            let bus_for_responses = bus_clone.clone();
            let _response_forwarder = tokio::spawn(async move {
                while let Some(response) = response_rx.recv().await {
                    if !response.is_empty() {
                        if response.starts_with("Presence error:")
                            || response.starts_with("Presence provider timed out")
                        {
                            bus_for_responses.send(AppEvent::LoopError(response));
                        } else {
                            // Log presence response as a visible PresenceLog entry
                            bus_for_responses.send(AppEvent::PresenceLog {
                                message: format!("[presence] {}", response),
                                level: None,
                                turn: None,
                            });
                            // Switch to follow-up mode after presence responds
                            bus_for_responses.send(AppEvent::PresenceReady);
                        }
                    }
                }
            });

            let agent_backend_for_presence = agent_backend.clone();
            let shared_external_agent_for_presence = shared_external_agent.clone();
            let shared_codex_config_for_presence = shared_codex_config.clone();
            let shared_gemini_config_for_presence = shared_gemini_config.clone();
            tokio::spawn(async move {
                let result = run_with_presence(
                    task,
                    project,
                    bus_clone.clone(),
                    autonomy_clone,
                    session_log_clone,
                    log_dir_clone,
                    presence_user_rx,
                    response_tx,
                    presence_event_rx,
                    agent_state,
                    force_direct,
                    presence_paused,
                    task_tx,
                    task_rx,
                    approval_registry_clone,
                    frame_registry.clone(),
                    context_injection_clone,
                    session_registry.clone(),
                    agent_backend_for_presence,
                    shared_external_agent_for_presence,
                    shared_codex_config_for_presence,
                    shared_gemini_config_for_presence,
                    if use_web { Some(web_port) } else { None },
                )
                .await;

                match result {
                    Ok(stats) => {
                        slog(&session_log_summary, |l| {
                            l.write_summary_with_rounds(
                                "(presence)",
                                "completed",
                                stats.turns,
                                Some(stats.rounds),
                            )
                        });
                    }
                    Err(e) => {
                        slog(&session_log_summary, |l| {
                            l.write_summary("(presence)", &format!("error: {}", e), 0)
                        });
                        bus_clone.send(AppEvent::LoopError(e.to_string()));
                    }
                }
            })
        } else {
            // Standard mode: direct agent loop.
            // When task is None, wait for the first follow-up message to
            // use as the task. This lets the TUI start idle.
            tokio::spawn(async move {
                let (task_str, follow_up_rx) = if let Some(t) = task {
                    (t, follow_up_rx)
                } else {
                    // Wait for the first message from the follow-up panel
                    match follow_up_rx.recv().await {
                        Some(first_task) => {
                            slog(&session_log_clone, |l| {
                                l.info(&format!("Task (from input): {}", first_task.text))
                            });
                            bus_clone.send(AppEvent::TurnStarted {
                                session_id: session_log_id(&session_log_clone),
                                turn: 0,
                                budget_pct: 0.0,
                                remaining: 0,
                            });
                            (first_task.text, follow_up_rx)
                        }
                        None => return, // channel closed before a task arrived
                    }
                };

                let result = if let Some(backend) = agent_backend {
                    run_external_agent_mode(
                        backend,
                        task_str,
                        project,
                        bus_clone.clone(),
                        autonomy_clone,
                        session_log_clone,
                        log_dir_clone,
                        follow_up_rx,
                        None,
                        approval_registry_clone,
                        context_injection_clone.clone(),
                        false, // not headless — TUI handles approval
                        web_port_for_agent,
                        UserAttachments::default(),
                        None,
                        None,
                        false,
                    )
                    .await
                } else {
                    // Re-select provider at task start (may have been None at startup)
                    let provider = match provider.or_else(|| provider::select_provider().ok()) {
                        Some(p) => p,
                        None => {
                            bus_clone.send(AppEvent::LoopError(
                                "No API key configured. Set OPENAI_API_KEY, ANTHROPIC_API_KEY, or GEMINI_API_KEY.".to_string()
                            ));
                            return;
                        }
                    };

                    if force_direct || is_simple_task(&task_str) {
                        run_direct_mode(
                            provider,
                            task_str,
                            project,
                            bus_clone.clone(),
                            autonomy_clone,
                            session_log_clone,
                            log_dir_clone,
                            mcp_mgr,
                            follow_up_rx,
                            None, // no JSON approval in TUI mode
                            approval_registry_clone,
                            context_injection_clone,
                            false, // not headless — TUI handles approval
                            UserAttachments::default(),
                        )
                        .await
                    } else {
                        run_user_mode(
                            provider,
                            task_str,
                            project,
                            bus_clone.clone(),
                            autonomy_clone,
                            session_log_clone,
                        )
                        .await
                    }
                };

                match result {
                    Ok(stats) => {
                        slog(&session_log_summary, |l| {
                            l.write_summary_with_rounds(
                                "(tui)",
                                "completed",
                                stats.turns,
                                Some(stats.rounds),
                            )
                        });
                    }
                    Err(e) => {
                        slog(&session_log_summary, |l| {
                            l.write_summary("(tui)", &format!("error: {}", e), 0)
                        });
                        bus_clone.send(AppEvent::LoopError(e.to_string()));
                    }
                }
            })
        };

        // Run the TUI event loop (blocks until quit).
        // In web mode, render to a buffer and stream to xterm.js.
        // In terminal mode, render directly to stdout.
        if use_web {
            let broadcast_tx = app.control_tx.clone().unwrap_or_else(|| {
                let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
                app.set_control_socket(tx.clone());
                tx
            });
            eprintln!("Web TUI: http://0.0.0.0:{}", web_port);
            let mut web_tui = tui::web::WebTui::new(120, 40, broadcast_tx)
                .map_err(|e| CallerError::Tui(format!("Failed to initialize Web TUI: {}", e)))?;
            let cmd_rx = web_tui_rx.expect("web_tui_rx must exist in web mode");
            let _ = web_tui.run(&mut app, event_rx, cmd_rx, bus.clone()).await;
        } else {
            let mut terminal = tui::Tui::new()
                .map_err(|e| CallerError::Tui(format!("Failed to initialize TUI: {}", e)))?;
            let _ = terminal.run(&mut app, event_rx, bus.clone()).await;
        }

        // Drop the App (and its follow_up_tx) so the round loop's recv()
        // returns None and exits gracefully, allowing write_summary to run.
        let session_id = app.session_id.clone();
        drop(app);

        // Give the agent task a moment to finish writing the session summary.
        // If it doesn't finish in time (e.g. stuck on an API call), abort it.
        match tokio::time::timeout(std::time::Duration::from_secs(5), &mut loop_handle).await {
            Ok(_) => {}                    // task finished naturally
            Err(_) => loop_handle.abort(), // timed out — force stop
        }

        if use_web && !session_id.is_empty() {
            bus.send(AppEvent::SessionEnded {
                session_id,
                reason: "completed".to_string(),
            });
            // Daemon mode: keep web gateway alive after TUI quits.
            // Fall through to a headless daemon loop (TUI is not re-created).
            eprintln!(
                "TUI exited. Web gateway still running on port {}. Waiting for new tasks...",
                web_port
            );
            run_daemon_loop(DaemonConfig {
                bus: bus.clone(),
                project_root: project_root.clone(),
                autonomy: autonomy.clone(),
                shared_external_agent: shared_external_agent.clone(),
                shared_codex_config: shared_codex_config.clone(),
                shared_gemini_config: shared_gemini_config.clone(),
                frame_registry: frame_registry_for_events.clone(),
                web_port: web_port_for_agent,
                flags_direct: flags.direct,
                shared_session: None,
            })
            .await;
        }

        control::cleanup();
    } else {
        // Headless mode always has a task (enforced above).
        let task = task.unwrap();

        // Headless mode (--no-tui or non-TTY)
        let bus = EventBus::new();
        let _recording_listener = recording::spawn_recording_listener(
            bus.subscribe(),
            recording_registry.clone(),
            bus.clone(),
            Some(session_registry.clone()),
        );
        let _user_display_listener = spawn_user_display_listener(
            bus.clone(),
            session_registry.clone(),
            Some(frame_registry.clone()),
        );
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler = if use_web {
            Some(debug::spawn_debug_screen_handler(
                bus.subscribe(),
                project.config.recording.clone(),
                web_port,
                bus.clone(),
            ))
        } else {
            None
        };

        // Outbound broadcast channel — shared by web gateway and JSON stdout subscriber
        let (outbound_tx, _) = tokio::sync::broadcast::channel::<String>(256);

        // Wire outbound broadcaster: converts AppEvents to OutboundEvents
        let _outbound_broadcaster =
            event::spawn_outbound_broadcaster(bus.subscribe(), outbound_tx.clone());

        // Wire session log writer: persists bus events that aren't logged inline.
        let _session_log_writer =
            event::spawn_session_log_writer(bus.subscribe(), session_log.clone());

        // File watcher: observes project directory for changes, emits FileChanged events.
        let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) = {
            let snapshot_dir = log_dir.join("file_snapshots");
            match file_watcher::FileWatcher::new(project.root.clone(), snapshot_dir, bus.clone()) {
                Ok(watcher) => {
                    let (fw, wh, rh) = watcher.start_shared();
                    (Some(fw), Some(wh), Some(rh))
                }
                Err(e) => {
                    eprintln!("[file_watcher] Failed to start: {}", e);
                    (None, None, None)
                }
            }
        };

        // JSON stdout subscriber: prints OutboundEvents as JSONL to stdout
        if flags.json_output {
            let mut json_rx = outbound_tx.subscribe();
            tokio::spawn(async move {
                loop {
                    match json_rx.recv().await {
                        Ok(line) => {
                            println!("{}", line);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        // Web gateway in headless mode
        let headless_shared_session: Option<web_gateway::SharedActiveSession> = if use_web {
            let transcriber: Option<std::sync::Arc<dyn transcription::Transcriber>> =
                if project.config.transcription.enabled {
                    match transcription::WhisperTranscriber::new(&project.config.transcription) {
                        Ok(t) => Some(std::sync::Arc::new(t)),
                        Err(e) => {
                            eprintln!("Transcription init failed: {}", e);
                            None
                        }
                    }
                } else {
                    None
                };
            let config = web_gateway::build_config(
                project.config.presence.live_provider.as_deref(),
                project.config.presence.live_model.as_deref(),
                project.config.transcription.enabled,
                project.config.webrtc.to_ice_config(),
                project.config.webrtc.federation_allow_h264,
            );
            let snapshot_dir = log_dir.join("file_snapshots");
            let shared_session =
                Arc::new(tokio::sync::RwLock::new(web_gateway::ActiveSessionState {
                    daemon_session_id: session_log_id(&session_log),
                    query_ctx: None,
                    frame_registry: Some(frame_registry.clone()),
                    session_log: Some(session_log.clone()),
                    recording_registry: Some(recording_registry.clone()),
                    session_registry: Some(session_registry.clone()),
                    snapshot_dir: Some(snapshot_dir.clone()),
                    project_root_for_changes: Some(project.root.clone()),
                    file_watcher: shared_file_watcher.clone(),
                }));
            let mut mcp_http_state = mcp::McpAppState::new(
                "none".into(),
                "none".into(),
                autonomy.clone(),
                log_dir.clone(),
            );
            mcp_http_state.frame_registry = Some(frame_registry.clone());
            mcp_http_state.session_registry = Some(session_registry.clone());
            mcp_http_state.screenshot_dir = Some(log_dir.join("screenshots"));
            let mcp_http_server = Some(Arc::new(mcp::IntendantServer::new(
                Arc::new(tokio::sync::RwLock::new(mcp_http_state)),
                bus.clone(),
            )));
            let peer_registry = build_and_hydrate_peer_registry(&log_dir, &project.config.peers);
            let advertise_urls = resolve_advertise_urls_from_flags_and_config(&flags, &project);
            let _web_handle = web_gateway::spawn_web_gateway(
                web_listener
                    .take()
                    .expect("web listener must exist when use_web"),
                bus.clone(),
                outbound_tx.clone(),
                config,
                shared_session.clone(),
                transcriber,
                None, // Headless mode: no WebTui
                None, // No task_tx in headless mode
                Some(project.root.clone()),
                mcp_http_server,
                Some(peer_registry),
                advertise_urls,
                project.config.server.auth.bearer_token.clone(),
                build_local_advertised_auth(
                    &project.config.server.auth,
                    &lan::backend::select_backend().cert_dir(),
                )?,
                web_tls_acceptor.clone(),
            );
            eprintln!("Web TUI: http://0.0.0.0:{}", web_port);
            Some(shared_session)
        } else {
            None
        };

        let mcp_mgr = if !project.config.mcp_servers.is_empty() {
            Some(mcp_client::McpClientManager::connect_all(&project.config.mcp_servers).await)
        } else {
            None
        };

        // Create follow-up channel. In JSON mode, spawn a stdin reader to enable
        // follow-up via stdin lines and JSON commands (approve, deny, input, etc.).
        // Otherwise, drop the sender immediately so recv() returns None → single-round.
        let (follow_up_tx, follow_up_rx) = tokio::sync::mpsc::channel::<FollowUpMessage>(1);
        let json_approval_slot = if flags.json_output {
            Some(new_json_approval_slot())
        } else {
            None
        };
        if flags.json_output {
            // JSON mode: read follow-up lines and control commands from stdin
            let approval_slot = json_approval_slot.clone().unwrap();
            let log_dir_for_stdin = log_dir.clone();
            tokio::spawn(async move {
                let stdin = tokio::io::stdin();
                let reader = tokio::io::BufReader::new(stdin);
                use tokio::io::AsyncBufReadExt;
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    // Try to parse as a JSON control command
                    if line.starts_with('{') {
                        if let Ok(msg) = serde_json::from_str::<event::ControlMsg>(&line) {
                            match msg {
                                event::ControlMsg::Approve { .. } => {
                                    let mut guard = approval_slot.lock().unwrap();
                                    if let Some((_id, tx)) = guard.take() {
                                        let _ = tx.send(event::ApprovalResponse::Approve);
                                    }
                                }
                                event::ControlMsg::Deny { .. } => {
                                    let mut guard = approval_slot.lock().unwrap();
                                    if let Some((_id, tx)) = guard.take() {
                                        let _ = tx.send(event::ApprovalResponse::Deny);
                                    }
                                }
                                event::ControlMsg::Skip { .. } => {
                                    let mut guard = approval_slot.lock().unwrap();
                                    if let Some((_id, tx)) = guard.take() {
                                        let _ = tx.send(event::ApprovalResponse::Skip);
                                    }
                                }
                                event::ControlMsg::ApproveAll { .. } => {
                                    let mut guard = approval_slot.lock().unwrap();
                                    if let Some((_id, tx)) = guard.take() {
                                        let _ = tx.send(event::ApprovalResponse::ApproveAll);
                                    }
                                }
                                event::ControlMsg::Input { text } => {
                                    // Write human_response file for askHuman IPC
                                    let resp_path = log_dir_for_stdin.join("human_response");
                                    let _ = std::fs::write(&resp_path, text.as_bytes());
                                }
                                event::ControlMsg::FollowUp {
                                    text, direct: _, ..
                                } => {
                                    // This stdin handler only exists in
                                    // the headless `--json` path where
                                    // there's no presence layer, so the
                                    // direct bit is implicitly always on.
                                    if follow_up_tx
                                        .send(FollowUpMessage::text(text))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                _ => {
                                    // Unknown command — ignore
                                }
                            }
                            continue;
                        }
                    }
                    // Plain text → follow-up message
                    if follow_up_tx
                        .send(FollowUpMessage::text(line))
                        .await
                        .is_err()
                    {
                        break; // receiver dropped
                    }
                }
            });
        } else {
            drop(follow_up_tx); // single-round: recv() returns None immediately
        }

        let session_id = log_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        bus.send(AppEvent::SessionStarted {
            session_id: session_id.clone(),
            task: Some(task.clone()),
        });

        // Save for daemon loop (project and autonomy are moved into the agent loop)
        let project_root = project.root.clone();
        let autonomy_for_daemon = autonomy.clone();

        // Resolve external agent backend: CLI flag > config default > None
        let agent_backend =
            resolve_agent_backend_from_config(flags.agent_backend.clone(), &project);
        // Shared state for dynamic external agent selection from the web UI (headless mode).
        let shared_external_agent: Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>> =
            Arc::new(tokio::sync::RwLock::new(agent_backend.clone()));
        let shared_codex_config: control_plane::SharedCodexConfig = {
            let cfg = &project.config.agent.codex;
            Arc::new(tokio::sync::RwLock::new(
                control_plane::CodexRuntimeConfig {
                    command: cfg.command.clone(),
                    sandbox: project::normalize_sandbox_mode(&cfg.sandbox),
                    approval_policy: project::normalize_approval_policy(&cfg.approval_policy),
                    model: cfg.model.clone(),
                    reasoning_effort: project::normalize_reasoning_effort(
                        cfg.reasoning_effort.as_deref(),
                    ),
                    web_search: cfg.web_search,
                    network_access: cfg.network_access,
                    writable_roots: cfg.writable_roots.clone(),
                },
            ))
        };
        let shared_gemini_config: control_plane::SharedGeminiConfig = {
            let cfg = &project.config.agent.gemini_cli;
            Arc::new(tokio::sync::RwLock::new(
                control_plane::GeminiRuntimeConfig {
                    model: cfg.model.clone(),
                    approval_mode: project::normalize_gemini_approval_mode(&cfg.approval_mode),
                    sandbox: cfg.sandbox,
                    extensions: cfg.extensions.clone(),
                    allowed_mcp_servers: cfg.allowed_mcp_servers.clone(),
                    include_directories: cfg.include_directories.clone(),
                    debug: cfg.debug,
                },
            ))
        };
        let _control_plane_handle = control_plane::spawn(
            bus.subscribe(),
            control_plane::ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: shared_external_agent.clone(),
                codex_config: shared_codex_config.clone(),
                gemini_config: shared_gemini_config.clone(),
                bus: bus.clone(),
                project_root: Some(project.root.clone()),
            },
        );

        let result = if let Some(backend) = agent_backend {
            run_external_agent_mode(
                backend,
                task.clone(),
                project,
                bus.clone(),
                autonomy,
                session_log.clone(),
                log_dir,
                follow_up_rx,
                json_approval_slot,
                event::ApprovalRegistry::default(),
                event::ContextInjectionQueue::default(),
                true, // headless mode
                web_port_for_agent,
                UserAttachments::default(),
                None,
                None,
                false,
            )
            .await
        } else {
            let provider = provider.ok_or_else(|| {
                CallerError::Config("Headless mode requires an API key".to_string())
            })?;
            if flags.direct || is_simple_task(&task) {
                run_direct_mode(
                    provider,
                    task.clone(),
                    project,
                    bus.clone(),
                    autonomy,
                    session_log.clone(),
                    log_dir,
                    mcp_mgr,
                    follow_up_rx,
                    json_approval_slot,
                    event::ApprovalRegistry::default(),
                    event::ContextInjectionQueue::default(),
                    true, // headless mode
                    UserAttachments::default(),
                )
                .await
            } else {
                run_user_mode(
                    provider,
                    task.clone(),
                    project,
                    EventBus::new(), // user_mode spawns orchestrator subprocess
                    autonomy,
                    session_log.clone(),
                )
                .await
            }
        };

        let reason = match &result {
            Ok(stats) => {
                slog(&session_log, |l| {
                    l.write_summary_with_rounds(&task, "completed", stats.turns, Some(stats.rounds))
                });
                "completed".to_string()
            }
            Err(e) => {
                slog(&session_log, |l| {
                    l.write_summary(&task, &format!("error: {}", e), 0)
                });
                format!("error: {}", e)
            }
        };

        bus.send(AppEvent::SessionEnded {
            session_id,
            reason: reason.clone(),
        });

        if use_web {
            // Daemon mode: keep web gateway alive, listen for new tasks from web UI.
            if let Some(ref shared_session) = headless_shared_session {
                // Clear session-specific state so new connections see "no active session"
                {
                    let mut ss = shared_session.write().await;
                    ss.query_ctx = None;
                    ss.session_log = None;
                    // Keep frame_registry and recording_registry alive
                }
            }
            eprintln!(
                "Session ended ({}). Web gateway running on port {}. Waiting for new tasks...",
                reason, web_port
            );

            run_daemon_loop(DaemonConfig {
                bus: bus.clone(),
                project_root: project_root.clone(),
                autonomy: autonomy_for_daemon.clone(),
                shared_external_agent: shared_external_agent.clone(),
                shared_codex_config: shared_codex_config.clone(),
                shared_gemini_config: shared_gemini_config.clone(),
                frame_registry: frame_registry.clone(),
                web_port: web_port_for_agent,
                flags_direct: flags.direct,
                shared_session: headless_shared_session.clone(),
            })
            .await;
        } else {
            result?;
        }
    }

    Ok(())
}
