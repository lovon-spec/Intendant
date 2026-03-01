#![allow(dead_code)]

use std::time::{SystemTime, UNIX_EPOCH};

pub fn get_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
}
