mod agent_runner;
mod autonomy;
mod computer_use;
mod control;
mod conversation;
mod debug;
mod error;
mod event;
mod frames;
mod frontend;
mod knowledge;
mod live_audio;
mod live_audio_types;
mod mcp;
mod mcp_client;
mod presence;
mod project;
mod audio_routing;
mod quarantine;
mod recording;
mod prompts;
mod provider;
mod sandbox;
mod schema_validator;
mod session_log;
mod skills;
mod sub_agent;
mod tool_batch;
mod tools;
mod transcription;
mod tui;
mod types;
mod user_mode;
mod vision;
mod app_state_pricing;
mod web_gateway;
mod worktree;

use autonomy::{AutonomyLevel, AutonomyState, SharedAutonomy};
use conversation::Conversation;
use error::CallerError;
use event::{AppEvent, EventBus};
use project::Project;
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tool_batch::{assemble_batch_from_tool_calls, map_results_to_tool_responses};

type SharedSessionLog = Arc<Mutex<session_log::SessionLog>>;

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
    /// Last model response content (for sub-agent result summaries).
    last_response: Option<String>,
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
    /// --web [PORT]: Serve TUI via web (xterm.js + optional voice).
    web: bool,
    web_port: u16,
    /// --transcription: Enable user speech transcription.
    transcription: bool,
    /// --record-display <ID>: Record an existing X11 display (repeatable).
    record_displays: Vec<u32>,
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
    println!("    --web [PORT]           Serve TUI via web (xterm.js + optional voice, default port: 8765)");
    println!("    --transcription       Enable user speech transcription");
    println!("    --record-display <ID> Record an existing X11 display (e.g. 50 for :50, repeatable)");
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
        web: false,
        web_port: web_gateway::DEFAULT_PORT,
        transcription: false,
        record_displays: Vec::new(),
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
            "--web" => {
                flags.web = true;
                // --web serves the TUI via web (xterm.js). Use --web --mcp
                // for voice-only MCP mode without the web TUI.
                // Optional port argument (next arg if it's numeric)
                if i + 1 < args.len() && args[i + 1].parse::<u16>().is_ok() {
                    flags.web_port = args[i + 1].parse().unwrap();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--transcription" => {
                flags.transcription = true;
                i += 1;
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
    bus: &EventBus,
) {
    if xvfb_guard.is_some() {
        return;
    }
    if !has_capture_screen_command(json_str) && !has_exec_command(json_str) {
        return;
    }
    // If a display is already accessible (e.g. DISPLAY was set before launch),
    // emit DisplayReady so the web UI knows about it, but skip Xvfb launch.
    if vision::is_display_accessible() {
        let display_id = std::env::var("DISPLAY")
            .ok()
            .and_then(|d| d.trim_start_matches(':').parse::<u32>().ok())
            .unwrap_or(99);
        let (width, height) = query_display_resolution(display_id);
        // Check if x11vnc is already running on this display
        let vnc_port = vision::detect_vnc_port(display_id);
        slog(session_log, |l| {
            l.info(&format!(
                "Using existing display :{} ({}x{}, vnc={:?})",
                display_id, width, height, vnc_port
            ))
        });
        bus.send(AppEvent::DisplayReady {
            display_id,
            vnc_port,
            width,
            height,
        });
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
            bus.send(AppEvent::DisplayReady {
                display_id,
                vnc_port,
                width: config.width,
                height: config.height,
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

/// Query the resolution of an existing X11 display via xdpyinfo.
/// Returns (width, height) or a default of (1280, 720) if detection fails.
fn query_display_resolution(display_id: u32) -> (u32, u32) {
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

/// Start recording external displays (--record-display) directly on the registry.
/// Also emits DisplayReady so the web UI shows the display slot.
async fn start_external_display_recordings(
    displays: &[u32],
    registry: &std::sync::Arc<tokio::sync::RwLock<recording::RecordingRegistry>>,
    bus: &EventBus,
) {
    for &id in displays {
        let (width, height) = query_display_resolution(id);
        eprintln!(
            "Recording external display :{} ({}x{})",
            id, width, height
        );
        let mut reg = registry.write().await;
        if !reg.is_enabled() {
            eprintln!("Recording not enabled in config — skipping :{}",id);
            continue;
        }
        if !recording::is_ffmpeg_available() {
            eprintln!("ffmpeg not available — skipping :{}", id);
            continue;
        }
        match reg.start_external_display(id, width, height).await {
            Ok(stream_name) => {
                bus.send(AppEvent::DisplayReady {
                    display_id: id,
                    vnc_port: None,
                    width,
                    height,
                });
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
    fn parse_brief_found() {
        let text = "I did a bunch of work.\n\nBRIEF: Implemented the login feature and added tests.";
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
            web: false,
            web_port: web_gateway::DEFAULT_PORT,
            transcription: false,
            record_displays: Vec::new(),
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
            transcription: false,
            record_displays: Vec::new(),
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
            transcription: false,
            record_displays: Vec::new(),
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
            bus.send(AppEvent::BudgetExhausted { remaining });
            exit_reason = LoopExitReason::BudgetExhausted;
            break;
        }

        // Drain context injection queue (display takeover messages, presence interjections, etc.)
        if let Ok(mut q) = context_injection.lock() {
            for inj in q.drain(..) {
                if inj.images.is_empty() {
                    conversation.add_user(format!("[System] {}", inj.text));
                } else {
                    conversation.add_user_with_images(
                        format!("[System] {}", inj.text),
                        inj.images,
                    );
                }
                slog(&session_log, |l| l.info(&format!("Context injected: {}", inj.text)));
            }
        }

        conversation.increment_turn();
        let budget_pct = conversation.usage_fraction() * 100.0;
        let remaining = conversation.remaining_budget();

        slog(&session_log, |l| l.turn_start(turn, budget_pct, remaining));

        bus.send(AppEvent::TurnStarted {
            turn,
            budget_pct,
            remaining,
        });

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
                        stream_bus.send(AppEvent::ModelResponseDelta { text: text.clone() });
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
                        bus.send(AppEvent::LoopError(e.to_string()));
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
                    bus.send(AppEvent::LoopError(e.to_string()));
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

        bus.send(AppEvent::ModelResponse {
            turn,
            content: response.content.clone(),
            usage: response.usage.clone(),
            reasoning: response.reasoning_summary.clone(),
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
                match discovered
                    .iter()
                    .find(|s| s.config.name == *skill_name)
                {
                    Some(skill) => {
                        let body = skills::load_skill_body(skill, arguments);
                        // Apply autonomy override if specified
                        if let Some(ref level_str) = skill.config.autonomy {
                            let level = AutonomyLevel::from_str_loose(level_str);
                            let mut state = autonomy.write().await;
                            state.level = level;
                            slog(&session_log, |l| {
                                l.info(&format!(
                                    "Skill '{}' set autonomy to {}",
                                    skill_name, level_str
                                ))
                            });
                        }
                        slog(&session_log, |l| {
                            l.info(&format!(
                                "Invoked skill '{}' (args: {})",
                                skill_name,
                                if arguments.is_empty() { "(none)" } else { arguments }
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

            // Handle live audio spawn requests
            for (call_id, session_id, args) in &batch.live_audio_spawns {
                let spec_result =
                    serde_json::from_value::<live_audio_types::LiveAudioSpec>(args.clone());
                match spec_result {
                    Ok(mut spec) => {
                        // Build the full system prompt from playbook + schema
                        let system_prompt = prompts::build_live_audio_prompt(
                            &spec.playbook,
                            &spec.response_schema,
                            Some(&project.root),
                        );
                        spec.playbook = system_prompt;

                        // Resolve API key for the chosen provider
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

                        // Create virtual audio bridge
                        let mut bridge = match audio_routing::create_bridge(session_id).await {
                            Ok(b) => b,
                            Err(e) => {
                                conversation.add_tool_result(
                                    call_id,
                                    "spawn_live_audio",
                                    &format!("Error creating audio bridge: {}", e),
                                );
                                continue;
                            }
                        };

                        // Set virtual devices as system defaults (global routing)
                        if let Err(e) = audio_routing::set_as_default(&mut bridge).await {
                            slog(&session_log, |l| {
                                l.warn(&format!(
                                    "Could not set audio bridge as default: {} (per-app routing may be needed)",
                                    e
                                ))
                            });
                        }

                        slog(&session_log, |l| {
                            l.info(&format!(
                                "Live audio session '{}' starting ({:?})",
                                session_id, spec.provider
                            ))
                        });

                        // Run the session (blocks until complete or timeout)
                        let result = live_audio::run_session(
                            &spec,
                            &api_key,
                            &bridge,
                            log_dir,
                            Some(bus),
                        )
                        .await;

                        // Bridge is dropped here, cleaning up PulseAudio modules
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
            if headless
                && json_approval.is_none()
                && has_ask_human_command(&json_str)
            {
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
                slog(&session_log, |l| {
                    l.approval(&cat.to_string(), &preview, "waiting")
                });

                if denied_by_policy {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "denied-policy")
                    });
                    bus.send(AppEvent::TaskComplete {
                        reason: format!("Denied by policy ({})", cat),
                        summary: None,
                    });
                    return Ok((loop_stats, LoopExitReason::Denied));
                }

                if let Some(slot) = json_approval {
                    // JSON mode: emit approval event and wait for stdin response
                    bus.send(AppEvent::ApprovalRequired {
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
                            slog(&session_log, |l| {
                                l.approval(&cat.to_string(), &preview, "denied")
                            });
                            bus.send(AppEvent::TaskComplete {
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
                        reason: format!(
                            "Approval required in headless mode ({})",
                            cat
                        ),
                        summary: None,
                    });
                    return Ok((loop_stats, LoopExitReason::Denied));
                } else {
                    // Interactive mode (TUI/MCP): approval via registry
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    approval_registry.lock().unwrap().insert(turn as u64, tx);
                    bus.send(AppEvent::ApprovalRequired {
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
                            slog(&session_log, |l| {
                                l.approval(&cat.to_string(), &preview, "denied")
                            });
                            bus.send(AppEvent::TaskComplete {
                                reason: "Denied by user".to_string(),
                                summary: None,
                            });
                            return Ok((loop_stats, LoopExitReason::Denied));
                        }
                    }
                }
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
            maybe_auto_launch_xvfb(&json_str, &mut xvfb_guard, provider.name(), &session_log, bus)
                .await;
            let preview = json_str.chars().take(300).collect::<String>();
            bus.send(AppEvent::AgentStarted {
                turn,
                commands_preview: preview.clone(),
            });

            let output = agent_runner::run_agent(&json_str, log_dir).await?;

            // Log agent output
            slog(&session_log, |l| {
                l.agent_output(&output.stdout, &output.stderr)
            });

            bus.send(AppEvent::AgentOutput {
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
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
                        conversation.add_tool_result_with_images(
                            call_id, tool_name, &text, images,
                        );
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
                ).await;
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
            ).await;
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
                        reason: "Task complete".to_string(),
                        summary: if brief.is_empty() { None } else { Some(brief.clone()) },
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
                            reason: "Task complete".to_string(),
                            summary: if brief.is_empty() { None } else { Some(brief.clone()) },
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
            if headless
                && json_approval.is_none()
                && has_ask_human_command(&json_str)
            {
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
                slog(&session_log, |l| {
                    l.approval(&cat.to_string(), &preview, "waiting")
                });

                if denied_by_policy {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "denied-policy")
                    });
                    bus.send(AppEvent::TaskComplete {
                        reason: format!("Denied by policy ({})", cat),
                        summary: None,
                    });
                    return Ok((loop_stats, LoopExitReason::Denied));
                }

                if let Some(slot) = json_approval {
                    // JSON mode: emit approval event and wait for stdin response
                    bus.send(AppEvent::ApprovalRequired {
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
                            slog(&session_log, |l| {
                                l.approval(&cat.to_string(), &preview, "denied")
                            });
                            bus.send(AppEvent::TaskComplete {
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
                        reason: format!(
                            "Approval required in headless mode ({})",
                            cat
                        ),
                        summary: None,
                    });
                    return Ok((loop_stats, LoopExitReason::Denied));
                } else {
                    // Interactive mode (TUI/MCP): approval via registry
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    approval_registry.lock().unwrap().insert(turn as u64, tx);
                    bus.send(AppEvent::ApprovalRequired {
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
                            slog(&session_log, |l| {
                                l.approval(&cat.to_string(), &preview, "denied")
                            });
                            bus.send(AppEvent::TaskComplete {
                                reason: "Denied by user".to_string(),
                                summary: None,
                            });
                            return Ok((loop_stats, LoopExitReason::Denied));
                        }
                    }
                }
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
            maybe_auto_launch_xvfb(&json_str, &mut xvfb_guard, provider.name(), &session_log, bus)
                .await;

            let preview = json_str.chars().take(300).collect::<String>();
            bus.send(AppEvent::AgentStarted {
                turn,
                commands_preview: preview.clone(),
            });

            let output = agent_runner::run_agent(&json_str, log_dir).await?;

            // Log agent output
            slog(&session_log, |l| {
                l.agent_output(&output.stdout, &output.stderr)
            });

            bus.send(AppEvent::AgentOutput {
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
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
                // Emit RoundComplete event
                let turns_in_round = stats.turns;
                bus.send(AppEvent::RoundComplete {
                    round,
                    turns_in_round,
                });

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
        true, // headless (sub-agents have no interactive UI)
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
        prompt_tokens: 0,
        completion_tokens: 0,
        cached_tokens: 0,
    });

    // 2. Resolve presence system prompt (independent of sub-agent role system)
    let presence_prompt = prompts::resolve_presence_prompt(Some(&project.root));

    // 3. task_tx/task_rx are now created by the caller and passed in.
    let fallback_task_tx = task_tx.clone();

    // 4. Create presence layer
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
        presence_paused,
        context_injection.clone(),
    );

    // 6. Send initial task to presence (if provided), with a timeout so a
    //    slow or misconfigured presence provider doesn't freeze the TUI.
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

    // If presence failed on the initial task, inject the task directly into
    // the task channel so the agent loop still runs.
    if let Some(failed_task) = presence_failed_task {
        let envelope = presence::TaskEnvelope {
            task: failed_task,
            force_direct: true,
            context_hints: vec![],
            reference_frame_ids: vec![],
        };
        let _ = fallback_task_tx.send(envelope).await;
    }
    drop(fallback_task_tx);

    // 7. Spawn presence.run() as a background task for user input + event narration.
    //    This loop handles both user messages and agent events, forwarding
    //    responses to the TUI via response_tx.
    let _presence_handle = tokio::spawn(async move {
        presence.run(user_rx, response_tx).await;
    });

    // 8. Persistent server conversation across all presence tasks.
    //    First task initializes the conversation; subsequent tasks inject new
    //    user messages into the same conversation. This preserves the server
    //    model's context across the entire presence session.
    let mut cumulative_stats = LoopStats::default();
    let project_root = project.root.clone();

    // Conversation, provider, project — created on first task, reused thereafter.
    let mut persistent_conv: Option<Conversation> = None;
    let mut persistent_provider: Option<Box<dyn provider::ChatProvider>> = None;
    let mut persistent_project: Option<Project> = None;

    while let Some(envelope) = task_rx.recv().await {
        slog(&session_log, |l| {
            l.info(&format!("Presence dispatched task: {}", envelope.task));
        });

        // Resolve frame context_hints → images
        let frame_images = resolve_frame_hints(
            &envelope.context_hints, &frame_registry
        ).await;

        // Resolve reference frames (what the user was looking at when they spoke)
        let reference_images = resolve_frame_ids(
            &envelope.reference_frame_ids, &frame_registry
        ).await;

        let has_reference_frames = !reference_images.is_empty();

        // ── Ephemeral CU task: lightweight, short-lived, no project context ──
        if has_reference_frames {
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
            let cu_provider = match provider::select_cu_provider(&proj.config.computer_use) {
                Ok(p) => p,
                Err(e) => {
                    bus.send(AppEvent::PresenceLog {
                        message: format!("CU provider error: {}", e),
                        level: Some(types::LogLevel::Error),
                        turn: None,
                    });
                    continue;
                }
            };

            slog(&session_log, |l| {
                l.info(&format!(
                    "CU task: {} (provider: {}, model: {})",
                    envelope.task, cu_provider.name(), cu_provider.model()
                ))
            });
            bus.send(AppEvent::PresenceLog {
                message: format!("Starting CU task: {}", envelope.task),
                level: None,
                turn: None,
            });

            match run_cu_task(
                cu_provider.as_ref(),
                &envelope.task,
                reference_images,
                frame_images,
                &session_log,
                &log_dir,
                &bus,
                &proj.config.computer_use,
            ).await {
                Ok(stats) => {
                    cumulative_stats.turns += stats.turns;
                    bus.send(AppEvent::PresenceLog {
                        message: format!("CU task complete ({} turns)", stats.turns),
                        level: None,
                        turn: None,
                    });
                }
                Err(e) => {
                    bus.send(AppEvent::PresenceLog {
                        message: format!("CU task error: {}", e),
                        level: Some(types::LogLevel::Error),
                        turn: None,
                    });
                }
            }
            continue;
        }

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
            let task_provider = match provider::select_provider() {
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

            slog(&session_log, |l| {
                l.info(&format!(
                    "Mode: direct (provider: {}, context: {})",
                    task_provider.name(), task_provider.context_window()
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

            // Add task with optional frame images
            if frame_images.is_empty() {
                conv.add_user(envelope.task);
            } else {
                conv.add_user_with_images(envelope.task, frame_images);
            }

            persistent_project = Some(proj);
            persistent_provider = Some(task_provider);
            persistent_conv = Some(conv);
        } else {
            // ── Subsequent task: inject into existing conversation ──
            let conv = persistent_conv.as_mut().unwrap();

            if frame_images.is_empty() {
                conv.add_user(format!("[New Task] {}", envelope.task));
            } else {
                conv.add_user_with_images(
                    format!("[New Task] {}", envelope.task),
                    frame_images,
                );
            }
        }

        // Run one round (agent loop until done/budget/error)
        let (follow_up_tx, mut follow_up_rx) = tokio::sync::mpsc::channel::<String>(1);
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
            false, // not headless
        ).await;

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
                if message.is_empty() { continue; }
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
                if message.is_empty() { continue; }
                message.to_string()
            }
            _ => {
                if message.is_empty() { continue; }
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
                    *orch_log_path_writer.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
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
        reason: reason.clone(),
        summary: Some(result.brief.clone()),
    });

    Ok(loop_stats)
}

async fn run_direct_mode(
    provider: Box<dyn provider::ChatProvider>,
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

/// Resolve `frames:` context hints into HQ images from the frame registry.
async fn resolve_frame_hints(
    hints: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> Vec<conversation::ImageData> {
    let mut images = Vec::new();
    for hint in hints {
        if let Some(frame_list) = hint.strip_prefix("frames:") {
            let reg = registry.read().await;
            for fid in frame_list.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
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

/// Maximum turns for an ephemeral CU task before giving up.
const CU_TASK_MAX_TURNS: usize = 20;

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
) -> Result<LoopStats, CallerError> {
    let mut stats = LoopStats::default();
    let mut cu_counter = 0u64;
    let backend = computer_use::DisplayBackend::from_config(&cu_config.backend);

    let display_id = std::env::var("DISPLAY")
        .ok()
        .and_then(|d| d.trim_start_matches(':').parse().ok())
        .unwrap_or(99);

    // Minimal system prompt for CU tasks
    let system_prompt = "You are a computer use agent. You interact with a desktop GUI \
        using native computer use tools. You can see the screen via screenshots and \
        perform actions like clicking, typing, scrolling, and dragging.\n\n\
        CRITICAL RULES:\n\
        - You are given ONE specific task. Perform ONLY that task and nothing else.\n\
        - Once the task is complete, STOP. Respond with just the word DONE and a one-sentence summary.\n\
        - Do NOT take additional actions after the task is finished.\n\
        - Do NOT open browsers, navigate to websites, or perform any action not explicitly requested.\n\
        - Do NOT \"explore\" or do anything beyond the exact scope of the task.\n\n\
        Workflow:\n\
        1. Take a screenshot to see the current screen state\n\
        2. Identify the target elements\n\
        3. Perform the required actions\n\
        4. Take a verification screenshot to confirm success\n\
        5. Respond with DONE and a brief summary — no further actions\n\n\
        Be precise with coordinates. Act efficiently.".to_string();

    let mut conv = Conversation::new(system_prompt, provider.context_window());

    // Inject reference frames
    if !reference_images.is_empty() {
        conv.add_user_with_images(
            "The user was looking at this screen when they made their request:".to_string(),
            reference_images,
        );
        conv.add_assistant("I can see the reference screen. I'll compare this with the current state.".to_string());
    }

    // Inject context images
    if !context_images.is_empty() {
        conv.add_user_with_images(
            "Additional context:".to_string(),
            context_images,
        );
        conv.add_assistant("Noted.".to_string());
    }

    // Add the task
    conv.add_user(task.to_string());

    // Log initial conversation state
    slog(session_log, |l| {
        l.info(&format!(
            "CU task starting: {} messages, provider={}, model={}, cu_enabled={}, cu_display={:?}",
            conv.messages().len(),
            provider.name(),
            provider.model(),
            provider.cu_enabled(),
            provider.cu_display(),
        ))
    });

    for turn in 1..=CU_TASK_MAX_TURNS {
        stats.turns = turn;

        slog(session_log, |l| l.info(&format!("CU turn {} starting", turn)));

        let response = provider.chat_stream(
            conv.messages(),
            &|event| {
                if let provider::StreamEvent::Delta(ref delta) = event {
                    bus.send(AppEvent::PresenceLog {
                        message: format!("[CU] {}", delta),
                        level: None,
                        turn: Some(turn),
                    });
                }
            },
        ).await?;

        conv.set_usage(response.usage.clone());

        // Log full turn details
        slog(session_log, |l| {
            l.info(&format!(
                "CU turn {} response: content_len={}, cu_calls={}, tool_calls={}, usage={{prompt={}, completion={}, total={}}}",
                turn,
                response.content.len(),
                response.cu_calls.len(),
                response.tool_calls.len(),
                response.usage.prompt_tokens,
                response.usage.completion_tokens,
                response.usage.total_tokens,
            ))
        });
        if !response.content.is_empty() {
            slog(session_log, |l| {
                l.info(&format!(
                    "CU turn {} text: {}",
                    turn,
                    &response.content[..response.content.len().min(500)]
                ))
            });
        }
        for (i, cu) in response.cu_calls.iter().enumerate() {
            slog(session_log, |l| {
                l.info(&format!(
                    "CU turn {} call[{}]: id={}, actions={:?}",
                    turn, i, cu.call_id,
                    cu.actions.iter().map(|a| format!("{:?}", a)).collect::<Vec<_>>()
                ))
            });
        }
        for tc in &response.tool_calls {
            slog(session_log, |l| {
                l.info(&format!(
                    "CU turn {} unexpected function call: {}({})",
                    turn, tc.name, &tc.arguments[..tc.arguments.len().min(200)]
                ))
            });
        }

        // Check for task completion
        let content_lower = response.content.to_lowercase();
        let is_done = content_lower.contains("done") && response.cu_calls.is_empty()
            && response.tool_calls.is_empty();

        // Store assistant message
        if !response.cu_calls.is_empty() {
            // CU calls: store as assistant with tool call refs
            let refs: Vec<conversation::ToolCallRef> = response.cu_calls.iter()
                .map(|cu| conversation::ToolCallRef {
                    id: cu.call_id.clone(),
                    call_id: cu.call_id.clone(),
                    name: "computer".to_string(),
                    arguments: String::new(),
                })
                .collect();
            conv.add_assistant_tool_calls(response.content.clone(), refs, response.raw_output.clone());
        } else {
            conv.add_assistant(response.content.clone());
        }

        if is_done {
            slog(session_log, |l| l.info(&format!("CU task complete at turn {}", turn)));
            break;
        }

        // Execute CU calls
        if !response.cu_calls.is_empty() {
            for cu_call in &response.cu_calls {
                slog(session_log, |l| {
                    l.info(&format!(
                        "CU turn {}: {} action(s)",
                        turn, cu_call.actions.len()
                    ))
                });

                let results = computer_use::execute_actions(
                    &cu_call.actions,
                    display_id,
                    backend,
                    log_dir,
                    &mut cu_counter,
                ).await;

                let last_screenshot = results.iter().rev().find_map(|r| r.screenshot.as_ref());
                let output = if results.iter().all(|r| r.success) {
                    "Actions executed successfully.".to_string()
                } else {
                    let errors: Vec<&str> = results.iter()
                        .filter_map(|r| r.error.as_deref())
                        .collect();
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
                l.warn(&format!(
                    "CU turn {}: no actions returned (text-only response)",
                    turn
                ))
            });
        }
        if turn >= CU_TASK_MAX_TURNS {
            slog(session_log, |l| l.warn("CU task hit max turns"));
        }
    }

    Ok(stats)
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
    let display_id = cu_display
        .map(|_| {
            // Use the display from DISPLAY env or default to 99
            std::env::var("DISPLAY")
                .ok()
                .and_then(|d| d.trim_start_matches(':').parse().ok())
                .unwrap_or(99)
        })
        .unwrap_or(99);

    for cu_call in cu_calls {
        slog(session_log, |l| {
            l.info(&format!(
                "CU: executing {} action(s) for call {}",
                cu_call.actions.len(),
                cu_call.call_id
            ))
        });

        let backend = computer_use::DisplayBackend::detect();
        let results = computer_use::execute_actions(
            &cu_call.actions,
            display_id,
            backend,
            log_dir,
            counter,
        ).await;

        // Find the last screenshot from results
        let last_screenshot = results.iter().rev().find_map(|r| r.screenshot.as_ref());
        let output = if results.iter().all(|r| r.success) {
            "Actions executed successfully.".to_string()
        } else {
            let errors: Vec<&str> = results.iter()
                .filter_map(|r| r.error.as_deref())
                .collect();
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

    // Create shared frame registry for video frame storage.
    let frame_registry: Arc<tokio::sync::RwLock<frames::FrameRegistry>> =
        Arc::new(tokio::sync::RwLock::new(frames::FrameRegistry::new(&log_dir)));

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

    configure_sandbox_env(&flags, &project, &log_dir);

    // CLI --transcription flag overrides config file setting
    if flags.transcription {
        project.config.transcription.enabled = true;
    }

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

    // Determine whether to use TUI (needed early for task resolution).
    // --web forces TUI mode (served via web) even without a real terminal.
    let use_tui = flags.web
        || (!flags.no_tui && !flags.mcp && io::stdin().is_terminal() && io::stdout().is_terminal());

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
        let bus = EventBus::new();
        let event_rx = bus.subscribe();
        let human_question_path = event::shared_question_path(log_dir.join("human_question"));
        let _human_monitor =
            event::spawn_human_question_monitor(bus.clone(), human_question_path.clone());
        let _tick_handle = event::spawn_tick_timer(bus.clone(), 1000);
        let _recording_listener = recording::spawn_recording_listener(
            bus.subscribe(), recording_registry.clone(), bus.clone(),
        );
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler = if flags.web {
            Some(debug::spawn_debug_screen_handler(
                bus.subscribe(),
                project.config.recording.clone(),
                flags.web_port,
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
        } else if flags.web {
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
        let _outbound_broadcaster = event::spawn_outbound_broadcaster(
            bus.subscribe(),
            outbound_tx.clone(),
        );

        // Wire session log writer: persists bus events that aren't logged inline.
        let _session_log_writer = event::spawn_session_log_writer(
            bus.subscribe(),
            session_log.clone(),
        );

        // Web gateway (WebSocket)
        let _web_handle = if flags.web {
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
            );
            let shared_session = Arc::new(tokio::sync::RwLock::new(
                web_gateway::ActiveSessionState {
                    query_ctx: None,
                    frame_registry: Some(frame_registry.clone()),
                    session_log: Some(session_log.clone()),
                    recording_registry: Some(recording_registry.clone()),
                },
            ));
            let handle = web_gateway::spawn_web_gateway(
                flags.web_port,
                bus.clone(),
                broadcast_tx,
                config,
                shared_session,
                transcriber,
                None, // MCP mode: no WebTui
            );
            slog(&session_log, |l| {
                l.info(&format!(
                    "Web TUI: http://0.0.0.0:{}",
                    flags.web_port
                ))
            });
            eprintln!(
                "Web TUI: http://0.0.0.0:{}",
                flags.web_port
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

                let approval_registry = mcp_state.read().await.approval_registry.clone();
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
        // In web mode (--web), key events come from WebSocket, not the terminal.
        let _crossterm_handle = if !flags.web {
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
            bus.subscribe(), recording_registry.clone(), bus.clone(),
        );
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler = if flags.web {
            Some(debug::spawn_debug_screen_handler(
                bus.subscribe(),
                project.config.recording.clone(),
                flags.web_port,
                bus.clone(),
            ))
        } else {
            None
        };

        // TUI is created later — just before run() — so that web mode
        // (--web) can use WebTui instead of the real terminal backend.

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

        // Per-connection WebTui command channel (only for --web mode).
        let (web_tui_tx, web_tui_rx) = if flags.web {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<tui::web::WebTuiCommand>();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Web gateway broadcast channel — shares with control socket if both enabled.
        // The actual web gateway spawn is deferred until after presence setup so we
        // can pass the WebQueryCtx (agent state, project root, etc.) for tool requests.
        let web_broadcast_tx = if flags.web {
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
            Some(event::spawn_outbound_broadcaster(bus.subscribe(), tx.clone()))
        } else {
            None
        };

        // Wire session log writer: persists bus events that aren't logged inline.
        let _session_log_writer = event::spawn_session_log_writer(
            bus.subscribe(),
            session_log.clone(),
        );

        if let Some(ref t) = task {
            app.log(types::LogLevel::Info, format!("Task: {}", t));
        }

        // Determine if presence layer should be active.
        // Note: --direct only forces single-agent mode for the worker; it does
        // NOT disable presence.  Use --no-presence to disable presence.
        let use_presence = !flags.no_presence
            && project.config.presence.enabled;

        // Create follow-up channel for multi-round support.
        // When there is no initial task, the follow-up channel also delivers
        // the very first task from the input panel.
        let (follow_up_tx, mut follow_up_rx) = tokio::sync::mpsc::channel::<String>(1);
        app.set_follow_up_sender(follow_up_tx);

        // If no task was provided, start in follow-up mode so the user sees
        // the input panel immediately.
        if task.is_none() {
            app.current_phase = types::Phase::WaitingFollowUp;
            app.mode = tui::app::AppMode::FollowUp;
            let mut textarea = tui_textarea::TextArea::default();
            textarea.set_cursor_line_style(ratatui::style::Style::default());
            app.follow_up_textarea = Some(textarea);
            app.log(
                types::LogLevel::Info,
                "Ready. Enter a task to get started.".to_string(),
            );
        }

        // If presence is active, create channels for user ↔ presence communication
        // and the shared agent state snapshot.
        let (presence_user_rx, presence_event_rx_for_task, presence_agent_state) = if use_presence {
            let (presence_tx, presence_user_rx) =
                tokio::sync::mpsc::channel::<String>(4);
            app.set_presence_sender(presence_tx);

            // Create presence event channel: TUI forwards filtered events here
            let (presence_event_tx, presence_event_rx) =
                tokio::sync::mpsc::channel::<presence::PresenceEvent>(64);
            app.set_presence_event_sender(presence_event_tx);

            // Shared agent state: updated by TUI (via forward_to_presence), read by presence tools
            let agent_state = Arc::new(std::sync::Mutex::new(presence::AgentStateSnapshot::default()));
            app.set_presence_agent_state(agent_state.clone());

            app.log_sourced(types::LogLevel::Info, "Presence layer active".to_string(), tui::app::LogSource::Presence, None);
            // If there's an initial task, set the phase to Thinking immediately
            // so the TUI doesn't sit at "Idle" during the presence API call.
            if task.is_some() {
                app.current_phase = types::Phase::Thinking;
            }
            (Some(presence_user_rx), Some(presence_event_rx), Some(agent_state))
        } else {
            (None, None, None)
        };

        // Create the shared PresenceSession for event replay and checkpoints
        let presence_session = {
            let sid = session_log.lock()
                .map(|l| l.session_id().to_string())
                .unwrap_or_default();
            Arc::new(Mutex::new(presence::PresenceSession::new(sid)))
        };
        app.presence_session = Some(presence_session.clone());
        app.session_log = Some(session_log.clone());

        // Deferred web gateway spawn — now we have the agent state for tool queries
        let _web_handle = if let Some(broadcast_tx) = web_broadcast_tx {
            let query_ctx = presence_agent_state.as_ref().map(|state| {
                web_gateway::WebQueryCtx {
                    agent_state: state.clone(),
                    project_root: project.root.clone(),
                    log_dir: log_dir.clone(),
                    knowledge_path: project.memory_path(),
                    presence_session: Some(presence_session.clone()),
                    context_injection: Some(app.context_injection.clone()),
                }
            });
            let transcriber: Option<std::sync::Arc<dyn transcription::Transcriber>> =
                if project.config.transcription.enabled {
                    match transcription::WhisperTranscriber::new(&project.config.transcription) {
                        Ok(t) => Some(std::sync::Arc::new(t)),
                        Err(e) => {
                            app.log(types::LogLevel::Warn, format!("Transcription init failed: {}", e));
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
            );
            let shared_session = Arc::new(tokio::sync::RwLock::new(
                web_gateway::ActiveSessionState {
                    query_ctx,
                    frame_registry: Some(frame_registry.clone()),
                    session_log: Some(session_log.clone()),
                    recording_registry: Some(recording_registry.clone()),
                },
            ));
            let handle = web_gateway::spawn_web_gateway(
                flags.web_port,
                bus.clone(),
                broadcast_tx,
                config,
                shared_session,
                transcriber,
                web_tui_tx.clone(),
            );
            app.log(
                types::LogLevel::Info,
                format!("Web TUI: http://0.0.0.0:{}", flags.web_port),
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
        let mut loop_handle = if use_presence {
            // Presence mode: the presence layer mediates between user and agent
            let presence_user_rx = presence_user_rx.unwrap();
            let presence_event_rx = presence_event_rx_for_task.unwrap();
            let agent_state = presence_agent_state.unwrap();
            let (response_tx, mut response_rx) =
                tokio::sync::mpsc::channel::<String>(8);

            // Shared paused ref-count: incremented by PresenceConnected, decremented by PresenceDisconnected.
            // Server-side presence is paused when count > 0 (any browser has active voice).
            let presence_paused = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            app.set_presence_paused_flag(presence_paused.clone());

            // Task dispatch channel: StartTask from browser/control/MCP goes
            // directly here, bypassing server-side presence.
            let (task_tx, task_rx) =
                tokio::sync::mpsc::channel::<presence::TaskEnvelope>(4);
            app.set_task_sender(task_tx.clone());

            // Forward presence responses to TUI as log entries + reset phase
            let bus_for_responses = bus_clone.clone();
            let _response_forwarder = tokio::spawn(async move {
                while let Some(response) = response_rx.recv().await {
                    if !response.is_empty() {
                        if response.starts_with("Presence error:") || response.starts_with("Presence provider timed out") {
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

        // Run the TUI event loop (blocks until quit).
        // In web mode (--web), render to a buffer and stream to xterm.js.
        // In terminal mode, render directly to stdout.
        if flags.web {
            let broadcast_tx = app.control_tx.clone().unwrap_or_else(|| {
                let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
                app.set_control_socket(tx.clone());
                tx
            });
            eprintln!("Web TUI: http://0.0.0.0:{}", flags.web_port);
            let mut web_tui = tui::web::WebTui::new(120, 40, broadcast_tx)
                .map_err(|e| CallerError::Tui(format!("Failed to initialize Web TUI: {}", e)))?;
            let cmd_rx = web_tui_rx.expect("web_tui_rx must exist in --web mode");
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
            Ok(_) => {} // task finished naturally
            Err(_) => loop_handle.abort(), // timed out — force stop
        }

        if flags.web && !session_id.is_empty() {
            bus.send(AppEvent::SessionEnded {
                session_id,
                reason: "completed".to_string(),
            });
            // Daemon mode: keep web gateway alive after TUI quits.
            // Fall through to a headless daemon loop (TUI is not re-created).
            eprintln!("TUI exited. Web gateway still running on port {}. Waiting for new tasks...", flags.web_port);
            let mut event_rx = bus.subscribe();
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        eprintln!("Shutting down.");
                        break;
                    }
                    event = event_rx.recv() => {
                        match event {
                            Ok(AppEvent::ControlCommand(event::ControlMsg::StartTask { task: new_task, orchestrate, reference_frame_ids })) => {
                                eprintln!("New session: {}", &new_task[..new_task.len().min(80)]);
                                // Create fresh session resources
                                let new_log_dir = session_log::SessionLog::resolve_path(None);
                                let new_session_log = match session_log::SessionLog::open(new_log_dir.clone()) {
                                    Ok(l) => Arc::new(Mutex::new(l)),
                                    Err(e) => { bus.send(AppEvent::LoopError(format!("Session create failed: {}", e))); continue; }
                                };
                                let new_project = match Project::from_root(project_root.clone()) {
                                    Ok(p) => p,
                                    Err(e) => { bus.send(AppEvent::LoopError(format!("Project load failed: {}", e))); continue; }
                                };

                                // CU path: when reference_frame_ids are present, run ephemeral CU task
                                if !reference_frame_ids.is_empty() {
                                    let reference_images = resolve_frame_ids(&reference_frame_ids, &frame_registry_for_events).await;
                                    if !reference_images.is_empty() {
                                        let cu_provider = match provider::select_cu_provider(&new_project.config.computer_use) {
                                            Ok(p) => p,
                                            Err(e) => { bus.send(AppEvent::LoopError(format!("CU provider failed: {}", e))); continue; }
                                        };
                                        bus.send(AppEvent::PresenceLog {
                                            message: format!("Starting CU task: {}", new_task),
                                            level: None, turn: None,
                                        });
                                        let bus_cu = bus.clone();
                                        let session_log_cu = new_session_log.clone();
                                        let cu_config = new_project.config.computer_use.clone();
                                        tokio::spawn(async move {
                                            match run_cu_task(
                                                cu_provider.as_ref(), &new_task, reference_images, vec![],
                                                &session_log_cu, &new_log_dir, &bus_cu, &cu_config,
                                            ).await {
                                                Ok(stats) => {
                                                    bus_cu.send(AppEvent::PresenceLog {
                                                        message: format!("CU task complete ({} turns)", stats.turns),
                                                        level: None, turn: None,
                                                    });
                                                }
                                                Err(e) => {
                                                    bus_cu.send(AppEvent::PresenceLog {
                                                        message: format!("CU task error: {}", e),
                                                        level: Some(types::LogLevel::Error), turn: None,
                                                    });
                                                }
                                            }
                                        });
                                        continue;
                                    }
                                }

                                let new_provider = match provider::select_provider() {
                                    Ok(p) => p,
                                    Err(e) => { bus.send(AppEvent::LoopError(format!("Provider failed: {}", e))); continue; }
                                };
                                let new_session_id = new_log_dir.file_name().and_then(|n| n.to_str()).unwrap_or("unknown").to_string();
                                bus.send(AppEvent::SessionStarted { session_id: new_session_id.clone(), task: Some(new_task.clone()) });
                                let bus_spawn = bus.clone();
                                let autonomy_spawn = autonomy.clone();
                                let session_log_spawn = new_session_log.clone();
                                let use_direct = orchestrate.map(|o| !o).unwrap_or_else(|| flags.direct || is_simple_task(&new_task));
                                tokio::spawn(async move {
                                    let (_, follow_up_rx) = tokio::sync::mpsc::channel::<String>(1);
                                    let result = if use_direct {
                                        run_direct_mode(
                                            new_provider, new_task.clone(), new_project,
                                            bus_spawn.clone(), autonomy_spawn, session_log_spawn.clone(),
                                            new_log_dir, None, follow_up_rx, None,
                                            event::ApprovalRegistry::default(), event::ContextInjectionQueue::default(), true,
                                        ).await
                                    } else {
                                        run_user_mode(
                                            new_provider, new_task.clone(), new_project,
                                            bus_spawn.clone(), autonomy_spawn, session_log_spawn.clone(),
                                        ).await
                                    };
                                    let reason = match &result {
                                        Ok(stats) => { slog(&session_log_spawn, |l| l.write_summary_with_rounds(&new_task, "completed", stats.turns, Some(stats.rounds))); "completed".to_string() }
                                        Err(e) => { slog(&session_log_spawn, |l| l.write_summary(&new_task, &format!("error: {}", e), 0)); format!("error: {}", e) }
                                    };
                                    bus_spawn.send(AppEvent::SessionEnded { session_id: new_session_id, reason });
                                });
                            }
                            Ok(_) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        }

        control::cleanup();
    } else {
        // Headless mode always has a task (enforced above).
        let task = task.unwrap();

        // Headless mode (--no-tui or non-TTY)
        let bus = EventBus::new();
        let _recording_listener = recording::spawn_recording_listener(
            bus.subscribe(), recording_registry.clone(), bus.clone(),
        );
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler = if flags.web {
            Some(debug::spawn_debug_screen_handler(
                bus.subscribe(),
                project.config.recording.clone(),
                flags.web_port,
                bus.clone(),
            ))
        } else {
            None
        };

        // Outbound broadcast channel — shared by web gateway and JSON stdout subscriber
        let (outbound_tx, _) = tokio::sync::broadcast::channel::<String>(256);

        // Wire outbound broadcaster: converts AppEvents to OutboundEvents
        let _outbound_broadcaster = event::spawn_outbound_broadcaster(
            bus.subscribe(),
            outbound_tx.clone(),
        );

        // Wire session log writer: persists bus events that aren't logged inline.
        let _session_log_writer = event::spawn_session_log_writer(
            bus.subscribe(),
            session_log.clone(),
        );

        // JSON stdout subscriber: prints OutboundEvents as JSONL to stdout
        if flags.json_output {
            let mut json_rx = outbound_tx.subscribe();
            tokio::spawn(async move {
                loop {
                    match json_rx.recv().await {
                        Ok(line) => { println!("{}", line); }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        // Web gateway in headless mode
        let headless_shared_session: Option<web_gateway::SharedActiveSession> = if flags.web {
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
            );
            let shared_session = Arc::new(tokio::sync::RwLock::new(
                web_gateway::ActiveSessionState {
                    query_ctx: None,
                    frame_registry: Some(frame_registry.clone()),
                    session_log: Some(session_log.clone()),
                    recording_registry: Some(recording_registry.clone()),
                },
            ));
            let _web_handle = web_gateway::spawn_web_gateway(
                flags.web_port,
                bus.clone(),
                outbound_tx.clone(),
                config,
                shared_session.clone(),
                transcriber,
                None, // Headless mode: no WebTui
            );
            eprintln!(
                "Web TUI: http://0.0.0.0:{}",
                flags.web_port
            );
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
        let (follow_up_tx, follow_up_rx) = tokio::sync::mpsc::channel::<String>(1);
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
                                    let resp_path =
                                        log_dir_for_stdin.join("human_response");
                                    let _ = std::fs::write(&resp_path, text.as_bytes());
                                }
                                event::ControlMsg::FollowUp { text } => {
                                    if follow_up_tx.send(text).await.is_err() {
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
                    if follow_up_tx.send(line).await.is_err() {
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

        let result = if flags.direct || is_simple_task(&task) {
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

        if flags.web {
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
            eprintln!("Session ended ({}). Web gateway running on port {}. Waiting for new tasks...", reason, flags.web_port);

            // Daemon loop: wait for StartTask from web UI or Ctrl+C
            let mut event_rx = bus.subscribe();
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        eprintln!("Shutting down.");
                        break;
                    }
                    event = event_rx.recv() => {
                        match event {
                            Ok(AppEvent::ControlCommand(event::ControlMsg::StartTask { task: new_task, orchestrate, reference_frame_ids })) => {
                                eprintln!("New session: {}", &new_task[..new_task.len().min(80)]);

                                // Create fresh session resources
                                let new_log_dir = session_log::SessionLog::resolve_path(None);
                                let new_session_log = match session_log::SessionLog::open(new_log_dir.clone()) {
                                    Ok(l) => Arc::new(Mutex::new(l)),
                                    Err(e) => {
                                        bus.send(AppEvent::LoopError(format!("Failed to create session: {}", e)));
                                        continue;
                                    }
                                };
                                let new_project = match Project::from_root(project_root.clone()) {
                                    Ok(p) => p,
                                    Err(e) => {
                                        bus.send(AppEvent::LoopError(format!("Failed to load project: {}", e)));
                                        continue;
                                    }
                                };

                                // CU path: when reference_frame_ids are present, run ephemeral CU task
                                if !reference_frame_ids.is_empty() {
                                    let reference_images = resolve_frame_ids(&reference_frame_ids, &frame_registry).await;
                                    if !reference_images.is_empty() {
                                        let cu_provider = match provider::select_cu_provider(&new_project.config.computer_use) {
                                            Ok(p) => p,
                                            Err(e) => { bus.send(AppEvent::LoopError(format!("CU provider failed: {}", e))); continue; }
                                        };
                                        bus.send(AppEvent::PresenceLog {
                                            message: format!("Starting CU task: {}", new_task),
                                            level: None, turn: None,
                                        });
                                        let bus_cu = bus.clone();
                                        let session_log_cu = new_session_log.clone();
                                        let cu_config = new_project.config.computer_use.clone();
                                        tokio::spawn(async move {
                                            match run_cu_task(
                                                cu_provider.as_ref(), &new_task, reference_images, vec![],
                                                &session_log_cu, &new_log_dir, &bus_cu, &cu_config,
                                            ).await {
                                                Ok(stats) => {
                                                    bus_cu.send(AppEvent::PresenceLog {
                                                        message: format!("CU task complete ({} turns)", stats.turns),
                                                        level: None, turn: None,
                                                    });
                                                }
                                                Err(e) => {
                                                    bus_cu.send(AppEvent::PresenceLog {
                                                        message: format!("CU task error: {}", e),
                                                        level: Some(types::LogLevel::Error), turn: None,
                                                    });
                                                }
                                            }
                                        });
                                        continue;
                                    }
                                }

                                let new_provider = match provider::select_provider() {
                                    Ok(p) => p,
                                    Err(e) => {
                                        bus.send(AppEvent::LoopError(format!("Failed to create provider: {}", e)));
                                        continue;
                                    }
                                };

                                // Update shared session state
                                if let Some(ref shared_session) = headless_shared_session {
                                    let mut ss = shared_session.write().await;
                                    ss.session_log = Some(new_session_log.clone());
                                }

                                let new_session_id = new_log_dir
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("unknown")
                                    .to_string();

                                bus.send(AppEvent::SessionStarted {
                                    session_id: new_session_id.clone(),
                                    task: Some(new_task.clone()),
                                });

                                // Spawn new agent loop
                                let bus_spawn = bus.clone();
                                let autonomy_spawn = autonomy_for_daemon.clone();
                                let session_log_spawn = new_session_log.clone();
                                let shared_cleanup = headless_shared_session.clone();
                                let use_direct = orchestrate.map(|o| !o).unwrap_or_else(|| flags.direct || is_simple_task(&new_task));

                                tokio::spawn(async move {
                                    let (_, follow_up_rx) = tokio::sync::mpsc::channel::<String>(1);
                                    let result = if use_direct {
                                        run_direct_mode(
                                            new_provider, new_task.clone(), new_project,
                                            bus_spawn.clone(), autonomy_spawn, session_log_spawn.clone(),
                                            new_log_dir, None, follow_up_rx, None,
                                            event::ApprovalRegistry::default(),
                                            event::ContextInjectionQueue::default(),
                                            true,
                                        ).await
                                    } else {
                                        run_user_mode(
                                            new_provider, new_task.clone(), new_project,
                                            bus_spawn.clone(), autonomy_spawn, session_log_spawn.clone(),
                                        ).await
                                    };

                                    let reason = match &result {
                                        Ok(stats) => {
                                            slog(&session_log_spawn, |l| {
                                                l.write_summary_with_rounds(&new_task, "completed", stats.turns, Some(stats.rounds))
                                            });
                                            "completed".to_string()
                                        }
                                        Err(e) => {
                                            slog(&session_log_spawn, |l| {
                                                l.write_summary(&new_task, &format!("error: {}", e), 0)
                                            });
                                            format!("error: {}", e)
                                        }
                                    };

                                    bus_spawn.send(AppEvent::SessionEnded {
                                        session_id: new_session_id,
                                        reason,
                                    });

                                    // Clear session state
                                    if let Some(ref ss) = shared_cleanup {
                                        let mut state = ss.write().await;
                                        state.session_log = None;
                                        state.query_ctx = None;
                                    }
                                });
                            }
                            Ok(_) => {} // Ignore other events
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        } else {
            result?;
        }
    }

    Ok(())
}
