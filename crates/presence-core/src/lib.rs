pub mod types;
pub mod tools;
pub mod dispatch;
pub mod format;
pub mod prompt;

// Re-exports for convenience
pub use types::*;
pub use dispatch::{PresenceAction, dispatch_tool_call};
pub use format::{format_event, truncate};
pub use tools::{ToolDefinition, presence_tools};
pub use prompt::DEFAULT_PRESENCE_PROMPT;
