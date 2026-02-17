use std::fmt;

#[derive(Debug)]
pub enum CallerError {
    Provider(String),
    Agent(String),
    SubAgent(String),
    Io(std::io::Error),
    Json(serde_json::Error),
    Http(reqwest::Error),
    Config(String),
    Toml(String),
}

impl fmt::Display for CallerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CallerError::Provider(msg) => write!(f, "Provider error: {}", msg),
            CallerError::Agent(msg) => write!(f, "Agent error: {}", msg),
            CallerError::SubAgent(msg) => write!(f, "Sub-agent error: {}", msg),
            CallerError::Io(e) => write!(f, "IO error: {}", e),
            CallerError::Json(e) => write!(f, "JSON error: {}", e),
            CallerError::Http(e) => write!(f, "HTTP error: {}", e),
            CallerError::Config(msg) => write!(f, "Config error: {}", msg),
            CallerError::Toml(msg) => write!(f, "TOML error: {}", msg),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_error_display() {
        let err = CallerError::Provider("rate limited".to_string());
        assert_eq!(format!("{}", err), "Provider error: rate limited");
    }

    #[test]
    fn agent_error_display() {
        let err = CallerError::Agent("spawn failed".to_string());
        assert_eq!(format!("{}", err), "Agent error: spawn failed");
    }

    #[test]
    fn sub_agent_error_display() {
        let err = CallerError::SubAgent("spawn failed".to_string());
        assert_eq!(format!("{}", err), "Sub-agent error: spawn failed");
    }

    #[test]
    fn config_error_display() {
        let err = CallerError::Config("missing key".to_string());
        assert_eq!(format!("{}", err), "Config error: missing key");
    }

    #[test]
    fn toml_error_display() {
        let err = CallerError::Toml("invalid syntax".to_string());
        assert_eq!(format!("{}", err), "TOML error: invalid syntax");
    }

    #[test]
    fn io_error_display() {
        let err = CallerError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        ));
        let msg = format!("{}", err);
        assert!(msg.contains("IO error"));
    }

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken");
        let caller_err: CallerError = io_err.into();
        match caller_err {
            CallerError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe),
            _ => panic!("expected Io variant"),
        }
    }

    #[test]
    fn from_json_error() {
        let json_err = serde_json::from_str::<String>("bad").unwrap_err();
        let caller_err: CallerError = json_err.into();
        match caller_err {
            CallerError::Json(_) => {}
            _ => panic!("expected Json variant"),
        }
    }

    #[test]
    fn error_is_std_error() {
        let err = CallerError::Config("test".to_string());
        let _: &dyn std::error::Error = &err;
    }
}
