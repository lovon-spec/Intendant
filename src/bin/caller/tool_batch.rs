use crate::{mcp_client, provider, tools};

/// Context directives extracted from manage_context / signal_done tool calls.
pub struct ToolBatchResult {
    /// JSON string of AgentInput to send to the runtime (None if no runtime commands).
    pub agent_input_json: Option<String>,
    /// Whether to apply context directives (from manage_context tool calls).
    pub context_directives: Option<serde_json::Value>,
    /// Whether the model signaled completion (signal_done).
    pub is_done: bool,
    /// Done message, if any.
    pub done_message: Option<String>,
    /// Map of nonce → tool call ID for routing results back.
    pub nonce_to_call_id: std::collections::HashMap<u64, String>,
    /// All tool call IDs and their names (for result routing).
    pub call_id_names: Vec<(String, String)>,
    /// MCP tool calls that should be routed through the MCP client manager.
    /// Vec of (call_id, tool_name, arguments_json).
    pub mcp_calls: Vec<(String, String, String)>,
    /// Tool-level validation errors generated before runtime execution.
    pub precomputed_results: Vec<(String, String, String)>,
}

/// Assemble an AgentInput batch from individual tool calls.
/// Separates manage_context/signal_done from runtime commands.
pub fn assemble_batch_from_tool_calls(tool_calls: &[provider::ToolCall]) -> ToolBatchResult {
    let mut commands = Vec::new();
    let mut nonce_to_call_id = std::collections::HashMap::new();
    let mut call_id_names = Vec::new();
    let mut context_directives = None;
    let mut is_done = false;
    let mut done_message = None;
    let mut mcp_calls = Vec::new();
    let mut precomputed_results = Vec::new();

    for tc in tool_calls {
        call_id_names.push((tc.call_id.clone(), tc.name.clone()));

        match tc.name.as_str() {
            "manage_context" => {
                if let Ok(args) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                    context_directives = Some(args);
                }
            }
            "signal_done" => {
                is_done = true;
                if let Ok(args) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                    done_message = args
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(String::from);
                }
            }
            tool_name if mcp_client::McpClientManager::is_mcp_tool(tool_name) => {
                mcp_calls.push((
                    tc.call_id.clone(),
                    tool_name.to_string(),
                    tc.arguments.clone(),
                ));
            }
            tool_name => {
                if let Some(function) = tools::tool_name_to_function(tool_name) {
                    if let Ok(mut args) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                        args["function"] = serde_json::Value::String(function.to_string());

                        if let Some(nonce) = args.get("nonce").and_then(|n| n.as_u64()) {
                            if nonce_to_call_id.contains_key(&nonce) {
                                precomputed_results.push((
                                    tc.call_id.clone(),
                                    tc.name.clone(),
                                    format!(
                                        "Error: duplicate nonce {} in tool-call batch; each runtime command must use a unique nonce.",
                                        nonce
                                    ),
                                ));
                                continue;
                            }
                            nonce_to_call_id.insert(nonce, tc.call_id.clone());
                        }

                        commands.push(args);
                    }
                }
            }
        }
    }

    let agent_input_json = if commands.is_empty() {
        None
    } else {
        let input = serde_json::json!({
            "commands": commands,
        });
        Some(serde_json::to_string(&input).unwrap_or_default())
    };

    ToolBatchResult {
        agent_input_json,
        context_directives,
        is_done,
        done_message,
        nonce_to_call_id,
        call_id_names,
        mcp_calls,
        precomputed_results,
    }
}

/// Map agent runtime output back to individual tool call responses.
/// Returns Vec<(call_id, tool_name, response_text)>.
pub fn map_results_to_tool_responses(
    agent_stdout: &str,
    agent_stderr: &str,
    nonce_to_call_id: &std::collections::HashMap<u64, String>,
    call_id_names: &[(String, String)],
) -> Vec<(String, String, String)> {
    let mut nonce_status: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    let mut nonce_results: std::collections::HashMap<u64, Vec<String>> =
        std::collections::HashMap::new();
    let mut other_lines = Vec::new();

    for line in agent_stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let msg_type = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let nonce = parsed.get("nonce").and_then(|n| n.as_u64());
            match (msg_type, nonce) {
                ("status", Some(n)) => {
                    let status_char = parsed.get("status").and_then(|s| s.as_str()).unwrap_or("?");
                    let exit_code = parsed
                        .get("exit_code")
                        .and_then(|e| e.as_i64())
                        .unwrap_or(0);
                    nonce_status.insert(n, format!("{}{}{}", n, status_char, exit_code));
                }
                ("result", Some(n)) => {
                    if let Some(data) = parsed.get("data").and_then(|d| d.as_str()) {
                        nonce_results.entry(n).or_default().push(data.to_string());
                    }
                }
                _ => {
                    other_lines.push(trimmed.to_string());
                }
            }
        } else {
            other_lines.push(trimmed.to_string());
        }
    }

    let other_output = other_lines.join("\n");
    let mut results = Vec::new();

    for (call_id, tool_name) in call_id_names {
        let nonce = nonce_to_call_id
            .iter()
            .find(|(_, cid)| *cid == call_id)
            .map(|(&n, _)| n);

        let mut parts = Vec::new();
        if let Some(n) = nonce {
            if let Some(status) = nonce_status.get(&n) {
                parts.push(status.clone());
            }
            if let Some(result_data) = nonce_results.get(&n) {
                for data in result_data {
                    parts.push(data.clone());
                }
            }
        }

        if tool_name == "manage_context" || tool_name == "signal_done" {
            results.push((call_id.clone(), tool_name.clone(), "OK".to_string()));
            continue;
        }

        if !other_output.is_empty() {
            parts.push(other_output.clone());
        }
        if !agent_stderr.is_empty() {
            parts.push(format!("stderr: {}", agent_stderr));
        }

        let response_text = if parts.is_empty() {
            "OK".to_string()
        } else {
            parts.join("\n")
        };
        results.push((call_id.clone(), tool_name.clone(), response_text));
    }

    results
}
