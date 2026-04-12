use serde::{Deserialize, Serialize};
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
    /// Spawning an untrusted live audio sub-agent.
    LiveAudioSpawn,
    /// Accessing the user's session display (screenshot, mouse, keyboard).
    DisplayControl,
}

impl ActionCategory {
    /// Return a severity score for display priority ordering.
    /// Higher = more severe. Used to show the most important category
    /// in approval prompts when multiple categories apply.
    pub fn severity(self) -> u8 {
        match self {
            Self::FileRead => 0,
            Self::CommandExec => 1,
            Self::NetworkRequest => 2,
            Self::FileWrite => 3,
            Self::FileDelete => 4,
            Self::Destructive => 5,
            Self::HumanInput => 6,
            Self::LiveAudioSpawn => 7,
            Self::DisplayControl => 8,
        }
    }

    /// Inverse of `Display`: parse the lowercase snake-case category name
    /// back into a variant.  Used by session-log replay to reconstruct
    /// `ApprovalRequired` events from persisted approval rows.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "file_read" => Some(Self::FileRead),
            "file_write" => Some(Self::FileWrite),
            "file_delete" => Some(Self::FileDelete),
            "command_exec" => Some(Self::CommandExec),
            "network" => Some(Self::NetworkRequest),
            "destructive" => Some(Self::Destructive),
            "human_input" => Some(Self::HumanInput),
            "live_audio_spawn" => Some(Self::LiveAudioSpawn),
            "display_control" => Some(Self::DisplayControl),
            _ => None,
        }
    }
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
            Self::LiveAudioSpawn => write!(f, "live_audio_spawn"),
            Self::DisplayControl => write!(f, "display_control"),
        }
    }
}

/// Per-category approval rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default)]
    pub display_control: ApprovalRule,
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
            display_control: ApprovalRule::Ask,
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
            ActionCategory::LiveAudioSpawn => ApprovalRule::Ask, // always ask
            ActionCategory::DisplayControl => self.display_control,
        }
    }
}

/// Combined autonomy state shared between the agent loop and TUI.
#[derive(Debug, Clone)]
pub struct AutonomyState {
    pub level: AutonomyLevel,
    pub rules: ApprovalConfig,
    /// Session-level grant for the user's session display.
    /// Once true, `DisplayControl` actions skip the approval prompt.
    pub user_display_granted: bool,
    /// Command signatures that have been approved this session.
    /// Retries of the same command (e.g. with different display param)
    /// skip the approval prompt.
    pub approved_commands: std::collections::HashSet<String>,
}

impl Default for AutonomyState {
    fn default() -> Self {
        Self {
            level: AutonomyLevel::Medium,
            rules: ApprovalConfig::default(),
            user_display_granted: false,
            approved_commands: std::collections::HashSet::new(),
        }
    }
}

impl AutonomyState {
    pub fn new(level: AutonomyLevel, rules: ApprovalConfig) -> Self {
        Self {
            level,
            rules,
            user_display_granted: false,
            approved_commands: std::collections::HashSet::new(),
        }
    }

    /// Generate a dedup key for a command. Strips nonce and display params
    /// so retries of the same command with different display/nonce are recognized.
    pub fn command_dedup_key(command: &str) -> String {
        // Remove nonce references like $NONCE[N] and display-like params
        let mut key = command.to_string();
        // Strip --display=N, display:N patterns
        key = key.replace(char::is_whitespace, " ");
        // Remove nonce references
        while let Some(start) = key.find("$NONCE[") {
            if let Some(end) = key[start..].find(']') {
                key.replace_range(start..start + end + 1, "NONCE");
            } else {
                break;
            }
        }
        key
    }

    /// Check if a command was already approved this session.
    pub fn was_command_approved(&self, command: &str) -> bool {
        self.approved_commands.contains(&Self::command_dedup_key(command))
    }

    /// Record a command as approved.
    pub fn record_approved_command(&mut self, command: &str) {
        self.approved_commands.insert(Self::command_dedup_key(command));
    }

    /// Determine whether approval is needed for a given action category.
    /// Returns true if the user must be prompted.
    pub fn needs_approval(&self, category: ActionCategory) -> bool {
        // HumanInput and LiveAudioSpawn always require human regardless of autonomy level
        if category == ActionCategory::HumanInput
            || category == ActionCategory::LiveAudioSpawn
        {
            return true;
        }

        // Full autonomy auto-approves everything except HumanInput
        if self.level == AutonomyLevel::Full {
            return false;
        }

        // DisplayControl: ask on first use, then session-grant takes over
        if category == ActionCategory::DisplayControl {
            return !self.user_display_granted;
        }

        // Low autonomy asks for everything except FileRead (unless Deny overrides)
        if self.level == AutonomyLevel::Low {
            let rule = self.rules.rule_for(category);
            if rule == ApprovalRule::Deny {
                return true;
            }
            return category != ActionCategory::FileRead;
        }

        // Check category-level rule (overrides global level)
        let rule = self.rules.rule_for(category);
        match rule {
            ApprovalRule::Auto => false,
            ApprovalRule::Deny => true, // deny acts like "ask" — will be denied
            ApprovalRule::Ask => {
                // Apply global autonomy level
                match self.level {
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
                    _ => false, // Low and Full handled above
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

    let targets_user_display = cmd
        .get("display")
        .and_then(|d| d.as_i64())
        .map_or(false, |id| id <= 0);

    match function {
        "inspectPath" | "recallMemory" => vec![ActionCategory::FileRead],
        "writeFile" | "editFile" | "storeMemory" => vec![ActionCategory::FileWrite],
        "captureScreen" => {
            let mut cats = vec![ActionCategory::FileRead];
            if targets_user_display {
                cats.push(ActionCategory::DisplayControl);
            }
            cats
        }
        "askHuman" => vec![ActionCategory::HumanInput],
        "browse" => vec![ActionCategory::NetworkRequest],
        "execAsAgent" | "execPty" => {
            let command_str = cmd.get("command").and_then(|c| c.as_str()).unwrap_or("");
            let mut cats = classify_shell_command(command_str);
            if targets_user_display {
                cats.push(ActionCategory::DisplayControl);
            }
            cats
        }
        _ => vec![ActionCategory::CommandExec],
    }
}

/// Split a compound shell command into individual sub-commands.
fn split_shell_commands(cmd: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    for line in cmd.split('\n') {
        // Split on &&, ||, and ; while preserving non-empty segments
        let mut remaining = line;
        while !remaining.is_empty() {
            if let Some(pos) = remaining.find("&&") {
                let part = remaining[..pos].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                remaining = remaining[pos + 2..].trim_start();
            } else if let Some(pos) = remaining.find("||") {
                let part = remaining[..pos].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                remaining = remaining[pos + 2..].trim_start();
            } else if let Some(pos) = remaining.find(';') {
                let part = remaining[..pos].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                remaining = remaining[pos + 1..].trim_start();
            } else {
                let part = remaining.trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                break;
            }
        }
    }
    parts
}

/// Classify a single shell sub-command into action categories.
fn classify_single_command(cmd: &str, categories: &mut Vec<ActionCategory>) {
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
}

/// Classify a shell command string into action categories.
/// Splits compound commands (&&, ||, ;, newlines) and classifies each part.
fn classify_shell_command(cmd: &str) -> Vec<ActionCategory> {
    let mut categories = vec![ActionCategory::CommandExec];
    for sub_cmd in split_shell_commands(cmd) {
        classify_single_command(sub_cmd, &mut categories);
    }
    categories.dedup();
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
    fn low_autonomy_asks_for_everything_except_file_read() {
        let state = AutonomyState::new(AutonomyLevel::Low, ApprovalConfig::default());
        // FileRead is never gated even at Low
        assert!(!state.needs_approval(ActionCategory::FileRead));
        // Everything else needs approval at Low, regardless of Auto rules
        assert!(state.needs_approval(ActionCategory::CommandExec));
        assert!(state.needs_approval(ActionCategory::FileWrite));
        assert!(state.needs_approval(ActionCategory::Destructive));
        assert!(state.needs_approval(ActionCategory::NetworkRequest));
        assert!(state.needs_approval(ActionCategory::FileDelete));
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
    fn low_autonomy_overrides_auto_rules() {
        let mut rules = ApprovalConfig::default();
        rules.file_write = ApprovalRule::Auto;
        let state = AutonomyState::new(AutonomyLevel::Low, rules);
        // Low overrides Auto — still asks for file_write
        assert!(state.needs_approval(ActionCategory::FileWrite));
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
    fn classify_multiline_rm() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "echo hello\nrm -rf /tmp/test"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::CommandExec));
        assert!(cats.contains(&ActionCategory::Destructive));
    }

    #[test]
    fn classify_chained_commands() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "echo hello && rm -rf /tmp/test"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::Destructive));
    }

    #[test]
    fn classify_semicolon_separated() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "echo hello; curl https://example.com"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::NetworkRequest));
    }

    #[test]
    fn classify_or_chain() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "ls /nonexist || rm -rf /tmp/bad"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::Destructive));
    }

    #[test]
    fn classify_bare_rm() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "rm file.txt"
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::Destructive));
    }

    #[test]
    fn shared_autonomy_default() {
        let state = AutonomyState::default();
        assert_eq!(state.level, AutonomyLevel::Medium);
    }

    #[test]
    fn severity_ordering() {
        // Destructive > FileDelete > FileWrite > NetworkRequest > CommandExec > FileRead
        assert!(ActionCategory::Destructive.severity() > ActionCategory::FileDelete.severity());
        assert!(ActionCategory::FileDelete.severity() > ActionCategory::FileWrite.severity());
        assert!(ActionCategory::FileWrite.severity() > ActionCategory::NetworkRequest.severity());
        assert!(ActionCategory::NetworkRequest.severity() > ActionCategory::CommandExec.severity());
        assert!(ActionCategory::CommandExec.severity() > ActionCategory::FileRead.severity());
    }

    #[test]
    fn human_input_highest_severity() {
        assert!(ActionCategory::HumanInput.severity() > ActionCategory::Destructive.severity());
    }

    #[test]
    fn display_control_highest_severity() {
        assert!(
            ActionCategory::DisplayControl.severity()
                > ActionCategory::LiveAudioSpawn.severity()
        );
    }

    #[test]
    fn display_control_category_display() {
        assert_eq!(ActionCategory::DisplayControl.to_string(), "display_control");
    }

    #[test]
    fn display_control_default_rule_is_ask() {
        let config = ApprovalConfig::default();
        assert_eq!(config.display_control, ApprovalRule::Ask);
        assert_eq!(
            config.rule_for(ActionCategory::DisplayControl),
            ApprovalRule::Ask
        );
    }

    #[test]
    fn display_control_needs_approval_when_not_granted() {
        // DisplayControl always needs approval until granted, at every autonomy level
        for level in [
            AutonomyLevel::Low,
            AutonomyLevel::Medium,
            AutonomyLevel::High,
        ] {
            let state = AutonomyState::new(level, ApprovalConfig::default());
            assert!(
                state.needs_approval(ActionCategory::DisplayControl),
                "DisplayControl should need approval at {:?} when not granted",
                level
            );
        }
        // Full autonomy auto-approves everything including DisplayControl
        let full = AutonomyState::new(AutonomyLevel::Full, ApprovalConfig::default());
        assert!(!full.needs_approval(ActionCategory::DisplayControl));
    }

    #[test]
    fn display_control_skips_approval_when_granted() {
        // Once granted, no approval needed at any level
        for level in [
            AutonomyLevel::Low,
            AutonomyLevel::Medium,
            AutonomyLevel::High,
            AutonomyLevel::Full,
        ] {
            let mut state = AutonomyState::new(level, ApprovalConfig::default());
            state.user_display_granted = true;
            assert!(
                !state.needs_approval(ActionCategory::DisplayControl),
                "DisplayControl should NOT need approval at {:?} when granted",
                level
            );
        }
    }

    #[test]
    fn classify_capture_screen_user_display() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "captureScreen",
            "nonce": 1,
            "display": 0
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::DisplayControl));
        assert!(cats.contains(&ActionCategory::FileRead));
    }

    #[test]
    fn classify_capture_screen_virtual_display() {
        // display: 99 should NOT trigger DisplayControl
        let cmd: serde_json::Value = serde_json::json!({
            "function": "captureScreen",
            "nonce": 1,
            "display": 99
        });
        let cats = classify_command(&cmd);
        assert!(!cats.contains(&ActionCategory::DisplayControl));
        assert!(cats.contains(&ActionCategory::FileRead));
    }

    #[test]
    fn classify_exec_user_display() {
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "xdotool key Return",
            "display": 0
        });
        let cats = classify_command(&cmd);
        assert!(cats.contains(&ActionCategory::DisplayControl));
        assert!(cats.contains(&ActionCategory::CommandExec));
    }

    #[test]
    fn classify_exec_no_display_no_control() {
        // No display field → no DisplayControl
        let cmd: serde_json::Value = serde_json::json!({
            "function": "execAsAgent",
            "nonce": 1,
            "command": "echo hello"
        });
        let cats = classify_command(&cmd);
        assert!(!cats.contains(&ActionCategory::DisplayControl));
    }
}
