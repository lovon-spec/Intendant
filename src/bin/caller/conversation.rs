use crate::provider::TokenUsage;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
pub enum MessageLayer {
    User,
    Orchestrator,
    SubAgent,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(skip)]
    pub layer: Option<MessageLayer>,
}

pub struct Conversation {
    messages: Vec<Message>,
    last_usage: Option<TokenUsage>,
    context_window: u64,
    turn: usize,
    protect_user_layer: bool,
}

impl Conversation {
    pub fn new(system_prompt: String, context_window: u64) -> Self {
        Self {
            messages: vec![Message {
                role: "system".to_string(),
                content: system_prompt,
                layer: None,
            }],
            last_usage: None,
            context_window,
            turn: 0,
            protect_user_layer: false,
        }
    }

    #[allow(dead_code)]
    pub fn set_protect_user_layer(&mut self, protect: bool) {
        self.protect_user_layer = protect;
    }

    pub fn add_user(&mut self, content: String) {
        self.messages.push(Message {
            role: "user".to_string(),
            content,
            layer: None,
        });
    }

    #[allow(dead_code)]
    pub fn add_user_with_layer(&mut self, content: String, layer: MessageLayer) {
        self.messages.push(Message {
            role: "user".to_string(),
            content,
            layer: Some(layer),
        });
    }

    pub fn add_assistant(&mut self, content: String) {
        self.messages.push(Message {
            role: "assistant".to_string(),
            content,
            layer: None,
        });
    }

    #[allow(dead_code)]
    pub fn add_assistant_with_layer(&mut self, content: String, layer: MessageLayer) {
        self.messages.push(Message {
            role: "assistant".to_string(),
            content,
            layer: Some(layer),
        });
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    #[allow(dead_code)]
    pub fn estimated_tokens(&self) -> usize {
        self.messages.iter().map(|m| m.content.len() / 4).sum()
    }

    pub fn set_usage(&mut self, usage: TokenUsage) {
        self.last_usage = Some(usage);
    }

    pub fn increment_turn(&mut self) {
        self.turn += 1;
    }

    #[allow(dead_code)]
    pub fn turn(&self) -> usize {
        self.turn
    }

    pub fn remaining_budget(&self) -> u64 {
        match &self.last_usage {
            Some(usage) => self.context_window.saturating_sub(usage.total_tokens),
            None => self.context_window,
        }
    }

    pub fn usage_fraction(&self) -> f64 {
        if self.context_window == 0 {
            return 1.0;
        }
        match &self.last_usage {
            Some(usage) => usage.total_tokens as f64 / self.context_window as f64,
            None => 0.0,
        }
    }

    pub fn budget_summary(&self) -> String {
        match &self.last_usage {
            Some(usage) => {
                let pct = (self.usage_fraction() * 100.0) as u64;
                format!(
                    "[Context: ~{}/{} tokens used ({}%), turn {}]",
                    format_tokens(usage.total_tokens),
                    format_tokens(self.context_window),
                    pct,
                    self.turn
                )
            }
            None => {
                format!(
                    "[Context: ~0/{} tokens used (0%), turn {}]",
                    format_tokens(self.context_window),
                    self.turn
                )
            }
        }
    }

    pub fn drop_turns(&mut self, indices: &[usize]) {
        let len = self.messages.len();
        let protected_min = if len >= 2 { len - 2 } else { len };

        let mut to_remove: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|&i| {
                if i == 0 || i >= protected_min {
                    return false;
                }
                // Protect User-layer messages when protect_user_layer is enabled
                if self.protect_user_layer {
                    if let Some(MessageLayer::User) = self.messages[i].layer {
                        return false;
                    }
                }
                true
            })
            .collect();

        to_remove.sort_unstable();
        to_remove.dedup();

        // Remove in reverse order to preserve indices
        for &i in to_remove.iter().rev() {
            self.messages.remove(i);
        }
    }

    pub fn summarize_turns(&mut self, indices: &[usize], summary: &str) {
        if indices.is_empty() {
            return;
        }

        let len = self.messages.len();
        let protected_min = if len >= 2 { len - 2 } else { len };

        let mut valid: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|&i| {
                if i == 0 || i >= protected_min {
                    return false;
                }
                if self.protect_user_layer {
                    if let Some(MessageLayer::User) = self.messages[i].layer {
                        return false;
                    }
                }
                true
            })
            .collect();

        valid.sort_unstable();
        valid.dedup();

        if valid.is_empty() {
            return;
        }

        let insert_pos = valid[0];

        // Remove in reverse order
        for &i in valid.iter().rev() {
            self.messages.remove(i);
        }

        self.messages.insert(
            insert_pos,
            Message {
                role: "user".to_string(),
                content: format!("[Context Summary] {}", summary),
                layer: None,
            },
        );
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!(
            "{},{:03},{:03}",
            n / 1_000_000,
            (n / 1_000) % 1_000,
            n % 1_000
        )
    } else if n >= 1_000 {
        format!("{},{:03}", n / 1_000, n % 1_000)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_conversation_has_system_prompt() {
        let conv = Conversation::new("You are a helpful assistant.".to_string(), 128_000);
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content, "You are a helpful assistant.");
    }

    #[test]
    fn add_user_message() {
        let mut conv = Conversation::new("system".to_string(), 128_000);
        conv.add_user("hello".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content, "hello");
    }

    #[test]
    fn add_assistant_message() {
        let mut conv = Conversation::new("system".to_string(), 128_000);
        conv.add_assistant("response".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content, "response");
    }

    #[test]
    fn conversation_ordering() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("msg1".to_string());
        conv.add_assistant("resp1".to_string());
        conv.add_user("msg2".to_string());
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[2].role, "assistant");
        assert_eq!(msgs[3].role, "user");
    }

    #[test]
    fn message_serialization() {
        let msg = Message {
            role: "user".to_string(),
            content: "test".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.role, "user");
        assert_eq!(deserialized.content, "test");
    }

    #[test]
    fn len_and_estimated_tokens() {
        let mut conv = Conversation::new("system prompt".to_string(), 128_000);
        assert_eq!(conv.len(), 1);
        conv.add_user("hello world".to_string());
        assert_eq!(conv.len(), 2);
        // estimated_tokens is len/4 per message
        let tokens = conv.estimated_tokens();
        assert!(tokens > 0);
    }

    #[test]
    fn drop_turns_protects_system_and_last_two() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string()); // 1
        conv.add_assistant("a1".to_string()); // 2
        conv.add_user("u2".to_string()); // 3
        conv.add_assistant("a2".to_string()); // 4
        conv.add_user("u3".to_string()); // 5
        conv.add_assistant("a3".to_string()); // 6

        // Try to drop system (0), middle messages (1,2), and last two (5,6)
        conv.drop_turns(&[0, 1, 2, 5, 6]);

        // System (0) protected, last two (5,6) protected
        // Only 1 and 2 should be removed
        assert_eq!(conv.len(), 5); // 7 - 2 = 5
        assert_eq!(conv.messages()[0].role, "system");
        assert_eq!(conv.messages()[0].content, "sys");
    }

    #[test]
    fn drop_turns_empty_indices() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.drop_turns(&[]);
        assert_eq!(conv.len(), 2);
    }

    #[test]
    fn drop_turns_duplicate_indices() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.add_assistant("a1".to_string());
        conv.add_user("u2".to_string());
        conv.add_assistant("a2".to_string());

        conv.drop_turns(&[1, 1, 1]);
        assert_eq!(conv.len(), 4); // only one removal
    }

    #[test]
    fn summarize_turns_replaces_range() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string()); // 1
        conv.add_assistant("a1".to_string()); // 2
        conv.add_user("u2".to_string()); // 3
        conv.add_assistant("a2".to_string()); // 4
        conv.add_user("u3".to_string()); // 5
        conv.add_assistant("a3".to_string()); // 6

        conv.summarize_turns(&[1, 2, 3, 4], "Set up the environment");

        // 7 original - 4 removed + 1 summary = 4
        assert_eq!(conv.len(), 4);
        assert_eq!(conv.messages()[0].content, "sys");
        assert!(conv.messages()[1].content.contains("[Context Summary]"));
        assert!(conv.messages()[1]
            .content
            .contains("Set up the environment"));
        assert_eq!(conv.messages()[2].content, "u3");
        assert_eq!(conv.messages()[3].content, "a3");
    }

    #[test]
    fn summarize_turns_empty() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.summarize_turns(&[], "summary");
        assert_eq!(conv.len(), 2);
    }

    #[test]
    fn summarize_turns_protects_system_and_last_two() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string()); // 1
        conv.add_assistant("a1".to_string()); // 2

        // Try to summarize all — system (0) and last two (1,2) are protected
        conv.summarize_turns(&[0, 1, 2], "summary");
        assert_eq!(conv.len(), 3); // unchanged
    }

    // --- Message layer tests ---

    #[test]
    fn message_layer_skipped_in_serialization() {
        let msg = Message {
            role: "user".to_string(),
            content: "test".to_string(),
            layer: Some(MessageLayer::User),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("layer"));
        let deserialized: Message = serde_json::from_str(&json).unwrap();
        assert!(deserialized.layer.is_none());
    }

    #[test]
    fn add_user_with_layer() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user_with_layer("hello".to_string(), MessageLayer::User);
        assert_eq!(conv.messages()[1].layer, Some(MessageLayer::User));
    }

    #[test]
    fn add_assistant_with_layer() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_assistant_with_layer("response".to_string(), MessageLayer::Orchestrator);
        assert_eq!(conv.messages()[1].layer, Some(MessageLayer::Orchestrator));
    }

    #[test]
    fn drop_turns_protects_user_layer() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.set_protect_user_layer(true);
        conv.add_user_with_layer("user msg".to_string(), MessageLayer::User); // 1
        conv.add_assistant("orch status".to_string()); // 2
        conv.add_user("orch output".to_string()); // 3
        conv.add_assistant("more output".to_string()); // 4
        conv.add_user("final".to_string()); // 5
        conv.add_assistant("done".to_string()); // 6

        // Try to drop index 1 (User-layer) and 2 (no layer)
        conv.drop_turns(&[1, 2]);

        // Index 1 (User layer) should be protected, index 2 should be dropped
        assert_eq!(conv.len(), 6);
        assert_eq!(conv.messages()[1].content, "user msg");
    }

    #[test]
    fn drop_turns_without_protection_removes_user_layer() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        // protect_user_layer is false by default
        conv.add_user_with_layer("user msg".to_string(), MessageLayer::User); // 1
        conv.add_assistant("response".to_string()); // 2
        conv.add_user("msg".to_string()); // 3
        conv.add_assistant("resp".to_string()); // 4

        conv.drop_turns(&[1]);
        assert_eq!(conv.len(), 4); // index 1 removed
    }

    #[test]
    fn summarize_turns_protects_user_layer() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.set_protect_user_layer(true);
        conv.add_user_with_layer("user task".to_string(), MessageLayer::User); // 1
        conv.add_assistant("status 1".to_string()); // 2
        conv.add_user("agent output 1".to_string()); // 3
        conv.add_assistant("status 2".to_string()); // 4
        conv.add_user("latest".to_string()); // 5
        conv.add_assistant("done".to_string()); // 6

        conv.summarize_turns(&[1, 2, 3], "Early progress");

        // 7 original. Index 1 (User layer) is protected.
        // Indices 2 and 3 are removed. Summary inserted at position 2.
        // 7 - 2 + 1 = 6
        assert_eq!(conv.len(), 6);
        assert_eq!(conv.messages()[1].content, "user task"); // preserved
        assert!(conv.messages()[2].content.contains("[Context Summary]"));
    }

    // --- Token budget tests ---

    #[test]
    fn remaining_budget_no_usage() {
        let conv = Conversation::new("sys".to_string(), 200_000);
        assert_eq!(conv.remaining_budget(), 200_000);
    }

    #[test]
    fn remaining_budget_with_usage() {
        let mut conv = Conversation::new("sys".to_string(), 200_000);
        conv.set_usage(TokenUsage {
            prompt_tokens: 30_000,
            completion_tokens: 15_000,
            total_tokens: 45_000,
        });
        assert_eq!(conv.remaining_budget(), 155_000);
    }

    #[test]
    fn remaining_budget_no_underflow() {
        let mut conv = Conversation::new("sys".to_string(), 100);
        conv.set_usage(TokenUsage {
            prompt_tokens: 80,
            completion_tokens: 50,
            total_tokens: 130,
        });
        assert_eq!(conv.remaining_budget(), 0);
    }

    #[test]
    fn usage_fraction_no_usage() {
        let conv = Conversation::new("sys".to_string(), 200_000);
        assert!((conv.usage_fraction() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn usage_fraction_with_usage() {
        let mut conv = Conversation::new("sys".to_string(), 200_000);
        conv.set_usage(TokenUsage {
            prompt_tokens: 50_000,
            completion_tokens: 50_000,
            total_tokens: 100_000,
        });
        assert!((conv.usage_fraction() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn usage_fraction_zero_window() {
        let conv = Conversation::new("sys".to_string(), 0);
        assert!((conv.usage_fraction() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn budget_summary_no_usage() {
        let conv = Conversation::new("sys".to_string(), 200_000);
        let summary = conv.budget_summary();
        assert!(summary.contains("0/200,000"));
        assert!(summary.contains("0%"));
        assert!(summary.contains("turn 0"));
    }

    #[test]
    fn budget_summary_with_usage() {
        let mut conv = Conversation::new("sys".to_string(), 200_000);
        conv.increment_turn();
        conv.increment_turn();
        conv.set_usage(TokenUsage {
            prompt_tokens: 30_000,
            completion_tokens: 15_000,
            total_tokens: 45_000,
        });
        let summary = conv.budget_summary();
        assert!(summary.contains("45,000"));
        assert!(summary.contains("200,000"));
        assert!(summary.contains("22%"));
        assert!(summary.contains("turn 2"));
    }

    #[test]
    fn format_tokens_small() {
        assert_eq!(format_tokens(500), "500");
    }

    #[test]
    fn format_tokens_thousands() {
        assert_eq!(format_tokens(45_000), "45,000");
        assert_eq!(format_tokens(200_000), "200,000");
    }

    #[test]
    fn format_tokens_millions() {
        assert_eq!(format_tokens(1_000_000), "1,000,000");
    }

    #[test]
    fn turn_tracking() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        assert_eq!(conv.turn(), 0);
        conv.increment_turn();
        assert_eq!(conv.turn(), 1);
        conv.increment_turn();
        assert_eq!(conv.turn(), 2);
    }
}
