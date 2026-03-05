mod agent_runner;
mod autonomy;
mod control;
mod conversation;
mod error;
mod frontend;
mod knowledge;
mod mcp;
mod mcp_client;
mod presence;
mod project;
mod prompts;
mod provider;
mod sandbox;
mod session_log;
mod sub_agent;
mod tool_batch;
mod tools;
mod tui;
mod user_mode;
mod vision;
mod live_gateway;
mod worktree;

use autonomy::{AutonomyLevel, AutonomyState, SharedAutonomy};
use conversation::Conversation;
use error::CallerError;
use project::Project;
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tool_batch::{assemble_batch_from_tool_calls, map_results_to_tool_responses};
use tui::event::{AppEvent, EventBus};

type SharedSessionLog = Arc<Mutex<session_log::SessionLog>>;

/// Module-level flag for --json output mode.
static JSON_OUTPUT: AtomicBool = AtomicBool::new(false);

/// Helper to write to the session log without propagating errors.
fn slog(log: &SharedSessionLog, f: impl FnOnce(&mut session_log::SessionLog)) {
    if let Ok(mut l) = log.lock() {
        f(&mut l);
    }
}

const SAFETY_CAP: usize = 500;
const MIN_BUDGET_TOKENS: u64 = 4096;
const BUDGET_WARNING_THRESHOLD: f64 = 0.85;

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
}

#[derive(Debug, Clone, Default)]
struct LoopStats {
    turns: usize,
    rounds: usize,
    usage: provider::TokenUsage,
}

type FollowUpReceiver = tokio::sync::mpsc::Receiver<String>;

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
    /// --live [PORT]: Enable live gateway WebSocket server (implies --mcp).
    live: bool,
    live_port: u16,
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
    println!("    --live [PORT]          Enable live gateway (audio/video/text, default port: 8765; implies --mcp)");
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
        live: false,
        live_port: live_gateway::DEFAULT_PORT,
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
            "--live" | "--voice-gateway" => {
                flags.live = true;
                flags.mcp = true; // --live implies --mcp
                // Optional port argument (next arg if it's numeric)
                if i + 1 < args.len() && args[i + 1].parse::<u16>().is_ok() {
                    flags.live_port = args[i + 1].parse().unwrap();
                    i += 2;
                } else {
                    i += 1;
                }
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
/// We launch on execAsAgent (not just captureScreen) because GUI applications
/// started in early turns must share the same display that captureScreen will
/// later capture. Launching only on captureScreen is too late — the app would
/// already be running on a different (or no) display.
async fn maybe_auto_launch_xvfb(
    json_str: &str,
    xvfb_guard: &mut Option<vision::XvfbGuard>,
    provider_name: &str,
    session_log: &SharedSessionLog,
    bus: &Option<EventBus>,
) {
    if xvfb_guard.is_some() {
        return;
    }
    if vision::is_display_accessible() {
        return;
    }
    if !has_capture_screen_command(json_str) && !has_exec_command(json_str) {
        return;
    }
    let config = vision::display_config_for_provider(provider_name);
    let trigger = if has_capture_screen_command(json_str) {
        "captureScreen"
    } else {
        "execAsAgent (display needed)"
    };
    slog(session_log, |l| {
        l.info(&format!(
            "Auto-launching Xvfb :{} at {}x{} for {}",
            config.display_id, config.width, config.height, trigger
        ))
    });
    match vision::launch_display(&config).await {
        Ok(guard) => {
            let vnc_port = guard.vnc_port();
            if let Some(port) = vnc_port {
                slog(session_log, |l| {
                    l.info(&format!("VNC server available at vnc://localhost:{}", port))
                });
            }
            let display_id = config.display_id;
            emit(
                bus,
                || AppEvent::DisplayReady {
                    display_id,
                    vnc_port,
                },
                || {},
            );
            *xvfb_guard = Some(guard);
        }
        Err(e) => {
            slog(session_log, |l| {
                l.warn(&format!("Failed to auto-launch Xvfb: {}", e))
            });
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
    // Fallback: first 200 chars of raw JSON
    json_str.chars().take(200).collect()
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

/// Emit a JSONL event to stdout (used in --json mode).
fn emit_json(event_type: &str, data: serde_json::Value) {
    let event = serde_json::json!({
        "type": event_type,
        "data": data,
    });
    if let Ok(line) = serde_json::to_string(&event) {
        println!("{}", line);
    }
}

/// Macro-like helper for conditional output: TUI event bus, JSON, or println.
fn emit(bus: &Option<EventBus>, event_fn: impl FnOnce() -> AppEvent, fallback: impl FnOnce()) {
    if let Some(bus) = bus {
        bus.send(event_fn());
    } else if JSON_OUTPUT.load(Ordering::Relaxed) {
        let event = event_fn();
        if let Some((event_type, data)) = app_event_to_json(&event) {
            emit_json(event_type, data);
        }
    } else {
        fallback();
    }
}

/// Convert an AppEvent to a (type, data) pair for JSON output.
fn app_event_to_json(event: &AppEvent) -> Option<(&'static str, serde_json::Value)> {
    match event {
        AppEvent::TurnStarted {
            turn,
            budget_pct,
            remaining,
        } => Some((
            "turn_started",
            serde_json::json!({
                "turn": turn,
                "budget_pct": budget_pct,
                "remaining": remaining,
            }),
        )),
        AppEvent::ModelResponse {
            turn,
            content,
            usage,
            reasoning,
        } => Some((
            "model_response",
            serde_json::json!({
                "turn": turn,
                "content": content,
                "usage": {
                    "prompt_tokens": usage.prompt_tokens,
                    "completion_tokens": usage.completion_tokens,
                    "total_tokens": usage.total_tokens,
                },
                "reasoning": reasoning,
            }),
        )),
        AppEvent::ModelResponseDelta { ref text } => Some((
            "model_response_delta",
            serde_json::json!({
                "text": text,
            }),
        )),
        AppEvent::AgentOutput { stdout, stderr } => Some((
            "agent_output",
            serde_json::json!({
                "stdout": stdout,
                "stderr": stderr,
            }),
        )),
        AppEvent::DoneSignal { message } => Some((
            "done",
            serde_json::json!({
                "message": message,
            }),
        )),
        AppEvent::LoopError(msg) => Some(("error", serde_json::json!({ "message": msg }))),
        AppEvent::BudgetWarning { pct, remaining } => Some((
            "budget_warning",
            serde_json::json!({
                "pct": pct,
                "remaining": remaining,
            }),
        )),
        AppEvent::BudgetExhausted { remaining } => Some((
            "budget_exhausted",
            serde_json::json!({ "remaining": remaining }),
        )),
        AppEvent::ApprovalRequired {
            id,
            command_preview,
            category,
            ..
        } => Some((
            "approval_required",
            serde_json::json!({
                "id": id,
                "command_preview": command_preview,
                "category": format!("{:?}", category),
            }),
        )),
        AppEvent::TaskComplete { reason } => {
            Some(("done", serde_json::json!({ "reason": reason })))
        }
        AppEvent::ContextManagement { turn } => {
            Some(("context_management", serde_json::json!({ "turn": turn })))
        }
        AppEvent::RoundComplete {
            round,
            turns_in_round,
        } => Some((
            "round_complete",
            serde_json::json!({
                "round": round,
                "turns_in_round": turns_in_round,
            }),
        )),
        _ => None, // Skip events that don't need JSON output (Key, Resize, Tick, etc.)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let project = Project {
            root: std::path::PathBuf::from("/tmp/proj"),
            config: project::ProjectConfig::default(),
        };
        let input = r#"{"commands":[{"function":"storeMemory","nonce":1,"memory_key":"test","memory_summary":"hello"}]}"#;
        let result = inject_project_context(input, &project);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            parsed["commands"][0]["memory_file"].as_str().unwrap(),
            "/tmp/proj/.intendant/memory.json"
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
            live: false,
            live_port: live_gateway::DEFAULT_PORT,
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
        assert!(!flags.live);
        assert_eq!(flags.live_port, 8765);
        assert_eq!(flags.autonomy, AutonomyLevel::Medium);
    }

    #[test]
    fn cli_live_flag() {
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
            live: true,
            live_port: live_gateway::DEFAULT_PORT,
        };
        assert!(flags.live);
        assert_eq!(flags.live_port, live_gateway::DEFAULT_PORT);
    }

    #[test]
    fn cli_live_with_port() {
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
            live: true,
            live_port: 9000,
        };
        assert!(flags.live);
        assert_eq!(flags.live_port, 9000);
    }

    #[test]
    fn emit_json_format() {
        // Test that emit_json produces valid JSONL
        let data = serde_json::json!({"turn": 1, "content": "hello"});
        let event = serde_json::json!({
            "type": "model_response",
            "data": data,
        });
        let line = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["type"], "model_response");
        assert_eq!(parsed["data"]["turn"], 1);
    }

    #[test]
    fn app_event_to_json_turn_started() {
        let event = AppEvent::TurnStarted {
            turn: 5,
            budget_pct: 42.0,
            remaining: 100_000,
        };
        let (event_type, data) = app_event_to_json(&event).unwrap();
        assert_eq!(event_type, "turn_started");
        assert_eq!(data["turn"], 5);
        assert_eq!(data["remaining"], 100_000);
    }

    #[test]
    fn app_event_to_json_done_signal() {
        let event = AppEvent::DoneSignal {
            message: Some("All done".to_string()),
        };
        let (event_type, data) = app_event_to_json(&event).unwrap();
        assert_eq!(event_type, "done");
        assert_eq!(data["message"], "All done");
    }

    #[test]
    fn app_event_to_json_skips_tick() {
        let event = AppEvent::Tick;
        assert!(app_event_to_json(&event).is_none());
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

    #[test]
    fn emit_with_bus() {
        let (bus, mut rx) = EventBus::new();
        let bus_opt = Some(bus);
        emit(
            &bus_opt,
            || AppEvent::Tick,
            || panic!("should not be called"),
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            match rx.recv().await.unwrap() {
                AppEvent::Tick => {}
                _ => panic!("expected Tick"),
            }
        });
    }

    #[test]
    fn emit_without_bus() {
        let bus_opt: Option<EventBus> = None;
        let mut called = false;
        emit(&bus_opt, || AppEvent::Tick, || called = true);
        assert!(called);
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
}

const PROGRESS_INTERVAL: usize = 5;

#[allow(clippy::too_many_arguments)]
async fn run_agent_loop(
    provider: &dyn provider::ChatProvider,
    conversation: &mut Conversation,
    project: &Project,
    sub_agent_mode: Option<&(String, sub_agent::SubAgentRole)>,
    bus: Option<EventBus>,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: &std::path::Path,
    mcp_mgr: Option<&mcp_client::McpClientManager>,
) -> Result<(LoopStats, LoopExitReason), CallerError> {
    let mut budget_warning_shown = false;
    let mut empty_command_streak = 0usize;
    let mut loop_stats = LoopStats::default();
    let mut seen_sub_agent_results: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut xvfb_guard: Option<vision::XvfbGuard> = None;
    let mut exit_reason = LoopExitReason::TaskComplete;

    for turn in 1..=SAFETY_CAP {
        // Check budget before sending
        if conversation.remaining_budget() <= MIN_BUDGET_TOKENS {
            let remaining = conversation.remaining_budget();
            slog(&session_log, |l| {
                l.warn(&format!(
                    "Budget exhausted ({} tokens remaining)",
                    remaining
                ))
            });
            emit(
                &bus,
                || AppEvent::BudgetExhausted { remaining },
                || {
                    println!(
                        "--- Context budget exhausted ({} tokens remaining) ---",
                        remaining
                    )
                },
            );
            exit_reason = LoopExitReason::BudgetExhausted;
            break;
        }

        conversation.increment_turn();
        let budget_pct = conversation.usage_fraction() * 100.0;
        let remaining = conversation.remaining_budget();

        slog(&session_log, |l| l.turn_start(turn, budget_pct, remaining));

        emit(
            &bus,
            || AppEvent::TurnStarted {
                turn,
                budget_pct,
                remaining,
            },
            || {
                println!(
                    "[Turn {}] Sending to model... {}",
                    turn,
                    conversation.budget_summary()
                )
            },
        );

        // Log the full messages array being sent to the API
        slog(&session_log, |l| {
            if let Ok(json) = serde_json::to_string_pretty(conversation.messages()) {
                l.messages_input(&json);
            }
        });

        let response = {
            const STREAM_RETRIES: u32 = 3;
            let mut last_stream_err = None;
            let mut resp = None;
            for attempt in 0..=STREAM_RETRIES {
                let stream_bus = bus.clone();
                let on_stream_event = move |event: crate::provider::StreamEvent| {
                    if let crate::provider::StreamEvent::Delta(ref text) = event {
                        if let Some(ref b) = stream_bus {
                            b.send(AppEvent::ModelResponseDelta { text: text.clone() });
                        }
                    }
                };
                match provider
                    .chat_stream(conversation.messages(), &on_stream_event)
                    .await
                {
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
                            tokio::time::sleep(delay).await;
                            last_stream_err = Some(e);
                            continue;
                        }
                        slog(&session_log, |l| l.error(&format!("API error: {}", e)));
                        emit(
                            &bus,
                            || AppEvent::LoopError(e.to_string()),
                            || eprintln!("Error: {}", e),
                        );
                        return Err(e);
                    }
                }
            }
            match resp {
                Some(r) => r,
                None => {
                    let e = last_stream_err.unwrap_or_else(|| {
                        CallerError::Provider("Stream failed after retries".to_string())
                    });
                    slog(&session_log, |l| l.error(&format!("API error: {}", e)));
                    emit(
                        &bus,
                        || AppEvent::LoopError(e.to_string()),
                        || eprintln!("Error: {}", e),
                    );
                    return Err(e);
                }
            }
        };
        conversation.set_usage(response.usage.clone());

        // Auto-compact when context usage exceeds 90%
        if conversation.auto_compact() {
            slog(&session_log, |l| {
                l.info(&format!("Auto-compacted conversation at turn {}", turn))
            });
            emit(
                &bus,
                || AppEvent::ContextManagement { turn },
                || eprintln!("Context compacted at turn {}", turn),
            );
        }

        loop_stats.turns = turn;
        loop_stats.usage.prompt_tokens += response.usage.prompt_tokens;
        loop_stats.usage.completion_tokens += response.usage.completion_tokens;
        loop_stats.usage.total_tokens += response.usage.total_tokens;

        // Store assistant message — with or without tool calls
        let has_tool_calls = !response.tool_calls.is_empty();
        if has_tool_calls {
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
            emit(
                &bus,
                || AppEvent::BudgetWarning { pct, remaining },
                || {
                    eprintln!(
                        "WARNING: Context budget is running low ({:.0}% used, {} tokens remaining)",
                        pct, remaining,
                    )
                },
            );
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

        emit(
            &bus,
            || AppEvent::ModelResponse {
                turn,
                content: response.content.clone(),
                usage: response.usage.clone(),
                reasoning: response.reasoning_summary.clone(),
            },
            || {
                println!("Model response:\n{}", response.content);
                println!();
            },
        );

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
                emit(
                    &bus,
                    || AppEvent::DoneSignal {
                        message: batch.done_message.clone(),
                    },
                    || {
                        if let Some(ref msg) = batch.done_message {
                            println!("{}", msg);
                        }
                        println!("--- Task complete ---");
                    },
                );
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

            // Headless askHuman check
            if bus.is_none() && has_ask_human_command(&json_str) {
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
                            // Deny is highest priority — pick highest severity among denies
                            if need.is_none_or(|(prev, _)| cat.severity() > prev.severity()) {
                                need = Some((cat, true));
                            }
                        } else if autonomy_state.needs_approval(cat) {
                            // Among non-deny approvals, pick highest severity
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
                slog(&session_log, |l| {
                    l.approval(&cat.to_string(), &preview, "waiting")
                });

                if denied_by_policy {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "denied-policy")
                    });
                    emit(
                        &bus,
                        || AppEvent::TaskComplete {
                            reason: format!("Denied by policy ({})", cat),
                        },
                        || println!("--- Denied by policy ({}) ---", cat),
                    );
                    return Ok((loop_stats, LoopExitReason::Denied));
                }

                if let Some(ref bus_ref) = bus {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    bus_ref.send(AppEvent::ApprovalRequired {
                        id: turn as u64,
                        command_preview: preview.clone(),
                        category: cat,
                        responder: tx,
                    });
                    match rx.await {
                        Ok(tui::event::ApprovalResponse::Approve) => {
                            slog(&session_log, |l| {
                                l.approval(&cat.to_string(), &preview, "approved")
                            });
                        }
                        Ok(tui::event::ApprovalResponse::ApproveAll) => {
                            slog(&session_log, |l| {
                                l.approval(&cat.to_string(), &preview, "approve-all")
                            });
                            let mut state = autonomy.write().await;
                            state.level = AutonomyLevel::Full;
                        }
                        Ok(tui::event::ApprovalResponse::Skip) => {
                            slog(&session_log, |l| {
                                l.approval(&cat.to_string(), &preview, "skipped")
                            });
                            should_skip = true;
                        }
                        Ok(tui::event::ApprovalResponse::Deny) | Err(_) => {
                            slog(&session_log, |l| {
                                l.approval(&cat.to_string(), &preview, "denied")
                            });
                            emit(
                                &bus,
                                || AppEvent::TaskComplete {
                                    reason: "Denied by user".to_string(),
                                },
                                || println!("--- Denied by user ---"),
                            );
                            return Ok((loop_stats, LoopExitReason::Denied));
                        }
                    }
                }
                if bus.is_none() {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "denied-no-approver")
                    });
                    emit(
                        &bus,
                        || AppEvent::TaskComplete {
                            reason: format!("Approval required in headless mode ({})", cat),
                        },
                        || println!("--- Approval required in headless mode ({}) ---", cat),
                    );
                    return Ok((loop_stats, LoopExitReason::Denied));
                }
            } else {
                // Commands auto-approved — log for visibility at Normal verbosity
                let preview = format_command_preview(&json_str);
                if !preview.is_empty() {
                    emit(
                        &bus,
                        || AppEvent::AutoApproved {
                            preview: preview.clone(),
                        },
                        || {},
                    );
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
            maybe_auto_launch_xvfb(&json_str, &mut xvfb_guard, provider.name(), &session_log, &bus)
                .await;
            let preview = json_str.chars().take(300).collect::<String>();
            emit(
                &bus,
                || AppEvent::AgentStarted {
                    turn,
                    commands_preview: preview.clone(),
                },
                || println!("[Turn {}] Running agent...", turn),
            );

            let output = agent_runner::run_agent(&json_str, log_dir).await?;

            // Log agent output
            slog(&session_log, |l| {
                l.agent_output(&output.stdout, &output.stderr)
            });

            emit(
                &bus,
                || AppEvent::AgentOutput {
                    stdout: output.stdout.clone(),
                    stderr: output.stderr.clone(),
                },
                || {
                    println!("Agent stdout:\n{}", output.stdout);
                    if !output.stderr.is_empty() {
                        eprintln!("Agent stderr:\n{}", output.stderr);
                    }
                },
            );

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
                        conversation.add_tool_result_with_images(
                            call_id, tool_name, &text, images,
                        );
                        continue;
                    }
                }
                conversation.add_tool_result(call_id, tool_name, &text);
            }
        } else {
            // --- Legacy text extraction path ---

            // Extract JSON from response
            let json_str = match extract_json(&response.content) {
                Some(json) => json.to_string(),
                None => {
                    slog(&session_log, |l| {
                        l.info("No JSON found in response — task complete")
                    });
                    emit(
                        &bus,
                        || AppEvent::TaskComplete {
                            reason: "Task complete".to_string(),
                        },
                        || println!("--- Task complete ---"),
                    );
                    exit_reason = LoopExitReason::TaskComplete;
                    break;
                }
            };

            slog(&session_log, |l| l.json_extracted(&json_str));

            emit(
                &bus,
                || AppEvent::JsonExtracted {
                    preview: json_str.chars().take(100).collect(),
                },
                || {},
            );

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
                    emit(
                        &bus,
                        || AppEvent::DoneSignal {
                            message: message.clone(),
                        },
                        || {
                            if let Some(ref msg) = message {
                                println!("{}", msg);
                            }
                            println!("--- Task complete ---");
                        },
                    );
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
                    emit(
                        &bus,
                        || AppEvent::ContextManagement { turn },
                        || println!("[Turn {}] Context management only, continuing...", turn),
                    );
                    conversation.add_user("Context updated.".to_string());
                    continue;
                } else {
                    empty_command_streak += 1;
                    if empty_command_streak >= 2 {
                        slog(&session_log, |l| {
                            l.info("No commands across consecutive turns — task complete")
                        });
                        emit(
                            &bus,
                            || AppEvent::TaskComplete {
                                reason: "Task complete".to_string(),
                            },
                            || println!("--- Task complete ---"),
                        );
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

            // In headless mode there is no askHuman input panel.
            if bus.is_none() && has_ask_human_command(&json_str) {
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
                            // Deny is highest priority — pick highest severity among denies
                            if need.is_none_or(|(prev, _)| cat.severity() > prev.severity()) {
                                need = Some((cat, true));
                            }
                        } else if autonomy_state.needs_approval(cat) {
                            // Among non-deny approvals, pick highest severity
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
                slog(&session_log, |l| {
                    l.approval(&cat.to_string(), &preview, "waiting")
                });

                if denied_by_policy {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "denied-policy")
                    });
                    emit(
                        &bus,
                        || AppEvent::TaskComplete {
                            reason: format!("Denied by policy ({})", cat),
                        },
                        || println!("--- Denied by policy ({}) ---", cat),
                    );
                    return Ok((loop_stats, LoopExitReason::Denied));
                }

                if let Some(ref bus_ref) = bus {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    bus_ref.send(AppEvent::ApprovalRequired {
                        id: turn as u64,
                        command_preview: preview.clone(),
                        category: cat,
                        responder: tx,
                    });
                    match rx.await {
                        Ok(tui::event::ApprovalResponse::Approve) => {
                            slog(&session_log, |l| {
                                l.approval(&cat.to_string(), &preview, "approved")
                            });
                        }
                        Ok(tui::event::ApprovalResponse::ApproveAll) => {
                            slog(&session_log, |l| {
                                l.approval(&cat.to_string(), &preview, "approve-all")
                            });
                            let mut state = autonomy.write().await;
                            state.level = AutonomyLevel::Full;
                        }
                        Ok(tui::event::ApprovalResponse::Skip) => {
                            slog(&session_log, |l| {
                                l.approval(&cat.to_string(), &preview, "skipped")
                            });
                            should_skip = true;
                        }
                        Ok(tui::event::ApprovalResponse::Deny) | Err(_) => {
                            slog(&session_log, |l| {
                                l.approval(&cat.to_string(), &preview, "denied")
                            });
                            emit(
                                &bus,
                                || AppEvent::TaskComplete {
                                    reason: "Denied by user".to_string(),
                                },
                                || println!("--- Denied by user ---"),
                            );
                            return Ok((loop_stats, LoopExitReason::Denied));
                        }
                    }
                }
                if bus.is_none() {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "denied-no-approver")
                    });
                    emit(
                        &bus,
                        || AppEvent::TaskComplete {
                            reason: format!("Approval required in headless mode ({})", cat),
                        },
                        || println!("--- Approval required in headless mode ({}) ---", cat),
                    );
                    return Ok((loop_stats, LoopExitReason::Denied));
                }
            } else {
                // Commands auto-approved — log for visibility at Normal verbosity
                let preview = format_command_preview(&json_str);
                if !preview.is_empty() {
                    emit(
                        &bus,
                        || AppEvent::AutoApproved {
                            preview: preview.clone(),
                        },
                        || {},
                    );
                }
            }

            if should_skip {
                conversation.add_user("Command skipped by user.".to_string());
                continue;
            }

            // Log the full JSON being sent to the agent
            slog(&session_log, |l| l.agent_input(&json_str));
            maybe_auto_launch_xvfb(&json_str, &mut xvfb_guard, provider.name(), &session_log, &bus)
                .await;

            let preview = json_str.chars().take(300).collect::<String>();
            emit(
                &bus,
                || AppEvent::AgentStarted {
                    turn,
                    commands_preview: preview.clone(),
                },
                || println!("[Turn {}] Running agent...", turn),
            );

            let output = agent_runner::run_agent(&json_str, log_dir).await?;

            // Log agent output
            slog(&session_log, |l| {
                l.agent_output(&output.stdout, &output.stderr)
            });

            emit(
                &bus,
                || AppEvent::AgentOutput {
                    stdout: output.stdout.clone(),
                    stderr: output.stderr.clone(),
                },
                || {
                    println!("Agent stdout:\n{}", output.stdout);
                    if !output.stderr.is_empty() {
                        eprintln!("Agent stderr:\n{}", output.stderr);
                    }
                },
            );

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
                    emit(
                        &bus,
                        || AppEvent::SubAgentResult {
                            formatted: msg.clone(),
                        },
                        || println!("{}", msg),
                    );
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
            emit(
                &bus,
                || AppEvent::SafetyCapReached,
                || println!("--- Safety cap ({}) reached ---", SAFETY_CAP),
            );
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
    bus: Option<EventBus>,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: &std::path::Path,
    mcp_mgr: Option<&mcp_client::McpClientManager>,
    mut follow_up_rx: FollowUpReceiver,
) -> Result<LoopStats, CallerError> {
    let mut round = 1usize;
    let mut cumulative_stats = LoopStats::default();

    loop {
        let (stats, exit_reason) = run_agent_loop(
            provider,
            conversation,
            project,
            sub_agent_mode,
            bus.clone(),
            autonomy.clone(),
            session_log.clone(),
            log_dir,
            mcp_mgr,
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
                // Emit RoundComplete event
                let turns_in_round = stats.turns;
                emit(
                    &bus,
                    || AppEvent::RoundComplete {
                        round,
                        turns_in_round,
                    },
                    || println!("--- Round {} complete ({} turns) ---", round, turns_in_round),
                );

                // Wait for follow-up message
                match follow_up_rx.recv().await {
                    Some(message) => {
                        round += 1;
                        slog(&session_log, |l| {
                            l.info(&format!("Round {} follow-up: {}", round, &message))
                        });
                        conversation.add_user(message);
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
            | LoopExitReason::Error => {
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
    let result = run_agent_loop(
        provider.as_ref(),
        &mut conversation,
        &project,
        Some(&sub_agent_info),
        None, // no TUI for sub-agents
        autonomy,
        session_log,
        &log_dir,
        None, // no MCP client for sub-agents
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
        let (status, summary, usage) = match &result {
            Ok(stats) => (
                sub_agent::SubAgentStatus::Completed,
                "Task completed successfully".to_string(),
                stats.usage.clone(),
            ),
            Err(e) => (
                sub_agent::SubAgentStatus::Failed(e.to_string()),
                format!("Task failed: {}", e),
                provider::TokenUsage::default(),
            ),
        };

        let agent_result = sub_agent::SubAgentResult {
            id,
            status,
            summary,
            findings: vec![],
            artifacts: vec![],
            usage,
        };
        let _ = sub_agent::write_result(std::path::Path::new(&result_path), &agent_result);
    }

    result
}

/// Run with the presence layer mediating between user and agent loop.
/// The presence layer has its own small-model provider and conversation,
/// delegates tasks to the agent loop via a channel, and narrates events.
#[allow(clippy::too_many_arguments)]
async fn run_with_presence(
    task: Option<String>,
    project: Project,
    bus: EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
    mut user_rx: tokio::sync::mpsc::Receiver<String>,
    response_tx: tokio::sync::mpsc::Sender<String>,
    presence_event_rx: tokio::sync::mpsc::Receiver<presence::PresenceEvent>,
) -> Result<LoopStats, CallerError> {
    // 1. Create presence provider (small/fast model)
    let presence_provider = provider::select_presence_provider(
        project.config.presence.provider.as_deref(),
        project.config.presence.model.as_deref(),
    )?;
    bus.send(AppEvent::PresenceUsageUpdate {
        total_tokens: 0,
        context_window: project.config.presence.context_window,
        usage_pct: 0.0,
        provider: presence_provider.name().to_string(),
        model: presence_provider.model().to_string(),
    });

    // 2. Resolve presence system prompt
    let presence_prompt = prompts::resolve_system_prompt(
        &sub_agent::SubAgentRole::Presence,
        Some(&project.root),
    )?;

    // 3. Create channels
    let (task_tx, mut task_rx) = tokio::sync::mpsc::channel::<presence::TaskEnvelope>(4);
    // The presence_event_rx is fed by the TUI via App.forward_to_presence() →
    // presence_event_tx, which was created and wired by the caller (TUI branch).
    let presence_event_rx = presence_event_rx;

    // 4. Create shared agent state
    let agent_state = Arc::new(Mutex::new(presence::AgentStateSnapshot::default()));

    // 5. Create presence layer
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
    );

    // 8. Send initial task to presence (if provided), with a timeout so a
    //    slow or misconfigured presence provider doesn't freeze the TUI.
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
                bus.send(AppEvent::LoopError(format!(
                    "Presence provider error: {}. Falling through to direct agent.",
                    e
                )));
            }
            Err(_) => {
                bus.send(AppEvent::LoopError(
                    "Presence provider timed out (30s). Falling through to direct agent."
                        .to_string(),
                ));
            }
            _ => {}
        }
    }

    // 9. Main loop: process tasks from presence + user follow-ups
    let mut cumulative_stats = LoopStats::default();
    let project_root = project.root.clone();

    loop {
        tokio::select! {
            // User sends follow-up text → route to presence
            Some(input) = user_rx.recv() => {
                match tokio::time::timeout(
                    tokio::time::Duration::from_secs(30),
                    presence.process_user_input(&input),
                ).await {
                    Ok(Ok(response)) if !response.is_empty() => {
                        let _ = response_tx.send(response).await;
                    }
                    Ok(Err(e)) => {
                        let _ = response_tx.send(format!("Presence error: {}", e)).await;
                    }
                    Err(_) => {
                        let _ = response_tx.send("Presence provider timed out.".to_string()).await;
                    }
                    _ => {}
                }
            }
            // Presence submitted a task → run agent loop
            Some(envelope) = task_rx.recv() => {
                slog(&session_log, |l| {
                    l.info(&format!("Presence dispatched task: {}", envelope.task));
                });

                // Create a fresh provider for each dispatched task
                let task_provider = match provider::select_provider() {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = response_tx.send(format!("Provider error: {}", e)).await;
                        continue;
                    }
                };
                let task_project = match Project::from_root(project_root.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = response_tx.send(format!("Project error: {}", e)).await;
                        continue;
                    }
                };

                let (follow_up_tx, follow_up_rx) = tokio::sync::mpsc::channel::<String>(1);
                drop(follow_up_tx); // single-round for delegated tasks

                let result = if envelope.force_direct || is_simple_task(&envelope.task) {
                    run_direct_mode(
                        task_provider,
                        envelope.task,
                        task_project,
                        Some(bus.clone()),
                        autonomy.clone(),
                        session_log.clone(),
                        log_dir.clone(),
                        None,
                        follow_up_rx,
                    )
                    .await
                } else {
                    run_user_mode(
                        task_provider,
                        envelope.task,
                        task_project,
                        Some(bus.clone()),
                        autonomy.clone(),
                        session_log.clone(),
                    )
                    .await
                };

                match result {
                    Ok(stats) => {
                        cumulative_stats.turns += stats.turns;
                        cumulative_stats.rounds += stats.rounds;
                        cumulative_stats.usage.prompt_tokens += stats.usage.prompt_tokens;
                        cumulative_stats.usage.completion_tokens += stats.usage.completion_tokens;
                        cumulative_stats.usage.total_tokens += stats.usage.total_tokens;
                    }
                    Err(e) => {
                        let _ = response_tx
                            .send(format!("Task error: {}", e))
                            .await;
                    }
                }
            }
            else => break,
        }
    }

    Ok(cumulative_stats)
}

async fn run_user_mode(
    _provider: Box<dyn provider::ChatProvider>,
    task: String,
    project: Project,
    bus: Option<EventBus>,
    _autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
) -> Result<LoopStats, CallerError> {
    slog(&session_log, |l| {
        l.info("Mode: user (orchestrator subprocess)");
    });
    emit(
        &bus,
        || AppEvent::OrchestratorProgress {
            turn: 0,
            status: "spawning".to_string(),
            last_action: String::new(),
        },
        || println!("Mode: user (spawning orchestrator subprocess)"),
    );

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

    // Capture stderr in a background task
    let stderr = child.stderr.take();
    let bus_stderr = bus.clone();
    let session_log_stderr = session_log.clone();
    let stderr_handle = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            use tokio::io::AsyncBufReadExt;
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                slog(&session_log_stderr, |l| {
                    l.debug(&format!("orchestrator stderr: {}", line));
                });
                emit(
                    &bus_stderr,
                    || AppEvent::AgentOutput {
                        stdout: String::new(),
                        stderr: line.clone(),
                    },
                    || eprintln!("orchestrator: {}", line),
                );
            }
        }
    });

    // Monitor loop: poll progress file and wait for process exit
    let mut last_progress_turn: usize = 0;
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
                        emit(
                            &bus,
                            || AppEvent::OrchestratorProgress {
                                turn: progress.turn,
                                status: progress.status.clone(),
                                last_action: progress.last_action.clone(),
                            },
                            || println!("{}", user_msg),
                        );
                    }
                }
            }
        }
    };

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
    emit(
        &bus,
        || AppEvent::SubAgentResult {
            formatted: result_msg.clone(),
        },
        || println!("{}", result_msg),
    );

    let reason = match &result.status {
        sub_agent::SubAgentStatus::Completed => "Task complete".to_string(),
        sub_agent::SubAgentStatus::Failed(reason) => format!("Orchestrator failed: {}", reason),
    };
    emit(
        &bus,
        || AppEvent::TaskComplete {
            reason: reason.clone(),
        },
        || println!("--- {} ---", reason),
    );

    Ok(loop_stats)
}

async fn run_direct_mode(
    provider: Box<dyn provider::ChatProvider>,
    task: String,
    project: Project,
    bus: Option<EventBus>,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
    mcp_mgr: Option<mcp_client::McpClientManager>,
    follow_up_rx: FollowUpReceiver,
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
    if bus.is_none() {
        println!(
            "Provider: {} (context window: {})",
            provider.name(),
            provider.context_window()
        );
    }

    // Try to resume from saved conversation if it exists in this session dir
    let conv_path = log_dir.join("conversation.jsonl");
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
                conv.add_user(format!("[Session resumed] Continue with: {}", task));
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
                setup_fresh_conversation(&mut conv, &project, &task);
                conv
            }
        }
    } else {
        let mut conv = Conversation::new(system_prompt, provider.context_window());
        setup_fresh_conversation(&mut conv, &project, &task);
        conv
    };

    // Register MCP tools so providers include them in API requests
    if let Some(ref mgr) = mcp_mgr {
        tools::register_extra_tools(mgr.all_tools());
    }

    if bus.is_none() {
        println!("Task: {}", task);
        println!("---");
    }

    run_round_loop(
        provider.as_ref(),
        &mut conversation,
        &project,
        None,
        bus,
        autonomy,
        session_log,
        &log_dir,
        mcp_mgr.as_ref(),
        follow_up_rx,
    )
    .await
}

/// Set up a fresh conversation with project context, memory, and task.
fn setup_fresh_conversation(conv: &mut Conversation, project: &Project, task: &str) {
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

    // Add the task
    conv.add_user(task.to_string());
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
    // Handle broken pipe (EPIPE) gracefully instead of panicking.
    // This occurs when stdout is piped into a consumer that exits early (e.g. `grep -q`),
    // or when the terminal is killed during headless output.
    {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Check if this is a broken pipe panic from println!/write!
            let msg = if let Some(s) = info.payload().downcast_ref::<String>() {
                s.contains("Broken pipe")
            } else if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.contains("Broken pipe")
            } else {
                false
            };
            if msg {
                std::process::exit(0);
            }
            default_hook(info);
        }));
    }

    // Load .env: cwd (+ parents) first, then project root, then ~/.config/intendant/
    dotenvy::dotenv().ok();
    let project = Project::detect()?;
    dotenvy::from_path(project.root.join(".env")).ok();
    if let Some(config_dir) = dirs::config_dir() {
        dotenvy::from_path(config_dir.join("intendant").join(".env")).ok();
    }

    // Override env vars from CLI flags before provider selection
    let flags = parse_cli_flags()?;
    if flags.json_output {
        JSON_OUTPUT.store(true, Ordering::Relaxed);
    }
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

    configure_sandbox_env(&flags, &project, &log_dir);

    // Install SIGTERM handler to mark session as interrupted before exit.
    // Rust's Drop trait does not run when the process is killed by a signal,
    // so we need an explicit handler to update session_meta.json.
    {
        let signal_session_log = session_log.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigterm =
                    signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
                sigterm.recv().await;
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

    let provider = provider::select_provider()?;
    slog(&session_log, |l| {
        l.info(&format!("Provider: {}", provider.name()));
        l.info(&format!("Model: {}", provider.model()));
        l.info(&format!("Project root: {}", project.root.display()));
        l.info(&format!("Autonomy: {}", flags.autonomy));
    });

    // Check if running as a sub-agent (headless, no TUI)
    if let Some((id, role)) = sub_agent::detect_sub_agent_mode() {
        run_sub_agent_mode(provider, id, role, session_log, log_dir).await?;
        return Ok(());
    }

    // Determine whether to use TUI (needed early for task resolution)
    let use_tui =
        !flags.no_tui && !flags.mcp && io::stdin().is_terminal() && io::stdout().is_terminal();

    // Task resolution: MCP and TUI modes allow starting without a task.
    // MCP mode must NOT call get_task_from_flags_or_env() because it would
    // print to stdout and read from stdin, both reserved for JSON-RPC.
    // TUI mode can accept a task later via the follow-up input panel.
    // Headless mode still requires a task upfront.
    let task = if flags.mcp {
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

    if flags.mcp {
        // MCP mode — speaks Model Context Protocol on stdio.
        // This is architecturally a peer of the TUI: same EventBus, same UserAction contract.
        let (bus, event_rx) = EventBus::new();
        let human_question_path = tui::event::shared_question_path(log_dir.join("human_question"));
        let _human_monitor =
            tui::event::spawn_human_question_monitor(bus.clone(), human_question_path.clone());
        let _tick_handle = tui::event::spawn_tick_timer(bus.clone(), 1000);
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

        // Live gateway (WebSocket)
        let _live_handle = if flags.live {
            let broadcast_tx = if let Some(ref tx) = mcp_control_tx {
                tx.clone()
            } else {
                let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
                tx
            };
            let config = live_gateway::build_config(
                project.config.presence.live_provider.as_deref(),
                project.config.presence.live_model.as_deref(),
            );
            let handle = live_gateway::spawn_live_gateway(
                flags.live_port,
                bus.clone(),
                broadcast_tx,
                config,
            );
            slog(&session_log, |l| {
                l.info(&format!(
                    "Live gateway: http://0.0.0.0:{}",
                    flags.live_port
                ))
            });
            eprintln!(
                "Live gateway: http://0.0.0.0:{}",
                flags.live_port
            );
            Some(handle)
        } else {
            None
        };

        let mut mcp_app_state = mcp::McpAppState::new(
            provider.name().to_string(),
            provider.model().to_string(),
            autonomy.clone(),
            log_dir.clone(),
        );
        mcp_app_state.context_window = provider.context_window();
        mcp_app_state.session_id = session_log.lock().map(|l| l.session_id().to_string()).unwrap_or_default();
        mcp_app_state.task_description = task.clone().unwrap_or_default();
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
                let (follow_up_tx, follow_up_rx) = tokio::sync::mpsc::channel::<String>(1);
                {
                    let mut s = mcp_state.write().await;
                    s.follow_up_tx = Some(follow_up_tx);
                }

                let bus_clone = bus.clone();
                let task_for_summary = task_str.clone();
                let session_log_summary = session_log.clone();
                let mcp_state_cleanup = mcp_state.clone();
                tokio::spawn(async move {
                    let result = if use_orchestration {
                        run_user_mode(
                            provider,
                            task_str,
                            project,
                            Some(bus_clone.clone()),
                            autonomy,
                            session_log,
                        )
                        .await
                    } else {
                        run_direct_mode(
                            provider,
                            task_str,
                            project,
                            Some(bus_clone.clone()),
                            autonomy,
                            session_log,
                            log_dir,
                            None,
                            follow_up_rx,
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
            s.phase = tui::app::Phase::Thinking;
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
        let (bus, event_rx) = EventBus::new();

        // Spawn background tasks
        let _crossterm_handle = tui::event::spawn_crossterm_reader(bus.clone());
        let _tick_handle = tui::event::spawn_tick_timer(bus.clone(), 100);
        let _human_monitor = tui::event::spawn_human_question_monitor(
            bus.clone(),
            tui::event::shared_question_path(log_dir.join("human_question")),
        );

        // Create TUI
        let mut terminal = tui::Tui::new()
            .map_err(|e| CallerError::Tui(format!("Failed to initialize TUI: {}", e)))?;

        // Create app state
        let mut app = tui::app::App::new(
            provider.name().to_string(),
            provider.model().to_string(),
            autonomy.clone(),
            log_dir.clone(),
        );
        app.context_window = provider.context_window();
        app.session_id = session_log.lock().map(|l| l.session_id().to_string()).unwrap_or_default();
        app.task_description = task.clone().unwrap_or_default();
        app.verbosity = if flags.verbose {
            tui::app::Verbosity::Debug
        } else {
            tui::app::Verbosity::Normal
        };
        if flags.control_socket {
            let (_control_handle, control_tx) = control::spawn_control_server(bus.clone());
            app.set_control_socket(control_tx);
            app.log(
                tui::app::LogLevel::Info,
                format!("Control socket: {}", control::socket_path().display()),
            );
        }

        // Live gateway (WebSocket) — shares broadcast channel with control socket if both enabled
        let _live_handle = if flags.live {
            let broadcast_tx = if let Some(ref tx) = app.control_tx {
                tx.clone()
            } else {
                let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
                app.set_control_socket(tx.clone());
                tx
            };
            let config = live_gateway::build_config(
                project.config.presence.live_provider.as_deref(),
                project.config.presence.live_model.as_deref(),
            );
            let handle = live_gateway::spawn_live_gateway(
                flags.live_port,
                bus.clone(),
                broadcast_tx,
                config,
            );
            app.log(
                tui::app::LogLevel::Info,
                format!("Live gateway: http://0.0.0.0:{}", flags.live_port),
            );
            Some(handle)
        } else {
            None
        };

        if let Some(ref t) = task {
            app.log(tui::app::LogLevel::Info, format!("Task: {}", t));
        }

        // Determine if presence layer should be active
        let use_presence = !flags.direct
            && !flags.no_presence
            && project.config.presence.enabled;

        // Create follow-up channel for multi-round support.
        // When there is no initial task, the follow-up channel also delivers
        // the very first task from the input panel.
        let (follow_up_tx, mut follow_up_rx) = tokio::sync::mpsc::channel::<String>(1);
        app.set_follow_up_sender(follow_up_tx);

        // If no task was provided, start in follow-up mode so the user sees
        // the input panel immediately.
        if task.is_none() {
            app.current_phase = tui::app::Phase::WaitingFollowUp;
            app.mode = tui::app::AppMode::FollowUp;
            let mut textarea = tui_textarea::TextArea::default();
            textarea.set_cursor_line_style(ratatui::style::Style::default());
            app.follow_up_textarea = Some(textarea);
            app.log(
                tui::app::LogLevel::Info,
                "Ready. Enter a task to get started.".to_string(),
            );
        }

        // If presence is active, create channels for user ↔ presence communication
        let (presence_user_rx, presence_event_rx_for_task) = if use_presence {
            let (presence_tx, presence_user_rx) =
                tokio::sync::mpsc::channel::<String>(4);
            app.set_presence_sender(presence_tx);

            // Create presence event channel: TUI forwards filtered events here
            let (presence_event_tx, presence_event_rx) =
                tokio::sync::mpsc::channel::<presence::PresenceEvent>(64);
            app.set_presence_event_sender(presence_event_tx);

            app.log(tui::app::LogLevel::Info, "Presence layer active".to_string());
            (Some(presence_user_rx), Some(presence_event_rx))
        } else {
            (None, None)
        };

        // Spawn the agent loop in a background task
        let bus_clone = bus.clone();
        let autonomy_clone = autonomy.clone();
        let session_log_clone = session_log.clone();
        let session_log_summary = session_log.clone();
        let log_dir_clone = log_dir.clone();
        let mcp_mgr = if !project.config.mcp_servers.is_empty() {
            Some(mcp_client::McpClientManager::connect_all(&project.config.mcp_servers).await)
        } else {
            None
        };
        let force_direct = flags.direct;
        let mut loop_handle = if use_presence {
            // Presence mode: the presence layer mediates between user and agent
            let presence_user_rx = presence_user_rx.unwrap();
            let presence_event_rx = presence_event_rx_for_task.unwrap();
            let (response_tx, mut response_rx) =
                tokio::sync::mpsc::channel::<String>(8);

            // Forward presence responses to TUI as log entries + reset phase
            let bus_for_responses = bus_clone.clone();
            let _response_forwarder = tokio::spawn(async move {
                while let Some(response) = response_rx.recv().await {
                    if !response.is_empty() {
                        // Use LoopError for errors, ModelResponseDelta for normal responses
                        if response.starts_with("Presence error:") || response.starts_with("Presence provider timed out") {
                            bus_for_responses.send(AppEvent::LoopError(response));
                        } else {
                            bus_for_responses.send(AppEvent::ModelResponseDelta {
                                text: format!("\n[Intendant] {}\n", response),
                            });
                            // Reset to follow-up phase after presence responds
                            bus_for_responses.send(AppEvent::RoundComplete {
                                round: 0,
                                turns_in_round: 0,
                            });
                        }
                    }
                }
            });

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
                            l.write_summary(
                                "(presence)",
                                &format!("error: {}", e),
                                0,
                            )
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
                                l.info(&format!("Task (from input): {}", first_task))
                            });
                            bus_clone.send(AppEvent::TurnStarted {
                                turn: 0,
                                budget_pct: 0.0,
                                remaining: 0,
                            });
                            (first_task, follow_up_rx)
                        }
                        None => return, // channel closed before a task arrived
                    }
                };

                let result = if force_direct || is_simple_task(&task_str) {
                    run_direct_mode(
                        provider,
                        task_str,
                        project,
                        Some(bus_clone.clone()),
                        autonomy_clone,
                        session_log_clone,
                        log_dir_clone,
                        mcp_mgr,
                        follow_up_rx,
                    )
                    .await
                } else {
                    run_user_mode(
                        provider,
                        task_str,
                        project,
                        Some(bus_clone.clone()),
                        autonomy_clone,
                        session_log_clone,
                    )
                    .await
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
                            l.write_summary(
                                "(tui)",
                                &format!("error: {}", e),
                                0,
                            )
                        });
                        bus_clone.send(AppEvent::LoopError(e.to_string()));
                    }
                }
            })
        };

        // Run the TUI event loop (blocks until quit)
        let _ = terminal.run(&mut app, event_rx).await;

        // Drop the App (and its follow_up_tx) so the round loop's recv()
        // returns None and exits gracefully, allowing write_summary to run.
        drop(app);

        // Give the agent task a moment to finish writing the session summary.
        // If it doesn't finish in time (e.g. stuck on an API call), abort it.
        match tokio::time::timeout(std::time::Duration::from_secs(5), &mut loop_handle).await {
            Ok(_) => {} // task finished naturally
            Err(_) => loop_handle.abort(), // timed out — force stop
        }

        control::cleanup();
        terminal
            .restore()
            .map_err(|e| CallerError::Tui(e.to_string()))?;
    } else {
        // Headless mode always has a task (enforced above).
        let task = task.unwrap();

        // Headless mode (--no-tui or non-TTY)

        // Live gateway in headless mode needs an EventBus + broadcast channel
        let headless_bus = if flags.live {
            let (bus, _rx) = EventBus::new();
            let (broadcast_tx, _) = tokio::sync::broadcast::channel::<String>(256);
            let config = live_gateway::build_config(
                project.config.presence.live_provider.as_deref(),
                project.config.presence.live_model.as_deref(),
            );
            let _live_handle = live_gateway::spawn_live_gateway(
                flags.live_port,
                bus.clone(),
                broadcast_tx,
                config,
            );
            eprintln!(
                "Live gateway: http://0.0.0.0:{}",
                flags.live_port
            );
            Some(bus)
        } else {
            None
        };

        let mcp_mgr = if !project.config.mcp_servers.is_empty() {
            Some(mcp_client::McpClientManager::connect_all(&project.config.mcp_servers).await)
        } else {
            None
        };

        // Create follow-up channel. In JSON mode, spawn a stdin reader to enable
        // follow-up via stdin lines. Otherwise, drop the sender immediately so
        // recv() returns None → single-round behavior.
        let (follow_up_tx, follow_up_rx) = tokio::sync::mpsc::channel::<String>(1);
        if flags.json_output {
            // JSON mode: read follow-up lines from stdin
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
                    if follow_up_tx.send(line).await.is_err() {
                        break; // receiver dropped
                    }
                }
            });
        } else {
            drop(follow_up_tx); // single-round: recv() returns None immediately
        }

        let result = if flags.direct || is_simple_task(&task) {
            run_direct_mode(
                provider,
                task.clone(),
                project,
                headless_bus,
                autonomy,
                session_log.clone(),
                log_dir,
                mcp_mgr,
                follow_up_rx,
            )
            .await
        } else {
            run_user_mode(
                provider,
                task.clone(),
                project,
                None,
                autonomy,
                session_log.clone(),
            )
            .await
        };
        match &result {
            Ok(stats) => slog(&session_log, |l| {
                l.write_summary_with_rounds(&task, "completed", stats.turns, Some(stats.rounds))
            }),
            Err(e) => slog(&session_log, |l| {
                l.write_summary(&task, &format!("error: {}", e), 0)
            }),
        }
        result?;
    }

    Ok(())
}
