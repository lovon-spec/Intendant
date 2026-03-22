use serde::Serialize;
use serde_json::{json, Value};

/// Provider-agnostic tool definition.
///
/// This is a presence-core-local copy of the main crate's `ToolDefinition`.
/// The main crate's version adds provider-specific conversion methods
/// (`to_openai()`, `to_anthropic()`, `to_gemini()`) which are not WASM-appropriate.
/// Conversion between the two is trivial via `From` since the field layout is identical.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Return the presence tool definitions for native tool calling.
pub fn presence_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "submit_task".to_string(),
            description: "Submit a coding task for workers to execute. Use for any multi-step work like implementing features, fixing bugs, running tests, or research.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "The task description."
                    },
                    "force_direct": {
                        "type": "boolean",
                        "description": "Force single-agent mode (no orchestrator). Default false."
                    },
                    "context_hints": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional hints to inject into the worker's context."
                    }
                },
                "required": ["task"]
            }),
        },
        ToolDefinition {
            name: "check_status".to_string(),
            description: "Check current agent status: phase, turn, budget, last command, workers."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolDefinition {
            name: "query_detail".to_string(),
            description: "Query detailed information. Scopes: current_turn, last_output, worker, diff, logs, file, task_result.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "scope": {
                        "type": "string",
                        "enum": ["current_turn", "last_output", "worker", "diff", "logs", "file", "task_result"],
                        "description": "What to query."
                    },
                    "target": {
                        "type": "string",
                        "description": "Target path (required for 'file' scope)."
                    }
                },
                "required": ["scope"]
            }),
        },
        ToolDefinition {
            name: "recall_memory".to_string(),
            description:
                "Search knowledge store and session logs for past context, decisions, and findings."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "keywords": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Keywords to search for."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Filter by tags."
                    },
                    "channel": {
                        "type": "string",
                        "description": "Filter by knowledge channel."
                    }
                },
                "required": []
            }),
        },
        ToolDefinition {
            name: "approve_action".to_string(),
            description: "Approve a pending action that requires user consent.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The approval ID."
                    }
                },
                "required": ["id"]
            }),
        },
        ToolDefinition {
            name: "deny_action".to_string(),
            description: "Deny a pending action, stopping the current command.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The approval ID."
                    }
                },
                "required": ["id"]
            }),
        },
        ToolDefinition {
            name: "skip_action".to_string(),
            description: "Skip a pending action, continuing with the next command.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The approval ID."
                    }
                },
                "required": ["id"]
            }),
        },
        ToolDefinition {
            name: "respond_to_question".to_string(),
            description: "Respond to a question from the worker (askHuman).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The response text."
                    }
                },
                "required": ["text"]
            }),
        },
        ToolDefinition {
            name: "set_autonomy".to_string(),
            description: "Set the autonomy level: low (ask for everything), medium (ask for writes/deletes), high (ask for destructive only), full (no approval needed).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "level": {
                        "type": "string",
                        "enum": ["low", "medium", "high", "full"],
                        "description": "The autonomy level."
                    }
                },
                "required": ["level"]
            }),
        },
        // ── Interjection tool ──
        ToolDefinition {
            name: "send_message".to_string(),
            description: "Send a message to the running worker agent as a mid-task interjection. The message will be injected into the agent's conversation at the start of its next turn. Use this for corrections, additional context, or redirections — NOT for new tasks (use submit_task for those). Optionally attach video frame references.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "The message to inject into the agent's conversation."
                    },
                    "frame_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional frame IDs to attach as HQ images (e.g. ['cam0-f00014', 'cam0-f00018'])."
                    }
                },
                "required": ["message"]
            }),
        },
        // ── Video / frame tools ──
        ToolDefinition {
            name: "inspect_frame".to_string(),
            description: "Retrieve the high-resolution version of a video frame. If frame_id is omitted, returns the latest frame. The HQ image will be injected into your context after the tool response.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "frame_id": {
                        "type": "string",
                        "description": "The frame ID to inspect (e.g. 'cam0-f00047'). Omit for the latest frame."
                    }
                },
                "required": []
            }),
        },
        ToolDefinition {
            name: "inspect_frames".to_string(),
            description: "Search for past video frames by time range or description. Returns frame metadata (IDs, timestamps, streams) without images — use inspect_frame on specific results to see them.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query: a time range like 'last 30s', a stream name like 'cam0', or a description."
                    },
                    "count": {
                        "type": "integer",
                        "description": "Maximum number of results to return. Default 10."
                    }
                },
                "required": ["query"]
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_tools_count_and_names() {
        let tools = presence_tools();
        assert_eq!(tools.len(), 12);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"submit_task"));
        assert!(names.contains(&"check_status"));
        assert!(names.contains(&"query_detail"));
        assert!(names.contains(&"recall_memory"));
        assert!(names.contains(&"approve_action"));
        assert!(names.contains(&"deny_action"));
        assert!(names.contains(&"skip_action"));
        assert!(names.contains(&"respond_to_question"));
        assert!(names.contains(&"set_autonomy"));
        assert!(names.contains(&"send_message"));
        assert!(names.contains(&"inspect_frame"));
        assert!(names.contains(&"inspect_frames"));
    }
}
