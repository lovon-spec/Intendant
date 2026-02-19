use crate::agent::Agent;
use crate::error::AgentError;
use crate::models::AgentInput;
use crate::status_monitor::StatusMonitor;
use std::io::{self, Read, Write};

mod agent;
mod error;
mod models;
mod status_monitor;
mod utils;

/// Write a line to stdout, returning false on broken pipe (caller killed us).
fn write_line(stdout: &mut io::StdoutLock, line: &str) -> bool {
    writeln!(stdout, "{}", line).is_ok() && stdout.flush().is_ok()
}

#[tokio::main]
async fn main() -> Result<(), AgentError> {
    // Initialize logging
    env_logger::init();

    // Create agent instance
    let agent = Agent::new()?;

    // Create and start status monitor
    let (monitor, mut status_rx) =
        StatusMonitor::new(agent.shared_mem.clone(), agent.process_map.clone());

    // Spawn status monitor task
    tokio::spawn(async move {
        monitor.run().await;
    });

    // Read entire JSON input
    let mut buffer = String::new();
    io::stdin().read_to_string(&mut buffer)?;

    // Parse single JSON input
    let input: AgentInput = match serde_json::from_str(&buffer) {
        Ok(input) => input,
        Err(e) => {
            eprintln!("JSON parse error: {}", e);
            eprintln!("Input was: {}", buffer);
            return Err(AgentError::Json(e));
        }
    };

    // Process commands and get initial results
    let results = agent.process_input(input).await?;

    // Print initial results; exit gracefully on broken pipe (caller killed us)
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    for result in results {
        if !write_line(&mut stdout, &result) {
            return Ok(());
        }
    }

    // Continue monitoring for status updates
    while let Some(status) = status_rx.recv().await {
        if !write_line(&mut stdout, &status) {
            return Ok(());
        }
    }

    Ok(())
}
