mod agent_runner;
mod autonomy;
mod control;
mod conversation;
mod error;
mod knowledge;
mod memory;
mod project;
mod prompts;
mod provider;
mod session_log;
mod sub_agent;
mod tui;
mod user_mode;
mod worktree;

use autonomy::{AutonomyLevel, AutonomyState, SharedAutonomy};
use conversation::Conversation;
use error::CallerError;
use project::Project;
use std::env;
use std::io::{self, BufRead, Write, IsTerminal};
use std::sync::{Arc, Mutex};
use tui::event::{AppEvent, EventBus};

type SharedSessionLog = Arc<Mutex<session_log::SessionLog>>;

/// Helper to write to the session log without propagating errors.
fn slog(log: &SharedSessionLog, f: impl FnOnce(&mut session_log::SessionLog)) {
    if let Ok(mut l) = log.lock() {
        f(&mut l);
    }
}

const SAFETY_CAP: usize = 500;
const MIN_BUDGET_TOKENS: u64 = 4096;
const BUDGET_WARNING_THRESHOLD: f64 = 0.85;

/// CLI flags parsed from command-line arguments.
struct CliFlags {
    task: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    verbose: bool,
    no_tui: bool,
    autonomy: AutonomyLevel,
    log_file: Option<String>,
    control_socket: bool,
}

fn print_help() {
    println!("intendant - AI agent runtime with process lifecycle management");
    println!();
    println!("USAGE:");
    println!("    intendant [OPTIONS] [TASK]");
    println!("    echo \"task\" | intendant [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("    --provider <NAME>     API provider (openai or anthropic)");
    println!("    --model <NAME>        Model name to use");
    println!("    --autonomy <LEVEL>    Autonomy level: low, medium, high, full");
    println!("    --log-file <DIR>      Override session log directory (default: ~/.intendant/logs/<ts>/)");
    println!("    --no-tui              Disable TUI, run headless");
    println!("    --verbose, -v         Enable verbose output");
    println!("    --control-socket      Enable Unix control socket");
    println!("    --help, -h            Show this help message");
    println!();
    println!("SESSION LOGS:");
    println!("    Logs are always written to ~/.intendant/logs/<timestamp>/ (override with --log-file).");
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
    println!("    PROVIDER              Default provider (openai or anthropic)");
    println!("    MODEL_NAME            Default model name");
    println!("    STRUCTURED_OUTPUT     Enable JSON structured output (true/false)");
    println!("    REASONING_EFFORT      Reasoning effort: low, medium, high");
    println!("    REASONING_SUMMARY     Reasoning summary: auto, concise, detailed");
}

fn parse_cli_flags() -> CliFlags {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut flags = CliFlags {
        task: None,
        provider: None,
        model: None,
        verbose: false,
        no_tui: false,
        autonomy: AutonomyLevel::Medium,
        log_file: None,
        control_socket: false,
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
                    i += 1;
                }
            }
            "--model" => {
                if i + 1 < args.len() {
                    flags.model = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
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
                    i += 1;
                }
            }
            "--log-file" => {
                if i + 1 < args.len() {
                    flags.log_file = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--control-socket" => {
                flags.control_socket = true;
                i += 1;
            }
            other => {
                task_parts.push(other.to_string());
                i += 1;
            }
        }
    }

    if !task_parts.is_empty() {
        flags.task = Some(task_parts.join(" "));
    }

    flags
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
fn apply_context_directives(
    json_str: &str,
    conversation: &mut Conversation,
) -> (String, bool) {
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

    (serde_json::to_string(&value).unwrap_or_else(|_| json_str.to_string()), had_context)
}

fn inject_project_context(
    json_str: &str,
    project: &Project,
) -> String {
    let mut value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return json_str.to_string(),
    };

    if let Some(commands) = value.get_mut("commands").and_then(|c| c.as_array_mut()) {
        let memory_file = project.memory_path().to_string_lossy().to_string();

        for cmd in commands.iter_mut() {
            if let Some("storeMemory" | "recallMemory") = cmd.get("function").and_then(|f| f.as_str()) {
                if cmd.get("memory_file").is_none() {
                    cmd["memory_file"] = serde_json::Value::String(memory_file.clone());
                }
            }
        }
    }

    serde_json::to_string(&value).unwrap_or_else(|_| json_str.to_string())
}

/// Format a human-readable command preview from raw JSON (for approval display).
fn format_command_preview(json_str: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
        if let Some(commands) = parsed.get("commands").and_then(|c| c.as_array()) {
            let summaries: Vec<String> = commands
                .iter()
                .map(|cmd| {
                    let func = cmd
                        .get("function")
                        .and_then(|f| f.as_str())
                        .unwrap_or("?");
                    match func {
                        "execAsAgent" => {
                            let command = cmd
                                .get("command")
                                .and_then(|c| c.as_str())
                                .unwrap_or("?");
                            format!("exec: {}", command)
                        }
                        "writeFile" | "editFile" => {
                            let path = cmd
                                .get("file_path")
                                .and_then(|p| p.as_str())
                                .unwrap_or("?");
                            format!("{}: {}", func, path)
                        }
                        "inspectPath" => {
                            let path = cmd
                                .get("path")
                                .and_then(|p| p.as_str())
                                .unwrap_or("?");
                            format!("inspect: {}", path)
                        }
                        "fetchStatus" => {
                            let nonce = cmd
                                .get("nonce")
                                .and_then(|n| n.as_u64())
                                .unwrap_or(0);
                            format!("fetchStatus(nonce={})", nonce)
                        }
                        "browse" => {
                            let url = cmd
                                .get("url")
                                .and_then(|u| u.as_str())
                                .unwrap_or("?");
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

/// Macro-like helper for conditional output: TUI event bus or println.
fn emit(bus: &Option<EventBus>, event_fn: impl FnOnce() -> AppEvent, fallback: impl FnOnce()) {
    if let Some(bus) = bus {
        bus.send(event_fn());
    } else {
        fallback();
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
        assert!(parsed.get("done").and_then(|d| d.as_bool()).unwrap_or(false));
        assert_eq!(parsed.get("message").and_then(|m| m.as_str()), Some("All tasks completed"));
    }

    #[test]
    fn done_signal_without_message() {
        let json = r#"{"commands":[],"done":true}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(parsed.get("done").and_then(|d| d.as_bool()).unwrap_or(false));
        assert!(parsed.get("message").and_then(|m| m.as_str()).is_none());
    }

    #[test]
    fn no_done_signal_continues() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(!parsed.get("done").and_then(|d| d.as_bool()).unwrap_or(false));
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
        assert!(!is_simple_task("research the database schema and document findings"));
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
            autonomy: AutonomyLevel::Medium,
            log_file: None,
            control_socket: false,
        };
        assert!(!flags.verbose);
        assert!(!flags.no_tui);
        assert_eq!(flags.autonomy, AutonomyLevel::Medium);
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
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"cargo build"},{"function":"fetchStatus","nonce":2}]}"#;
        let preview = format_command_preview(json);
        assert!(preview.contains("exec: cargo build"));
        assert!(preview.contains("fetchStatus(nonce=2)"));
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
    fn emit_with_bus() {
        let (bus, mut rx) = EventBus::new();
        let bus_opt = Some(bus);
        emit(&bus_opt, || AppEvent::Tick, || panic!("should not be called"));
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
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
}

const PROGRESS_INTERVAL: usize = 5;

async fn run_agent_loop(
    provider: &dyn provider::ChatProvider,
    conversation: &mut Conversation,
    project: &Project,
    sub_agent_mode: Option<&(String, sub_agent::SubAgentRole)>,
    bus: Option<EventBus>,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
) -> Result<(), CallerError> {
    let mut budget_warning_shown = false;

    for turn in 1..=SAFETY_CAP {
        // Check budget before sending
        if conversation.remaining_budget() <= MIN_BUDGET_TOKENS {
            let remaining = conversation.remaining_budget();
            slog(&session_log, |l| l.warn(&format!("Budget exhausted ({} tokens remaining)", remaining)));
            emit(
                &bus,
                || AppEvent::BudgetExhausted { remaining },
                || println!("--- Context budget exhausted ({} tokens remaining) ---", remaining),
            );
            break;
        }

        conversation.increment_turn();
        let budget_pct = conversation.usage_fraction() * 100.0;
        let remaining = conversation.remaining_budget();

        slog(&session_log, |l| l.turn_start(turn, budget_pct, remaining));

        emit(
            &bus,
            || AppEvent::TurnStarted { turn, budget_pct, remaining },
            || println!("[Turn {}] Sending to model... {}", turn, conversation.budget_summary()),
        );

        let response = match provider.chat(conversation.messages()).await {
            Ok(r) => r,
            Err(e) => {
                slog(&session_log, |l| l.error(&format!("API error: {}", e)));
                emit(
                    &bus,
                    || AppEvent::LoopError(e.to_string()),
                    || eprintln!("Error: {}", e),
                );
                return Err(e);
            }
        };
        conversation.set_usage(response.usage.clone());
        conversation.add_assistant(response.content.clone());

        // Log the full model response (no truncation)
        slog(&session_log, |l| l.model_response(
            &response.content,
            response.usage.prompt_tokens,
            response.usage.completion_tokens,
            response.usage.total_tokens,
        ));

        // Check budget warning
        if !budget_warning_shown && conversation.usage_fraction() >= BUDGET_WARNING_THRESHOLD {
            let pct = conversation.usage_fraction() * 100.0;
            let remaining = conversation.remaining_budget();
            slog(&session_log, |l| l.warn(&format!(
                "Budget warning: {:.0}% used, {} remaining", pct, remaining
            )));
            emit(
                &bus,
                || AppEvent::BudgetWarning { pct, remaining },
                || eprintln!(
                    "WARNING: Context budget is running low ({:.0}% used, {} tokens remaining)",
                    pct, remaining,
                ),
            );
            budget_warning_shown = true;
        }

        // Write sub-agent progress periodically
        if let Some((id, _role)) = sub_agent_mode {
            if turn % PROGRESS_INTERVAL == 0 {
                if let Ok(progress_path) = env::var("INTENDANT_PROGRESS_FILE") {
                    let last_action = response.content.chars().take(200).collect::<String>();
                    let progress = sub_agent::SubAgentProgress {
                        id: id.clone(),
                        turn,
                        status: "running".to_string(),
                        last_action,
                        question: None,
                    };
                    let _ = sub_agent::write_progress(std::path::Path::new(&progress_path), &progress);
                }
            }
        }

        emit(
            &bus,
            || AppEvent::ModelResponse {
                content: response.content.clone(),
                usage: response.usage.clone(),
            },
            || {
                println!("Model response:\n{}", response.content);
                println!();
            },
        );

        // Extract JSON from response
        let json_str = match extract_json(&response.content) {
            Some(json) => json.to_string(),
            None => {
                slog(&session_log, |l| l.info("No JSON found in response — task complete"));
                emit(
                    &bus,
                    || AppEvent::TaskComplete { reason: "Task complete".to_string() },
                    || println!("--- Task complete ---"),
                );
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
            if parsed.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
                let message = parsed.get("message").and_then(|m| m.as_str()).map(String::from);
                slog(&session_log, |l| l.info(&format!(
                    "Done signal received: {}",
                    message.as_deref().unwrap_or("(no message)")
                )));
                emit(
                    &bus,
                    || AppEvent::DoneSignal { message: message.clone() },
                    || {
                        if let Some(ref msg) = message {
                            println!("{}", msg);
                        }
                        println!("--- Task complete ---");
                    },
                );
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
                // Context directives were applied, but no commands — context management turn
                slog(&session_log, |l| l.debug(&format!("Turn {}: context management only", turn)));
                emit(
                    &bus,
                    || AppEvent::ContextManagement { turn },
                    || println!("[Turn {}] Context management only, continuing...", turn),
                );
                conversation.add_user("Context updated.".to_string());
                continue;
            } else {
                // No commands and no context directives — task complete
                slog(&session_log, |l| l.info("No commands and no context directives — task complete"));
                emit(
                    &bus,
                    || AppEvent::TaskComplete { reason: "Task complete".to_string() },
                    || println!("--- Task complete ---"),
                );
                break;
            }
        }

        // Inject project context (memory_file) into commands
        let json_str = inject_project_context(&json_str, project);

        // Check autonomy / approval for commands
        let needs_approval = {
            let classifications = autonomy::classify_batch(&json_str);
            let autonomy_state = autonomy.read().await;
            let mut need = None;
            for (_idx, categories) in &classifications {
                for &cat in categories {
                    if autonomy_state.needs_approval(cat) {
                        need = Some(cat);
                        break;
                    }
                }
                if need.is_some() {
                    break;
                }
            }
            need
        }; // autonomy_state read lock dropped here

        let mut should_skip = false;
        if let Some(cat) = needs_approval {
            let preview = format_command_preview(&json_str);
            slog(&session_log, |l| l.approval(&cat.to_string(), &preview, "waiting"));

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
                        slog(&session_log, |l| l.approval(&cat.to_string(), &preview, "approved"));
                    }
                    Ok(tui::event::ApprovalResponse::ApproveAll) => {
                        slog(&session_log, |l| l.approval(&cat.to_string(), &preview, "approve-all"));
                        let mut state = autonomy.write().await;
                        state.level = AutonomyLevel::Full;
                    }
                    Ok(tui::event::ApprovalResponse::Skip) => {
                        slog(&session_log, |l| l.approval(&cat.to_string(), &preview, "skipped"));
                        should_skip = true;
                    }
                    Ok(tui::event::ApprovalResponse::Deny) | Err(_) => {
                        slog(&session_log, |l| l.approval(&cat.to_string(), &preview, "denied"));
                        emit(
                            &bus,
                            || AppEvent::TaskComplete { reason: "Denied by user".to_string() },
                            || println!("--- Denied by user ---"),
                        );
                        return Ok(());
                    }
                }
            }
            // In headless mode, just proceed (no approval UI)
        }

        if should_skip {
            conversation.add_user("Command skipped by user.".to_string());
            continue;
        }

        // Log the full JSON being sent to the agent
        slog(&session_log, |l| l.agent_input(&json_str));

        emit(
            &bus,
            || AppEvent::AgentStarted { turn },
            || println!("[Turn {}] Running agent...", turn),
        );

        let output = agent_runner::run_agent(&json_str).await?;

        // Log full agent output (no truncation)
        slog(&session_log, |l| l.agent_output(&output.stdout, &output.stderr));

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
                let msg = sub_agent::format_result_message(result);
                slog(&session_log, |l| l.info(&format!("Sub-agent result: {}", msg)));
                emit(
                    &bus,
                    || AppEvent::SubAgentResult { formatted: msg.clone() },
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

        if turn == SAFETY_CAP {
            slog(&session_log, |l| l.warn(&format!("Safety cap ({}) reached", SAFETY_CAP)));
            emit(
                &bus,
                || AppEvent::SafetyCapReached,
                || println!("--- Safety cap ({}) reached ---", SAFETY_CAP),
            );
        }
    }

    slog(&session_log, |l| l.info("Agent loop finished"));
    Ok(())
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
) -> Result<(), CallerError> {
    let project = Project::detect()?;
    let system_prompt = prompts::resolve_system_prompt(&role, Some(&project.root))?;
    let task = get_task()?;

    if task.is_empty() {
        return Err(CallerError::Config("No task provided".to_string()));
    }

    slog(&session_log, |l| {
        l.info(&format!("Sub-agent mode: {} (role: {})", id, role.as_str()));
        l.info(&format!("Provider: {} (context window: {})", provider.name(), provider.context_window()));
    });
    println!("Running as sub-agent: {} (role: {})", id, role.as_str());
    println!("Provider: {} (context window: {})", provider.name(), provider.context_window());

    let mut conversation = Conversation::new(system_prompt, provider.context_window());

    // Inject memory if inherited
    if env::var("INTENDANT_INHERIT_MEMORY").is_ok() {
        if let Some(store) = memory::load_memory(&project) {
            if let Some(memory_msg) = memory::format_memory_message(&store) {
                conversation.add_user(memory_msg);
                conversation.add_assistant("Acknowledged. I have loaded the project memory.".to_string());
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
    let result = run_agent_loop(
        provider.as_ref(),
        &mut conversation,
        &project,
        Some(&sub_agent_info),
        None, // no TUI for sub-agents
        autonomy,
        session_log,
    ).await;

    // Write result file
    if let Ok(result_path) = env::var("INTENDANT_RESULT_FILE") {
        let (status, summary) = match &result {
            Ok(()) => (
                sub_agent::SubAgentStatus::Completed,
                "Task completed successfully".to_string(),
            ),
            Err(e) => (
                sub_agent::SubAgentStatus::Failed(e.to_string()),
                format!("Task failed: {}", e),
            ),
        };

        let agent_result = sub_agent::SubAgentResult {
            id,
            status,
            summary,
            findings: vec![],
            artifacts: vec![],
            usage: provider::TokenUsage::default(),
        };
        let _ = sub_agent::write_result(std::path::Path::new(&result_path), &agent_result);
    }

    result
}

async fn run_user_mode(
    provider: Box<dyn provider::ChatProvider>,
    task: String,
    project: Project,
    bus: Option<EventBus>,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
) -> Result<(), CallerError> {
    let system_prompt = prompts::resolve_system_prompt(&sub_agent::SubAgentRole::Custom("user".to_string()), Some(&project.root))?;

    slog(&session_log, |l| {
        l.info(&format!("Mode: user (provider: {}, context: {})", provider.name(), provider.context_window()));
    });
    if bus.is_none() {
        println!("Provider: {} (context window: {})", provider.name(), provider.context_window());
        println!("Mode: user (orchestrator will be spawned for complex tasks)");
    }

    let mut conversation = Conversation::new(system_prompt, provider.context_window());
    conversation.set_protect_user_layer(true);

    // Inject memory
    if let Some(store) = memory::load_memory(&project) {
        if let Some(memory_msg) = memory::format_memory_message(&store) {
            conversation.add_user_with_layer(memory_msg, conversation::MessageLayer::User);
            conversation.add_assistant("Acknowledged. I have loaded the project memory.".to_string());
        }
    }

    conversation.add_user_with_layer(task.clone(), conversation::MessageLayer::User);
    if bus.is_none() {
        println!("Task: {}", task);
        println!("---");
    }

    run_agent_loop(
        provider.as_ref(),
        &mut conversation,
        &project,
        None,
        bus,
        autonomy,
        session_log,
    ).await
}

async fn run_direct_mode(
    provider: Box<dyn provider::ChatProvider>,
    task: String,
    project: Project,
    bus: Option<EventBus>,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
) -> Result<(), CallerError> {
    let system_prompt = prompts::resolve_system_prompt(&sub_agent::SubAgentRole::Custom("direct".to_string()), Some(&project.root))?;

    slog(&session_log, |l| {
        l.info(&format!("Mode: direct (provider: {}, context: {})", provider.name(), provider.context_window()));
    });
    if bus.is_none() {
        println!("Provider: {} (context window: {})", provider.name(), provider.context_window());
    }

    let mut conversation = Conversation::new(system_prompt, provider.context_window());

    // Inject memory
    if let Some(store) = memory::load_memory(&project) {
        if let Some(memory_msg) = memory::format_memory_message(&store) {
            conversation.add_user(memory_msg);
            conversation.add_assistant("Acknowledged. I have loaded the project memory.".to_string());
        }
    }

    conversation.add_user(task.clone());
    if bus.is_none() {
        println!("Task: {}", task);
        println!("---");
    }

    run_agent_loop(
        provider.as_ref(),
        &mut conversation,
        &project,
        None,
        bus,
        autonomy,
        session_log,
    ).await
}

fn is_simple_task(task: &str) -> bool {
    // A simple task is a single line with no complex indicators
    let lines: Vec<&str> = task.lines().collect();
    if lines.len() > 3 {
        return false;
    }

    let lower = task.to_lowercase();
    let complex_indicators = [
        "research", "investigate", "implement", "build",
        "refactor", "migrate", "deploy", "set up",
        "analyze", "compare", "design", "create a",
    ];

    for indicator in &complex_indicators {
        if lower.contains(indicator) {
            return false;
        }
    }

    // Short tasks are simple
    task.len() < 100
}

#[tokio::main]
async fn main() -> Result<(), CallerError> {
    // Load .env: cwd (+ parents) first, then project root, then ~/.config/intendant/
    dotenvy::dotenv().ok();
    let project = Project::detect()?;
    dotenvy::from_path(project.root.join(".env")).ok();
    if let Some(config_dir) = dirs::config_dir() {
        dotenvy::from_path(config_dir.join("intendant").join(".env")).ok();
    }

    // Override env vars from CLI flags before provider selection
    let flags = parse_cli_flags();
    if let Some(ref p) = flags.provider {
        env::set_var("PROVIDER", p);
    }
    if let Some(ref m) = flags.model {
        env::set_var("MODEL_NAME", m);
    }

    // Create session log (always enabled; --log-file overrides directory)
    let log_dir = session_log::SessionLog::resolve_path(flags.log_file.as_deref());
    let session_log: SharedSessionLog = match session_log::SessionLog::open(log_dir.clone()) {
        Ok(log) => {
            eprintln!("Session log: {}/session.jsonl", log.dir().display());
            Arc::new(Mutex::new(log))
        }
        Err(e) => {
            eprintln!("Warning: Could not create session log at {}: {}", log_dir.display(), e);
            // Fallback to /tmp
            let fallback = std::path::PathBuf::from("/tmp/intendant_session");
            let log = session_log::SessionLog::open(fallback)
                .map_err(|e| CallerError::Config(format!("Cannot create session log: {}", e)))?;
            eprintln!("Session log (fallback): {}/session.jsonl", log.dir().display());
            Arc::new(Mutex::new(log))
        }
    };

    let provider = provider::select_provider()?;
    slog(&session_log, |l| {
        l.info(&format!("Provider: {}", provider.name()));
        l.info(&format!("Model: {}", env::var("MODEL_NAME").unwrap_or_else(|_| "default".to_string())));
        l.info(&format!("Project root: {}", project.root.display()));
        l.info(&format!("Autonomy: {}", flags.autonomy));
    });

    // Check if running as a sub-agent (headless, no TUI)
    if let Some((id, role)) = sub_agent::detect_sub_agent_mode() {
        return run_sub_agent_mode(provider, id, role, session_log).await;
    }
    let task = get_task_from_flags_or_env(&flags)?;

    if task.is_empty() {
        return Err(CallerError::Config("No task provided".to_string()));
    }

    slog(&session_log, |l| l.info(&format!("Task: {}", task)));

    // Determine whether to use TUI
    let use_tui = !flags.no_tui && io::stdin().is_terminal();

    // Build autonomy state from project config + CLI flags
    let autonomy_state = AutonomyState::new(
        flags.autonomy,
        project.config.approval.clone(),
    );
    let autonomy = autonomy::shared_autonomy(autonomy_state);

    if use_tui {
        // TUI mode
        let (bus, event_rx) = EventBus::new();

        // Spawn background tasks
        let _crossterm_handle = tui::event::spawn_crossterm_reader(bus.clone());
        let _tick_handle = tui::event::spawn_tick_timer(bus.clone(), 100);
        let _human_monitor = tui::event::spawn_human_question_monitor(bus.clone());

        // Spawn control socket
        let (_control_handle, _control_tx) = control::spawn_control_server(bus.clone());

        // Create TUI
        let mut terminal = tui::Tui::new()
            .map_err(|e| CallerError::Tui(format!("Failed to initialize TUI: {}", e)))?;

        // Create app state
        let mut app = tui::app::App::new(
            provider.name().to_string(),
            format!("{}", env::var("MODEL_NAME").unwrap_or_else(|_| "default".to_string())),
            autonomy.clone(),
        );
        app.verbose = flags.verbose;

        app.log(tui::app::LogLevel::Info, format!("Task: {}", task));

        // Spawn the agent loop in a background task
        let bus_clone = bus.clone();
        let autonomy_clone = autonomy.clone();
        let task_clone = task.clone();
        let task_for_summary = task.clone();
        let session_log_clone = session_log.clone();
        let session_log_summary = session_log.clone();
        let loop_handle = tokio::spawn(async move {
            let result = if is_simple_task(&task_clone) {
                run_direct_mode(provider, task_clone, project, Some(bus_clone.clone()), autonomy_clone, session_log_clone).await
            } else {
                run_user_mode(provider, task_clone, project, Some(bus_clone.clone()), autonomy_clone, session_log_clone).await
            };

            match result {
                Ok(()) => {
                    slog(&session_log_summary, |l| l.write_summary(&task_for_summary, "completed", 0));
                    bus_clone.send(AppEvent::TaskComplete {
                        reason: "Task complete".to_string(),
                    });
                }
                Err(e) => {
                    slog(&session_log_summary, |l| l.write_summary(&task_for_summary, &format!("error: {}", e), 0));
                    bus_clone.send(AppEvent::LoopError(e.to_string()));
                }
            }
        });

        // Run the TUI event loop (blocks until quit)
        let _ = terminal.run(&mut app, event_rx).await;

        // Clean up
        loop_handle.abort();
        control::cleanup();
        terminal.restore().map_err(|e| CallerError::Tui(e.to_string()))?;
    } else {
        // Headless mode (--no-tui or non-TTY)
        let result = if is_simple_task(&task) {
            run_direct_mode(provider, task.clone(), project, None, autonomy, session_log.clone()).await
        } else {
            run_user_mode(provider, task.clone(), project, None, autonomy, session_log.clone()).await
        };
        match &result {
            Ok(()) => slog(&session_log, |l| l.write_summary(&task, "completed", 0)),
            Err(e) => slog(&session_log, |l| l.write_summary(&task, &format!("error: {}", e), 0)),
        }
        result?;
    }

    Ok(())
}
