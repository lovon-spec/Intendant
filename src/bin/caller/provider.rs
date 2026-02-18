use crate::conversation::Message;
use crate::error::CallerError;
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

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub usage: TokenUsage,
}

#[async_trait]
pub trait ChatProvider: Send + Sync {
    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError>;
    fn name(&self) -> &str;
    fn context_window(&self) -> u64;
    #[allow(dead_code)]
    fn max_output_tokens(&self) -> u64;
}

// --- OpenAI (Responses API) ---

#[derive(Serialize)]
struct OpenAIResponsesRequest {
    model: String,
    input: Vec<ResponsesInputMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<TextConfig>,
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

#[derive(Serialize)]
struct ResponsesInputMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OpenAIResponsesResponse {
    output_text: Option<String>,
    output: Option<Vec<ResponsesOutputItem>>,
    usage: Option<ResponsesUsage>,
}

#[derive(Deserialize)]
struct ResponsesOutputItem {
    content: Option<Vec<ResponsesContentItem>>,
}

#[derive(Deserialize)]
struct ResponsesContentItem {
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
}

impl OpenAIProvider {
    pub fn new(api_key: String, model: String, context_window: u64, max_output_tokens: u64) -> Self {
        let structured_output = resolve_structured_output(&model);
        let reasoning = resolve_reasoning(&model);

        Self {
            client: Client::new(),
            api_key,
            model,
            context_window,
            max_output_tokens,
            structured_output,
            reasoning,
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

        // Pass through all non-system roles (user, assistant, developer, tool)
        let mut input: Vec<ResponsesInputMessage> = messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| ResponsesInputMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();

        // When structured output is enabled, OpenAI requires the word "json" in the
        // input messages (instructions alone don't count). Inject a developer message.
        if self.structured_output {
            input.insert(0, ResponsesInputMessage {
                role: "developer".to_string(),
                content: "Always respond with valid JSON matching the command schema.".to_string(),
            });
        }

        let text = if self.structured_output {
            Some(TextConfig {
                format: TextFormat {
                    r#type: "json_object".to_string(),
                },
            })
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

        // Prefer output_text, fall back to digging into output array
        let content = resp
            .output_text
            .or_else(|| {
                resp.output.as_ref().and_then(|items| {
                    items.iter().find_map(|item| {
                        item.content.as_ref().and_then(|contents| {
                            contents.iter().find_map(|c| c.text.clone())
                        })
                    })
                })
            })
            .ok_or_else(|| CallerError::Provider("No response content".to_string()))?;

        let usage = resp
            .usage
            .map(|u| TokenUsage {
                prompt_tokens: u.input_tokens,
                completion_tokens: u.output_tokens,
                total_tokens: u.total_tokens,
            })
            .unwrap_or_default();

        Ok(ChatResponse { content, usage })
    }

    fn name(&self) -> &str {
        "openai"
    }

    fn context_window(&self) -> u64 {
        self.context_window
    }

    fn max_output_tokens(&self) -> u64 {
        self.max_output_tokens
    }
}

// --- Anthropic ---

#[derive(Serialize)]
struct AnthropicChatRequest {
    model: String,
    system: String,
    messages: Vec<AnthropicMessage>,
    max_tokens: u64,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct AnthropicChatResponse {
    content: Vec<AnthropicContent>,
    usage: Option<AnthropicUsage>,
}

#[derive(Deserialize)]
struct AnthropicContent {
    text: Option<String>,
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
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String, context_window: u64, max_output_tokens: u64) -> Self {
        Self {
            client: Client::new(),
            api_key,
            model,
            context_window,
            max_output_tokens,
        }
    }
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError> {
        // Extract system message and filter to user/assistant only
        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone())
            .unwrap_or_default();

        let api_messages: Vec<AnthropicMessage> = messages
            .iter()
            .filter(|m| m.role == "user" || m.role == "assistant")
            .map(|m| AnthropicMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();

        let request = AnthropicChatRequest {
            model: self.model.clone(),
            system,
            messages: api_messages,
            max_tokens: self.max_output_tokens,
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
        let content = chat_response
            .content
            .first()
            .and_then(|c| c.text.clone())
            .ok_or_else(|| CallerError::Provider("No response content".to_string()))?;

        let usage = chat_response
            .usage
            .map(|u| TokenUsage {
                prompt_tokens: u.input_tokens,
                completion_tokens: u.output_tokens,
                total_tokens: u.input_tokens + u.output_tokens,
            })
            .unwrap_or_default();

        Ok(ChatResponse { content, usage })
    }

    fn name(&self) -> &str {
        "anthropic"
    }

    fn context_window(&self) -> u64 {
        self.context_window
    }

    fn max_output_tokens(&self) -> u64 {
        self.max_output_tokens
    }
}

// --- Provider selection ---

fn default_context_window(model: &str) -> u64 {
    match model {
        m if m.starts_with("gpt-5") => 400_000,
        m if m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") => 200_000,
        m if m.contains("claude") => 200_000,
        _ => 200_000,
    }
}

fn default_max_output_tokens(model: &str) -> u64 {
    match model {
        m if m.starts_with("gpt-5") => 128_000,
        m if m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") => 100_000,
        m if m.contains("claude") => 8_192,
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
    model.starts_with("gpt-5")
        || model.starts_with("o3")
        || model.starts_with("o4")
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
    let effort = env::var("REASONING_EFFORT").ok().filter(|s| !s.is_empty())?;
    let summary = env::var("REASONING_SUMMARY").ok().filter(|s| !s.is_empty());
    Some(ReasoningConfig { effort, summary })
}

pub fn select_provider() -> Result<Box<dyn ChatProvider>, CallerError> {
    let openai_key = env::var("OPENAI_API_KEY")
        .or_else(|_| env::var("OPENAI"))
        .ok();
    let anthropic_key = env::var("ANTHROPIC_API_KEY")
        .or_else(|_| env::var("ANTHROPIC"))
        .ok();

    let preferred = env::var("PROVIDER").ok();

    match (openai_key, anthropic_key, preferred.as_deref()) {
        // Both available, check PROVIDER preference
        (Some(oai), Some(ant), Some("anthropic")) => {
            let _ = oai;
            let model = env::var("MODEL_NAME")
                .unwrap_or_else(|_| "claude-sonnet-4-5-20250929".to_string());
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
            let model = env::var("MODEL_NAME")
                .unwrap_or_else(|_| "claude-sonnet-4-5-20250929".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(AnthropicProvider::new(ant, model, ctx, max_out)))
        }
        (Some(_oai), Some(_ant), Some(other)) => {
            Err(CallerError::Config(format!(
                "Unknown PROVIDER value: '{}'. Expected 'openai' or 'anthropic'.",
                other
            )))
        }
        (None, None, _) => Err(CallerError::Config(
            "No API key found. Set OPENAI_API_KEY or ANTHROPIC_API_KEY in your environment, \
             a .env file in your project root, or ~/.config/agent/.env for global use.".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_provider_name() {
        let provider = OpenAIProvider::new("key".to_string(), "gpt-5.2-codex".to_string(), 400_000, 128_000);
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn anthropic_provider_name() {
        let provider = AnthropicProvider::new("key".to_string(), "claude-sonnet-4-5-20250929".to_string(), 200_000, 8_192);
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
                content: m.content.clone(),
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
    fn default_context_window_known_models() {
        assert_eq!(default_context_window("gpt-5.2-codex"), 400_000);
        assert_eq!(default_context_window("gpt-5"), 400_000);
        assert_eq!(default_context_window("claude-sonnet-4-5-20250929"), 200_000);
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
        assert_eq!(default_max_output_tokens("claude-sonnet-4-5-20250929"), 8_192);
        assert_eq!(default_max_output_tokens("o1-preview"), 100_000);
    }

    #[test]
    fn context_window_methods() {
        let provider = OpenAIProvider::new("key".to_string(), "gpt-5.2-codex".to_string(), 400_000, 128_000);
        assert_eq!(provider.context_window(), 400_000);
        assert_eq!(provider.max_output_tokens(), 128_000);

        let provider = AnthropicProvider::new("key".to_string(), "claude-sonnet-4-5-20250929".to_string(), 200_000, 8_192);
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
        assert_eq!(resp.output_text.as_deref(), Some("Hello from Responses API!"));
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
                item.content.as_ref().and_then(|contents| {
                    contents.iter().find_map(|c| c.text.clone())
                })
            })
        });
        assert_eq!(text.as_deref(), Some("Fallback text"));
    }

    #[test]
    fn responses_api_request_serialization() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![
                ResponsesInputMessage { role: "user".to_string(), content: "Hello".to_string() },
            ],
            instructions: Some("Be helpful.".to_string()),
            max_output_tokens: Some(128_000),
            reasoning: None,
            text: None,
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
            input: vec![
                ResponsesInputMessage { role: "user".to_string(), content: "Hi".to_string() },
            ],
            instructions: None,
            max_output_tokens: None,
            reasoning: None,
            text: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("instructions"));
        assert!(!json.contains("max_output_tokens"));
        assert!(!json.contains("reasoning"));
        assert!(!json.contains("text"));
    }

    #[test]
    fn responses_api_request_with_reasoning() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![
                ResponsesInputMessage { role: "user".to_string(), content: "Hi".to_string() },
            ],
            instructions: None,
            max_output_tokens: Some(128_000),
            reasoning: Some(ReasoningConfig {
                effort: "high".to_string(),
                summary: Some("auto".to_string()),
            }),
            text: None,
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
            input: vec![
                ResponsesInputMessage { role: "user".to_string(), content: "Hi".to_string() },
            ],
            instructions: None,
            max_output_tokens: Some(100_000),
            reasoning: Some(ReasoningConfig {
                effort: "medium".to_string(),
                summary: None,
            }),
            text: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"effort\":\"medium\""));
        assert!(!json.contains("\"summary\""));
    }

    #[test]
    fn responses_api_request_with_structured_output() {
        let request = OpenAIResponsesRequest {
            model: "gpt-5.2-codex".to_string(),
            input: vec![
                ResponsesInputMessage { role: "user".to_string(), content: "Hi".to_string() },
            ],
            instructions: None,
            max_output_tokens: Some(128_000),
            reasoning: None,
            text: Some(TextConfig {
                format: TextFormat {
                    r#type: "json_object".to_string(),
                },
            }),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"text\""));
        assert!(json.contains("\"json_object\""));
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
            Message { role: "system".to_string(), content: "System prompt".to_string(), ..Default::default() },
            Message { role: "developer".to_string(), content: "Developer note".to_string(), ..Default::default() },
            Message { role: "user".to_string(), content: "Hello".to_string(), ..Default::default() },
            Message { role: "assistant".to_string(), content: "Hi".to_string(), ..Default::default() },
        ];

        let instructions = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone());
        assert_eq!(instructions.as_deref(), Some("System prompt"));

        let input: Vec<ResponsesInputMessage> = messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| ResponsesInputMessage { role: m.role.clone(), content: m.content.clone() })
            .collect();

        assert_eq!(input.len(), 3);
        assert_eq!(input[0].role, "developer");
        assert_eq!(input[1].role, "user");
        assert_eq!(input[2].role, "assistant");
    }

    #[test]
    fn openai_provider_stores_config() {
        let provider = OpenAIProvider::new("key".to_string(), "gpt-5.2-codex".to_string(), 400_000, 128_000);
        assert_eq!(provider.max_output_tokens(), 128_000);
        // gpt-5 supports structured output by default
        assert!(provider.structured_output);
    }
}
