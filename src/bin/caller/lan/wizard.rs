//! Interactive prompts for optional inputs during `intendant lan setup`.
//!
//! Intentionally minimal — most flags are passed on the command line.
//! This module only provides helpers for reading lines from stdin with
//! a default, used when a required input isn't on the command line.

use std::io::{self, BufRead, Write};

use super::{LanError, LanResult};

#[allow(dead_code)]
pub fn prompt(msg: &str, default: Option<&str>) -> LanResult<String> {
    let suffix = default.map(|d| format!(" [{d}]")).unwrap_or_default();
    print!("  {msg}{suffix}: ");
    io::stdout().flush().map_err(LanError::from)?;
    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line).map_err(LanError::from)?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        match default {
            Some(d) => Ok(d.to_string()),
            None => Err(LanError("input required".into())),
        }
    } else {
        Ok(trimmed.to_string())
    }
}
