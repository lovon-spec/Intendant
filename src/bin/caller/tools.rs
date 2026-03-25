use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Mutex;

/// Extra tool definitions registered at runtime (e.g. from MCP servers).
static EXTRA_TOOLS: Mutex<Vec<ToolDefinition>> = Mutex::new(Vec::new());

/// Provider-agnostic tool definition.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value, // JSON Schema object
}

impl ToolDefinition {
    /// Convert to OpenAI Responses API tool format.
    pub fn to_openai(&self) -> Value {
        json!({
            "type": "function",
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters,
        })
    }

    /// Convert to Anthropic Messages API tool format.
    pub fn to_anthropic(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.parameters,
        })
    }

    /// Convert to Gemini API functionDeclaration format.
    pub fn to_gemini(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters,
        })
    }
}

/// Maps a snake_case tool name to the runtime's camelCase function name.
pub fn tool_name_to_function(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "exec_command" => Some("execAsAgent"),
        "capture_screen" => Some("captureScreen"),
        "inspect_path" => Some("inspectPath"),
        "edit_file" => Some("editFile"),
        "browse_url" => Some("browse"),
        "ask_human" => Some("askHuman"),
        "exec_pty" => Some("execPty"),
        "store_memory" => Some("storeMemory"),
        "recall_memory" => Some("recallMemory"),
        _ => None,
    }
}

/// Returns all 12 tool definitions.
pub fn all_tools() -> Vec<ToolDefinition> {
    let mut tools = Vec::with_capacity(12);

    // 1. exec_command → execAsAgent
    {
        let props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "command": {
                "type": "string",
                "description": "The Bash command to run."
            }
        });

        tools.push(ToolDefinition {
            name: "exec_command".to_string(),
            description: "Execute a Bash command and wait for completion. Returns exit code, stdout tail (last 10KB), and stderr tail directly. DISPLAY and XAUTHORITY are set automatically. Reference a previous command's PID with $NONCE[id]. For daemons, background in bash (`cmd &`) — the shell exits and the tool returns while the daemon keeps running. Optional fields (omit unless needed): `display` (integer) — X11 display number for GUI commands, use 0 for the user's session display (requires approval); `wait_for_port` (integer) — TCP port to wait for before executing.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "command"],
                "additionalProperties": true
            }),
        });
    }

    // 2. capture_screen → captureScreen
    {
        let props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            }
        });

        tools.push(ToolDefinition {
            name: "capture_screen".to_string(),
            description: "Capture a screenshot of an X11 display. The screenshot image is sent back to you for visual inspection. Screenshots are also saved to the log directory. The runtime auto-discovers active virtual displays by default. Use `display: 0` for the user's session display (requires one-time approval). Chain after UI interactions to verify success. Optional: `display` (integer) — display number, 0 for user session (requires approval), omit for auto-discover.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce"],
                "additionalProperties": true
            }),
        });
    }

    // 3. inspect_path → inspectPath
    {
        let props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "path": {
                "type": "string",
                "description": "Filesystem path to inspect."
            }
        });

        tools.push(ToolDefinition {
            name: "inspect_path".to_string(),
            description: "Inspect a filesystem path and return metadata (exists, type, size, permissions, timestamps).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "path"],
                "additionalProperties": false
            }),
        });
    }

    // 4. edit_file → editFile
    {
        let props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "file_path": {
                "type": "string",
                "description": "Target file path."
            },
            "operation": {
                "type": "string",
                "enum": ["write", "append", "replace", "insert_at", "replace_lines"],
                "description": "File operation to perform."
            },
            "content": {
                "type": "string",
                "description": "Content to write, append, insert, or use as replacement."
            }
        });

        tools.push(ToolDefinition {
            name: "edit_file".to_string(),
            description: "Perform structured file editing (write, append, replace, insert_at, replace_lines) without spawning a shell. Operation-specific fields (include only when needed): `match_content` (string) — text to find for 'replace'; `line_number` (integer) — 0-based line for 'insert_at'/'replace_lines'; `end_line` (integer) — end line (exclusive) for 'replace_lines'.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "file_path", "operation"],
                "additionalProperties": true
            }),
        });
    }

    // 5. browse_url → browse
    {
        let props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "url": {
                "type": "string",
                "description": "URL to fetch (must start with http:// or https://)."
            }
        });

        tools.push(ToolDefinition {
            name: "browse_url".to_string(),
            description: "Fetch a URL and convert HTML to readable plain text (truncated to 50KB)."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "url"],
                "additionalProperties": false
            }),
        });
    }

    // 6. ask_human → askHuman
    {
        let props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "question": {
                "type": "string",
                "description": "Question to ask the human operator."
            }
        });

        tools.push(ToolDefinition {
            name: "ask_human".to_string(),
            description: "Ask the human operator a question and wait for their response. Use when stuck or need clarification. Optional: `timeout_ms` (integer) — timeout in milliseconds (default: 300000 = 5 minutes).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "question"],
                "additionalProperties": true
            }),
        });
    }

    // 7. exec_pty → execPty
    {
        let props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "command": {
                "type": "string",
                "description": "Command to run in the PTY session."
            }
        });

        tools.push(ToolDefinition {
            name: "exec_pty".to_string(),
            description: "Execute a command in a persistent PTY session where shell state (cwd, env vars) persists between calls within the same turn. Optional: `shell_id` (string) — session identifier (default: 'default'), use different IDs for independent sessions.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "command"],
                "additionalProperties": true
            }),
        });
    }

    // 8. store_memory → storeMemory
    {
        let props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "memory_key": {
                "type": "string",
                "description": "Key for the memory entry."
            },
            "memory_summary": {
                "type": "string",
                "description": "Summary/value of the memory entry."
            }
        });

        tools.push(ToolDefinition {
            name: "store_memory".to_string(),
            description: "Store a key-value memory entry that persists across sessions for this project. Optional: `memory_tags` (string) — comma-separated tags; `memory_channel` (string) — channel/namespace.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "memory_key", "memory_summary"],
                "additionalProperties": true
            }),
        });
    }

    // 9. recall_memory → recallMemory
    {
        let props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "memory_query": {
                "type": "string",
                "description": "Space-separated keywords to search the memory store."
            }
        });

        tools.push(ToolDefinition {
            name: "recall_memory".to_string(),
            description: "Search the project's memory store by keywords, optionally filtered by tags or channel. Optional: `memory_tags` (string) — comma-separated tags; `memory_channel` (string) — channel/namespace; `memory_since` (integer) — Unix timestamp filter.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "memory_query"],
                "additionalProperties": true
            }),
        });
    }

    // 10. manage_context (caller-handled, not sent to runtime)
    tools.push(ToolDefinition {
        name: "manage_context".to_string(),
        description: "Manage conversation context by dropping or summarizing old messages to keep the conversation focused.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "drop_turns": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Message indices to remove from conversation history. Index 0 (system prompt) and the last 2 messages are always protected."
                },
                "summarize": {
                    "type": "object",
                    "properties": {
                        "turns": {
                            "type": "array",
                            "items": { "type": "integer" },
                            "description": "Message indices to replace with the summary."
                        },
                        "summary": {
                            "type": "string",
                            "description": "Summary text to replace the specified turns."
                        }
                    },
                    "required": ["turns", "summary"]
                }
            },
            "additionalProperties": false
        }),
    });

    // 11. signal_done (caller-handled, not sent to runtime)
    tools.push(ToolDefinition {
        name: "signal_done".to_string(),
        description: "Signal that the task is complete. Call this when you have finished all work."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Optional completion message summarizing what was accomplished."
                }
            },
            "additionalProperties": false
        }),
    });

    // 12. invoke_skill (caller-handled, not sent to runtime)
    tools.push(ToolDefinition {
        name: "invoke_skill".to_string(),
        description: "Invoke a named skill. The skill's instructions will be loaded and you should follow them. Use this when a task matches an available skill's description, or when the user explicitly requests a skill by name.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "skill_name": {
                    "type": "string",
                    "description": "Name of the skill to invoke."
                },
                "arguments": {
                    "type": "string",
                    "description": "Arguments to pass to the skill (replaces $ARGUMENTS in skill instructions)."
                }
            },
            "required": ["skill_name"],
            "additionalProperties": false
        }),
    });

    // 13. spawn_live_audio (caller-handled, untrusted live audio sub-agent)
    tools.push(ToolDefinition {
        name: "spawn_live_audio".to_string(),
        description: "Spawn an untrusted live audio sub-agent to conduct a voice conversation through an app on the display. Connects to a live audio model (Gemini Live or OpenAI Realtime) and routes audio through virtual devices (PulseAudio on Linux, BlackHole on macOS). Requires virtual audio driver to be installed. The sub-agent has zero tools and zero file access. Returns structured data matching the response_schema, or quarantine references for unexpected content.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Unique identifier for this live audio session."
                },
                "provider": {
                    "type": "string",
                    "enum": ["gemini", "openai"],
                    "description": "Live audio model provider."
                },
                "playbook": {
                    "type": "string",
                    "description": "System prompt with goal, talking points, and decision tree for the conversation."
                },
                "response_schema": {
                    "type": "object",
                    "description": "Schema defining the structured response the live model must produce. Contains 'fields' array where each field has 'name', 'field_type' (string/integer/boolean/array with constraints), 'required', and 'description'."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Hard timeout in seconds. Default: 300 (5 minutes)."
                },
                "voice": {
                    "type": "string",
                    "description": "Voice name (e.g., 'Aoede' for Gemini, 'alloy' for OpenAI)."
                },
                "display_id": {
                    "type": "integer",
                    "description": "X11 display number where the target app is running."
                }
            },
            "required": ["id", "provider", "playbook", "response_schema"],
            "additionalProperties": false
        }),
    });

    // Append any extra tools registered at runtime (MCP servers, etc.)
    if let Ok(extra) = EXTRA_TOOLS.lock() {
        tools.extend(extra.iter().cloned());
    }

    tools
}

/// Register additional tool definitions (e.g. from MCP servers).
/// These will be included in subsequent calls to `all_tools()`.
pub fn register_extra_tools(new_tools: Vec<ToolDefinition>) {
    if let Ok(mut extra) = EXTRA_TOOLS.lock() {
        extra.extend(new_tools);
    }
}

/// Tool for CU model to escalate non-display tasks to the full coding agent.
pub fn escalate_to_agent_tool() -> ToolDefinition {
    ToolDefinition {
        name: "escalate_to_agent".to_string(),
        description: "Hand this task off to the full coding agent. Call this when the task \
            does NOT involve interacting with the display (clicking, typing, scrolling). \
            Tasks to escalate: coding, file editing, research, shell commands, git, debugging."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task description to forward to the coding agent."
                }
            },
            "required": ["task"]
        }),
    }
}

/// Clear all extra registered tools.
#[allow(dead_code)]
pub fn clear_extra_tools() {
    if let Ok(mut extra) = EXTRA_TOOLS.lock() {
        extra.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_tools_has_13_definitions() {
        let tools = all_tools();
        assert_eq!(tools.len(), 13);
    }

    #[test]
    fn tool_names_are_snake_case() {
        for tool in all_tools() {
            assert!(
                !tool.name.contains(char::is_uppercase),
                "Tool '{}' has uppercase characters",
                tool.name
            );
            assert!(!tool.name.contains('-'), "Tool '{}' has hyphens", tool.name);
        }
    }

    #[test]
    fn all_tools_have_descriptions() {
        for tool in all_tools() {
            assert!(
                !tool.description.is_empty(),
                "Tool '{}' has empty description",
                tool.name
            );
        }
    }

    #[test]
    fn all_tools_have_valid_parameters() {
        for tool in all_tools() {
            let params = &tool.parameters;
            assert_eq!(
                params["type"].as_str(),
                Some("object"),
                "Tool '{}' parameters is not an object",
                tool.name
            );
            assert!(
                params.get("properties").is_some(),
                "Tool '{}' has no properties",
                tool.name
            );
        }
    }

    #[test]
    fn runtime_tools_have_nonce() {
        let runtime_tools = [
            "exec_command",
            "capture_screen",
            "inspect_path",
            "edit_file",
            "browse_url",
            "ask_human",
            "exec_pty",
            "store_memory",
            "recall_memory",
        ];
        let tools = all_tools();
        for name in &runtime_tools {
            let tool = tools.iter().find(|t| t.name == *name).unwrap();
            let required = tool.parameters["required"].as_array().unwrap();
            assert!(
                required.iter().any(|v| v.as_str() == Some("nonce")),
                "Tool '{}' does not require nonce",
                name
            );
        }
    }

    #[test]
    fn caller_tools_have_no_nonce() {
        let caller_tools = ["manage_context", "signal_done", "invoke_skill", "spawn_live_audio"];
        let tools = all_tools();
        for name in &caller_tools {
            let tool = tools.iter().find(|t| t.name == *name).unwrap();
            let has_nonce = tool.parameters["properties"]
                .as_object()
                .map(|p| p.contains_key("nonce"))
                .unwrap_or(false);
            assert!(!has_nonce, "Caller tool '{}' should not have nonce", name);
        }
    }

    #[test]
    fn tool_name_to_function_mappings() {
        assert_eq!(tool_name_to_function("exec_command"), Some("execAsAgent"));
        assert_eq!(
            tool_name_to_function("capture_screen"),
            Some("captureScreen")
        );
        assert_eq!(tool_name_to_function("fetch_status"), None);
        assert_eq!(tool_name_to_function("inspect_path"), Some("inspectPath"));
        assert_eq!(tool_name_to_function("edit_file"), Some("editFile"));
        assert_eq!(tool_name_to_function("browse_url"), Some("browse"));
        assert_eq!(tool_name_to_function("ask_human"), Some("askHuman"));
        assert_eq!(tool_name_to_function("exec_pty"), Some("execPty"));
        assert_eq!(tool_name_to_function("store_memory"), Some("storeMemory"));
        assert_eq!(tool_name_to_function("recall_memory"), Some("recallMemory"));
        assert_eq!(tool_name_to_function("manage_context"), None);
        assert_eq!(tool_name_to_function("signal_done"), None);
        assert_eq!(tool_name_to_function("invoke_skill"), None);
        assert_eq!(tool_name_to_function("nonexistent"), None);
    }

    #[test]
    fn to_openai_format() {
        let tools = all_tools();
        let exec = tools.iter().find(|t| t.name == "exec_command").unwrap();
        let oai = exec.to_openai();
        assert_eq!(oai["type"].as_str(), Some("function"));
        assert_eq!(oai["name"].as_str(), Some("exec_command"));
        assert!(!oai["description"].as_str().unwrap().is_empty());
        assert!(oai["parameters"]["properties"].is_object());
    }

    #[test]
    fn to_anthropic_format() {
        let tools = all_tools();
        let exec = tools.iter().find(|t| t.name == "exec_command").unwrap();
        let ant = exec.to_anthropic();
        assert_eq!(ant["name"].as_str(), Some("exec_command"));
        assert!(!ant["description"].as_str().unwrap().is_empty());
        assert!(ant["input_schema"]["properties"].is_object());
        // Anthropic format does NOT have "type" at the top level
        assert!(ant.get("type").is_none());
    }

    #[test]
    fn to_gemini_format() {
        let tools = all_tools();
        let exec = tools.iter().find(|t| t.name == "exec_command").unwrap();
        let gem = exec.to_gemini();
        assert_eq!(gem["name"].as_str(), Some("exec_command"));
        assert!(!gem["description"].as_str().unwrap().is_empty());
        assert!(gem["parameters"]["properties"].is_object());
    }

    #[test]
    fn all_providers_produce_valid_tool_arrays() {
        let tools = all_tools();

        let oai_tools: Vec<Value> = tools.iter().map(|t| t.to_openai()).collect();
        assert_eq!(oai_tools.len(), 13);
        for t in &oai_tools {
            assert_eq!(t["type"].as_str(), Some("function"));
        }

        let ant_tools: Vec<Value> = tools.iter().map(|t| t.to_anthropic()).collect();
        assert_eq!(ant_tools.len(), 13);
        for t in &ant_tools {
            assert!(t["input_schema"].is_object());
        }

        let gem_tools: Vec<Value> = tools.iter().map(|t| t.to_gemini()).collect();
        assert_eq!(gem_tools.len(), 13);
        for t in &gem_tools {
            assert!(t["parameters"].is_object());
        }
    }

    #[test]
    fn exec_command_has_no_dependency_params() {
        let tools = all_tools();
        let exec = tools.iter().find(|t| t.name == "exec_command").unwrap();
        let props = exec.parameters["properties"].as_object().unwrap();
        assert!(!props.contains_key("depending_nonce"));
        assert!(!props.contains_key("expected_status"));
        assert!(!props.contains_key("wait"));
    }

    #[test]
    fn no_fetch_status_tool() {
        let tools = all_tools();
        assert!(tools.iter().find(|t| t.name == "fetch_status").is_none());
    }

    #[test]
    fn edit_file_has_all_operations() {
        let tools = all_tools();
        let ef = tools.iter().find(|t| t.name == "edit_file").unwrap();
        let ops = ef.parameters["properties"]["operation"]["enum"]
            .as_array()
            .unwrap();
        let op_strs: Vec<&str> = ops.iter().filter_map(|v| v.as_str()).collect();
        assert!(op_strs.contains(&"write"));
        assert!(op_strs.contains(&"append"));
        assert!(op_strs.contains(&"replace"));
        assert!(op_strs.contains(&"insert_at"));
        assert!(op_strs.contains(&"replace_lines"));
    }

    #[test]
    fn manage_context_has_correct_schema() {
        let tools = all_tools();
        let mc = tools.iter().find(|t| t.name == "manage_context").unwrap();
        let props = mc.parameters["properties"].as_object().unwrap();
        assert!(props.contains_key("drop_turns"));
        assert!(props.contains_key("summarize"));
        let summarize_props = props["summarize"]["properties"].as_object().unwrap();
        assert!(summarize_props.contains_key("turns"));
        assert!(summarize_props.contains_key("summary"));
    }

    #[test]
    fn signal_done_has_optional_message() {
        let tools = all_tools();
        let sd = tools.iter().find(|t| t.name == "signal_done").unwrap();
        let props = sd.parameters["properties"].as_object().unwrap();
        assert!(props.contains_key("message"));
        // No required fields
        let required = sd.parameters.get("required");
        assert!(
            required.is_none() || required.unwrap().as_array().unwrap().is_empty(),
            "signal_done should have no required fields"
        );
    }

    #[test]
    fn unique_tool_names() {
        let tools = all_tools();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        names.sort();
        let original_len = names.len();
        names.dedup();
        assert_eq!(names.len(), original_len, "Duplicate tool names found");
    }
}
