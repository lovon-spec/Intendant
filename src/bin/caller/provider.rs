use crate::conversation::Message;
use crate::error::CallerError;
use crate::tools::ToolDefinition;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// A tool call returned by the model.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub usage: TokenUsage,
    pub reasoning_summary: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError>;
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    fn context_window(&self) -> u64;
    #[allow(dead_code)]
    fn max_output_tokens(&self) -> u64;

    /// Whether this provider instance has native tool calling enabled.
    fn use_tools(&self) -> bool {
        false
    }

    /// Return tool definitions when native tool calling is enabled.
    #[allow(dead_code)]
    fn tools(&self) -> Vec<ToolDefinition> {
        vec![]
    }
}

// --- OpenAI (Responses API) ---

#[derive(Serialize)]
struct OpenAIResponsesRequest {
    model: String,
    input: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<TextConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
}

#[derive(Serialize, Clone)]
struct ReasoningConfig {
    effort: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
}

#[derive(Serialize)]
struct TextConfig {
    format: TextFormat,
}

#[derive(Serialize)]
struct TextFormat {
    r#type: String,
}

/// Build a Responses API message input item.
fn openai_message_item(role: &str, content: &str) -> serde_json::Value {
    serde_json::json!({
        "role": role,
        "content": content,
    })
}

/// Build a Responses API function_call_output input item.
fn openai_function_call_output(call_id: &str, output: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": output,
    })
}

#[derive(Deserialize)]
struct OpenAIResponsesResponse {
    output_text: Option<String>,
    output: Option<Vec<ResponsesOutputItem>>,
    usage: Option<ResponsesUsage>,
}

#[derive(Deserialize)]
struct ResponsesOutputItem {
    #[serde(rename = "type")]
    item_type: Option<String>,
    content: Option<Vec<ResponsesContentItem>>,
    summary: Option<Vec<ResponsesSummaryItem>>,
    // function_call fields (type="function_call")
    call_id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ResponsesContentItem {
    #[serde(rename = "type")]
    item_type: Option<String>,
    text: Option<String>,
}

#[derive(Deserialize)]
struct ResponsesSummaryItem {
    text: Option<String>,
}

#[derive(Deserialize)]
struct ResponsesUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

pub struct OpenAIProvider {
    client: Client,
    api_key: String,
    model: String,
    context_window: u64,
    max_output_tokens: u64,
    structured_output: bool,
    reasoning: Option<ReasoningConfig>,
    use_tools: bool,
}

impl OpenAIProvider {
    pub fn new(
        api_key: String,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
    ) -> Self {
        let structured_output = resolve_structured_output(&model);
        let reasoning = resolve_reasoning(&model);
        let use_tools = resolve_use_tools();

        Self {
            client: Client::new(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            structured_output,
            reasoning,
            use_tools,
        }
    }
}

#[async_trait]
impl ChatProvider for OpenAIProvider {
    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError> {
        // Extract system instructions
        let instructions = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone());

        // Build input items from messages
        let mut input: Vec<serde_json::Value> = messages
            .iter()
            .filter(|m| m.role != "system")
            .flat_map(|m| {
                let mut items = Vec::new();

                // If this is an assistant message with tool calls, emit function_call items
                if m.role == "assistant" {
                    if let Some(ref tcs) = m.tool_calls {
                        // Emit the assistant message first (may have text content)
                        if !m.content.is_empty() {
                            items.push(openai_message_item(&m.role, &m.content));
                        }
                        // Then emit each function_call as a separate output item
                        for tc in tcs {
                            items.push(serde_json::json!({
                                "type": "function_call",
                                "id": tc.id,
                                "call_id": tc.id,
                                "name": tc.name,
                                "arguments": tc.arguments,
                            }));
                        }
                        return items;
                    }
                }

                // If this is a tool result message, emit as function_call_output
                if m.role == "tool" {
                    if let Some(ref call_id) = m.tool_call_id {
                        items.push(openai_function_call_output(call_id, &m.content));
                        return items;
                    }
                }

                items.push(openai_message_item(&m.role, &m.content));
                items
            })
            .collect();

        // When structured output is enabled (and tools are NOT), require JSON mode
        let use_structured = self.structured_output && !self.use_tools;
        if use_structured {
            input.insert(
                0,
                openai_message_item(
                    "developer",
                    "Always respond with valid JSON matching the command schema.",
                ),
            );
        }

        let text = if use_structured {
            Some(TextConfig {
                format: TextFormat {
                    r#type: "json_object".to_string(),
                },
            })
        } else {
            None
        };

        let tools = if self.use_tools {
            let defs = crate::tools::all_tools();
            Some(defs.iter().map(|t| t.to_openai()).collect())
        } else {
            None
        };

        let request = OpenAIResponsesRequest {
            model: self.model.clone(),
            input,
            instructions,
            max_output_tokens: Some(self.max_output_tokens),
            reasoning: self.reasoning.clone(),
            text,
            tools,
        };

        let response = self
            .client
            .post("https://api.openai.com/v1/responses")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!("{}: {}", status, body)));
        }

        let resp: OpenAIResponsesResponse = response.json().await?;

        // Extract function_call items from the output array
        let mut tool_calls = Vec::new();
        if let Some(ref output_items) = resp.output {
            for item in output_items {
                if item.item_type.as_deref() == Some("function_call") {
                    if let (Some(call_id), Some(name), Some(arguments)) =
                        (&item.call_id, &item.name, &item.arguments)
                    {
                        tool_calls.push(ToolCall {
                            id: call_id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        });
                    }
                }
            }
        }

        // Prefer output_text, fall back to digging into output array.
        // When tool calls are present, text content is optional.
        let content = resp
            .output_text
            .or_else(|| {
                resp.output.as_ref().and_then(|items| {
                    items.iter().find_map(|item| {
                        item.content
                            .as_ref()
                            .and_then(|contents| contents.iter().find_map(|c| c.text.clone()))
                    })
                })
            })
            .unwrap_or_default();

        let usage = resp
            .usage
            .map(|u| TokenUsage {
                prompt_tokens: u.input_tokens,
                completion_tokens: u.output_tokens,
                total_tokens: u.total_tokens,
            })
            .unwrap_or_default();

        // Extract reasoning summary and full content if present in Responses output.
        let reasoning_summary = resp.output.as_ref().and_then(|items| {
            let parts: Vec<String> = items
                .iter()
                .filter(|item| item.item_type.as_deref() == Some("reasoning"))
                .flat_map(|item| {
                    if let Some(summary) = &item.summary {
                        summary
                            .iter()
                            .filter_map(|s| s.text.clone())
                            .collect::<Vec<String>>()
                    } else if let Some(content) = &item.content {
                        content
                            .iter()
                            .filter(|c| {
                                c.item_type
                                    .as_deref()
                                    .is_some_and(|t| t.contains("summary"))
                            })
                            .filter_map(|c| c.text.clone())
                            .collect::<Vec<String>>()
                    } else {
                        Vec::new()
                    }
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        });

        // Extract full reasoning content (all text from reasoning items, not just summaries).
        let reasoning_content = resp.output.as_ref().and_then(|items| {
            let parts: Vec<String> = items
                .iter()
                .filter(|item| item.item_type.as_deref() == Some("reasoning"))
                .flat_map(|item| {
                    let mut texts = Vec::new();
                    if let Some(content) = &item.content {
                        for c in content {
                            if let Some(text) = &c.text {
                                texts.push(text.clone());
                            }
                        }
                    }
                    texts
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        });

        Ok(ChatResponse {
            content,
            usage,
            reasoning_summary,
            reasoning_content,
            tool_calls,
        })
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn context_window(&self) -> u64 {
        self.context_window
    }

    fn max_output_tokens(&self) -> u64 {
        self.max_output_tokens
    }

    fn use_tools(&self) -> bool {
        self.use_tools
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        if self.use_tools {
            crate::tools::all_tools()
        } else {
            vec![]
        }
    }
}

// --- Anthropic ---

#[derive(Serialize)]
struct AnthropicChatRequest {
    model: String,
    system: String,
    messages: Vec<AnthropicMessage>,
    max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
}

/// Anthropic message with content as either a plain string or structured blocks.
#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: serde_json::Value, // String or array of content blocks
}

#[derive(Deserialize)]
struct AnthropicChatResponse {
    content: Vec<AnthropicContent>,
    usage: Option<AnthropicUsage>,
}

#[derive(Deserialize)]
struct AnthropicContent {
    #[serde(rename = "type")]
    content_type: Option<String>,
    text: Option<String>,
    // tool_use fields
    id: Option<String>,
    name: Option<String>,
    input: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u64,
    output_tokens: u64,
}

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    model: String,
    context_window: u64,
    max_output_tokens: u64,
    use_tools: bool,
}

impl AnthropicProvider {
    pub fn new(
        api_key: String,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
    ) -> Self {
        let use_tools = resolve_use_tools();
        Self {
            client: Client::new(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            use_tools,
        }
    }
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError> {
        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone())
            .unwrap_or_default();

        // Build messages, converting tool calls and tool results to content blocks
        let mut api_messages: Vec<AnthropicMessage> = Vec::new();
        for m in messages {
            if m.role == "system" {
                continue;
            }

            // Tool result messages → user message with tool_result content block
            if m.role == "tool" {
                if let Some(ref call_id) = m.tool_call_id {
                    let block = serde_json::json!([{
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": m.content,
                    }]);
                    api_messages.push(AnthropicMessage {
                        role: "user".to_string(),
                        content: block,
                    });
                    continue;
                }
            }

            // Assistant messages with tool calls → content blocks
            if m.role == "assistant" {
                if let Some(ref tcs) = m.tool_calls {
                    let mut blocks = Vec::new();
                    if !m.content.is_empty() {
                        blocks.push(serde_json::json!({
                            "type": "text",
                            "text": m.content,
                        }));
                    }
                    for tc in tcs {
                        let input: serde_json::Value =
                            serde_json::from_str(&tc.arguments).unwrap_or(serde_json::json!({}));
                        blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name,
                            "input": input,
                        }));
                    }
                    api_messages.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: serde_json::Value::Array(blocks),
                    });
                    continue;
                }
            }

            // Regular user/assistant messages
            if m.role == "user" || m.role == "assistant" {
                api_messages.push(AnthropicMessage {
                    role: m.role.clone(),
                    content: serde_json::Value::String(m.content.clone()),
                });
            }
        }

        let tools = if self.use_tools {
            let defs = crate::tools::all_tools();
            Some(defs.iter().map(|t| t.to_anthropic()).collect())
        } else {
            None
        };

        let request = AnthropicChatRequest {
            model: self.model.clone(),
            system,
            messages: api_messages,
            max_tokens: self.max_output_tokens,
            tools,
        };

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!("{}: {}", status, body)));
        }

        let chat_response: AnthropicChatResponse = response.json().await?;

        // Extract text content and tool_use blocks
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();

        for block in &chat_response.content {
            match block.content_type.as_deref() {
                Some("text") => {
                    if let Some(ref text) = block.text {
                        text_parts.push(text.clone());
                    }
                }
                Some("tool_use") => {
                    if let (Some(id), Some(name), Some(input)) =
                        (&block.id, &block.name, &block.input)
                    {
                        tool_calls.push(ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: serde_json::to_string(input).unwrap_or_default(),
                        });
                    }
                }
                _ => {
                    // Legacy: text field without explicit type
                    if let Some(ref text) = block.text {
                        text_parts.push(text.clone());
                    }
                }
            }
        }

        let content = text_parts.join("");

        let usage = chat_response
            .usage
            .map(|u| TokenUsage {
                prompt_tokens: u.input_tokens,
                completion_tokens: u.output_tokens,
                total_tokens: u.input_tokens + u.output_tokens,
            })
            .unwrap_or_default();

        Ok(ChatResponse {
            content,
            usage,
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls,
        })
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn context_window(&self) -> u64 {
        self.context_window
    }

    fn max_output_tokens(&self) -> u64 {
        self.max_output_tokens
    }

    fn use_tools(&self) -> bool {
        self.use_tools
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        if self.use_tools {
            crate::tools::all_tools()
        } else {
            vec![]
        }
    }
}

// --- Gemini ---

pub struct GeminiProvider {
    client: Client,
    api_key: String,
    model: String,
    context_window: u64,
    max_output_tokens: u64,
    use_tools: bool,
    endpoint: String,
}

impl GeminiProvider {
    pub fn new(
        api_key: String,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
    ) -> Self {
        let use_tools = resolve_use_tools();
        let endpoint = env::var("GEMINI_ENDPOINT").unwrap_or_else(|_| {
            "https://generativelanguage.googleapis.com".to_string()
        });
        Self {
            client: Client::new(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            use_tools,
            endpoint,
        }
    }
}

/// Map our role names to Gemini roles.
fn gemini_role(role: &str) -> &str {
    match role {
        "assistant" => "model",
        "user" | "developer" | "tool" => "user",
        _ => "user",
    }
}

#[async_trait]
impl ChatProvider for GeminiProvider {
    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError> {
        // Extract system instruction
        let system_text = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone());

        // Build contents array
        let mut contents: Vec<serde_json::Value> = Vec::new();
        for m in messages {
            if m.role == "system" {
                continue;
            }

            let role = gemini_role(&m.role);

            // Tool result messages → functionResponse parts
            if m.role == "tool" {
                if let (Some(ref _call_id), Some(ref tool_name)) =
                    (&m.tool_call_id, &m.tool_name)
                {
                    let response_val: serde_json::Value =
                        serde_json::from_str(&m.content).unwrap_or(serde_json::json!({
                            "output": m.content,
                        }));
                    contents.push(serde_json::json!({
                        "role": role,
                        "parts": [{
                            "functionResponse": {
                                "name": tool_name,
                                "response": response_val,
                            }
                        }]
                    }));
                    continue;
                }
            }

            // Assistant messages with tool calls → functionCall parts
            if m.role == "assistant" {
                if let Some(ref tcs) = m.tool_calls {
                    let mut parts = Vec::new();
                    if !m.content.is_empty() {
                        parts.push(serde_json::json!({"text": m.content}));
                    }
                    for tc in tcs {
                        let args: serde_json::Value =
                            serde_json::from_str(&tc.arguments).unwrap_or(serde_json::json!({}));
                        parts.push(serde_json::json!({
                            "functionCall": {
                                "name": tc.name,
                                "args": args,
                            }
                        }));
                    }
                    contents.push(serde_json::json!({
                        "role": role,
                        "parts": parts,
                    }));
                    continue;
                }
            }

            // Regular text messages
            contents.push(serde_json::json!({
                "role": role,
                "parts": [{"text": m.content}]
            }));
        }

        let mut request_body = serde_json::json!({
            "contents": contents,
            "generationConfig": {
                "maxOutputTokens": self.max_output_tokens,
            }
        });

        if let Some(ref sys) = system_text {
            request_body["systemInstruction"] = serde_json::json!({
                "parts": [{"text": sys}]
            });
        }

        if self.use_tools {
            let defs = crate::tools::all_tools();
            let func_decls: Vec<serde_json::Value> =
                defs.iter().map(|t| t.to_gemini()).collect();
            request_body["tools"] = serde_json::json!([{
                "functionDeclarations": func_decls,
            }]);
        }

        let url = format!(
            "{}/v1beta/models/{}:generateContent?key={}",
            self.endpoint, self.model, self.api_key
        );

        let response = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&request_body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!("{}: {}", status, body)));
        }

        let resp: serde_json::Value = response.json().await?;

        // Extract content from candidates[0].content.parts[]
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();

        if let Some(parts) = resp
            .pointer("/candidates/0/content/parts")
            .and_then(|p| p.as_array())
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(text.to_string());
                }
                if let Some(fc) = part.get("functionCall") {
                    let name = fc
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = fc
                        .get("args")
                        .map(|a| serde_json::to_string(a).unwrap_or_default())
                        .unwrap_or_else(|| "{}".to_string());
                    // Gemini doesn't provide call IDs; generate one
                    let id = format!("gemini_call_{}", tool_calls.len());
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: args,
                    });
                }
            }
        }

        let content = text_parts.join("");

        // Extract usage
        let usage = resp.get("usageMetadata").map(|u| {
            let prompt = u
                .get("promptTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let completion = u
                .get("candidatesTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let total = u
                .get("totalTokenCount")
                .and_then(|v| v.as_u64())
                .unwrap_or(prompt + completion);
            TokenUsage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: total,
            }
        }).unwrap_or_default();

        Ok(ChatResponse {
            content,
            usage,
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls,
        })
    }

    fn name(&self) -> &str {
        "gemini"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn context_window(&self) -> u64 {
        self.context_window
    }

    fn max_output_tokens(&self) -> u64 {
        self.max_output_tokens
    }

    fn use_tools(&self) -> bool {
        self.use_tools
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        if self.use_tools {
            crate::tools::all_tools()
        } else {
            vec![]
        }
    }
}

// --- Provider selection ---

fn default_context_window(model: &str) -> u64 {
    match model {
        m if m.starts_with("gpt-5") => 400_000,
        m if m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") => 200_000,
        m if m.contains("claude") => 200_000,
        m if m.starts_with("gemini") => 1_048_576,
        _ => 200_000,
    }
}

fn default_max_output_tokens(model: &str) -> u64 {
    match model {
        m if m.starts_with("gpt-5") => 128_000,
        m if m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") => 100_000,
        m if m.contains("claude") => 8_192,
        m if m.starts_with("gemini") => 65_536,
        _ => 16_384,
    }
}

fn resolve_context_window(model: &str) -> u64 {
    env::var("MODEL_CONTEXT_WINDOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| default_context_window(model))
}

fn resolve_max_output_tokens(model: &str) -> u64 {
    env::var("MAX_OUTPUT_TOKENS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| default_max_output_tokens(model))
}

fn supports_structured_output(model: &str) -> bool {
    model.starts_with("gpt-5") || model.starts_with("o3") || model.starts_with("o4")
}

fn resolve_structured_output(model: &str) -> bool {
    env::var("STRUCTURED_OUTPUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| supports_structured_output(model))
}

fn supports_reasoning(model: &str) -> bool {
    model.starts_with("gpt-5") || model.starts_with("o3") || model.starts_with("o4")
}

fn resolve_reasoning(model: &str) -> Option<ReasoningConfig> {
    if !supports_reasoning(model) {
        return None;
    }
    let effort = env::var("REASONING_EFFORT")
        .ok()
        .and_then(|v| {
            let v = v.trim().to_string();
            if v.is_empty() || v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(v)
            }
        })
        .unwrap_or_else(|| "low".to_string());
    let summary = env::var("REASONING_SUMMARY")
        .ok()
        .and_then(|s| {
            let s = s.trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        })
        .or_else(|| Some("auto".to_string()));
    Some(ReasoningConfig { effort, summary })
}

/// Resolve whether native tool calling should be enabled.
/// Checks `USE_NATIVE_TOOLS` env var, defaults to `true`.
pub fn resolve_use_tools() -> bool {
    env::var("USE_NATIVE_TOOLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(true)
}

pub fn select_provider() -> Result<Box<dyn ChatProvider>, CallerError> {
    let openai_key = env::var("OPENAI_API_KEY")
        .or_else(|_| env::var("OPENAI"))
        .ok();
    let anthropic_key = env::var("ANTHROPIC_API_KEY")
        .or_else(|_| env::var("ANTHROPIC"))
        .ok();
    let gemini_key = env::var("GEMINI_API_KEY")
        .or_else(|_| env::var("GEMINI"))
        .ok();

    let preferred = env::var("PROVIDER").ok();

    // Explicit Gemini selection
    if preferred.as_deref() == Some("gemini") {
        let key = gemini_key.ok_or_else(|| {
            CallerError::Config("PROVIDER=gemini but no GEMINI_API_KEY found.".to_string())
        })?;
        let model =
            env::var("MODEL_NAME").unwrap_or_else(|_| "gemini-2.5-pro".to_string());
        let ctx = resolve_context_window(&model);
        let max_out = resolve_max_output_tokens(&model);
        return Ok(Box::new(GeminiProvider::new(key, model, ctx, max_out)));
    }

    match (openai_key, anthropic_key, preferred.as_deref()) {
        // Both available, check PROVIDER preference
        (Some(oai), Some(ant), Some("anthropic")) => {
            let _ = oai;
            let model =
                env::var("MODEL_NAME").unwrap_or_else(|_| "claude-sonnet-4-5-20250929".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(AnthropicProvider::new(ant, model, ctx, max_out)))
        }
        (Some(oai), Some(_ant), Some("openai")) | (Some(oai), Some(_ant), None) => {
            let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gpt-5.2-codex".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(OpenAIProvider::new(oai, model, ctx, max_out)))
        }
        (Some(oai), None, _) => {
            let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gpt-5.2-codex".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(OpenAIProvider::new(oai, model, ctx, max_out)))
        }
        (None, Some(ant), _) => {
            let model =
                env::var("MODEL_NAME").unwrap_or_else(|_| "claude-sonnet-4-5-20250929".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(AnthropicProvider::new(ant, model, ctx, max_out)))
        }
        // Only Gemini key available (no explicit PROVIDER)
        (None, None, _) if gemini_key.is_some() => {
            let key = gemini_key.unwrap();
            let model =
                env::var("MODEL_NAME").unwrap_or_else(|_| "gemini-2.5-pro".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(GeminiProvider::new(key, model, ctx, max_out)))
        }
        (Some(_oai), Some(_ant), Some(other)) => Err(CallerError::Config(format!(
            "Unknown PROVIDER value: '{}'. Expected 'openai', 'anthropic', or 'gemini'.",
            other
        ))),
        (None, None, _) => Err(CallerError::Config(
            "No API key found. Set OPENAI_API_KEY, ANTHROPIC_API_KEY, or GEMINI_API_KEY in your environment, \
             a .env file in your project root, or ~/.config/intendant/.env for global use."
                .to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_provider_name() {
        let provider = OpenAIProvider::new(
            "key".to_string(),
            "gpt-5.2-codex".to_string(),
            400_000,
            128_000,
        );
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn anthropic_provider_name() {
        let provider = AnthropicProvider::new(
            "key".to_string(),
            "claude-sonnet-4-5-20250929".to_string(),
            200_000,
            8_192,
        );
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn anthropic_extracts_system_message() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "You are helpful.".to_string(),
                ..Default::default()
            },
            Message {
                role: "user".to_string(),
                content: "Hello".to_string(),
                ..Default::default()
            },
            Message {
                role: "assistant".to_string(),
                content: "Hi!".to_string(),
                ..Default::default()
            },
        ];

        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone())
            .unwrap_or_default();

        assert_eq!(system, "You are helpful.");

        let api_messages: Vec<AnthropicMessage> = messages
            .iter()
            .filter(|m| m.role == "user" || m.role == "assistant")
            .map(|m| AnthropicMessage {
                role: m.role.clone(),
                content: serde_json::Value::String(m.content.clone()),
            })
            .collect();

        assert_eq!(api_messages.len(), 2);
        assert_eq!(api_messages[0].role, "user");
        assert_eq!(api_messages[1].role, "assistant");
    }

    #[test]
    fn anthropic_no_system_message() {
        let messages = vec![Message {
            role: "user".to_string(),
            content: "Hello".to_string(),
            ..Default::default()
        }];

        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone())
            .unwrap_or_default();

        assert_eq!(system, "");
    }

    #[test]
    fn token_usage_default() {
        let usage = TokenUsage::default();
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
        assert_eq!(usage.total_tokens, 0);
    }

    #[test]
    fn token_usage_serialization() {
        let usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
        };
        let json = serde_json::to_string(&usage).unwrap();
        let deserialized: TokenUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.prompt_tokens, 100);
        assert_eq!(deserialized.completion_tokens, 50);
        assert_eq!(deserialized.total_tokens, 150);
    }

    #[test]
    fn anthropic_usage_deserialization() {
        let json = r#"{
            "content": [{"text": "Hi", "type": "text"}],
            "usage": {"input_tokens": 20, "output_tokens": 10}
        }"#;
        let resp: AnthropicChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.content[0].text.as_deref(), Some("Hi"));
        assert_eq!(resp.content[0].content_type.as_deref(), Some("text"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.output_tokens, 10);
    }

    #[test]
    fn anthropic_usage_missing() {
        let json = r#"{
            "content": [{"text": "Hi", "type": "text"}]
        }"#;
        let resp: AnthropicChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.usage.is_none());
    }

    #[test]
    fn anthropic_tool_use_deserialization() {
        let json = r#"{
            "content": [
                {"type": "text", "text": "I'll list the files."},
                {
                    "type": "tool_use",
                    "id": "toolu_abc123",
                    "name": "exec_command",
                    "input": {"nonce": 1, "command": "ls -la"}
                }
            ],
            "usage": {"input_tokens": 50, "output_tokens": 30}
        }"#;
        let resp: AnthropicChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.content.len(), 2);
        assert_eq!(resp.content[0].content_type.as_deref(), Some("text"));
        assert_eq!(resp.content[0].text.as_deref(), Some("I'll list the files."));
        assert_eq!(resp.content[1].content_type.as_deref(), Some("tool_use"));
        assert_eq!(resp.content[1].id.as_deref(), Some("toolu_abc123"));
        assert_eq!(resp.content[1].name.as_deref(), Some("exec_command"));
        assert!(resp.content[1].input.is_some());
    }

    #[test]
    fn anthropic_request_with_tools() {
        let tool_defs = crate::tools::all_tools();
        let tools: Vec<serde_json::Value> = tool_defs.iter().map(|t| t.to_anthropic()).collect();
        let request = AnthropicChatRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            system: "You are an agent.".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::Value::String("list files".to_string()),
            }],
            max_tokens: 8192,
            tools: Some(tools),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"tools\""));
        assert!(json.contains("exec_command"));
    }

    #[test]
    fn anthropic_request_without_tools() {
        let request = AnthropicChatRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            system: "You are helpful.".to_string(),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::Value::String("hello".to_string()),
            }],
            max_tokens: 8192,
            tools: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("\"tools\""));
    }

    #[test]
    fn anthropic_message_structured_content() {
        let msg = AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!([
                {"type": "text", "text": "Running command"},
                {"type": "tool_use", "id": "toolu_1", "name": "exec_command", "input": {"nonce": 1}}
            ]),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("tool_use"));
        assert!(json.contains("toolu_1"));
    }

    #[test]
    fn default_context_window_known_models() {
        assert_eq!(default_context_window("gpt-5.2-codex"), 400_000);
        assert_eq!(default_context_window("gpt-5"), 400_000);
        assert_eq!(
            default_context_window("claude-sonnet-4-5-20250929"),
            200_000
        );
        assert_eq!(default_context_window("o1-preview"), 200_000);
        assert_eq!(default_context_window("o3-mini"), 200_000);
    }

    #[test]
    fn default_context_window_unknown_model() {
        assert_eq!(default_context_window("some-unknown-model"), 200_000);
    }

    #[test]
    fn default_max_output_known_models() {
        assert_eq!(default_max_output_tokens("gpt-5.2-codex"), 128_000);
        assert_eq!(default_max_output_tokens("gpt-5"), 128_000);
        assert_eq!(
            default_max_output_tokens("claude-sonnet-4-5-20250929"),
            8_192
        );
        assert_eq!(default_max_output_tokens("o1-preview"), 100_000);
    }

    #[test]
    fn context_window_methods() {
        let provider = OpenAIProvider::new(
            "key".to_string(),
            "gpt-5.2-codex".to_string(),
            400_000,
            128_000,
        );
        assert_eq!(provider.context_window(), 400_000);
        assert_eq!(provider.max_output_tokens(), 128_000);

        let provider = AnthropicProvider::new(
            "key".to_string(),
            "claude-sonnet-4-5-20250929".to_string(),
            200_000,
            8_192,
        );
        assert_eq!(provider.context_window(), 200_000);
        assert_eq!(provider.max_output_tokens(), 8_192);
    }

    #[test]
    fn responses_api_response_deserialization() {
        let json = r#"{
            "id": "resp_123",
            "object": "response",
            "output_text": "Hello from Responses API!",
            "output": [
                {
                    "content": [
                        {
                            "text": "Hello from Responses API!",
                            "type": "output_text"
                        }
                    ],
                    "role": "assistant",
                    "type": "message"
                }
            ],
            "usage": {"input_tokens": 25, "output_tokens": 8, "total_tokens": 33}
        }"#;
        let resp: OpenAIResponsesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(
            resp.output_text.as_deref(),
            Some("Hello from Responses API!")
        );
        let usage = resp.usage.unwrap();
        assert_eq!(usage.input_tokens, 25);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.total_tokens, 33);
    }

    #[test]
    fn responses_api_fallback_to_output_array() {
        let json = r#"{
            "id": "resp_456",
            "object": "response",
            "output": [
                {
                    "content": [
                        {
                            "text": "Fallback text",
                            "type": "output_text"
                        }
                    ],
                    "role": "assistant",
                    "type": "message"
                }
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
        }"#;
        let resp: OpenAIResponsesResponse = serde_json::from_str(json).unwrap();
        assert!(resp.output_text.is_none());
        let text = resp.output.as_ref().and_then(|items| {
            items.iter().find_map(|item| {
                item.content
                    .as_ref()
                    .and_then(|contents| contents.iter().find_map(|c| c.text.clone()))
            })
        });
        assert_eq!(text.as_deref(), Some("Fallback text"));
    }

    #[test]
    fn responses_api_request_serialization() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "Hello")],
            instructions: Some("Be helpful.".to_string()),
            max_output_tokens: Some(128_000),
            reasoning: None,
            text: None,
            tools: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"model\":\"gpt-5.2-codex\""));
        assert!(json.contains("\"instructions\":\"Be helpful.\""));
        assert!(json.contains("\"max_output_tokens\":128000"));
        assert!(json.contains("\"role\":\"user\""));
    }

    #[test]
    fn responses_api_request_omits_null_instructions() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "Hi")],
            instructions: None,
            max_output_tokens: None,
            reasoning: None,
            text: None,
            tools: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("instructions"));
        assert!(!json.contains("max_output_tokens"));
        assert!(!json.contains("reasoning"));
        assert!(!json.contains("text"));
        assert!(!json.contains("tools"));
    }

    #[test]
    fn responses_api_request_with_reasoning() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "Hi")],
            instructions: None,
            max_output_tokens: Some(128_000),
            reasoning: Some(ReasoningConfig {
                effort: "high".to_string(),
                summary: Some("auto".to_string()),
            }),
            text: None,
            tools: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"reasoning\""));
        assert!(json.contains("\"effort\":\"high\""));
        assert!(json.contains("\"summary\":\"auto\""));
    }

    #[test]
    fn responses_api_request_reasoning_without_summary() {
        let request = OpenAIResponsesRequest {
            model: "o3-mini".to_string(),
            input: vec![openai_message_item("user", "Hi")],
            instructions: None,
            max_output_tokens: Some(100_000),
            reasoning: Some(ReasoningConfig {
                effort: "medium".to_string(),
                summary: None,
            }),
            text: None,
            tools: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"effort\":\"medium\""));
        assert!(!json.contains("\"summary\""));
    }

    #[test]
    fn responses_api_request_with_structured_output() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "Hi")],
            instructions: None,
            max_output_tokens: Some(128_000),
            reasoning: None,
            text: Some(TextConfig {
                format: TextFormat {
                    r#type: "json_object".to_string(),
                },
            }),
            tools: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"text\""));
        assert!(json.contains("\"json_object\""));
    }

    #[test]
    fn responses_api_request_with_tools() {
        let tool_defs = crate::tools::all_tools();
        let tools: Vec<serde_json::Value> = tool_defs.iter().map(|t| t.to_openai()).collect();
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "list files")],
            instructions: Some("You are an agent.".to_string()),
            max_output_tokens: Some(128_000),
            reasoning: None,
            text: None,
            tools: Some(tools),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"tools\""));
        assert!(json.contains("exec_command"));
        // When tools are present, text/json_object should not be
        assert!(!json.contains("json_object"));
    }

    #[test]
    fn responses_api_function_call_deserialization() {
        let json = r#"{
            "output": [
                {
                    "type": "function_call",
                    "call_id": "call_abc123",
                    "name": "exec_command",
                    "arguments": "{\"nonce\":1,\"command\":\"ls -la\"}"
                },
                {
                    "type": "function_call",
                    "call_id": "call_def456",
                    "name": "fetch_status",
                    "arguments": "{\"nonce\":1,\"status_type\":\"stdout\"}"
                }
            ],
            "usage": {"input_tokens": 100, "output_tokens": 50, "total_tokens": 150}
        }"#;
        let resp: OpenAIResponsesResponse = serde_json::from_str(json).unwrap();
        let items = resp.output.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].item_type.as_deref(), Some("function_call"));
        assert_eq!(items[0].call_id.as_deref(), Some("call_abc123"));
        assert_eq!(items[0].name.as_deref(), Some("exec_command"));
        assert!(items[0].arguments.as_ref().unwrap().contains("ls -la"));
        assert_eq!(items[1].name.as_deref(), Some("fetch_status"));
    }

    #[test]
    fn openai_function_call_output_format() {
        let item = openai_function_call_output("call_abc", "1c0");
        assert_eq!(item["type"].as_str(), Some("function_call_output"));
        assert_eq!(item["call_id"].as_str(), Some("call_abc"));
        assert_eq!(item["output"].as_str(), Some("1c0"));
    }

    #[test]
    fn supports_structured_output_models() {
        assert!(supports_structured_output("gpt-5.2-codex"));
        assert!(supports_structured_output("gpt-5"));
        assert!(supports_structured_output("o3-mini"));
        assert!(supports_structured_output("o4-mini"));
        assert!(!supports_structured_output("claude-sonnet-4-5-20250929"));
        assert!(!supports_structured_output("some-unknown-model"));
    }

    #[test]
    fn supports_reasoning_models() {
        assert!(supports_reasoning("gpt-5.2-codex"));
        assert!(supports_reasoning("gpt-5"));
        assert!(supports_reasoning("o3-mini"));
        assert!(supports_reasoning("o4-mini"));
        assert!(!supports_reasoning("claude-sonnet-4-5-20250929"));
    }

    #[test]
    fn default_context_window_o4() {
        assert_eq!(default_context_window("o4-mini"), 200_000);
        assert_eq!(default_context_window("o4"), 200_000);
    }

    #[test]
    fn default_max_output_o4() {
        assert_eq!(default_max_output_tokens("o4-mini"), 100_000);
        assert_eq!(default_max_output_tokens("o4"), 100_000);
    }

    #[test]
    fn responses_role_mapping_preserves_developer() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "System prompt".to_string(),
                ..Default::default()
            },
            Message {
                role: "developer".to_string(),
                content: "Developer note".to_string(),
                ..Default::default()
            },
            Message {
                role: "user".to_string(),
                content: "Hello".to_string(),
                ..Default::default()
            },
            Message {
                role: "assistant".to_string(),
                content: "Hi".to_string(),
                ..Default::default()
            },
        ];

        let instructions = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone());
        assert_eq!(instructions.as_deref(), Some("System prompt"));

        let input: Vec<serde_json::Value> = messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| openai_message_item(&m.role, &m.content))
            .collect();

        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"].as_str(), Some("developer"));
        assert_eq!(input[1]["role"].as_str(), Some("user"));
        assert_eq!(input[2]["role"].as_str(), Some("assistant"));
    }

    #[test]
    fn openai_provider_stores_config() {
        let provider = OpenAIProvider::new(
            "key".to_string(),
            "gpt-5.2-codex".to_string(),
            400_000,
            128_000,
        );
        assert_eq!(provider.max_output_tokens(), 128_000);
        // gpt-5 supports structured output by default
        assert!(provider.structured_output);
    }

    #[test]
    fn chat_response_default_empty_tool_calls() {
        let resp = ChatResponse {
            content: "hello".to_string(),
            usage: TokenUsage::default(),
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls: vec![],
        };
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn tool_call_fields() {
        let tc = ToolCall {
            id: "call_123".to_string(),
            name: "exec_command".to_string(),
            arguments: r#"{"nonce":1,"command":"ls"}"#.to_string(),
        };
        assert_eq!(tc.id, "call_123");
        assert_eq!(tc.name, "exec_command");
        assert!(tc.arguments.contains("nonce"));
    }

    #[test]
    fn resolve_use_tools_default() {
        // When USE_NATIVE_TOOLS is not set, defaults to true.
        // We can't guarantee the env state, but the function should not panic.
        let _ = resolve_use_tools();
    }

    #[test]
    fn openai_provider_use_tools_trait() {
        let provider = OpenAIProvider::new(
            "key".to_string(),
            "gpt-5.2-codex".to_string(),
            400_000,
            128_000,
        );
        // use_tools depends on env, but tools() should return matching vec
        if provider.use_tools() {
            assert!(!provider.tools().is_empty());
        } else {
            assert!(provider.tools().is_empty());
        }
    }

    #[test]
    fn anthropic_provider_use_tools_trait() {
        let provider = AnthropicProvider::new(
            "key".to_string(),
            "claude-sonnet-4-5-20250929".to_string(),
            200_000,
            8_192,
        );
        if provider.use_tools() {
            assert!(!provider.tools().is_empty());
        } else {
            assert!(provider.tools().is_empty());
        }
    }

    // --- Gemini tests ---

    #[test]
    fn gemini_provider_name() {
        let provider = GeminiProvider::new(
            "key".to_string(),
            "gemini-2.5-pro".to_string(),
            1_048_576,
            65_536,
        );
        assert_eq!(provider.name(), "gemini");
        assert_eq!(provider.model(), "gemini-2.5-pro");
        assert_eq!(provider.context_window(), 1_048_576);
        assert_eq!(provider.max_output_tokens(), 65_536);
    }

    #[test]
    fn gemini_role_mapping() {
        assert_eq!(gemini_role("assistant"), "model");
        assert_eq!(gemini_role("user"), "user");
        assert_eq!(gemini_role("developer"), "user");
        assert_eq!(gemini_role("tool"), "user");
        assert_eq!(gemini_role("system"), "user");
    }

    #[test]
    fn default_context_window_gemini() {
        assert_eq!(default_context_window("gemini-2.5-pro"), 1_048_576);
        assert_eq!(default_context_window("gemini-2.5-flash"), 1_048_576);
    }

    #[test]
    fn default_max_output_gemini() {
        assert_eq!(default_max_output_tokens("gemini-2.5-pro"), 65_536);
        assert_eq!(default_max_output_tokens("gemini-2.5-flash"), 65_536);
    }

    #[test]
    fn gemini_response_text_parsing() {
        let resp: serde_json::Value = serde_json::from_str(r#"{
            "candidates": [{
                "content": {
                    "parts": [{"text": "Hello from Gemini!"}],
                    "role": "model"
                }
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15
            }
        }"#).unwrap();

        let text = resp
            .pointer("/candidates/0/content/parts/0/text")
            .and_then(|t| t.as_str());
        assert_eq!(text, Some("Hello from Gemini!"));

        let total = resp
            .pointer("/usageMetadata/totalTokenCount")
            .and_then(|v| v.as_u64());
        assert_eq!(total, Some(15));
    }

    #[test]
    fn gemini_response_function_call_parsing() {
        let resp: serde_json::Value = serde_json::from_str(r#"{
            "candidates": [{
                "content": {
                    "parts": [
                        {
                            "functionCall": {
                                "name": "exec_command",
                                "args": {"nonce": 1, "command": "ls -la"}
                            }
                        },
                        {
                            "functionCall": {
                                "name": "fetch_status",
                                "args": {"nonce": 1, "status_type": "stdout"}
                            }
                        }
                    ],
                    "role": "model"
                }
            }],
            "usageMetadata": {"promptTokenCount": 50, "candidatesTokenCount": 20, "totalTokenCount": 70}
        }"#).unwrap();

        let parts = resp
            .pointer("/candidates/0/content/parts")
            .and_then(|p| p.as_array())
            .unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0]["functionCall"]["name"].as_str(),
            Some("exec_command")
        );
        assert_eq!(
            parts[1]["functionCall"]["name"].as_str(),
            Some("fetch_status")
        );
    }

    #[test]
    fn gemini_provider_use_tools_trait() {
        let provider = GeminiProvider::new(
            "key".to_string(),
            "gemini-2.5-pro".to_string(),
            1_048_576,
            65_536,
        );
        if provider.use_tools() {
            assert!(!provider.tools().is_empty());
        } else {
            assert!(provider.tools().is_empty());
        }
    }

    #[test]
    fn gemini_endpoint_default() {
        let provider = GeminiProvider::new(
            "key".to_string(),
            "gemini-2.5-pro".to_string(),
            1_048_576,
            65_536,
        );
        assert!(provider.endpoint.contains("generativelanguage.googleapis.com"));
    }
}
