mod agent_runner;
mod conversation;
mod error;
mod knowledge;
mod memory;
mod project;
mod provider;
mod sub_agent;
mod user_mode;
mod worktree;

use conversation::Conversation;
use error::CallerError;
use project::Project;
use std::env;
use std::io::{self, BufRead, Write};

const SAFETY_CAP: usize = 500;
const MIN_BUDGET_TOKENS: u64 = 4096;
const BUDGET_WARNING_THRESHOLD: f64 = 0.85;

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

fn apply_context_directives(
    json_str: &str,
    conversation: &mut Conversation,
) -> String {
    let mut value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return json_str.to_string(),
    };

    if let Some(context) = value.get("context").cloned() {
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

    // Check if there are commands; if not, return empty to signal context-only turn
    let has_commands = value
        .get("commands")
        .and_then(|c| c.as_array())
        .is_some_and(|a| !a.is_empty());

    if !has_commands {
        return String::new();
    }

    serde_json::to_string(&value).unwrap_or_else(|_| json_str.to_string())
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
        let result = apply_context_directives(json, &mut conv);

        // Messages 1,2 dropped (u1, a1)
        assert_eq!(conv.len(), 5);
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
        let result = apply_context_directives(json, &mut conv);

        assert_eq!(conv.len(), 4); // sys + summary + u3 + a3
        assert!(conv.messages()[1].content.contains("Setup phase"));
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
        let result = apply_context_directives(json, &mut conv);
        assert!(result.is_empty()); // no commands = context-only
    }

    #[test]
    fn apply_context_directives_no_context() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.add_assistant("a1".to_string());

        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#;
        let result = apply_context_directives(json, &mut conv);
        assert_eq!(conv.len(), 3); // unchanged
        assert!(!result.is_empty());
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
            "/tmp/proj/.agent/memory.json"
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
}

const PROGRESS_INTERVAL: usize = 5;

async fn run_agent_loop(
    provider: &dyn provider::ChatProvider,
    conversation: &mut Conversation,
    project: &Project,
    sub_agent_mode: Option<&(String, sub_agent::SubAgentRole)>,
) -> Result<(), CallerError> {
    let mut budget_warning_shown = false;

    for turn in 1..=SAFETY_CAP {
        // Check budget before sending
        if conversation.remaining_budget() <= MIN_BUDGET_TOKENS {
            println!("--- Context budget exhausted ({} tokens remaining) ---", conversation.remaining_budget());
            break;
        }

        conversation.increment_turn();

        println!("[Turn {}] Sending to model... {}", turn, conversation.budget_summary());

        let response = provider.chat(conversation.messages()).await?;
        conversation.set_usage(response.usage.clone());
        conversation.add_assistant(response.content.clone());

        // Check budget warning
        if !budget_warning_shown && conversation.usage_fraction() >= BUDGET_WARNING_THRESHOLD {
            eprintln!(
                "WARNING: Context budget is running low ({:.0}% used, {} tokens remaining)",
                conversation.usage_fraction() * 100.0,
                conversation.remaining_budget()
            );
            budget_warning_shown = true;
        }

        // Write sub-agent progress periodically
        if let Some((id, _role)) = sub_agent_mode {
            if turn % PROGRESS_INTERVAL == 0 {
                if let Ok(progress_path) = env::var("AGENT_PROGRESS_FILE") {
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

        println!("Model response:\n{}", response.content);
        println!();

        // Extract JSON from response
        let json_str = match extract_json(&response.content) {
            Some(json) => json.to_string(),
            None => {
                println!("--- Task complete ---");
                break;
            }
        };

        // Apply context directives (drop_turns, summarize) before sending to agent
        let json_str = apply_context_directives(&json_str, conversation);

        // Context-only turn (no commands)
        if json_str.is_empty() {
            println!("[Turn {}] Context management only, continuing...", turn);
            conversation.add_user("Context updated.".to_string());
            continue;
        }

        // Inject project context (memory_file) into commands
        let json_str = inject_project_context(&json_str, project);

        println!("[Turn {}] Running agent...", turn);
        let output = agent_runner::run_agent(&json_str).await?;

        println!("Agent stdout:\n{}", output.stdout);
        if !output.stderr.is_empty() {
            eprintln!("Agent stderr:\n{}", output.stderr);
        }

        // Check for completed sub-agent results
        let sub_agent_dir = project.sub_agent_dir();
        if sub_agent_dir.exists() {
            let results = sub_agent::scan_completed_results(&sub_agent_dir);
            for result in &results {
                let msg = sub_agent::format_result_message(result);
                println!("{}", msg);
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
            println!("--- Safety cap ({}) reached ---", SAFETY_CAP);
        }
    }

    Ok(())
}

fn get_task() -> Result<String, CallerError> {
    if env::args().len() > 1 {
        Ok(env::args().skip(1).collect::<Vec<_>>().join(" "))
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
) -> Result<(), CallerError> {
    let system_prompt = user_mode::resolve_system_prompt(&role)?;
    let project = Project::detect()?;
    let task = get_task()?;

    if task.is_empty() {
        return Err(CallerError::Config("No task provided".to_string()));
    }

    println!("Running as sub-agent: {} (role: {})", id, role.as_str());
    println!("Provider: {} (context window: {})", provider.name(), provider.context_window());

    let mut conversation = Conversation::new(system_prompt, provider.context_window());

    // Inject memory if inherited
    if env::var("AGENT_INHERIT_MEMORY").is_ok() {
        if let Some(store) = memory::load_memory(&project) {
            if let Some(memory_msg) = memory::format_memory_message(&store) {
                conversation.add_user(memory_msg);
                conversation.add_assistant("Acknowledged. I have loaded the project memory.".to_string());
            }
        }
    }

    conversation.add_user(task.clone());
    println!("Task: {}", task);
    println!("---");

    let sub_agent_info = (id.clone(), role);
    let result = run_agent_loop(
        provider.as_ref(),
        &mut conversation,
        &project,
        Some(&sub_agent_info),
    ).await;

    // Write result file
    if let Ok(result_path) = env::var("AGENT_RESULT_FILE") {
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
) -> Result<(), CallerError> {
    let system_prompt = user_mode::resolve_system_prompt(&sub_agent::SubAgentRole::Custom("user".to_string()))?;

    println!("Provider: {} (context window: {})", provider.name(), provider.context_window());
    println!("Mode: user (orchestrator will be spawned for complex tasks)");

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
    println!("Task: {}", task);
    println!("---");

    // In user mode, run the loop which will handle orchestrator spawning
    // The model (with user system prompt) decides whether to handle directly or spawn orchestrator
    run_agent_loop(
        provider.as_ref(),
        &mut conversation,
        &project,
        None,
    ).await
}

async fn run_direct_mode(
    provider: Box<dyn provider::ChatProvider>,
    task: String,
    project: Project,
) -> Result<(), CallerError> {
    let system_prompt = std::fs::read_to_string("SysPrompt.md")
        .map_err(|e| CallerError::Config(format!("Failed to read SysPrompt.md: {}", e)))?;

    println!("Provider: {} (context window: {})", provider.name(), provider.context_window());

    let mut conversation = Conversation::new(system_prompt, provider.context_window());

    // Inject memory
    if let Some(store) = memory::load_memory(&project) {
        if let Some(memory_msg) = memory::format_memory_message(&store) {
            conversation.add_user(memory_msg);
            conversation.add_assistant("Acknowledged. I have loaded the project memory.".to_string());
        }
    }

    conversation.add_user(task.clone());
    println!("Task: {}", task);
    println!("---");

    run_agent_loop(
        provider.as_ref(),
        &mut conversation,
        &project,
        None,
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
    dotenvy::dotenv().ok();

    let provider = provider::select_provider()?;

    // Check if running as a sub-agent
    if let Some((id, role)) = sub_agent::detect_sub_agent_mode() {
        return run_sub_agent_mode(provider, id, role).await;
    }

    let project = Project::detect()?;
    let task = get_task()?;

    if task.is_empty() {
        return Err(CallerError::Config("No task provided".to_string()));
    }

    // Decide mode: user mode (complex tasks) or direct mode (simple tasks)
    if is_simple_task(&task) {
        run_direct_mode(provider, task, project).await
    } else {
        run_user_mode(provider, task, project).await
    }
}
