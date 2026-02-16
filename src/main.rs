use crate::agent::Agent;
use crate::error::AgentError;
use crate::models::AgentInput;
use crate::status_monitor::StatusMonitor;
use std::io::{self, Write, Read};


mod agent;
mod error;
mod models;
mod status_monitor;
mod utils;

#[tokio::main]
async fn main() -> Result<(), AgentError> {
    // Initialize logging
    env_logger::init();

    // Create agent instance
    let agent = Agent::new()?;
    
    // Create and start status monitor
    let (monitor, mut status_rx) = StatusMonitor::new(
        agent.shared_mem.clone(),
        agent.process_map.clone()
    );
    
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
    
    // Print initial results
    for result in results {
        println!("{}", result);
    }
    io::stdout().flush()?;

    // Continue monitoring for status updates
    while let Some(status) = status_rx.recv().await {
        println!("{}", status);
        io::stdout().flush()?;
    }

    Ok(())
}