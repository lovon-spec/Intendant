pub mod dispatch;
pub mod format;
pub mod prompt;
pub mod tools;
pub mod types;

#[cfg(target_arch = "wasm32")]
pub mod wasm;

// Re-exports for convenience (includes protocol types: PresenceConnect, PresenceWelcome, etc.)
pub use dispatch::{action_confirmation, dispatch_tool_call, PresenceAction};
pub use format::{format_agent_output, format_event, truncate, FormattedOutput};
pub use prompt::DEFAULT_PRESENCE_PROMPT;
pub use tools::{presence_tools, ToolDefinition};
pub use types::*;
