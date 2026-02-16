use thiserror::Error;
use std::io;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    
    #[error("Process error: {0}")]
    Process(String),
    
    #[error("Invalid nonce: {0}")]
    InvalidNonce(u64),
}
