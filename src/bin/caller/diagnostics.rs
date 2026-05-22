//! Tiny diagnostics-side helpers for the Phase 0 visual-freshness scaffold
//! (task #83).
//!
//! Sized for one job: take a session-scoped NDJSON blob from the browser
//! sampler and append it to a per-session file under
//! `~/.intendant/diagnostics/visual-freshness/<session_id>.ndjson`. The
//! browser is responsible for emitting valid NDJSON (one JSON object per
//! `\n`-terminated line); the server is just an authenticated append
//! point. No parsing, no schema validation, no aggregation — that's all
//! browser-side or post-hoc analysis on the transcript file.
//!
//! Lives at module scope so it can grow into a small diagnostics subsystem
//! when Phase 0 Level 2 lands (clock-sync round-trip endpoint) and Phase 1
//! starts (tcpdump trigger endpoints, coturn counter scrapes). For now,
//! one helper trio + boundary tests.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Sanitize a session id from query-string input down to a filesystem-
/// safe slug. Accepts ASCII alphanumerics, `-`, and `_`; everything else
/// is dropped. Returns `None` if the result is empty (which protects the
/// caller from accidentally writing to a bare-`.ndjson` file when the
/// browser forgot the query param).
///
/// Sanitization is conservative on purpose. Real session ids in this
/// codebase are UUIDs (`9c71d8e1-85aa-4b3e-9c6b-209cd9ecc322`) which
/// already pass the filter; pinning the rule means an operator can't
/// silently land traversal sequences (`../`, absolute paths, NUL) in
/// the diagnostic transcript path even if the dashboard were
/// compromised.
pub fn sanitize_session_id(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Resolve the visual-freshness transcript path for `session_id`.
/// Returns `None` when the session id sanitizes to empty (caller should
/// reject the request before reaching the disk).
///
/// Path shape: `<intendant_state_dir>/diagnostics/visual-freshness/<session_id>.ndjson`.
/// `intendant_state_dir` resolves to `$HOME/.intendant` (or `/tmp/.intendant`
/// when `HOME` is unset, matching the convention the session-log writer
/// already uses).
pub fn visual_freshness_path(session_id: &str) -> Option<PathBuf> {
    let slug = sanitize_session_id(session_id)?;
    Some(
        intendant_state_dir()
            .join("diagnostics")
            .join("visual-freshness")
            .join(format!("{slug}.ndjson")),
    )
}

/// Append `body` verbatim to the visual-freshness transcript file for
/// `session_id`. Creates parent directories on first call. The browser
/// sampler is responsible for emitting `\n`-terminated NDJSON records;
/// this function does not add framing. Returns the number of bytes
/// written (== `body.len()` on success).
///
/// Concurrent appends from multiple browsers viewing the same session are
/// serialized by the OS append-mode `O_APPEND` semantics — each `write`
/// call is atomic up to the per-platform pipe-write boundary (PIPE_BUF
/// on POSIX, typically 4 KB). The browser sampler batches its records
/// into single ~5s POSTs of much less than 4 KB so concurrent
/// interleaving is not a practical concern at the smoke-run scale.
pub fn append_visual_freshness_record(session_id: &str, body: &[u8]) -> std::io::Result<usize> {
    let path = visual_freshness_path(session_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session_id sanitizes to empty",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(body)?;
    Ok(body.len())
}

/// Resolve the Intendant state directory (`$HOME/.intendant`) with the
/// same fallback the session-log writer uses (`/tmp/.intendant` when
/// HOME is unset). Pulled into its own helper so test code can override
/// HOME and verify path construction without touching production calls.
fn intendant_state_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    Path::new(&home).join(".intendant")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_uuid_passes_through() {
        let uuid = "9c71d8e1-85aa-4b3e-9c6b-209cd9ecc322";
        assert_eq!(sanitize_session_id(uuid).as_deref(), Some(uuid));
    }

    #[test]
    fn sanitize_strips_path_traversal() {
        assert_eq!(
            sanitize_session_id("../../../etc/passwd").as_deref(),
            Some("etcpasswd"),
        );
        assert_eq!(
            sanitize_session_id("/absolute/path").as_deref(),
            Some("absolutepath"),
        );
        assert_eq!(
            sanitize_session_id("session\0nul").as_deref(),
            Some("sessionnul"),
        );
    }

    #[test]
    fn sanitize_keeps_underscore_and_dash() {
        assert_eq!(
            sanitize_session_id("phase-0_level-1").as_deref(),
            Some("phase-0_level-1"),
        );
    }

    #[test]
    fn sanitize_rejects_empty_after_filter() {
        assert!(sanitize_session_id("").is_none());
        assert!(sanitize_session_id("///").is_none());
        assert!(sanitize_session_id("\0\0").is_none());
        assert!(sanitize_session_id(".").is_none());
    }

    #[test]
    fn visual_freshness_path_uses_intendant_state_dir() {
        let p = visual_freshness_path("abc-123").expect("non-empty after sanitize");
        // Assert on path components rather than a hardcoded '/'-joined
        // string so the test holds on Windows (separator '\\') as well as
        // POSIX. `visual_freshness_path` builds the path with `PathBuf::join`,
        // so it is already platform-correct.
        let expected_tail: PathBuf = ["diagnostics", "visual-freshness", "abc-123.ndjson"]
            .iter()
            .collect();
        assert!(
            p.ends_with(&expected_tail),
            "unexpected path tail: {p:?}"
        );
        assert!(
            p.components()
                .any(|c| c.as_os_str() == std::ffi::OsStr::new(".intendant")),
            "path should be under .intendant state dir: {p:?}"
        );
    }

    #[test]
    fn visual_freshness_path_none_for_empty_session_id() {
        assert!(visual_freshness_path("").is_none());
        assert!(visual_freshness_path("///").is_none());
    }

    #[test]
    fn append_creates_parent_dirs_and_writes_body() {
        // Sandbox HOME to a tempdir so the test doesn't write into the
        // user's real .intendant directory.
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", tmp.path());

        let session_id = "phase0-test-12345";
        let body = b"{\"t\":\"transition\",\"v\":1}\n{\"t\":\"transition\",\"v\":2}\n";
        let written = append_visual_freshness_record(session_id, body).expect("append");
        assert_eq!(written, body.len());

        // File contents should match exactly (one batch).
        let path = visual_freshness_path(session_id).unwrap();
        let read = std::fs::read(&path).expect("read transcript");
        assert_eq!(read, body);

        // A second append should concatenate, not truncate.
        let body2 = b"{\"t\":\"summary\",\"transitions\":2}\n";
        append_visual_freshness_record(session_id, body2).expect("append 2");
        let read2 = std::fs::read(&path).expect("read transcript 2");
        assert_eq!(read2.len(), body.len() + body2.len());
        assert!(read2.starts_with(body));
        assert!(read2.ends_with(body2));

        // Restore HOME.
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn append_rejects_empty_session_id() {
        let result = append_visual_freshness_record("", b"{\"x\":1}\n");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
}
