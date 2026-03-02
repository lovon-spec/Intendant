use serde::Deserialize;
use std::fmt;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Global autonomy level controlling how much user approval is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutonomyLevel {
    /// Ask before every command execution
    Low,
    /// Ask before writes, network, destructive (default)
    Medium,
    /// Only ask for unavoidable human input
    High,
    /// Never ask (fully autonomous)
    Full,
}

impl AutonomyLevel {
    pub fn from_str_loose(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "low" | "l" | "0" => Self::Low,
            "medium" | "med" | "m" | "1" => Self::Medium,
            "high" | "h" | "2" => Self::High,
            "full" | "f" | "3" => Self::Full,
            _ => Self::Medium,
        }
    }

    pub fn cycle_up(self) -> Self {
        match self {
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Full,
            Self::Full => Self::Full,
        }
    }

    pub fn cycle_down(self) -> Self {
        match self {
            Self::Low => Self::Low,
            Self::Medium => Self::Low,
            Self::High => Self::Medium,
            Self::Full => Self::High,
        }
    }
}

impl Default for AutonomyLevel {
    fn default() -> Self {
        Self::Medium
    }
}

impl fmt::Display for AutonomyLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Low => write!(f, "Low"),
            Self::Medium => write!(f, "Medium"),
            Self::High => write!(f, "High"),
            Self::Full => write!(f, "Full"),
        }
    }
}

/// Categories of actions that the agent can perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionCategory {
    FileRead,
    FileWrite,
    #[allow(dead_code)]
    FileDelete,
    CommandExec,
    NetworkRequest,
    Destructive,
    HumanInput,
}

impl fmt::Display for ActionCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileRead => write!(f, "file_read"),
            Self::FileWrite => write!(f, "file_write"),
            Self::FileDelete => write!(f, "file_delete"),
            Self::CommandExec => write!(f, "command_exec"),
            Self::NetworkRequest => write!(f, "network"),
            Self::Destructive => write!(f, "destructive"),
            Self::HumanInput => write!(f, "human_input"),
        }
    }
}

/// Per-category approval rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalRule {
    Auto,
    Ask,
    Deny,
}

impl Default for ApprovalRule {
    fn default() -> Self {
        Self::Ask
    }
}

/// Category-level approval rules parsed from intendant.toml [approval] section.
#[derive(Debug, Clone, Deserialize)]
pub struct ApprovalConfig {
    #[serde(default = "default_auto")]
    pub file_read: ApprovalRule,
    #[serde(default)]
    pub file_write: ApprovalRule,
    #[serde(default)]
    pub file_delete: ApprovalRule,
    #[serde(default = "default_auto")]
    pub command_exec: ApprovalRule,
    #[serde(default = "default_auto")]
    pub network: ApprovalRule,
    #[serde(default)]
    pub destructive: ApprovalRule,
}

fn default_auto() -> ApprovalRule {
    ApprovalRule::Auto
}

impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            file_read: ApprovalRule::Auto,
            file_write: ApprovalRule::Ask,
            file_delete: ApprovalRule::Ask,
            command_exec: ApprovalRule::Auto,
            network: ApprovalRule::Auto,
            destructive: ApprovalRule::Ask,
        }
    }
}

impl ApprovalConfig {
    pub fn rule_for(&self, category: ActionCategory) -> ApprovalRule {
        match category {
            ActionCategory::FileRead => self.file_read,
            ActionCategory::FileWrite => self.file_write,
            ActionCategory::FileDelete => self.file_delete,
            ActionCategory::CommandExec => self.command_exec,
            ActionCategory::NetworkRequest => self.network,
            ActionCategory::Destructive => self.destructive,
            ActionCategory::HumanInput => ApprovalRule::Ask, // always ask
        }
    }
}

/// Combined autonomy state shared between the agent loop and TUI.
#[derive(Debug, Clone)]
pub struct AutonomyState {
    pub level: AutonomyLevel,
    pub rules: ApprovalConfig,
}

impl Default for AutonomyState {
    fn default() -> Self {
        Self {
            level: AutonomyLevel::Medium,
            rules: ApprovalConfig::default(),
        }
    }
}

impl AutonomyState {
    pub fn new(level: AutonomyLevel, rules: ApprovalConfig) -> Self {
        Self { level, rules }
    }

    /// Determine whether approval is needed for a given action category.
    /// Returns true if the user must be prompted.
    pub fn needs_approval(&self, category: ActionCategory) -> bool {
        // HumanInput always requires human regardless of autonomy level
        if category == ActionCategory::HumanInput {
            return true;
        }

        // Full autonomy auto-approves everything except HumanInput
        if self.level == AutonomyLevel::Full {
            return false;
        }

        // Check category-level rule (overrides global level)
        let rule = self.rules.rule_for(category);
        match rule {
            ApprovalRule::Auto => false,
            ApprovalRule::Deny => true, // deny acts like "ask" — will be denied
            ApprovalRule::Ask => {
                // Apply global autonomy level
                match self.level {
                    AutonomyLevel::Low => true, // ask for everything
                    AutonomyLevel::Medium => {
                        // Ask for writes, deletes, destructive, network
                        matches!(
                            category,
                            ActionCategory::FileWrite
                                | ActionCategory::FileDelete
                                | ActionCategory::Destructive
                                | ActionCategory::NetworkRequest
                        )
                    }
                    AutonomyLevel::High => false,
                    AutonomyLevel::Full => false, // unreachable, handled above
                }
            }
        }
    }
}

/// Shared autonomy state wrapped in Arc<RwLock> for concurrent access.
pub type SharedAutonomy = Arc<RwLock<AutonomyState>>;

pub fn shared_autonomy(state: AutonomyState) -> SharedAutonomy {
    Arc::new(RwLock::new(state))
}

/// Classify an agent command JSON into action categories.
pub fn classify_command(cmd: &serde_json::Value) -> Vec<ActionCategory> {
    let function = cmd.get("function").and_then(|f| f.as_str()).unwrap_or("");

    match function {
        "inspectPath" | "recallMemory" => vec![ActionCategory::FileRead],
        "writeFile" | "editFile" | "storeMemory" => vec![ActionCategory::FileWrite],
        "captureScreen" => vec![ActionCategory::FileRead],
        "askHuman" => vec![ActionCategory::HumanInput],
        "browse" => vec![ActionCategory::NetworkRequest],
        "execAsAgent" | "execPty" => {
            let command_str = cmd.get("command").and_then(|c| c.as_str()).unwrap_or("");
            classify_shell_command(command_str)
        }
        _ => vec![ActionCategory::CommandExec],
    }
}

/// Classify a shell command string into action categories.
fn classify_shell_command(cmd: &str) -> Vec<ActionCategory> {
    let mut categories = vec![ActionCategory::CommandExec];
    let lower = cmd.to_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();

    // Skip leading `sudo` to classify the actual command
    let first = if tokens.first().copied() == Some("sudo") {
        tokens.get(1).copied().unwrap_or("")
    } else {
        tokens.first().copied().unwrap_or("")
    };

    // Detect sudo usage as destructive (privilege escalation)
    if tokens.first().copied() == Some("sudo") {
        categories.push(ActionCategory::Destructive);
    }

    // Destructive commands
    let destructive_commands = [
        "rm", "rmdir", "kill", "killall", "pkill", "shutdown", "reboot", "mkfs", "dd",
    ];
    if destructive_commands.contains(&first) || lower.contains("rm -rf") || lower.contains("rm -r")
    {
        categories.push(ActionCategory::Destructive);
    }

    // Network commands
    let network_commands = [
        "curl",
        "wget",
        "ssh",
        "scp",
        "rsync",
        "nc",
        "ncat",
        "ping",
        "traceroute",
        "dig",
        "nslookup",
        "git",
    ];
    if network_commands.contains(&first) || lower.contains("apt") || lower.contains("pip install") {
        categories.push(ActionCategory::NetworkRequest);
    }

    // File write indicators
    if lower.contains(" > ")
        || lower.contains(" >> ")
        || first == "tee"
        || first == "mv"
        || first == "cp"
    {
        categories.push(ActionCategory::FileWrite);
    }

    categories
}

/// Classify all commands in a JSON input batch.
pub fn classify_batch(json_str: &str) -> Vec<(usize, Vec<ActionCategory>)> {
    let value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let commands = match value.get("commands").and_then(|c| c.as_array()) {
        Some(cmds) => cmds,
        None => return vec![],
    };

    commands
        .iter()
        .enumerate()
        .map(|(i, cmd)| (i, classify_command(cmd)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autonomy_level_display() {
        assert_eq!(AutonomyLevel::Low.to_string(), "Low");
        assert_eq!(AutonomyLevel::Medium.to_string(), "Medium");
        assert_eq!(AutonomyLevel::High.to_string(), "High");
        assert_eq!(AutonomyLevel::Full.to_string(), "Full");
    }

    #[test]
    fn autonomy_level_from_str() {
        assert_eq!(AutonomyLevel::from_str_loose("low"), AutonomyLevel::Low);
        assert_eq!(AutonomyLevel::from_str_loose("HIGH"), AutonomyLevel::High);
        assert_eq!(AutonomyLevel::from_str_loose("f"), AutonomyLevel::Full);
        assert_eq!(
            AutonomyLevel::from_str_loose("unknown"),
            AutonomyLevel::Medium
        );
        assert_eq!(AutonomyLevel::from_str_loose("0"), AutonomyLevel::Low);
        assert_eq!(AutonomyLevel::from_str_loose("3"), AutonomyLevel::Full);
    }

    #[test]
    fn autonomy_level_cycle() {
        assert_eq!(AutonomyLevel::Low.cycle_up(), AutonomyLevel::Medium);
        assert_eq!(AutonomyLevel::Medium.cycle_up(), AutonomyLevel::High);
        assert_eq!(AutonomyLevel::High.cycle_up(), AutonomyLevel::Full);
        assert_eq!(AutonomyLevel::Full.cycle_up(), AutonomyLevel::Full);

        assert_eq!(AutonomyLevel::Full.cycle_down(), AutonomyLevel::High);
        assert_eq!(AutonomyLevel::High.cycle_down(), AutonomyLevel::Medium);
        assert_eq!(AutonomyLevel::Medium.cycle_down(), AutonomyLevel::Low);
        assert_eq!(AutonomyLevel::Low.cycle_down(), AutonomyLevel::Low);
    }

    #[test]
    fn action_category_display() {
        assert_eq!(ActionCategory::FileRead.to_string(), "file_read");
        assert_eq!(ActionCategory::FileWrite.to_string(), "file_write");
        assert_eq!(ActionCategory::Destructive.to_string(), "destructive");
        assert_eq!(ActionCategory::HumanInput.to_string(), "human_input");
    }

    #[test]
    fn approval_config_default_rules() {
        let config = ApprovalConfig::default();
        assert_eq!(config.file_read, ApprovalRule::Auto);
        assert_eq!(config.file_write, ApprovalRule::Ask);
        assert_eq!(config.file_delete, ApprovalRule::Ask);
        assert_eq!(config.command_exec, ApprovalRule::Auto);
        assert_eq!(config.network, ApprovalRule::Auto);
        assert_eq!(config.destructive, ApprovalRule::Ask);
    }

    #[test]
    fn approval_config_from_toml() {
        let toml_str = r#"
file_read = "auto"
file_write = "deny"
file_delete = "deny"
command_exec = "ask"
network = "ask"
destructive = "deny"
"#;
        let config: ApprovalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.file_read, ApprovalRule::Auto);
        assert_eq!(config.file_write, ApprovalRule::Deny);
        assert_eq!(config.command_exec, ApprovalRule::Ask);
    }

    #[test]
    fn human_input_always_needs_approval() {
        let state = AutonomyState::new(AutonomyLevel::Full, ApprovalConfig::default());
        assert!(state.needs_approval(ActionCategory::HumanInput));

        let state = AutonomyState::new(AutonomyLevel::High, ApprovalConfig::default());
        assert!(state.needs_approval(ActionCategory::HumanInput));
    }

    #[test]
    fn full_autonomy_approves_everything_except_human() {
        let state = AutonomyState::new(AutonomyLevel::Full, ApprovalConfig::default());
        assert!(!state.needs_approval(ActionCategory::FileRead));
        assert!(!state.needs_approval(ActionCategory::FileWrite));
        assert!(!state.needs_approval(ActionCategory::FileDelete));
        assert!(!state.needs_approval(ActionCategory::CommandExec));
        assert!(!state.needs_approval(ActionCategory::Destructive));
        assert!(!state.needs_approval(ActionCategory::NetworkRequest));
        assert!(state.needs_approval(ActionCategory::HumanInput));
    }

    #[test]
    fn low_autonomy_asks_for_ask_rules() {
        let state = AutonomyState::new(AutonomyLevel::Low, ApprovalConfig::default());
        // file_read and command_exec are Auto in default config, so not asked
        assert!(!state.needs_approval(ActionCategory::FileRead));
        assert!(!state.needs_approval(ActionCategory::CommandExec));
        // file_write is Ask, and Low asks for everything with Ask rule
        assert!(state.needs_approval(ActionCategory::FileWrite));
        assert!(state.needs_approval(ActionCategory::Destructive));
    }

    #[test]
    fn medium_autonomy_asks_for_writes_and_destructive() {
        let state = AutonomyState::new(AutonomyLevel::Medium, ApprovalConfig::default());
        assert!(!state.needs_approval(ActionCategory::FileRead));
        assert!(!state.needs_approval(ActionCategory::CommandExec));
        assert!(state.needs_approval(ActionCategory::FileWrite));
        assert!(state.needs_approval(ActionCategory::FileDelete));
        assert!(state.needs_approval(ActionCategory::Destructive));
    }

    #[test]
    fn high_autonomy_only_asks_human() {
        let state = AutonomyState::new(AutonomyLevel::High, ApprovalConfig::default());
        assert!(!state.needs_approval(ActionCategory::FileRead));
        assert!(!state.needs_approval(ActionCategory::FileWrite));
        assert!(!state.needs_approval(ActionCategory::FileDelete));
        assert!(!state.needs_approval(ActionCategory::CommandExec));
        assert!(!state.needs_approval(ActionCategory::Destructive));
        assert!(state.needs_approval(ActionCategory::HumanInput));
    }

    #[test]
    fn category_rule_auto_overrides_low_autonomy() {
        let mut rules = ApprovalConfig::default();
        rules.file_write = ApprovalRule::Auto;
        let state = AutonomyState::new(AutonomyLevel::Low, rules);
        // file_write is Auto, so even Low autonomy won't ask
        assert!(!state.needs_approval(ActionCategory::FileWrite));
    }

    #[test]
    fn classify_exec_command() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "ls -la /tmp"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::CommandExec));
        assert!(!cats.contains(&ActionCategory::Destructive));
    }

    #[test]
    fn classify_destructive_rm() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "rm -rf /tmp/test"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::CommandExec));
        assert!(cats.contains(&ActionCategory::Destructive));
    }

    #[test]
    fn classify_network_curl() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "curl https://example.com"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::NetworkRequest));
    }

    #[test]
    fn classify_file_write_redirect() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "echo hello > /tmp/out.txt"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::FileWrite));
    }

    #[test]
    fn classify_edit_file() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "editFile",
            "nonce": 1,
            "file": "/tmp/test.txt"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::FileWrite));
    }

    #[test]
    fn classify_ask_human() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "askHuman",
            "nonce": 1,
            "question": "Which database?"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::HumanInput));
    }

    #[test]
    fn classify_browse() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "browse",
            "nonce": 1,
            "url": "https://example.com"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::NetworkRequest));
    }

    #[test]
    fn classify_inspect_path() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "inspectPath",
            "nonce": 1,
            "path": "/tmp"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::FileRead));
    }

    #[test]
    fn classify_batch_multiple() {
        let json = r#"{"commands":[
            {"function":"execAsAgent","nonce":1,"command":"ls"},
            {"function":"editFile","nonce":2,"file":"/tmp/x"},
            {"function":"askHuman","nonce":3,"question":"ok?"}
        ]}"#;
        let result = classify_batch(json);
        assert_eq!(result.len(), 3);
        assert!(result[0].1.contains(&ActionCategory::CommandExec));
        assert!(result[1].1.contains(&ActionCategory::FileWrite));
        assert!(result[2].1.contains(&ActionCategory::HumanInput));
    }

    #[test]
    fn classify_batch_invalid_json() {
        let result = classify_batch("not json");
        assert!(result.is_empty());
    }

    #[test]
    fn classify_batch_no_commands() {
        let result = classify_batch(r#"{"commands":[]}"#);
        assert!(result.is_empty());
    }

    #[test]
    fn shared_autonomy_default() {
        let state = AutonomyState::default();
        assert_eq!(state.level, AutonomyLevel::Medium);
    }
}
