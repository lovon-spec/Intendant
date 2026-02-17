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

// --- OpenAI ---

#[derive(Serialize)]
struct OpenAIChatRequest {
    model: String,
    messages: Vec<Message>,
}

#[derive(Deserialize)]
struct OpenAIChatResponse {
    choices: Vec<OpenAIChoice>,
    usage: Option<OpenAIUsage>,
}

#[derive(Deserialize)]
struct OpenAIChoice {
    message: OpenAIResponseMessage,
}

#[derive(Deserialize)]
struct OpenAIResponseMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct OpenAIUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

pub struct OpenAIProvider {
    client: Client,
    api_key: String,
    model: String,
    context_window: u64,
    #[allow(dead_code)]
    max_output_tokens: u64,
}

impl OpenAIProvider {
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
impl ChatProvider for OpenAIProvider {
    async fn chat(&self, messages: &[Message]) -> Result<ChatResponse, CallerError> {
        let request = OpenAIChatRequest {
            model: self.model.clone(),
            messages: messages.to_vec(),
        };

        let response = self
            .client
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CallerError::Provider(format!("{}: {}", status, body)));
        }

        let chat_response: OpenAIChatResponse = response.json().await?;
        let content = chat_response
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .ok_or_else(|| CallerError::Provider("No response content".to_string()))?;

        let usage = chat_response
            .usage
            .map(|u| TokenUsage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
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
        m if m.starts_with("gpt-4o") => 128_000,
        m if m.starts_with("gpt-4-turbo") => 128_000,
        m if m.starts_with("gpt-4") => 8_192,
        m if m.starts_with("gpt-3.5") => 16_385,
        m if m.starts_with("o1") || m.starts_with("o3") => 200_000,
        m if m.contains("claude") => 200_000,
        _ => 128_000,
    }
}

fn default_max_output_tokens(model: &str) -> u64 {
    match model {
        m if m.starts_with("gpt-4o") => 16_384,
        m if m.starts_with("gpt-4-turbo") => 4_096,
        m if m.starts_with("gpt-4") => 8_192,
        m if m.starts_with("gpt-3.5") => 4_096,
        m if m.starts_with("o1") || m.starts_with("o3") => 100_000,
        m if m.contains("claude") => 8_192,
        _ => 8_192,
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
            let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gpt-4o".to_string());
            let ctx = resolve_context_window(&model);
            let max_out = resolve_max_output_tokens(&model);
            Ok(Box::new(OpenAIProvider::new(oai, model, ctx, max_out)))
        }
        (Some(oai), None, _) => {
            let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gpt-4o".to_string());
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
            "No API key found. Set OPENAI_API_KEY or ANTHROPIC_API_KEY.".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_provider_name() {
        let provider = OpenAIProvider::new("key".to_string(), "gpt-4o".to_string(), 128_000, 16_384);
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
    fn openai_usage_deserialization() {
        let json = r#"{
            "choices": [{"message": {"content": "Hello"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        }"#;
        let resp: OpenAIChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn openai_usage_missing() {
        let json = r#"{
            "choices": [{"message": {"content": "Hello"}}]
        }"#;
        let resp: OpenAIChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.usage.is_none());
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
        assert_eq!(default_context_window("gpt-4o"), 128_000);
        assert_eq!(default_context_window("gpt-4o-mini"), 128_000);
        assert_eq!(default_context_window("gpt-4-turbo-preview"), 128_000);
        assert_eq!(default_context_window("claude-sonnet-4-5-20250929"), 200_000);
        assert_eq!(default_context_window("o1-preview"), 200_000);
        assert_eq!(default_context_window("o3-mini"), 200_000);
    }

    #[test]
    fn default_context_window_unknown_model() {
        assert_eq!(default_context_window("some-unknown-model"), 128_000);
    }

    #[test]
    fn default_max_output_known_models() {
        assert_eq!(default_max_output_tokens("gpt-4o"), 16_384);
        assert_eq!(default_max_output_tokens("claude-sonnet-4-5-20250929"), 8_192);
        assert_eq!(default_max_output_tokens("o1-preview"), 100_000);
    }

    #[test]
    fn context_window_methods() {
        let provider = OpenAIProvider::new("key".to_string(), "gpt-4o".to_string(), 128_000, 16_384);
        assert_eq!(provider.context_window(), 128_000);
        assert_eq!(provider.max_output_tokens(), 16_384);

        let provider = AnthropicProvider::new("key".to_string(), "claude-sonnet-4-5-20250929".to_string(), 200_000, 8_192);
        assert_eq!(provider.context_window(), 200_000);
        assert_eq!(provider.max_output_tokens(), 8_192);
    }
}
