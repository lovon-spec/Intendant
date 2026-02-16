use std::fmt;

#[derive(Debug)]
pub enum CallerError {
    OpenAI(String),
    Agent(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    Http(reqwest::Error),
    Config(String),
}

impl fmt::Display for CallerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallerError::OpenAI(msg) => write!(f, "OpenAI error: {}", msg),
            CallerError::Agent(msg) => write!(f, "Agent error: {}", msg),
            CallerError::Io(e) => write!(f, "IO error: {}", e),
            CallerError::Json(e) => write!(f, "JSON error: {}", e),
            CallerError::Http(e) => write!(f, "HTTP error: {}", e),
            CallerError::Config(msg) => write!(f, "Config error: {}", msg),
        }
    }
}

impl std::error::Error for CallerError {}

impl From<std::io::Error> for CallerError {
    fn from(e: std::io::Error) -> Self {
        CallerError::Io(e)
    }
}

impl From<serde_json::Error> for CallerError {
    fn from(e: serde_json::Error) -> Self {
        CallerError::Json(e)
    }
}

impl From<reqwest::Error> for CallerError {
    fn from(e: reqwest::Error) -> Self {
        CallerError::Http(e)
    }
}
