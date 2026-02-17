#![allow(dead_code)]

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_reasonable() {
        let ts = get_timestamp();
        // Should be after 2024-01-01 (1704067200)
        assert!(ts > 1704067200, "timestamp {} seems too small", ts);
    }

    #[test]
    fn timestamp_is_monotonic() {
        let ts1 = get_timestamp();
        let ts2 = get_timestamp();
        assert!(ts2 >= ts1);
    }

    #[test]
    fn format_status_output_running() {
        let output = format_status_output(1, 'r', 0);
        assert_eq!(output, "1r0");
    }

    #[test]
    fn format_status_output_completed() {
        let output = format_status_output(42, 'c', 0);
        assert_eq!(output, "42c0");
    }

    #[test]
    fn format_status_output_failed() {
        let output = format_status_output(100, 'f', 1);
        assert_eq!(output, "100f1");
    }

    #[test]
    fn format_status_output_negative_exit_code() {
        let output = format_status_output(7, 'f', -1);
        assert_eq!(output, "7f-1");
    }

    #[test]
    fn format_status_output_large_nonce() {
        let output = format_status_output(999999, 'c', 0);
        assert_eq!(output, "999999c0");
    }
}
