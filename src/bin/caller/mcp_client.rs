use crate::error::CallerError;
use crate::project::McpServerConfig;
use crate::tools::ToolDefinition;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientInfo, Implementation};
use rmcp::service::{Peer, RoleClient, RunningService, ServiceExt};
use rmcp::transport::child_process::TokioChildProcess;
use tokio::process::Command;

/// A connected MCP server with its tools.
struct ConnectedServer {
    name: String,
    peer: Peer<RoleClient>,
    tools: Vec<ToolDefinition>,
    _running: RunningService<RoleClient, McpClientHandler>,
}

/// Minimal client handler that does nothing (no sampling, no roots).
struct McpClientHandler;

impl ClientHandler for McpClientHandler {
    fn get_info(&self) -> ClientInfo {
        ClientInfo {
            client_info: Implementation {
                name: "intendant".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: None,
                description: None,
                icons: None,
                website_url: None,
            },
            ..Default::default()
        }
    }
}

/// Manages connections to external MCP servers configured in intendant.toml.
pub struct McpClientManager {
    servers: Vec<ConnectedServer>,
}

impl McpClientManager {
    /// Connect to all configured MCP servers. Servers that fail to connect
    /// are logged and skipped (graceful degradation).
    pub async fn connect_all(configs: &[McpServerConfig]) -> Self {
        let mut servers = Vec::new();

        for config in configs {
            match Self::connect_one(config).await {
                Ok(server) => {
                    eprintln!(
                        "MCP client: connected to '{}' ({} tools)",
                        server.name,
                        server.tools.len()
                    );
                    servers.push(server);
                }
                Err(e) => {
                    eprintln!("MCP client: failed to connect to '{}': {}", config.name, e);
                }
            }
        }

        Self { servers }
    }

    async fn connect_one(config: &McpServerConfig) -> Result<ConnectedServer, CallerError> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);
        for (k, v) in &config.env {
            cmd.env(k, v);
        }

        let transport = TokioChildProcess::new(cmd).map_err(|e| {
            CallerError::Config(format!(
                "Failed to spawn MCP server '{}': {}",
                config.name, e
            ))
        })?;

        let running: RunningService<RoleClient, McpClientHandler> =
            McpClientHandler.serve(transport).await.map_err(|e| {
                CallerError::Config(format!(
                    "MCP handshake with '{}' failed: {}",
                    config.name, e
                ))
            })?;

        let peer: Peer<RoleClient> = running.peer().clone();

        // Discover tools
        let mcp_tools = peer.list_all_tools().await.map_err(|e| {
            CallerError::Config(format!(
                "Failed to list tools from '{}': {}",
                config.name, e
            ))
        })?;

        let tools: Vec<ToolDefinition> = mcp_tools
            .into_iter()
            .map(|t| {
                let schema = serde_json::to_value(&*t.input_schema)
                    .unwrap_or_else(|_| serde_json::json!({"type": "object", "properties": {}}));
                ToolDefinition {
                    name: format!("mcp__{}_{}", config.name, t.name),
                    description: t.description.map(|d| d.to_string()).unwrap_or_default(),
                    parameters: schema,
                }
            })
            .collect();

        Ok(ConnectedServer {
            name: config.name.clone(),
            peer,
            tools,
            _running: running,
        })
    }

    /// Returns all discovered tools across all connected servers.
    pub fn all_tools(&self) -> Vec<ToolDefinition> {
        self.servers.iter().flat_map(|s| s.tools.clone()).collect()
    }

    /// Call a tool on the appropriate server.
    /// Tool names are expected in `mcp__<server>_<tool>` format.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, CallerError> {
        let (server, actual_tool) = self
            .servers
            .iter()
            .filter_map(|s| {
                parse_mcp_tool_name_for_server(tool_name, &s.name).map(|tool| (s, tool))
            })
            .max_by_key(|(s, _)| s.name.len())
            .ok_or_else(|| CallerError::Config(format!("Invalid MCP tool name: {}", tool_name)))?;

        let args_map: Option<serde_json::Map<String, serde_json::Value>> =
            if let serde_json::Value::Object(map) = arguments {
                Some(map)
            } else {
                None
            };

        let result = server
            .peer
            .call_tool(CallToolRequestParams {
                name: actual_tool.to_string().into(),
                arguments: args_map,
                meta: None,
                task: None,
            })
            .await
            .map_err(|e| CallerError::Provider(format!("MCP tool call failed: {}", e)))?;

        Ok(format_call_result(&result))
    }

    /// Check if a tool name belongs to an MCP server.
    pub fn is_mcp_tool(name: &str) -> bool {
        name.starts_with("mcp__")
    }
}

/// Parse `mcp__<server>_<tool>` into `(server, tool)`.
#[allow(dead_code)]
fn parse_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let underscore_pos = rest.find('_')?;
    if underscore_pos == 0 || underscore_pos == rest.len() - 1 {
        return None;
    }
    Some((&rest[..underscore_pos], &rest[underscore_pos + 1..]))
}

fn parse_mcp_tool_name_for_server<'a>(name: &'a str, server: &str) -> Option<&'a str> {
    let prefix = format!("mcp__{}_", server);
    name.strip_prefix(&prefix)
}

/// Format a CallToolResult into a string for the agent.
fn format_call_result(result: &CallToolResult) -> String {
    let mut output = String::new();
    for content in &result.content {
        match content.raw {
            rmcp::model::RawContent::Text(ref t) => {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&t.text);
            }
            _ => {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str("[non-text content]");
            }
        }
    }
    if result.is_error.unwrap_or(false) && output.is_empty() {
        output = "Tool call returned an error with no content.".to_string();
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mcp_tool_name_valid() {
        assert_eq!(
            parse_mcp_tool_name("mcp__github_list_issues"),
            Some(("github", "list_issues"))
        );
    }

    #[test]
    fn parse_mcp_tool_name_single_char_parts() {
        assert_eq!(parse_mcp_tool_name("mcp__a_b"), Some(("a", "b")));
    }

    #[test]
    fn parse_mcp_tool_name_server_with_underscore() {
        assert_eq!(
            parse_mcp_tool_name_for_server("mcp__my_server_list_issues", "my_server"),
            Some("list_issues")
        );
    }

    #[test]
    fn parse_mcp_tool_name_invalid_prefix() {
        assert_eq!(parse_mcp_tool_name("not_mcp__tool"), None);
    }

    #[test]
    fn parse_mcp_tool_name_no_underscore() {
        assert_eq!(parse_mcp_tool_name("mcp__serveronly"), None);
    }

    #[test]
    fn parse_mcp_tool_name_empty_server() {
        assert_eq!(parse_mcp_tool_name("mcp___tool"), None);
    }

    #[test]
    fn is_mcp_tool_true() {
        assert!(McpClientManager::is_mcp_tool("mcp__github_list"));
    }

    #[test]
    fn is_mcp_tool_false() {
        assert!(!McpClientManager::is_mcp_tool("exec_command"));
    }

    #[test]
    fn tool_name_routing() {
        let (server, tool) = parse_mcp_tool_name("mcp__filesystem_read_file").unwrap();
        assert_eq!(server, "filesystem");
        assert_eq!(tool, "read_file");
    }
}
