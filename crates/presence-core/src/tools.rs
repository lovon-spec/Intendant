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

/// Return the 9 presence tool definitions for native tool calling.
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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_tools_count_and_names() {
        let tools = presence_tools();
        assert_eq!(tools.len(), 9);
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
    }
}
