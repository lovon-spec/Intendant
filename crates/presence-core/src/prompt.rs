/// Default compiled-in presence system prompt.
pub const DEFAULT_PRESENCE_PROMPT: &str = include_str!("../prompts/SysPrompt_presence.md");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_prompt_not_empty() {
        assert!(!DEFAULT_PRESENCE_PROMPT.is_empty());
        assert!(DEFAULT_PRESENCE_PROMPT.contains("Intendant"));
    }
}
