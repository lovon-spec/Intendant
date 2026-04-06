use crate::conversation::Message;
use crate::error::CallerError;
use crate::tools::ToolDefinition;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::time::Duration;

/// HTTP client timeout for API requests (120 seconds).
const API_TIMEOUT: Duration = Duration::from_secs(120);

/// Maximum number of retries for rate-limited or server-error responses.
const MAX_RETRIES: u32 = 5;

fn api_client() -> Client {
    Client::builder()
        .timeout(API_TIMEOUT)
        .build()
        .unwrap_or_else(|_| Client::new())
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn backoff_delay(attempt: u32) -> Duration {
    let base_ms = 1000u64 * 2u64.saturating_pow(attempt);
    // Add simple jitter: up to 500ms
    let jitter_ms = (attempt as u64 * 137) % 500;
    Duration::from_millis(base_ms + jitter_ms)
}

async fn send_with_retry(
    _client: &Client,
    build_request: impl Fn() -> reqwest::RequestBuilder,
    max_retries: u32,
) -> Result<reqwest::Response, CallerError> {
    let mut last_err = None;
    for attempt in 0..=max_retries {
        let response = build_request().send().await?;
        if response.status().is_success() || !is_retryable_status(response.status()) {
            return Ok(response);
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        last_err = Some(format!("{}: {}", status, mask_api_keys(&body)));
        if attempt < max_retries {
            tokio::time::sleep(backoff_delay(attempt)).await;
        }
    }
    Err(CallerError::Provider(last_err.unwrap_or_else(|| {
        "request failed after retries".to_string()
    })))
}

/// Parse Server-Sent Events from a byte stream. Returns (event_type, data) pairs.
/// Handles multi-line data fields by joining with newlines.
fn parse_sse_line(line: &str) -> Option<(&str, &str)> {
    if let Some(rest) = line.strip_prefix("data: ") {
        Some(("data", rest))
    } else if let Some(rest) = line.strip_prefix("event: ") {
        Some(("event", rest))
    } else {
        None
    }
}

/// Streaming timeout for SSE connections (10 minutes).
const STREAM_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// Tokens served from cache (subset of prompt_tokens, cheaper pricing).
    #[serde(default)]
    pub cached_tokens: u64,
}

/// A tool call returned by the model.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// The item ID (fc_-prefixed for Responses API, call_-prefixed for others).
    pub id: String,
    /// The correlation key used to pair calls with outputs (call_-prefixed).
    /// For Responses API this is distinct from `id`; for other providers it equals `id`.
    pub call_id: String,
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
    /// Native computer-use tool calls (parsed from provider-specific format).
    pub cu_calls: Vec<super::computer_use::CuToolCall>,
    /// Raw output items from the Responses API (reasoning + function_call items).
    /// Echoed back verbatim in subsequent requests per the API contract.
    pub raw_output: Option<Vec<serde_json::Value>>,
}

/// Events emitted during streaming responses.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A text delta from the model.
    Delta(String),
    /// A tool call delta (accumulated; final call emitted with Complete).
    #[allow(dead_code)]
    ToolCallDelta {
        index: usize,
        id: String,
        name: String,
        arguments_delta: String,
    },
    /// The complete response (same as non-streaming `chat()` would return).
    #[allow(dead_code)]
    Complete(ChatResponse),
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

    /// Whether this provider instance has native computer-use enabled.
    fn cu_enabled(&self) -> bool {
        false
    }

    /// Display dimensions for CU (width, height), if CU is enabled.
    fn cu_display(&self) -> Option<(u32, u32)> {
        None
    }

    /// Override display dimensions for CU. Used when the actual display size
    /// differs from the default (e.g. user's real display vs virtual display).
    fn set_cu_display(&mut self, _dims: (u32, u32)) {}


    /// Return tool definitions when native tool calling is enabled.
    #[allow(dead_code)]
    fn tools(&self) -> Vec<ToolDefinition> {
        vec![]
    }

    /// Stream a chat response, emitting deltas via the callback.
    /// Default implementation falls back to non-streaming `chat()`.
    async fn chat_stream(
        &self,
        messages: &[Message],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ChatResponse, CallerError> {
        let response = self.chat(messages).await?;
        if !response.content.is_empty() {
            on_event(StreamEvent::Delta(response.content.clone()));
        }
        on_event(StreamEvent::Complete(response.clone()));
        Ok(response)
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
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
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

/// Parse an OpenAI computer_call action into a CuAction.
fn parse_openai_cu_action(action: &serde_json::Value) -> Option<super::computer_use::CuAction> {
    use super::computer_use::*;

    let action_type = action.get("type")?.as_str()?;
    let x = || action.get("x").and_then(|v| v.as_i64()).map(|v| v as i32);
    let y = || action.get("y").and_then(|v| v.as_i64()).map(|v| v as i32);

    match action_type {
        "screenshot" => Some(CuAction::Screenshot),
        "click" => {
            let button = match action.get("button").and_then(|v| v.as_str()) {
                Some("right") => MouseButton::Right,
                Some("middle") => MouseButton::Middle,
                _ => MouseButton::Left,
            };
            Some(CuAction::Click { x: x()?, y: y()?, button })
        }
        "double_click" => {
            Some(CuAction::DoubleClick { x: x()?, y: y()?, button: MouseButton::Left })
        }
        "type" => {
            let text = action.get("text")?.as_str()?.to_string();
            Some(CuAction::Type { text })
        }
        "keypress" => {
            let keys = action.get("keys")?.as_array()?;
            let key = keys.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("+");
            Some(CuAction::Key { key })
        }
        "scroll" => {
            let scroll_x = action.get("scroll_x").and_then(|v| v.as_i64()).unwrap_or(0);
            let scroll_y = action.get("scroll_y").and_then(|v| v.as_i64()).unwrap_or(0);
            let (direction, amount) = if scroll_y < 0 {
                (ScrollDirection::Up, (-scroll_y) as i32)
            } else if scroll_y > 0 {
                (ScrollDirection::Down, scroll_y as i32)
            } else if scroll_x < 0 {
                (ScrollDirection::Left, (-scroll_x) as i32)
            } else {
                (ScrollDirection::Right, scroll_x.max(1) as i32)
            };
            // Convert pixel scroll to click counts (roughly 120px per notch)
            let clicks = (amount / 120).max(1);
            Some(CuAction::Scroll { x: x()?, y: y()?, direction, amount: clicks })
        }
        "drag" => {
            let path = action.get("path")?.as_array()?;
            let start = path.first()?;
            let end = path.last()?;
            Some(CuAction::Drag {
                start_x: start.get("x")?.as_i64()? as i32,
                start_y: start.get("y")?.as_i64()? as i32,
                end_x: end.get("x")?.as_i64()? as i32,
                end_y: end.get("y")?.as_i64()? as i32,
            })
        }
        "move" => Some(CuAction::MoveMouse { x: x()?, y: y()? }),
        "wait" => {
            let ms = action.get("ms").and_then(|v| v.as_u64()).unwrap_or(1000);
            Some(CuAction::Wait { ms })
        }
        _ => None,
    }
}

#[derive(Deserialize)]
struct OpenAIResponsesResponse {
    output_text: Option<String>,
    output: Option<Vec<ResponsesOutputItem>>,
    usage: Option<ResponsesUsage>,
}

/// Minimal wrapper to capture raw output items as JSON values.
#[derive(Deserialize)]
struct OpenAIResponsesRawOutput {
    output: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct ResponsesOutputItem {
    #[serde(rename = "type")]
    item_type: Option<String>,
    content: Option<Vec<ResponsesContentItem>>,
    summary: Option<Vec<ResponsesSummaryItem>>,
    // function_call fields (type="function_call")
    /// Item ID (fc_-prefixed), used when echoing function_call back in input.
    id: Option<String>,
    /// Correlation key (call_-prefixed), used for function_call_output.
    call_id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
    // computer_call fields (type="computer_call")
    actions: Option<Vec<serde_json::Value>>,
    pending_safety_checks: Option<Vec<serde_json::Value>>,
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
    /// Cached input tokens (subset of input_tokens). OpenAI Responses API
    /// returns this in `input_tokens_details.cached_tokens`.
    #[serde(default)]
    input_tokens_details: Option<ResponsesInputTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct ResponsesInputTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
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
    custom_tools: Option<Vec<ToolDefinition>>,
    /// When true, include native computer-use tool in API requests.
    pub cu_enabled: bool,
    /// Display dimensions for CU (width, height).
    pub cu_display: Option<(u32, u32)>,
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
            client: api_client(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            structured_output,
            reasoning,
            use_tools,
            custom_tools: None,
            cu_enabled: false,
            cu_display: None,
        }
    }

    pub fn new_plain(
        api_key: String,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
    ) -> Self {
        Self {
            client: api_client(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            structured_output: false,
            reasoning: None,
            use_tools: false,
            custom_tools: None,
            cu_enabled: false,
            cu_display: None,
        }
    }

    pub fn new_with_tools(
        api_key: String,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
        tools: Vec<ToolDefinition>,
    ) -> Self {
        Self {
            client: api_client(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            structured_output: false,
            reasoning: None,
            use_tools: true,
            custom_tools: Some(tools),
            cu_enabled: false,
            cu_display: None,
        }
    }
}

#[async_trait]
impl ChatProvider for OpenAIProvider {
    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError> {
        let (instructions, input, text, tools) = build_openai_request_parts(messages, self);

        let request = OpenAIResponsesRequest {
            model: self.model.clone(),
            input,
            instructions,
            max_output_tokens: Some(self.max_output_tokens),
            reasoning: self.reasoning.clone(),
            text,
            tools,
            stream: false,
        };

        // Note: OpenAI Responses API uses automatic prompt caching for prompts
        // longer than 1024 tokens. No explicit API changes are needed — caching
        // is applied server-side and reported via usage.prompt_tokens_details.
        let request_json = serde_json::to_value(&request).map_err(CallerError::Json)?;
        let client = &self.client;
        let api_key = &self.api_key;
        let response = send_with_retry(
            client,
            || {
                client
                    .post("https://api.openai.com/v1/responses")
                    .header("Authorization", format!("Bearer {}", api_key))
                    .json(&request_json)
            },
            MAX_RETRIES,
        )
        .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!(
                "{}: {}",
                status,
                mask_api_keys(&body)
            )));
        }

        let body = response.text().await?;
        let resp: OpenAIResponsesResponse = serde_json::from_str(&body)?;
        // Capture raw output items for verbatim echo-back (reasoning + function_call items)
        let raw_output = serde_json::from_str::<OpenAIResponsesRawOutput>(&body)
            .ok()
            .and_then(|r| r.output);

        // Extract function_call and computer_call items from the output array
        let mut tool_calls = Vec::new();
        let mut cu_calls = Vec::new();
        if let Some(ref output_items) = resp.output {
            for item in output_items {
                match item.item_type.as_deref() {
                    Some("function_call") => {
                        if let (Some(call_id), Some(name), Some(arguments)) =
                            (&item.call_id, &item.name, &item.arguments)
                        {
                            tool_calls.push(ToolCall {
                                id: item.id.clone().unwrap_or_else(|| call_id.clone()),
                                call_id: call_id.clone(),
                                name: name.clone(),
                                arguments: arguments.clone(),
                            });
                        }
                    }
                    Some("computer_call") if self.cu_enabled => {
                        if let Some(call_id) = &item.call_id {
                            let actions = item.actions.as_ref()
                                .map(|arr| arr.iter().filter_map(parse_openai_cu_action).collect())
                                .unwrap_or_default();
                            let safety = item.pending_safety_checks.clone().unwrap_or_default();
                            cu_calls.push(super::computer_use::CuToolCall {
                                call_id: call_id.clone(),
                                actions,
                                metadata: super::computer_use::CuCallMetadata {
                                    pending_safety_checks: safety,
                                    ..Default::default()
                                },
                            });
                        }
                    }
                    _ => {}
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
            .map(|u| {
                let cached = u
                    .input_tokens_details
                    .as_ref()
                    .map(|d| d.cached_tokens)
                    .unwrap_or(0);
                TokenUsage {
                    prompt_tokens: u.input_tokens,
                    completion_tokens: u.output_tokens,
                    total_tokens: u.total_tokens,
                    cached_tokens: cached,
                }
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
            cu_calls,
            raw_output,
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

    fn cu_enabled(&self) -> bool {
        self.cu_enabled
    }

    fn cu_display(&self) -> Option<(u32, u32)> {
        self.cu_display
    }

    fn set_cu_display(&mut self, dims: (u32, u32)) {
        self.cu_display = Some(dims);
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        if self.use_tools {
            self.custom_tools.clone().unwrap_or_else(|| crate::tools::all_tools())
        } else {
            vec![]
        }
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ChatResponse, CallerError> {
        let (instructions, input, text, tools) = build_openai_request_parts(messages, self);
        let request = OpenAIResponsesRequest {
            model: self.model.clone(),
            input,
            instructions,
            max_output_tokens: Some(self.max_output_tokens),
            reasoning: self.reasoning.clone(),
            text,
            tools,
            stream: true,
        };
        let request_json = serde_json::to_value(&request).map_err(CallerError::Json)?;
        let client = &self.client;
        let api_key = &self.api_key;

        let response = client
            .post("https://api.openai.com/v1/responses")
            .header("Authorization", format!("Bearer {}", api_key))
            .timeout(STREAM_TIMEOUT)
            .json(&request_json)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!(
                "{}: {}",
                status,
                mask_api_keys(&body)
            )));
        }

        // Parse SSE stream
        let mut text_parts = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut cu_calls: Vec<super::computer_use::CuToolCall> = Vec::new();
        let mut raw_output_items: Vec<serde_json::Value> = Vec::new();
        let mut usage = TokenUsage::default();
        let mut reasoning_summary_parts = Vec::new();
        let reasoning_content_parts: Vec<String> = Vec::new();
        // Track in-progress function calls by index
        let mut pending_tools: std::collections::HashMap<usize, ToolCall> =
            std::collections::HashMap::new();
        let mut line_buf = String::new();

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| CallerError::Provider(format!("Stream error: {}", e)))?;
            let chunk_str = String::from_utf8_lossy(&chunk);
            line_buf.push_str(&chunk_str);

            while let Some(newline_pos) = line_buf.find('\n') {
                let line = line_buf[..newline_pos].trim_end_matches('\r').to_string();
                line_buf = line_buf[newline_pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }
                if let Some(("data", data)) = parse_sse_line(&line) {
                    if data == "[DONE]" {
                        continue;
                    }
                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match event_type {
                            "response.output_text.delta" => {
                                if let Some(delta) = event.get("delta").and_then(|d| d.as_str()) {
                                    text_parts.push(delta.to_string());
                                    on_event(StreamEvent::Delta(delta.to_string()));
                                }
                            }
                            "response.output_item.added" => {
                                // Track raw output items
                                if let Some(item) = event.get("item") {
                                    raw_output_items.push(item.clone());
                                    let item_type =
                                        item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    if item_type == "function_call" {
                                        let idx = event
                                            .get("output_index")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0)
                                            as usize;
                                        let id = item
                                            .get("id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let call_id = item
                                            .get("call_id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let name = item
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        pending_tools.insert(
                                            idx,
                                            ToolCall {
                                                id,
                                                call_id,
                                                name,
                                                arguments: String::new(),
                                            },
                                        );
                                    }
                                }
                            }
                            "response.function_call_arguments.delta" => {
                                let idx = event
                                    .get("output_index")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    as usize;
                                if let Some(delta) = event.get("delta").and_then(|d| d.as_str()) {
                                    if let Some(tc) = pending_tools.get_mut(&idx) {
                                        tc.arguments.push_str(delta);
                                    }
                                }
                            }
                            "response.function_call_arguments.done" => {
                                let idx = event
                                    .get("output_index")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0)
                                    as usize;
                                if let Some(tc) = pending_tools.remove(&idx) {
                                    tool_calls.push(tc);
                                }
                            }
                            "response.output_item.done" => {
                                // Update raw output with final item
                                if let Some(item) = event.get("item") {
                                    let idx = event
                                        .get("output_index")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0)
                                        as usize;
                                    if idx < raw_output_items.len() {
                                        raw_output_items[idx] = item.clone();
                                    }
                                    let item_type =
                                        item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    // Parse computer_call items
                                    if item_type == "computer_call" && self.cu_enabled {
                                        if let Some(call_id) = item.get("call_id").and_then(|v| v.as_str()) {
                                            let actions = item.get("actions")
                                                .and_then(|a| a.as_array())
                                                .map(|arr| arr.iter().filter_map(parse_openai_cu_action).collect())
                                                .unwrap_or_default();
                                            let safety = item.get("pending_safety_checks")
                                                .and_then(|v| v.as_array())
                                                .cloned()
                                                .unwrap_or_default();
                                            cu_calls.push(super::computer_use::CuToolCall {
                                                call_id: call_id.to_string(),
                                                actions,
                                                metadata: super::computer_use::CuCallMetadata {
                                                    pending_safety_checks: safety,
                                                    ..Default::default()
                                                },
                                            });
                                        }
                                    }
                                    // Extract reasoning summary
                                    if item_type == "reasoning" {
                                        if let Some(summary) =
                                            item.get("summary").and_then(|s| s.as_array())
                                        {
                                            for s in summary {
                                                if let Some(text) =
                                                    s.get("text").and_then(|t| t.as_str())
                                                {
                                                    reasoning_summary_parts.push(text.to_string());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            "response.completed" => {
                                if let Some(resp) = event.get("response") {
                                    if let Some(u) = resp.get("usage") {
                                        usage.prompt_tokens = u
                                            .get("input_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0);
                                        usage.completion_tokens = u
                                            .get("output_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0);
                                        usage.total_tokens = u
                                            .get("total_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(
                                                usage.prompt_tokens + usage.completion_tokens,
                                            );
                                        usage.cached_tokens = u
                                            .get("input_tokens_details")
                                            .and_then(|d| d.get("cached_tokens"))
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // Flush any remaining pending tool calls
        let mut remaining_indices: Vec<usize> = pending_tools.keys().copied().collect();
        remaining_indices.sort();
        for idx in remaining_indices {
            if let Some(tc) = pending_tools.remove(&idx) {
                tool_calls.push(tc);
            }
        }

        let content = text_parts.join("");
        let reasoning_summary = if reasoning_summary_parts.is_empty() {
            None
        } else {
            Some(reasoning_summary_parts.join("\n"))
        };
        let reasoning_content = if reasoning_content_parts.is_empty() {
            None
        } else {
            Some(reasoning_content_parts.join(""))
        };
        let raw_output = if raw_output_items.is_empty() {
            None
        } else {
            Some(raw_output_items)
        };

        let response = ChatResponse {
            content,
            usage,
            reasoning_summary,
            reasoning_content,
            tool_calls,
            cu_calls,
            raw_output,
        };
        on_event(StreamEvent::Complete(response.clone()));
        Ok(response)
    }
}

/// Build OpenAI request parts (shared between streaming and non-streaming).
fn build_openai_request_parts(
    messages: &[Message],
    provider: &OpenAIProvider,
) -> (
    Option<String>,
    Vec<serde_json::Value>,
    Option<TextConfig>,
    Option<Vec<serde_json::Value>>,
) {
    let instructions = messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| m.content.clone());

    let mut input: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| m.role != "system")
        .flat_map(|m| {
            let mut items = Vec::new();
            if m.role == "assistant" && m.tool_calls.is_some() {
                if let Some(ref raw) = m.raw_output {
                    items.extend(raw.iter().cloned());
                    return items;
                }
                if let Some(ref tcs) = m.tool_calls {
                    if !m.content.is_empty() {
                        items.push(openai_message_item(&m.role, &m.content));
                    }
                    for tc in tcs {
                        items.push(serde_json::json!({
                            "type": "function_call",
                            "id": tc.id,
                            "call_id": tc.call_id,
                            "name": tc.name,
                            "arguments": tc.arguments,
                        }));
                    }
                    return items;
                }
            }
            if m.role == "tool" {
                if let Some(ref call_id) = m.tool_call_id {
                    if m.is_cu_result {
                        // Native CU result: computer_call_output format
                        let screenshot = m.images.as_ref().and_then(|imgs| imgs.first());
                        let mut output_item = serde_json::json!({
                            "type": "computer_call_output",
                            "call_id": call_id,
                        });
                        if let Some(img) = screenshot {
                            output_item["output"] = serde_json::json!({
                                "type": "computer_screenshot",
                                "image_url": format!("data:{};base64,{}", img.media_type, img.data),
                            });
                        }
                        items.push(output_item);
                    } else {
                        items.push(openai_function_call_output(call_id, &m.content));
                        if let Some(ref images) = m.images {
                            let mut content_parts = vec![serde_json::json!({
                                "type": "input_text",
                                "text": "Screenshot from the previous tool call:",
                            })];
                            for img in images {
                                content_parts.push(serde_json::json!({
                                    "type": "input_image",
                                    "image_url": format!("data:{};base64,{}", img.media_type, img.data),
                                }));
                            }
                            items.push(serde_json::json!({
                                "role": "user",
                                "content": content_parts,
                            }));
                        }
                    }
                    return items;
                }
            }
            // User messages with images: multipart content
            if m.role == "user" {
                if let Some(ref images) = m.images {
                    let mut content_parts = vec![serde_json::json!({
                        "type": "input_text",
                        "text": m.content,
                    })];
                    for img in images {
                        content_parts.push(serde_json::json!({
                            "type": "input_image",
                            "image_url": format!("data:{};base64,{}", img.media_type, img.data),
                        }));
                    }
                    items.push(serde_json::json!({
                        "role": "user",
                        "content": content_parts,
                    }));
                    return items;
                }
            }
            items.push(openai_message_item(&m.role, &m.content));
            items
        })
        .collect();

    let use_structured = provider.structured_output && !provider.use_tools;
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

    let mut tools_vec: Vec<serde_json::Value> = Vec::new();
    if provider.use_tools {
        let defs = provider.tools();
        tools_vec.extend(defs.iter().map(|t| t.to_openai()));
    }
    if provider.cu_enabled {
        if let Some((w, h)) = provider.cu_display {
            tools_vec.push(serde_json::json!({
                "type": "computer",
                "display_width": w,
                "display_height": h
            }));
        }
    }
    let tools = if tools_vec.is_empty() { None } else { Some(tools_vec) };

    (instructions, input, text, tools)
}

// --- Anthropic ---

#[derive(Serialize)]
struct AnthropicChatRequest {
    model: String,
    system: serde_json::Value,
    messages: Vec<AnthropicMessage>,
    max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
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
    /// Tokens read from Anthropic prompt cache (subset of input_tokens).
    #[serde(default)]
    cache_read_input_tokens: u64,
}

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    model: String,
    context_window: u64,
    max_output_tokens: u64,
    use_tools: bool,
    custom_tools: Option<Vec<ToolDefinition>>,
    /// When true, include native computer-use tool in API requests.
    pub cu_enabled: bool,
    /// Display dimensions for CU (width, height).
    pub cu_display: Option<(u32, u32)>,
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
            client: api_client(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            use_tools,
            custom_tools: None,
            cu_enabled: false,
            cu_display: None,
        }
    }

    pub fn new_plain(
        api_key: String,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
    ) -> Self {
        Self {
            client: api_client(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            use_tools: false,
            custom_tools: None,
            cu_enabled: false,
            cu_display: None,
        }
    }

    pub fn new_with_tools(
        api_key: String,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
        tools: Vec<ToolDefinition>,
    ) -> Self {
        Self {
            client: api_client(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            use_tools: true,
            custom_tools: Some(tools),
            cu_enabled: false,
            cu_display: None,
        }
    }
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError> {
        let (system, api_messages) = build_anthropic_messages(messages);

        let mut tools_vec: Vec<serde_json::Value> = Vec::new();
        if self.use_tools {
            let defs = self.tools();
            tools_vec.extend(defs.iter().map(|t| t.to_anthropic()));
        }
        if self.cu_enabled {
            if let Some((w, h)) = self.cu_display {
                tools_vec.push(serde_json::json!({
                    "type": "computer_20251124",
                    "name": "computer",
                    "display_width_px": w,
                    "display_height_px": h
                }));
            }
        }
        let tools = if tools_vec.is_empty() { None } else { Some(tools_vec) };

        let request = AnthropicChatRequest {
            model: self.model.clone(),
            system,
            messages: api_messages,
            max_tokens: self.max_output_tokens,
            tools,
            stream: false,
        };

        let request_json = serde_json::to_value(&request).map_err(CallerError::Json)?;
        let client = &self.client;
        let api_key = &self.api_key;

        let beta_header = if self.cu_enabled {
            "prompt-caching-2024-07-31,computer-use-2025-11-24"
        } else {
            "prompt-caching-2024-07-31"
        };

        let response = send_with_retry(
            client,
            || {
                client
                    .post("https://api.anthropic.com/v1/messages")
                    .header("x-api-key", api_key)
                    .header("anthropic-version", "2023-06-01")
                    .header("anthropic-beta", beta_header)
                    .header("content-type", "application/json")
                    .json(&request_json)
            },
            MAX_RETRIES,
        )
        .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!(
                "{}: {}",
                status,
                mask_api_keys(&body)
            )));
        }

        let chat_response: AnthropicChatResponse = response.json().await?;

        // Extract text content, tool_use blocks, and CU blocks
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut cu_calls = Vec::new();

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
                        if name == "computer" && self.cu_enabled {
                            // Native CU tool call
                            if let Some(action) = parse_anthropic_cu_action(input) {
                                cu_calls.push(super::computer_use::CuToolCall {
                                    call_id: id.clone(),
                                    actions: vec![action],
                                    metadata: super::computer_use::CuCallMetadata::default(),
                                });
                            }
                        } else {
                            tool_calls.push(ToolCall {
                                id: id.clone(),
                                call_id: id.clone(),
                                name: name.clone(),
                                arguments: serde_json::to_string(input).unwrap_or_default(),
                            });
                        }
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
                cached_tokens: u.cache_read_input_tokens,
            })
            .unwrap_or_default();

        Ok(ChatResponse {
            content,
            usage,
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls,
            cu_calls,
            raw_output: None,
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

    fn cu_enabled(&self) -> bool {
        self.cu_enabled
    }

    fn cu_display(&self) -> Option<(u32, u32)> {
        self.cu_display
    }

    fn set_cu_display(&mut self, dims: (u32, u32)) {
        self.cu_display = Some(dims);
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        if self.use_tools {
            self.custom_tools.clone().unwrap_or_else(|| crate::tools::all_tools())
        } else {
            vec![]
        }
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ChatResponse, CallerError> {
        let (system, api_messages) = build_anthropic_messages(messages);

        let mut tools_vec: Vec<serde_json::Value> = Vec::new();
        if self.use_tools {
            let defs = self.tools();
            tools_vec.extend(defs.iter().map(|t| t.to_anthropic()));
        }
        if self.cu_enabled {
            if let Some((w, h)) = self.cu_display {
                tools_vec.push(serde_json::json!({
                    "type": "computer_20251124",
                    "name": "computer",
                    "display_width_px": w,
                    "display_height_px": h
                }));
            }
        }
        let tools = if tools_vec.is_empty() { None } else { Some(tools_vec) };

        let request = AnthropicChatRequest {
            model: self.model.clone(),
            system,
            messages: api_messages,
            max_tokens: self.max_output_tokens,
            tools,
            stream: true,
        };
        let request_json = serde_json::to_value(&request).map_err(CallerError::Json)?;
        let client = &self.client;
        let api_key = &self.api_key;

        let beta_header = if self.cu_enabled {
            "prompt-caching-2024-07-31,computer-use-2025-11-24"
        } else {
            "prompt-caching-2024-07-31"
        };

        let response = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", beta_header)
            .header("content-type", "application/json")
            .timeout(STREAM_TIMEOUT)
            .json(&request_json)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!(
                "{}: {}",
                status,
                mask_api_keys(&body)
            )));
        }

        // Parse SSE stream
        let mut text_parts = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut cu_calls: Vec<super::computer_use::CuToolCall> = Vec::new();
        let mut current_tool_json = String::new();
        let mut current_tool_id = String::new();
        let mut current_tool_name = String::new();
        let mut usage = TokenUsage::default();
        let mut line_buf = String::new();

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| CallerError::Provider(format!("Stream error: {}", e)))?;
            let chunk_str = String::from_utf8_lossy(&chunk);

            line_buf.push_str(&chunk_str);

            // Process complete lines
            while let Some(newline_pos) = line_buf.find('\n') {
                let line = line_buf[..newline_pos].trim_end_matches('\r').to_string();
                line_buf = line_buf[newline_pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                if let Some(("data", data)) = parse_sse_line(&line) {
                    if data == "[DONE]" {
                        continue;
                    }
                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match event_type {
                            "content_block_start" => {
                                if let Some(cb) = event.get("content_block") {
                                    let cb_type =
                                        cb.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    if cb_type == "tool_use" {
                                        current_tool_id = cb
                                            .get("id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        current_tool_name = cb
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        current_tool_json.clear();
                                    }
                                }
                            }
                            "content_block_delta" => {
                                if let Some(delta) = event.get("delta") {
                                    let delta_type =
                                        delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                    match delta_type {
                                        "text_delta" => {
                                            if let Some(text) =
                                                delta.get("text").and_then(|t| t.as_str())
                                            {
                                                text_parts.push(text.to_string());
                                                on_event(StreamEvent::Delta(text.to_string()));
                                            }
                                        }
                                        "input_json_delta" => {
                                            if let Some(json) =
                                                delta.get("partial_json").and_then(|t| t.as_str())
                                            {
                                                current_tool_json.push_str(json);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            "content_block_stop" => {
                                if !current_tool_id.is_empty() {
                                    if current_tool_name == "computer" && self.cu_enabled {
                                        if let Ok(input) = serde_json::from_str::<serde_json::Value>(&current_tool_json) {
                                            if let Some(action) = parse_anthropic_cu_action(&input) {
                                                cu_calls.push(super::computer_use::CuToolCall {
                                                    call_id: current_tool_id.clone(),
                                                    actions: vec![action],
                                                    metadata: super::computer_use::CuCallMetadata::default(),
                                                });
                                            }
                                        }
                                    } else {
                                        tool_calls.push(ToolCall {
                                            id: current_tool_id.clone(),
                                            call_id: current_tool_id.clone(),
                                            name: current_tool_name.clone(),
                                            arguments: current_tool_json.clone(),
                                        });
                                    }
                                    current_tool_id.clear();
                                    current_tool_name.clear();
                                    current_tool_json.clear();
                                }
                            }
                            "message_delta" => {
                                if let Some(u) = event.get("usage") {
                                    let output = u
                                        .get("output_tokens")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    usage.completion_tokens = output;
                                }
                            }
                            "message_start" => {
                                if let Some(msg) = event.get("message") {
                                    if let Some(u) = msg.get("usage") {
                                        let input = u
                                            .get("input_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0);
                                        usage.prompt_tokens = input;
                                        usage.cached_tokens = u
                                            .get("cache_read_input_tokens")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
        let content = text_parts.join("");
        let response = ChatResponse {
            content,
            usage,
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls,
            cu_calls,
            raw_output: None,
        };
        on_event(StreamEvent::Complete(response.clone()));
        Ok(response)
    }
}

/// Build Anthropic API messages from our message format (shared between streaming and non-streaming).
/// Parse an Anthropic computer tool_use input into a CuAction.
fn parse_anthropic_cu_action(input: &serde_json::Value) -> Option<super::computer_use::CuAction> {
    use super::computer_use::*;

    let action = input.get("action")?.as_str()?;
    let coord = || -> Option<(i32, i32)> {
        let arr = input.get("coordinate")?.as_array()?;
        Some((arr.first()?.as_i64()? as i32, arr.get(1)?.as_i64()? as i32))
    };

    match action {
        "screenshot" => Some(CuAction::Screenshot),
        "left_click" => {
            let (x, y) = coord()?;
            Some(CuAction::Click { x, y, button: MouseButton::Left })
        }
        "right_click" => {
            let (x, y) = coord()?;
            Some(CuAction::Click { x, y, button: MouseButton::Right })
        }
        "middle_click" => {
            let (x, y) = coord()?;
            Some(CuAction::Click { x, y, button: MouseButton::Middle })
        }
        "double_click" => {
            let (x, y) = coord()?;
            Some(CuAction::DoubleClick { x, y, button: MouseButton::Left })
        }
        "type" => {
            let text = input.get("text")?.as_str()?.to_string();
            Some(CuAction::Type { text })
        }
        "key" => {
            let key = input.get("text")?.as_str()?.to_string();
            Some(CuAction::Key { key })
        }
        "mouse_move" => {
            let (x, y) = coord()?;
            Some(CuAction::MoveMouse { x, y })
        }
        "scroll" => {
            let (x, y) = coord()?;
            let dir_str = input.get("scroll_direction")?.as_str()?;
            let direction = match dir_str {
                "up" => ScrollDirection::Up,
                "down" => ScrollDirection::Down,
                "left" => ScrollDirection::Left,
                "right" => ScrollDirection::Right,
                _ => return None,
            };
            let amount = input.get("scroll_amount").and_then(|v| v.as_i64()).unwrap_or(3) as i32;
            Some(CuAction::Scroll { x, y, direction, amount })
        }
        "left_click_drag" => {
            let (sx, sy) = coord()?;
            let end = input.get("end_coordinate")?.as_array()?;
            let ex = end.first()?.as_i64()? as i32;
            let ey = end.get(1)?.as_i64()? as i32;
            Some(CuAction::Drag { start_x: sx, start_y: sy, end_x: ex, end_y: ey })
        }
        "wait" => {
            let ms = input.get("duration").and_then(|v| v.as_u64()).unwrap_or(1000);
            Some(CuAction::Wait { ms })
        }
        _ => None,
    }
}

fn build_anthropic_messages(messages: &[Message]) -> (serde_json::Value, Vec<AnthropicMessage>) {
    let system_text = messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let system = serde_json::json!([{
        "type": "text",
        "text": system_text,
        "cache_control": {"type": "ephemeral"}
    }]);

    let mut api_messages: Vec<AnthropicMessage> = Vec::new();
    for m in messages {
        if m.role == "system" {
            continue;
        }
        if m.role == "tool" {
            if let Some(ref call_id) = m.tool_call_id {
                let tool_content = if let Some(ref images) = m.images {
                    let mut parts = vec![serde_json::json!({
                        "type": "text",
                        "text": m.content,
                    })];
                    for img in images {
                        parts.push(serde_json::json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": img.media_type,
                                "data": img.data,
                            }
                        }));
                    }
                    serde_json::Value::Array(parts)
                } else {
                    serde_json::Value::String(m.content.clone())
                };
                let block = serde_json::json!([{
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": tool_content,
                }]);
                api_messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: block,
                });
                continue;
            }
        }
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
        if m.role == "user" || m.role == "assistant" {
            let content = if m.role == "user" {
                if let Some(ref images) = m.images {
                    let mut parts = vec![serde_json::json!({
                        "type": "text",
                        "text": m.content,
                    })];
                    for img in images {
                        parts.push(serde_json::json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": img.media_type,
                                "data": img.data,
                            }
                        }));
                    }
                    serde_json::Value::Array(parts)
                } else {
                    serde_json::Value::String(m.content.clone())
                }
            } else {
                serde_json::Value::String(m.content.clone())
            };
            api_messages.push(AnthropicMessage {
                role: m.role.clone(),
                content,
            });
        }
    }
    (system, api_messages)
}

// --- Gemini ---

pub struct GeminiProvider {
    client: Client,
    api_key: String,
    model: String,
    context_window: u64,
    max_output_tokens: u64,
    use_tools: bool,
    custom_tools: Option<Vec<ToolDefinition>>,
    endpoint: String,
    /// When true, include native computer-use tool in API requests.
    pub cu_enabled: bool,
    /// Display dimensions for CU (width, height).
    pub cu_display: Option<(u32, u32)>,
}

impl GeminiProvider {
    pub fn new(
        api_key: String,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
    ) -> Self {
        let use_tools = resolve_use_tools();
        let endpoint = env::var("GEMINI_ENDPOINT")
            .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string());
        Self {
            client: api_client(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            use_tools,
            custom_tools: None,
            endpoint,
            cu_enabled: false,
            cu_display: None,
        }
    }

    /// Create a provider with native tool calling explicitly disabled.
    pub fn new_plain(
        api_key: String,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
    ) -> Self {
        let endpoint = env::var("GEMINI_ENDPOINT")
            .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string());
        Self {
            client: api_client(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            use_tools: false,
            custom_tools: None,
            endpoint,
            cu_enabled: false,
            cu_display: None,
        }
    }

    pub fn new_with_tools(
        api_key: String,
        model: String,
        context_window: u64,
        max_output_tokens: u64,
        tools: Vec<ToolDefinition>,
    ) -> Self {
        let endpoint = env::var("GEMINI_ENDPOINT")
            .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string());
        Self {
            client: api_client(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            use_tools: true,
            custom_tools: Some(tools),
            endpoint,
            cu_enabled: false,
            cu_display: None,
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
        let (system_text, _contents, mut request_body) = build_gemini_request_parts(messages, self);

        if let Some(ref sys) = system_text {
            request_body["systemInstruction"] = serde_json::json!({
                "parts": [{"text": sys}]
            });
        }

        // Note: Gemini API uses implicit context caching. Requests with the same
        // prefix are automatically cached server-side. No explicit API changes needed.
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.endpoint, self.model
        );

        let client = &self.client;
        let api_key = &self.api_key;
        let response = send_with_retry(
            client,
            || {
                client
                    .post(&url)
                    .header("content-type", "application/json")
                    .header("x-goog-api-key", api_key)
                    .json(&request_body)
            },
            MAX_RETRIES,
        )
        .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!(
                "{}: {}",
                status,
                mask_api_keys(&body)
            )));
        }

        let resp: serde_json::Value = response.json().await?;

        // Extract content from candidates[0].content.parts[]
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut cu_calls = Vec::new();

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
                    let args_val = fc.get("args").cloned().unwrap_or(serde_json::json!({}));

                    // Check if this is a CU function call
                    if self.cu_enabled && GEMINI_CU_FUNCTIONS.contains(&name.as_str()) {
                        let (dw, dh) = self.cu_display.unwrap_or((1440, 900));
                        if let Some(action) = parse_gemini_cu_action(&name, &args_val, dw, dh) {
                            let id = format!("gemini_cu_{}", cu_calls.len());
                            cu_calls.push(super::computer_use::CuToolCall {
                                call_id: id,
                                actions: vec![action],
                                metadata: super::computer_use::CuCallMetadata::default(),
                            });
                        }
                    } else {
                        let args = serde_json::to_string(&args_val).unwrap_or_else(|_| "{}".to_string());
                        let id = format!("gemini_call_{}", tool_calls.len());
                        tool_calls.push(ToolCall {
                            id: id.clone(),
                            call_id: id,
                            name,
                            arguments: args,
                        });
                    }
                }
            }
        }

        let content = text_parts.join("");

        // Extract usage
        let usage = resp
            .get("usageMetadata")
            .map(|u| {
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
                let cached = u
                    .get("cachedContentTokenCount")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                TokenUsage {
                    prompt_tokens: prompt,
                    completion_tokens: completion,
                    total_tokens: total,
                    cached_tokens: cached,
                }
            })
            .unwrap_or_default();

        Ok(ChatResponse {
            content,
            usage,
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls,
            cu_calls,
            raw_output: None,
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

    fn cu_enabled(&self) -> bool {
        self.cu_enabled
    }

    fn cu_display(&self) -> Option<(u32, u32)> {
        self.cu_display
    }

    fn set_cu_display(&mut self, dims: (u32, u32)) {
        self.cu_display = Some(dims);
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        if self.use_tools {
            self.custom_tools.clone().unwrap_or_else(|| crate::tools::all_tools())
        } else {
            vec![]
        }
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<ChatResponse, CallerError> {
        let (system_text, contents, request_body_base) = build_gemini_request_parts(messages, self);

        let mut request_body = request_body_base;
        if let Some(ref sys) = system_text {
            request_body["systemInstruction"] = serde_json::json!({
                "parts": [{"text": sys}]
            });
        }

        // Use streamGenerateContent endpoint
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.endpoint, self.model
        );

        let client = &self.client;
        let api_key = &self.api_key;
        let response = client
            .post(&url)
            .header("content-type", "application/json")
            .header("x-goog-api-key", api_key)
            .timeout(STREAM_TIMEOUT)
            .json(&request_body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!(
                "{}: {}",
                status,
                mask_api_keys(&body)
            )));
        }

        let mut text_parts = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut cu_calls: Vec<super::computer_use::CuToolCall> = Vec::new();
        let mut raw_model_parts: Vec<serde_json::Value> = Vec::new();
        let mut usage = TokenUsage::default();
        let mut line_buf = String::new();

        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| CallerError::Provider(format!("Stream error: {}", e)))?;
            let chunk_str = String::from_utf8_lossy(&chunk);
            line_buf.push_str(&chunk_str);

            while let Some(newline_pos) = line_buf.find('\n') {
                let line = line_buf[..newline_pos].trim_end_matches('\r').to_string();
                line_buf = line_buf[newline_pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                // Gemini streaming with alt=sse returns SSE format
                let data = if let Some(("data", d)) = parse_sse_line(&line) {
                    d
                } else {
                    continue;
                };

                if let Ok(resp) = serde_json::from_str::<serde_json::Value>(data) {
                    // Extract text and function calls from candidates
                    if let Some(parts) = resp
                        .pointer("/candidates/0/content/parts")
                        .and_then(|p| p.as_array())
                    {
                        for part in parts {
                            // Capture raw parts for verbatim echo-back (preserves thoughtSignature)
                            raw_model_parts.push(part.clone());

                            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                                text_parts.push(text.to_string());
                                on_event(StreamEvent::Delta(text.to_string()));
                            }
                            if let Some(fc) = part.get("functionCall") {
                                let name = fc
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let args_val = fc.get("args").cloned().unwrap_or(serde_json::json!({}));

                                if self.cu_enabled && GEMINI_CU_FUNCTIONS.contains(&name.as_str()) {
                                    let (dw, dh) = self.cu_display.unwrap_or((1440, 900));
                                    if let Some(action) = parse_gemini_cu_action(&name, &args_val, dw, dh) {
                                        let id = format!("gemini_cu_{}", cu_calls.len());
                                        cu_calls.push(super::computer_use::CuToolCall {
                                            call_id: id,
                                            actions: vec![action],
                                            metadata: super::computer_use::CuCallMetadata::default(),
                                        });
                                    }
                                } else {
                                    let args = serde_json::to_string(&args_val).unwrap_or_else(|_| "{}".to_string());
                                    let id = format!("gemini_call_{}", tool_calls.len());
                                    tool_calls.push(ToolCall {
                                        id: id.clone(),
                                        call_id: id,
                                        name,
                                        arguments: args,
                                    });
                                }
                            }
                        }
                    }

                    // Extract usage from the last chunk
                    if let Some(u) = resp.get("usageMetadata") {
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
                        let cached = u
                            .get("cachedContentTokenCount")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        usage = TokenUsage {
                            prompt_tokens: prompt,
                            completion_tokens: completion,
                            total_tokens: total,
                            cached_tokens: cached,
                        };
                    }
                }
            }
        }

        let content = text_parts.join("");
        let _ = (contents, system_text); // consumed above
        // Store raw parts for echo-back (preserves thoughtSignature for Gemini CU)
        let raw_output = if !raw_model_parts.is_empty() {
            Some(raw_model_parts)
        } else {
            None
        };
        let response = ChatResponse {
            content,
            usage,
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls,
            cu_calls,
            raw_output,
        };
        on_event(StreamEvent::Complete(response.clone()));
        Ok(response)
    }
}

/// Build Gemini request parts (shared between streaming and non-streaming).
fn build_gemini_request_parts(
    messages: &[Message],
    provider: &GeminiProvider,
) -> (Option<String>, Vec<serde_json::Value>, serde_json::Value) {
    let system_text = messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| m.content.clone());

    let mut contents: Vec<serde_json::Value> = Vec::new();
    for m in messages {
        if m.role == "system" {
            continue;
        }
        let role = gemini_role(&m.role);
        if m.role == "tool" {
            if let (Some(ref _call_id), Some(ref tool_name)) = (&m.tool_call_id, &m.tool_name) {
                if m.is_cu_result {
                    // CU result: screenshot goes INSIDE functionResponse.parts (not as sibling)
                    let response_val = serde_json::json!({
                        "output": m.content,
                        "url": "desktop://local",
                    });
                    let mut fr = serde_json::json!({
                        "functionResponse": {
                            "name": tool_name,
                            "response": response_val,
                        }
                    });
                    if let Some(ref images) = m.images {
                        let fr_parts: Vec<serde_json::Value> = images.iter().map(|img| {
                            serde_json::json!({
                                "inlineData": {
                                    "mimeType": img.media_type,
                                    "data": img.data,
                                }
                            })
                        }).collect();
                        if !fr_parts.is_empty() {
                            fr["functionResponse"]["parts"] = serde_json::Value::Array(fr_parts);
                        }
                    }
                    contents.push(serde_json::json!({
                        "role": role,
                        "parts": [fr],
                    }));
                } else {
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
                    if let Some(ref images) = m.images {
                        let mut parts = vec![serde_json::json!({
                            "text": "Screenshot from the previous tool call:",
                        })];
                        for img in images {
                            parts.push(serde_json::json!({
                                "inlineData": {
                                    "mimeType": img.media_type,
                                    "data": img.data,
                                }
                            }));
                        }
                        contents.push(serde_json::json!({
                            "role": "user",
                            "parts": parts,
                        }));
                    }
                }
                continue;
            }
        }
        if m.role == "assistant" {
            if let Some(ref tcs) = m.tool_calls {
                // Use raw_output if available (preserves thoughtSignature for Gemini CU)
                if let Some(ref raw) = m.raw_output {
                    contents.push(serde_json::json!({
                        "role": role,
                        "parts": raw,
                    }));
                    continue;
                }
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
        if m.role == "user" {
            if let Some(ref images) = m.images {
                let mut parts = vec![serde_json::json!({"text": m.content})];
                for img in images {
                    parts.push(serde_json::json!({
                        "inlineData": {
                            "mimeType": img.media_type,
                            "data": img.data,
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
        contents.push(serde_json::json!({
            "role": role,
            "parts": [{"text": m.content}]
        }));
    }

    let mut request_body = serde_json::json!({
        "contents": contents,
        "generationConfig": {
            "maxOutputTokens": provider.max_output_tokens,
        }
    });

    let has_func_tools = provider.use_tools;
    let has_cu = provider.cu_enabled;
    if has_func_tools || has_cu {
        let mut tools_arr = Vec::new();
        if has_func_tools {
            let defs = provider.tools();
            let func_decls: Vec<serde_json::Value> = defs.iter().map(|t| t.to_gemini()).collect();
            tools_arr.push(serde_json::json!({
                "functionDeclarations": func_decls,
            }));
        }
        if has_cu {
            // Gemini v1beta only supports ENVIRONMENT_BROWSER for computer_use.
            // No display_size field is available — the model infers dimensions
            // from screenshot resolution and uses normalized 0-999 coordinates.
            tools_arr.push(serde_json::json!({
                "computer_use": {
                    "environment": "ENVIRONMENT_BROWSER"
                }
            }));
        }
        request_body["tools"] = serde_json::Value::Array(tools_arr);
    }

    (system_text, contents, request_body)
}

/// CU function names used by Gemini's computer_use tool.
const GEMINI_CU_FUNCTIONS: &[&str] = &[
    "click_at", "type_text_at", "hover_at", "scroll_document", "scroll_at",
    "key_combination", "navigate", "go_back", "go_forward", "search",
    "open_web_browser", "wait_5_seconds", "drag_and_drop",
];

/// Parse a Gemini CU function call into a CuAction.
/// Gemini uses 0-999 normalized coordinates; they are converted to pixels here.
fn parse_gemini_cu_action(
    name: &str,
    args: &serde_json::Value,
    display_width: u32,
    display_height: u32,
) -> Option<super::computer_use::CuAction> {
    use super::computer_use::*;

    let coord = |xk: &str, yk: &str| -> Option<(i32, i32)> {
        let nx = args.get(xk)?.as_i64()? as i32;
        let ny = args.get(yk)?.as_i64()? as i32;
        Some(normalized_to_pixels(nx, ny, display_width, display_height))
    };

    match name {
        "click_at" => {
            let (x, y) = coord("x", "y")?;
            Some(CuAction::Click { x, y, button: MouseButton::Left })
        }
        "type_text_at" => {
            let (x, y) = coord("x", "y")?;
            let text = args.get("text")?.as_str()?.to_string();
            let press_enter = args.get("press_enter").and_then(|v| v.as_bool()).unwrap_or(false);
            // Click to focus, then type
            // We return just the Type action; the click is handled by the executor
            // Actually, return Click + Type as separate actions is complex.
            // For simplicity, just return Type and let caller handle focus.
            let mut result_text = text;
            if press_enter {
                result_text.push('\n');
            }
            // First click to position, then type. We'll do this as a Click action
            // followed by a Type action at the agent loop level.
            // For now, just return Type — the model already positions via click_at.
            let _ = (x, y); // coordinates ignored; model handles focus separately
            Some(CuAction::Type { text: result_text })
        }
        "hover_at" => {
            let (x, y) = coord("x", "y")?;
            Some(CuAction::MoveMouse { x, y })
        }
        "scroll_document" | "scroll_at" => {
            let dir_str = args.get("direction")?.as_str()?;
            let direction = match dir_str {
                "up" => ScrollDirection::Up,
                "down" => ScrollDirection::Down,
                "left" => ScrollDirection::Left,
                "right" => ScrollDirection::Right,
                _ => return None,
            };
            let amount = args.get("magnitude").and_then(|v| v.as_i64()).unwrap_or(3) as i32;
            let (x, y) = if name == "scroll_at" {
                coord("x", "y").unwrap_or((display_width as i32 / 2, display_height as i32 / 2))
            } else {
                (display_width as i32 / 2, display_height as i32 / 2)
            };
            Some(CuAction::Scroll { x, y, direction, amount })
        }
        "key_combination" => {
            let keys = args.get("keys")?.as_str()?.to_string();
            Some(CuAction::Key { key: keys })
        }
        "wait_5_seconds" => Some(CuAction::Wait { ms: 5000 }),
        "drag_and_drop" => {
            let (sx, sy) = coord("x", "y")?;
            let (ex, ey) = coord("destination_x", "destination_y")?;
            Some(CuAction::Drag { start_x: sx, start_y: sy, end_x: ex, end_y: ey })
        }
        // Browser-like navigation actions — mapped to keyboard shortcuts / xdg-open
        "navigate" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("about:blank");
            // Type the URL into the address bar via xdg-open (best-effort)
            Some(CuAction::Key { key: format!("xdg-open {}", url) })
        }
        "open_web_browser" => {
            // No-op screenshot — the model wants to see the screen
            Some(CuAction::Screenshot)
        }
        "go_back" => Some(CuAction::Key { key: "alt+Left".to_string() }),
        "go_forward" => Some(CuAction::Key { key: "alt+Right".to_string() }),
        "search" => Some(CuAction::Key { key: "ctrl+l".to_string() }),
        _ => None,
    }
}

// --- Provider selection ---

fn default_context_window(model: &str) -> u64 {
    match model {
        m if m.starts_with("gpt-5") => 1_000_000,
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
        .unwrap_or_else(|| "high".to_string());
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

/// Mask API keys in error messages to prevent accidental leakage.
pub(crate) fn mask_api_keys(s: &str) -> String {
    static API_KEY_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        // Match sk- (OpenAI), key- (Anthropic), AIza (Google) prefixed keys
        // Capture first 14 chars (prefix + 10) then mask the rest
        regex::Regex::new(r"(sk-[A-Za-z0-9_-]{10})[A-Za-z0-9_-]+|(key-[A-Za-z0-9_-]{10})[A-Za-z0-9_-]+|(AIzaSy[A-Za-z0-9_-]{6})[A-Za-z0-9_-]+").unwrap()
    });
    API_KEY_RE
        .replace_all(s, |caps: &regex::Captures| {
            if let Some(m) = caps.get(1) {
                format!("{}***", m.as_str())
            } else if let Some(m) = caps.get(2) {
                format!("{}***", m.as_str())
            } else if let Some(m) = caps.get(3) {
                format!("{}***", m.as_str())
            } else {
                caps[0].to_string()
            }
        })
        .to_string()
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
        let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gemini-2.5-pro".to_string());
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
            let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gpt-5.4".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(OpenAIProvider::new(oai, model, ctx, max_out)))
        }
        (Some(oai), None, _) => {
            let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gpt-5.4".to_string());
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

/// Like `select_provider()` but accepts explicit provider/model overrides
/// instead of reading from the primary `PROVIDER`/`MODEL_NAME` env vars.
/// Falls back to env-based API key resolution.
pub fn select_provider_with_overrides(
    provider_name: Option<&str>,
    model_name: Option<&str>,
) -> Result<Box<dyn ChatProvider>, CallerError> {
    // Also check PRESENCE_PROVIDER / PRESENCE_MODEL env vars as secondary fallback
    let provider_str = provider_name
        .map(|s| s.to_string())
        .or_else(|| env::var("PRESENCE_PROVIDER").ok())
        .or_else(|| env::var("PROVIDER").ok());
    let model_str = model_name
        .map(|s| s.to_string())
        .or_else(|| env::var("PRESENCE_MODEL").ok());

    let openai_key = env::var("OPENAI_API_KEY")
        .or_else(|_| env::var("OPENAI"))
        .ok();
    let anthropic_key = env::var("ANTHROPIC_API_KEY")
        .or_else(|_| env::var("ANTHROPIC"))
        .ok();
    let gemini_key = env::var("GEMINI_API_KEY")
        .or_else(|_| env::var("GEMINI"))
        .ok();

    match provider_str.as_deref() {
        Some("gemini") => {
            let key = gemini_key.ok_or_else(|| {
                CallerError::Config("Presence provider=gemini but no GEMINI_API_KEY found.".into())
            })?;
            let model = model_str.unwrap_or_else(|| "gemini-2.5-flash".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(GeminiProvider::new(key, model, ctx, max_out)))
        }
        Some("anthropic") => {
            let key = anthropic_key.ok_or_else(|| {
                CallerError::Config(
                    "Presence provider=anthropic but no ANTHROPIC_API_KEY found.".into(),
                )
            })?;
            let model =
                model_str.unwrap_or_else(|| "claude-sonnet-4-5-20250929".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(AnthropicProvider::new(key, model, ctx, max_out)))
        }
        Some("openai") => {
            let key = openai_key.ok_or_else(|| {
                CallerError::Config("Presence provider=openai but no OPENAI_API_KEY found.".into())
            })?;
            let model = model_str.unwrap_or_else(|| "gpt-5.2-codex".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(OpenAIProvider::new(key, model, ctx, max_out)))
        }
        Some(other) => Err(CallerError::Config(format!(
            "Unknown presence provider: '{}'. Expected 'openai', 'anthropic', or 'gemini'.",
            other
        ))),
        None => {
            // No explicit override — fall back to the standard select_provider logic
            select_provider()
        }
    }
}

/// Select a provider for computer-use tasks (tasks with reference frames).
///
/// Priority: explicit config > CU_PROVIDER/CU_MODEL env > default select_provider.
pub fn select_cu_provider(
    cu_config: &crate::project::ComputerUseConfig,
) -> Result<Box<dyn ChatProvider>, CallerError> {
    let provider_str = cu_config
        .provider
        .as_deref()
        .map(String::from)
        .or_else(|| env::var("CU_PROVIDER").ok())
        .or_else(|| env::var("PROVIDER").ok());
    let model_str = cu_config
        .model
        .as_deref()
        .map(String::from)
        .or_else(|| env::var("CU_MODEL").ok());

    let openai_key = env::var("OPENAI_API_KEY")
        .or_else(|_| env::var("OPENAI"))
        .ok();
    let anthropic_key = env::var("ANTHROPIC_API_KEY")
        .or_else(|_| env::var("ANTHROPIC"))
        .ok();
    let gemini_key = env::var("GEMINI_API_KEY")
        .or_else(|_| env::var("GEMINI"))
        .ok();

    // CU providers get native CU tools + escalation function tool
    let escalate_tools = vec![crate::tools::escalate_to_agent_tool()];

    match provider_str.as_deref() {
        Some("gemini") => {
            let key = gemini_key.ok_or_else(|| {
                CallerError::Config("CU provider=gemini but no GEMINI_API_KEY found.".into())
            })?;
            let model = model_str.unwrap_or_else(|| "gemini-3-flash-preview".to_string());
            let display = crate::vision::display_config_for_provider("gemini");
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            let mut p = GeminiProvider::new_with_tools(key, model, ctx, max_out, escalate_tools);
            p.cu_enabled = true;
            p.cu_display = Some((display.width, display.height));
            Ok(Box::new(p))
        }
        Some("anthropic") => {
            let key = anthropic_key.ok_or_else(|| {
                CallerError::Config(
                    "CU provider=anthropic but no ANTHROPIC_API_KEY found.".into(),
                )
            })?;
            let model =
                model_str.unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string());
            let display = crate::vision::display_config_for_provider("anthropic");
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            let mut p = AnthropicProvider::new_with_tools(key, model, ctx, max_out, escalate_tools);
            p.cu_enabled = true;
            p.cu_display = Some((display.width, display.height));
            Ok(Box::new(p))
        }
        Some("openai") => {
            let key = openai_key.ok_or_else(|| {
                CallerError::Config("CU provider=openai but no OPENAI_API_KEY found.".into())
            })?;
            let model = model_str.unwrap_or_else(|| "gpt-5.4-mini".to_string());
            let display = crate::vision::display_config_for_provider("openai");
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            let mut p = OpenAIProvider::new_with_tools(key, model, ctx, max_out, escalate_tools);
            p.cu_enabled = true;
            p.cu_display = Some((display.width, display.height));
            Ok(Box::new(p))
        }
        Some(other) => Err(CallerError::Config(format!(
            "Unknown CU provider: '{}'. Expected 'openai', 'anthropic', or 'gemini'.",
            other
        ))),
        None => {
            // No CU-specific override — auto-detect best CU provider with CU enabled.
            // Default to gemini-3-flash-preview (fast, cheap CU model).
            if let Some(key) = gemini_key {
                let model = model_str.unwrap_or_else(|| "gemini-3-flash-preview".to_string());
                let display = crate::vision::display_config_for_provider("gemini");
                let ctx = resolve_context_window(&model);
                let max_out = resolve_max_output_tokens(&model);
                let mut p =
                    GeminiProvider::new_with_tools(key, model, ctx, max_out, escalate_tools);
                p.cu_enabled = true;
                p.cu_display = Some((display.width, display.height));
                Ok(Box::new(p))
            } else if let Some(key) = openai_key {
                let model = model_str.unwrap_or_else(|| "gpt-5.4-mini".to_string());
                let display = crate::vision::display_config_for_provider("openai");
                let ctx = resolve_context_window(&model);
                let max_out = resolve_max_output_tokens(&model);
                let mut p =
                    OpenAIProvider::new_with_tools(key, model, ctx, max_out, escalate_tools);
                p.cu_enabled = true;
                p.cu_display = Some((display.width, display.height));
                Ok(Box::new(p))
            } else if let Some(key) = anthropic_key {
                let model =
                    model_str.unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string());
                let display = crate::vision::display_config_for_provider("anthropic");
                let ctx = resolve_context_window(&model);
                let max_out = resolve_max_output_tokens(&model);
                let mut p =
                    AnthropicProvider::new_with_tools(key, model, ctx, max_out, escalate_tools);
                p.cu_enabled = true;
                p.cu_display = Some((display.width, display.height));
                Ok(Box::new(p))
            } else {
                Err(CallerError::Config(
                    "No API key found for CU provider. Set GEMINI_API_KEY, OPENAI_API_KEY, or ANTHROPIC_API_KEY.".into(),
                ))
            }
        }
    }
}

/// Select a provider for the presence layer (text mode).
///
/// Priority: explicit config > PRESENCE_PROVIDER/PRESENCE_MODEL env > auto-detect.
/// Auto-detect prefers gemini (gemini-2.5-flash) when GEMINI_API_KEY is set,
/// falling back to the cheapest available provider.
///
/// Presence providers are created with `new_plain()` — no native agent tools.
/// The presence layer has its own tool set (submit_task, check_status, etc.)
/// managed at the conversation level, not through the provider.
pub fn select_presence_provider(
    provider_name: Option<&str>,
    model_name: Option<&str>,
) -> Result<Box<dyn ChatProvider>, CallerError> {
    use crate::presence;

    let provider_str = provider_name
        .map(|s| s.to_string())
        .or_else(|| env::var("PRESENCE_PROVIDER").ok());
    let model_str = model_name
        .map(|s| s.to_string())
        .or_else(|| env::var("PRESENCE_MODEL").ok());

    let openai_key = env::var("OPENAI_API_KEY")
        .or_else(|_| env::var("OPENAI"))
        .ok();
    let anthropic_key = env::var("ANTHROPIC_API_KEY")
        .or_else(|_| env::var("ANTHROPIC"))
        .ok();
    let gemini_key = env::var("GEMINI_API_KEY")
        .or_else(|_| env::var("GEMINI"))
        .ok();

    let tools = presence::presence_tools();

    match provider_str.as_deref() {
        Some("gemini") => {
            let key = gemini_key.ok_or_else(|| {
                CallerError::Config("Presence provider=gemini but no GEMINI_API_KEY found.".into())
            })?;
            let model =
                model_str.unwrap_or_else(|| presence::DEFAULT_TEXT_MODEL.to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(GeminiProvider::new_with_tools(key, model, ctx, max_out, tools)))
        }
        Some("anthropic") => {
            let key = anthropic_key.ok_or_else(|| {
                CallerError::Config(
                    "Presence provider=anthropic but no ANTHROPIC_API_KEY found.".into(),
                )
            })?;
            let model =
                model_str.unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(AnthropicProvider::new_with_tools(key, model, ctx, max_out, tools)))
        }
        Some("openai") => {
            let key = openai_key.ok_or_else(|| {
                CallerError::Config("Presence provider=openai but no OPENAI_API_KEY found.".into())
            })?;
            let model = model_str.unwrap_or_else(|| "gpt-4.1-mini".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(OpenAIProvider::new_with_tools(key, model, ctx, max_out, tools)))
        }
        Some(other) => Err(CallerError::Config(format!(
            "Unknown presence provider: '{}'. Expected 'openai', 'anthropic', or 'gemini'.",
            other
        ))),
        None => {
            // Auto-detect: prefer gemini (cheapest/fastest for presence)
            if let Some(key) = gemini_key {
                let model =
                    model_str.unwrap_or_else(|| presence::DEFAULT_TEXT_MODEL.to_string());
                let ctx = resolve_context_window(&model);
                let max_out = resolve_max_output_tokens(&model);
                Ok(Box::new(GeminiProvider::new_with_tools(key, model, ctx, max_out, tools)))
            } else if let Some(key) = anthropic_key {
                let model =
                    model_str.unwrap_or_else(|| "claude-haiku-4-5-20251001".to_string());
                let ctx = resolve_context_window(&model);
                let max_out = resolve_max_output_tokens(&model);
                Ok(Box::new(AnthropicProvider::new_with_tools(key, model, ctx, max_out, tools)))
            } else if let Some(key) = openai_key {
                let model = model_str.unwrap_or_else(|| "gpt-4.1-mini".to_string());
                let ctx = resolve_context_window(&model);
                let max_out = resolve_max_output_tokens(&model);
                Ok(Box::new(OpenAIProvider::new_with_tools(key, model, ctx, max_out, tools)))
            } else {
                Err(CallerError::Config(
                    "No API key found for presence layer. Set GEMINI_API_KEY, ANTHROPIC_API_KEY, or OPENAI_API_KEY.".into(),
                ))
            }
        }
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
        ..Default::default()
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
        assert_eq!(
            resp.content[0].text.as_deref(),
            Some("I'll list the files.")
        );
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
            system: serde_json::json!([{
                "type": "text",
                "text": "You are an agent.",
                "cache_control": {"type": "ephemeral"}
            }]),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::Value::String("list files".to_string()),
            }],
            max_tokens: 8192,
            tools: Some(tools),
            stream: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"tools\""));
        assert!(json.contains("exec_command"));
        assert!(json.contains("cache_control"));
    }

    #[test]
    fn anthropic_request_without_tools() {
        let request = AnthropicChatRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            system: serde_json::json!([{
                "type": "text",
                "text": "You are helpful.",
                "cache_control": {"type": "ephemeral"}
            }]),
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: serde_json::Value::String("hello".to_string()),
            }],
            max_tokens: 8192,
            tools: None,
            stream: false,
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
        assert_eq!(default_context_window("gpt-5.2-codex"), 1_000_000);
        assert_eq!(default_context_window("gpt-5"), 1_000_000);
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
            stream: false,
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
            stream: false,
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
            stream: false,
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
            stream: false,
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
            stream: false,
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
            stream: false,
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
                    "id": "fc_abc123",
                    "type": "function_call",
                    "call_id": "call_abc123",
                    "name": "exec_command",
                    "arguments": "{\"nonce\":1,\"command\":\"ls -la\"}"
                },
                {
                    "id": "fc_def456",
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
        assert_eq!(items[0].id.as_deref(), Some("fc_abc123"));
        assert_eq!(items[0].call_id.as_deref(), Some("call_abc123"));
        assert_eq!(items[0].name.as_deref(), Some("exec_command"));
        assert!(items[0].arguments.as_ref().unwrap().contains("ls -la"));
        assert_eq!(items[1].id.as_deref(), Some("fc_def456"));
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
        assert!(supports_reasoning("gpt-5.4"));
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
            cu_calls: vec![],
            raw_output: None,
        };
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn tool_call_fields() {
        let tc = ToolCall {
            id: "fc_123".to_string(),
            call_id: "call_123".to_string(),
            name: "exec_command".to_string(),
            arguments: r#"{"nonce":1,"command":"ls"}"#.to_string(),
        };
        assert_eq!(tc.id, "fc_123");
        assert_eq!(tc.call_id, "call_123");
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
        let resp: serde_json::Value = serde_json::from_str(
            r#"{
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
        }"#,
        )
        .unwrap();

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
        assert!(provider
            .endpoint
            .contains("generativelanguage.googleapis.com"));
    }

    #[test]
    fn is_retryable_429() {
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
    }

    #[test]
    fn is_retryable_500() {
        assert!(is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    #[test]
    fn is_retryable_502() {
        assert!(is_retryable_status(reqwest::StatusCode::BAD_GATEWAY));
    }

    #[test]
    fn not_retryable_400() {
        assert!(!is_retryable_status(reqwest::StatusCode::BAD_REQUEST));
    }

    #[test]
    fn not_retryable_401() {
        assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn not_retryable_200() {
        assert!(!is_retryable_status(reqwest::StatusCode::OK));
    }

    #[test]
    fn backoff_delay_increases() {
        let d0 = backoff_delay(0);
        let d1 = backoff_delay(1);
        let d2 = backoff_delay(2);
        // Base doubles each time: 1s, 2s, 4s
        assert!(d1 > d0);
        assert!(d2 > d1);
    }

    #[test]
    fn mask_openai_key() {
        let s = "Error: key sk-abcdefghijklmnopqrstuvwxyz123456 is invalid";
        let masked = mask_api_keys(s);
        assert!(masked.contains("sk-abcdefghij***"));
        assert!(!masked.contains("klmnopqrstuvwxyz123456"));
    }

    #[test]
    fn mask_gemini_key() {
        let s = "Error with key AIzaSyB12345678901234567890";
        let masked = mask_api_keys(s);
        assert!(masked.contains("AIzaSyB12345***"));
        assert!(!masked.contains("678901234567890"));
    }

    #[test]
    fn mask_preserves_normal_text() {
        let s = "This is a normal error message without any keys";
        assert_eq!(mask_api_keys(s), s);
    }

    #[test]
    fn mask_short_prefix_not_matched() {
        let s = "sk-short";
        // Less than 10 chars after prefix, not matched
        assert_eq!(mask_api_keys(s), s);
    }

    // --- Streaming tests ---

    #[test]
    fn parse_sse_line_data() {
        let (kind, content) = parse_sse_line("data: {\"type\":\"ping\"}").unwrap();
        assert_eq!(kind, "data");
        assert_eq!(content, "{\"type\":\"ping\"}");
    }

    #[test]
    fn parse_sse_line_event() {
        let (kind, content) = parse_sse_line("event: message_start").unwrap();
        assert_eq!(kind, "event");
        assert_eq!(content, "message_start");
    }

    #[test]
    fn parse_sse_line_unknown() {
        assert!(parse_sse_line("id: 123").is_none());
        assert!(parse_sse_line("").is_none());
        assert!(parse_sse_line("random text").is_none());
    }

    #[test]
    fn stream_event_delta_clone() {
        let event = StreamEvent::Delta("hello".to_string());
        let cloned = event.clone();
        if let StreamEvent::Delta(text) = cloned {
            assert_eq!(text, "hello");
        } else {
            panic!("Expected Delta variant");
        }
    }

    #[test]
    fn stream_event_complete_clone() {
        let resp = ChatResponse {
            content: "done".to_string(),
            usage: TokenUsage::default(),
            reasoning_summary: None,
            reasoning_content: None,
            tool_calls: vec![],
            cu_calls: vec![],
            raw_output: None,
        };
        let event = StreamEvent::Complete(resp);
        let cloned = event.clone();
        if let StreamEvent::Complete(r) = cloned {
            assert_eq!(r.content, "done");
        } else {
            panic!("Expected Complete variant");
        }
    }

    #[test]
    fn anthropic_request_stream_field_serialization() {
        let request = AnthropicChatRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            system: serde_json::json!([{
                "type": "text",
                "text": "test",
                "cache_control": {"type": "ephemeral"}
            }]),
            messages: vec![],
            max_tokens: 8192,
            tools: None,
            stream: true,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"stream\":true"));
    }

    #[test]
    fn anthropic_request_no_stream_when_false() {
        let request = AnthropicChatRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            system: serde_json::json!([{
                "type": "text",
                "text": "test",
                "cache_control": {"type": "ephemeral"}
            }]),
            messages: vec![],
            max_tokens: 8192,
            tools: None,
            stream: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("stream"));
    }

    #[test]
    fn openai_request_stream_field_serialization() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "Hi")],
            instructions: None,
            max_output_tokens: None,
            reasoning: None,
            text: None,
            tools: None,
            stream: true,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"stream\":true"));
    }

    #[test]
    fn openai_request_no_stream_when_false() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![openai_message_item("user", "Hi")],
            instructions: None,
            max_output_tokens: None,
            reasoning: None,
            text: None,
            tools: None,
            stream: false,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("stream"));
    }

    #[test]
    fn build_anthropic_messages_extracts_system() {
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
        ];
        let (system, api_msgs) = build_anthropic_messages(&messages);
        let sys_text = system[0]["text"].as_str().unwrap();
        assert_eq!(sys_text, "You are helpful.");
        assert_eq!(api_msgs.len(), 1);
        assert_eq!(api_msgs[0].role, "user");
    }

    #[test]
    fn build_gemini_request_parts_includes_contents() {
        let provider = GeminiProvider::new(
            "key".to_string(),
            "gemini-2.5-pro".to_string(),
            1_048_576,
            65_536,
        );
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "System".to_string(),
                ..Default::default()
            },
            Message {
                role: "user".to_string(),
                content: "Hi".to_string(),
                ..Default::default()
            },
        ];
        let (sys, contents, body) = build_gemini_request_parts(&messages, &provider);
        assert_eq!(sys.as_deref(), Some("System"));
        assert_eq!(contents.len(), 1);
        assert!(body.get("contents").is_some());
    }

    // --- Image/vision provider tests ---

    fn tool_msg_with_images() -> Message {
        use crate::conversation::ImageData;
        Message {
            role: "tool".to_string(),
            content: "screenshot taken".to_string(),
            tool_call_id: Some("call_1".to_string()),
            tool_name: Some("capture_screen".to_string()),
            images: Some(vec![ImageData {
                media_type: "image/png".to_string(),
                data: "iVBORw0KGgo=".to_string(),
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn openai_builder_includes_image_after_tool_result() {
        let provider = OpenAIProvider::new(
            "key".to_string(),
            "gpt-4".to_string(),
            128_000,
            16_384,
        );
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            tool_msg_with_images(),
        ];
        let (_instr, input, _text, _tools) = build_openai_request_parts(&messages, &provider);
        // Should have function_call_output + user message with image
        assert!(input.len() >= 2);
        let image_msg = &input[1];
        assert_eq!(image_msg["role"].as_str(), Some("user"));
        let content = image_msg["content"].as_array().unwrap();
        assert_eq!(content[0]["type"].as_str(), Some("input_text"));
        assert_eq!(content[1]["type"].as_str(), Some("input_image"));
        let url = content[1]["image_url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn openai_builder_no_image_without_images_field() {
        let provider = OpenAIProvider::new(
            "key".to_string(),
            "gpt-4".to_string(),
            128_000,
            16_384,
        );
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            Message {
                role: "tool".to_string(),
                content: "result".to_string(),
                tool_call_id: Some("call_1".to_string()),
                ..Default::default()
            },
        ];
        let (_instr, input, _text, _tools) = build_openai_request_parts(&messages, &provider);
        // Should have only the function_call_output, no user image message
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"].as_str(), Some("function_call_output"));
    }

    #[test]
    fn anthropic_builder_includes_image_in_tool_result() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            tool_msg_with_images(),
        ];
        let (_system, api_msgs) = build_anthropic_messages(&messages);
        assert_eq!(api_msgs.len(), 1);
        let content = api_msgs[0].content.as_array().unwrap();
        let tool_result = &content[0];
        assert_eq!(tool_result["type"].as_str(), Some("tool_result"));
        let inner = tool_result["content"].as_array().unwrap();
        assert_eq!(inner[0]["type"].as_str(), Some("text"));
        assert_eq!(inner[1]["type"].as_str(), Some("image"));
        assert_eq!(inner[1]["source"]["type"].as_str(), Some("base64"));
        assert_eq!(inner[1]["source"]["media_type"].as_str(), Some("image/png"));
    }

    #[test]
    fn anthropic_builder_plain_string_without_images() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            Message {
                role: "tool".to_string(),
                content: "result".to_string(),
                tool_call_id: Some("call_1".to_string()),
                ..Default::default()
            },
        ];
        let (_system, api_msgs) = build_anthropic_messages(&messages);
        let content = api_msgs[0].content.as_array().unwrap();
        let tool_result = &content[0];
        // content should be a plain string, not an array
        assert!(tool_result["content"].is_string());
    }

    #[test]
    fn gemini_builder_includes_image_after_function_response() {
        let provider = GeminiProvider::new(
            "key".to_string(),
            "gemini-2.5-pro".to_string(),
            1_048_576,
            65_536,
        );
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            tool_msg_with_images(),
        ];
        let (_sys, contents, _body) = build_gemini_request_parts(&messages, &provider);
        // Should have functionResponse + user message with inlineData
        assert_eq!(contents.len(), 2);
        let img_msg = &contents[1];
        assert_eq!(img_msg["role"].as_str(), Some("user"));
        let parts = img_msg["parts"].as_array().unwrap();
        assert_eq!(parts[0]["text"].as_str().unwrap(), "Screenshot from the previous tool call:");
        assert_eq!(parts[1]["inlineData"]["mimeType"].as_str(), Some("image/png"));
        assert_eq!(parts[1]["inlineData"]["data"].as_str(), Some("iVBORw0KGgo="));
    }

    #[test]
    fn gemini_builder_no_image_without_images_field() {
        let provider = GeminiProvider::new(
            "key".to_string(),
            "gemini-2.5-pro".to_string(),
            1_048_576,
            65_536,
        );
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "sys".to_string(),
                ..Default::default()
            },
            Message {
                role: "tool".to_string(),
                content: r#"{"output":"result"}"#.to_string(),
                tool_call_id: Some("call_1".to_string()),
                tool_name: Some("capture_screen".to_string()),
                ..Default::default()
            },
        ];
        let (_sys, contents, _body) = build_gemini_request_parts(&messages, &provider);
        // Should have only the functionResponse, no user image message
        assert_eq!(contents.len(), 1);
    }
}
