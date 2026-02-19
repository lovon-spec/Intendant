use std::io;
use thiserror::Error;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_display() {
        let err = AgentError::Io(io::Error::new(io::ErrorKind::NotFound, "file not found"));
        let msg = format!("{}", err);
        assert!(msg.contains("IO error"));
        assert!(msg.contains("file not found"));
    }

    #[test]
    fn process_error_display() {
        let err = AgentError::Process("something went wrong".to_string());
        assert_eq!(format!("{}", err), "Process error: something went wrong");
    }

    #[test]
    fn invalid_nonce_display() {
        let err = AgentError::InvalidNonce(42);
        assert_eq!(format!("{}", err), "Invalid nonce: 42");
    }

    #[test]
    fn from_io_error() {
        let io_err = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        let agent_err: AgentError = io_err.into();
        match agent_err {
            AgentError::Io(e) => assert_eq!(e.kind(), io::ErrorKind::PermissionDenied),
            _ => panic!("expected Io variant"),
        }
    }

    #[test]
    fn from_json_error() {
        let json_err = serde_json::from_str::<String>("not json").unwrap_err();
        let agent_err: AgentError = json_err.into();
        match agent_err {
            AgentError::Json(_) => {}
            _ => panic!("expected Json variant"),
        }
    }
}
