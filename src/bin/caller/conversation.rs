use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

pub struct Conversation {
    messages: Vec<Message>,
}

impl Conversation {
    pub fn new(system_prompt: String) -> Self {
        Self {
            messages: vec![Message {
                role: "system".to_string(),
                content: system_prompt,
            }],
        }
    }

    pub fn add_user(&mut self, content: String) {
        self.messages.push(Message {
            role: "user".to_string(),
            content,
        });
    }

    pub fn add_assistant(&mut self, content: String) {
        self.messages.push(Message {
            role: "assistant".to_string(),
            content,
        });
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
}
