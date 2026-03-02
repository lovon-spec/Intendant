use crate::agent::Agent;
use crate::error::AgentError;
use crate::models::AgentInput;
use std::io::{self, Read, Write};

mod agent;
mod error;
mod models;
mod utils;

/// Maximum bytes to read from stdin (64 MB).
const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

#[cfg(target_os = "linux")]
fn apply_sandbox_from_env() -> Result<(), AgentError> {
    use landlock::{AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI};
    use std::path::PathBuf;

    let paths = std::env::var("INTENDANT_SANDBOX_WRITE_PATHS").unwrap_or_default();
    if paths.trim().is_empty() {
        return Ok(());
    }

    let write_paths: Vec<PathBuf> = paths
        .split(':')
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .collect();

    if write_paths.is_empty() {
        return Ok(());
    }

    let abi = ABI::V5;
    let read_access = AccessFs::from_read(abi);
    let write_access = AccessFs::from_read(abi) | AccessFs::from_write(abi);

    let mut ruleset_created = Ruleset::default()
        .handle_access(write_access)
        .map_err(|e| AgentError::Process(format!("Landlock ruleset creation failed: {}", e)))?
        .create()
        .map_err(|e| AgentError::Process(format!("Landlock ruleset create failed: {}", e)))?;

    if let Ok(root_fd) = PathFd::new("/") {
        ruleset_created = ruleset_created
            .add_rule(PathBeneath::new(root_fd, read_access))
            .map_err(|e| AgentError::Process(format!("Landlock add read rule failed: {}", e)))?;
    }

    for path in write_paths {
        if !path.exists() {
            continue;
        }
        if let Ok(fd) = PathFd::new(&path) {
            ruleset_created = ruleset_created
                .add_rule(PathBeneath::new(fd, write_access))
                .map_err(|e| {
                    AgentError::Process(format!("Landlock add write rule failed: {}", e))
                })?;
        }
    }

    let status = ruleset_created
        .restrict_self()
        .map_err(|e| AgentError::Process(format!("Landlock restrict_self failed: {}", e)))?;
    if status.ruleset == landlock::RulesetStatus::NotEnforced {
        eprintln!("Sandbox requested but Landlock not enforced by kernel");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn apply_sandbox_from_env() -> Result<(), AgentError> {
    Ok(())
}

/// Write a line to stdout, returning false on broken pipe (caller killed us).
fn write_line(stdout: &mut io::StdoutLock, line: &str) -> bool {
    writeln!(stdout, "{}", line).is_ok() && stdout.flush().is_ok()
}

#[tokio::main]
async fn main() -> Result<(), AgentError> {
    // Initialize logging
    env_logger::init();

    // Read entire JSON input (bounded)
    let mut buffer = String::new();
    io::stdin()
        .take(MAX_INPUT_BYTES)
        .read_to_string(&mut buffer)?;

    // Parse single JSON input
    let input: AgentInput = match serde_json::from_str(&buffer) {
        Ok(input) => input,
        Err(e) => {
            eprintln!("JSON parse error: {}", e);
            eprintln!("Input was: {}", buffer);
            return Err(AgentError::Json(e));
        }
    };

    // Apply filesystem sandbox before running commands.
    apply_sandbox_from_env()?;

    // Create agent instance
    let agent = Agent::new()?;

    // Process commands sequentially and get results
    let results = agent.process_input(input).await?;

    // Print results; exit gracefully on broken pipe
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    for result in results {
        if !write_line(&mut stdout, &result) {
            return Ok(());
        }
    }

    Ok(())
}
