use std::time::{SystemTime, UNIX_EPOCH};

pub fn get_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn format_status_output(nonce: u64, status: char, exit_code: i32) -> String {
    format!("{}{}{}", nonce, status, exit_code)
}
