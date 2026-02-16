mod agent_runner;
mod conversation;
mod error;
mod openai;

use conversation::Conversation;
use error::CallerError;
use openai::OpenAIClient;
use std::env;
use std::io::{self, BufRead, Write};

const MAX_TURNS: usize = 50;

fn extract_json(text: &str) -> Option<&str> {
    // Try to find JSON in ```json code fences
    if let Some(start) = text.find("```json") {
        let json_start = start + 7;
        if let Some(end) = text[json_start..].find("```") {
            let candidate = text[json_start..json_start + end].trim();
            if candidate.starts_with('{') {
                return Some(candidate);
            }
        }
    }

    // Try generic code fences
    if let Some(start) = text.find("```") {
        let after_fence = start + 3;
        let json_start = if let Some(nl) = text[after_fence..].find('\n') {
            after_fence + nl + 1
        } else {
            after_fence
        };
        if let Some(end) = text[json_start..].find("```") {
            let candidate = text[json_start..json_start + end].trim();
            if candidate.starts_with('{') {
                return Some(candidate);
            }
        }
    }

    // Try bare JSON - find first { and last }
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            if end > start {
                let candidate = &text[start..=end];
                if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

#[tokio::main]
async fn main() -> Result<(), CallerError> {
    dotenvy::dotenv().ok();

    let api_key = env::var("OPENAI_API_KEY")
        .or_else(|_| env::var("OPENAI"))
        .map_err(|_| CallerError::Config("OPENAI_API_KEY or OPENAI env var required".to_string()))?;

    let model = env::var("MODEL_NAME").unwrap_or_else(|_| "gpt-4o".to_string());

    let system_prompt = std::fs::read_to_string("SysPrompt.md")
        .map_err(|e| CallerError::Config(format!("Failed to read SysPrompt.md: {}", e)))?;

    // Get task from CLI args or interactive prompt
    let task = if env::args().len() > 1 {
        env::args().skip(1).collect::<Vec<_>>().join(" ")
    } else {
        print!("Enter task: ");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        line.trim().to_string()
    };

    if task.is_empty() {
        return Err(CallerError::Config("No task provided".to_string()));
    }

    let client = OpenAIClient::new(api_key, model);
    let mut conversation = Conversation::new(system_prompt);
    conversation.add_user(task.clone());

    println!("Task: {}", task);
    println!("---");

    for turn in 1..=MAX_TURNS {
        println!("[Turn {}/{}] Sending to model...", turn, MAX_TURNS);

        let response = client.chat(conversation.messages()).await?;
        conversation.add_assistant(response.clone());

        println!("Model response:\n{}", response);
        println!();

        // Extract JSON from response
        let json_str = match extract_json(&response) {
            Some(json) => json.to_string(),
            None => {
                println!("--- Task complete ---");
                break;
            }
        };

        println!("[Turn {}] Running agent...", turn);
        let output = agent_runner::run_agent(&json_str).await?;

        println!("Agent stdout:\n{}", output.stdout);
        if !output.stderr.is_empty() {
            eprintln!("Agent stderr:\n{}", output.stderr);
        }

        // Format agent output as next user message
        let mut user_msg = format!("Agent output:\n{}", output.stdout);
        if !output.stderr.is_empty() {
            user_msg.push_str(&format!("\nStderr:\n{}", output.stderr));
        }
        conversation.add_user(user_msg);

        if turn == MAX_TURNS {
            println!("--- Max turns ({}) reached ---", MAX_TURNS);
        }
    }

    Ok(())
}
