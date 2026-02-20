use serde::Serialize;
use serde_json::{json, Value};

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
        "fetch_status" => Some("fetchStatus"),
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

/// Common dependency parameters shared by all runtime tool definitions.
fn dependency_properties() -> Value {
    json!({
        "depending_nonce": {
            "type": "integer",
            "description": "Nonce of a command that must finish before this one starts."
        },
        "expected_status": {
            "type": "integer",
            "description": "Required exit code of the dependency (default: 0)."
        },
        "wait": {
            "type": "boolean",
            "description": "If true, block until the dependency finishes. If false, skip if not done yet."
        }
    })
}

/// Returns all 12 tool definitions.
pub fn all_tools() -> Vec<ToolDefinition> {
    let dep = dependency_properties();

    let mut tools = Vec::with_capacity(12);

    // 1. exec_command → execAsAgent
    {
        let mut props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "command": {
                "type": "string",
                "description": "The Bash command to run."
            },
            "display": {
                "type": "integer",
                "description": "X11 display ID (default: 1)."
            },
            "wait_for_port": {
                "type": "integer",
                "description": "TCP port to wait for (up to 30s) before executing."
            }
        });
        merge_properties(&mut props, &dep);

        tools.push(ToolDefinition {
            name: "exec_command".to_string(),
            description: "Execute a Bash command in the background. Stdout/stderr are logged to disk; use fetch_status to read them. Reference a previous command's PID with $NONCE[id].".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "command"],
                "additionalProperties": false
            }),
        });
    }

    // 2. capture_screen → captureScreen
    {
        let mut props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "display": {
                "type": "integer",
                "description": "X11 display ID to capture (default: 1)."
            }
        });
        merge_properties(&mut props, &dep);

        tools.push(ToolDefinition {
            name: "capture_screen".to_string(),
            description: "Capture a screenshot of the specified X11 display.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce"],
                "additionalProperties": false
            }),
        });
    }

    // 3. fetch_status → fetchStatus
    {
        let mut props = json!({
            "nonce": {
                "type": "integer",
                "description": "Nonce of the command to query."
            },
            "status_type": {
                "type": "string",
                "enum": ["status", "stdout", "stderr", "exit_code"],
                "description": "What to retrieve: process status, stdout log, stderr log, or exit code."
            },
            "offset": {
                "type": "integer",
                "description": "Byte offset for stdout/stderr log reading."
            },
            "limit": {
                "type": "integer",
                "description": "Max bytes to read from the log."
            },
            "cursor": {
                "type": "integer",
                "description": "Cursor position for incremental log reading."
            }
        });
        merge_properties(&mut props, &dep);

        tools.push(ToolDefinition {
            name: "fetch_status".to_string(),
            description: "Retrieve status, stdout, stderr, or exit code for a previously launched command by its nonce.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "status_type"],
                "additionalProperties": false
            }),
        });
    }

    // 4. inspect_path → inspectPath
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

    // 5. edit_file → editFile
    {
        let mut props = json!({
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
            },
            "match_content": {
                "type": "string",
                "description": "Text to find (required for 'replace' operation)."
            },
            "line_number": {
                "type": "integer",
                "description": "0-based line number (required for 'insert_at' and 'replace_lines')."
            },
            "end_line": {
                "type": "integer",
                "description": "End line (exclusive) for 'replace_lines' operation."
            }
        });
        merge_properties(&mut props, &dep);

        tools.push(ToolDefinition {
            name: "edit_file".to_string(),
            description: "Perform structured file editing (write, append, replace, insert_at, replace_lines) without spawning a shell.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "file_path", "operation"],
                "additionalProperties": false
            }),
        });
    }

    // 6. browse_url → browse
    {
        let mut props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "url": {
                "type": "string",
                "description": "URL to fetch (must start with http:// or https://)."
            }
        });
        merge_properties(&mut props, &dep);

        tools.push(ToolDefinition {
            name: "browse_url".to_string(),
            description: "Fetch a URL and convert HTML to readable plain text (truncated to 50KB).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "url"],
                "additionalProperties": false
            }),
        });
    }

    // 7. ask_human → askHuman
    {
        let mut props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "question": {
                "type": "string",
                "description": "Question to ask the human operator."
            },
            "timeout_ms": {
                "type": "integer",
                "description": "Timeout in milliseconds (default: 300000 = 5 minutes)."
            }
        });
        merge_properties(&mut props, &dep);

        tools.push(ToolDefinition {
            name: "ask_human".to_string(),
            description: "Ask the human operator a question and wait for their response. Use when stuck or need clarification.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "question"],
                "additionalProperties": false
            }),
        });
    }

    // 8. exec_pty → execPty
    {
        let mut props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "command": {
                "type": "string",
                "description": "Command to run in the PTY session."
            },
            "shell_id": {
                "type": "string",
                "description": "PTY session identifier (default: 'default'). Use different IDs for independent sessions."
            }
        });
        merge_properties(&mut props, &dep);

        tools.push(ToolDefinition {
            name: "exec_pty".to_string(),
            description: "Execute a command in a persistent PTY session where shell state (cwd, env vars) persists between calls within the same turn.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "command"],
                "additionalProperties": false
            }),
        });
    }

    // 9. store_memory → storeMemory
    {
        let mut props = json!({
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
            },
            "memory_tags": {
                "type": "string",
                "description": "Comma-separated tags for categorization."
            },
            "memory_channel": {
                "type": "string",
                "description": "Channel/namespace for the memory entry."
            }
        });
        merge_properties(&mut props, &dep);

        tools.push(ToolDefinition {
            name: "store_memory".to_string(),
            description: "Store a key-value memory entry that persists across sessions for this project.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "memory_key", "memory_summary"],
                "additionalProperties": false
            }),
        });
    }

    // 10. recall_memory → recallMemory
    {
        let mut props = json!({
            "nonce": {
                "type": "integer",
                "description": "Unique identifier for this command."
            },
            "memory_query": {
                "type": "string",
                "description": "Space-separated keywords to search the memory store."
            },
            "memory_tags": {
                "type": "string",
                "description": "Comma-separated tags to filter results."
            },
            "memory_channel": {
                "type": "string",
                "description": "Channel/namespace to search within."
            },
            "memory_since": {
                "type": "integer",
                "description": "Only return entries created after this Unix timestamp."
            }
        });
        merge_properties(&mut props, &dep);

        tools.push(ToolDefinition {
            name: "recall_memory".to_string(),
            description: "Search the project's memory store by keywords, optionally filtered by tags or channel.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": props,
                "required": ["nonce", "memory_query"],
                "additionalProperties": false
            }),
        });
    }

    // 11. manage_context (caller-handled, not sent to runtime)
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

    // 12. signal_done (caller-handled, not sent to runtime)
    tools.push(ToolDefinition {
        name: "signal_done".to_string(),
        description: "Signal that the task is complete. Call this when you have finished all work.".to_string(),
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

    tools
}

/// Merge properties from `src` into `dest` (both must be JSON objects).
fn merge_properties(dest: &mut Value, src: &Value) {
    if let (Some(d), Some(s)) = (dest.as_object_mut(), src.as_object()) {
        for (k, v) in s {
            d.insert(k.clone(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_tools_has_12_definitions() {
        let tools = all_tools();
        assert_eq!(tools.len(), 12);
    }

    #[test]
    fn tool_names_are_snake_case() {
        for tool in all_tools() {
            assert!(
                !tool.name.contains(char::is_uppercase),
                "Tool '{}' has uppercase characters",
                tool.name
            );
            assert!(
                !tool.name.contains('-'),
                "Tool '{}' has hyphens",
                tool.name
            );
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
            "fetch_status",
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
        let caller_tools = ["manage_context", "signal_done"];
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
        assert_eq!(
            tool_name_to_function("fetch_status"),
            Some("fetchStatus")
        );
        assert_eq!(
            tool_name_to_function("inspect_path"),
            Some("inspectPath")
        );
        assert_eq!(tool_name_to_function("edit_file"), Some("editFile"));
        assert_eq!(tool_name_to_function("browse_url"), Some("browse"));
        assert_eq!(tool_name_to_function("ask_human"), Some("askHuman"));
        assert_eq!(tool_name_to_function("exec_pty"), Some("execPty"));
        assert_eq!(
            tool_name_to_function("store_memory"),
            Some("storeMemory")
        );
        assert_eq!(
            tool_name_to_function("recall_memory"),
            Some("recallMemory")
        );
        assert_eq!(tool_name_to_function("manage_context"), None);
        assert_eq!(tool_name_to_function("signal_done"), None);
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
        assert_eq!(oai_tools.len(), 12);
        for t in &oai_tools {
            assert_eq!(t["type"].as_str(), Some("function"));
        }

        let ant_tools: Vec<Value> = tools.iter().map(|t| t.to_anthropic()).collect();
        assert_eq!(ant_tools.len(), 12);
        for t in &ant_tools {
            assert!(t["input_schema"].is_object());
        }

        let gem_tools: Vec<Value> = tools.iter().map(|t| t.to_gemini()).collect();
        assert_eq!(gem_tools.len(), 12);
        for t in &gem_tools {
            assert!(t["parameters"].is_object());
        }
    }

    #[test]
    fn exec_command_has_dependency_params() {
        let tools = all_tools();
        let exec = tools.iter().find(|t| t.name == "exec_command").unwrap();
        let props = exec.parameters["properties"].as_object().unwrap();
        assert!(props.contains_key("depending_nonce"));
        assert!(props.contains_key("expected_status"));
        assert!(props.contains_key("wait"));
    }

    #[test]
    fn fetch_status_has_log_params() {
        let tools = all_tools();
        let fs = tools.iter().find(|t| t.name == "fetch_status").unwrap();
        let props = fs.parameters["properties"].as_object().unwrap();
        assert!(props.contains_key("offset"));
        assert!(props.contains_key("limit"));
        assert!(props.contains_key("cursor"));
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
