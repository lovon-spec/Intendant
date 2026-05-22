use std::time::{SystemTime, UNIX_EPOCH};

#[allow(dead_code)]
pub fn get_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Select the shell program and argument vector that runs an agent command
/// string as a single shell invocation (the non-PTY `execAsAgent` path).
///
/// The agent passes a free-form command line (`echo foo && bar | baz`) that
/// must be interpreted by a shell, not exec'd directly. We hand the *whole*
/// string to the platform shell's "run this command line" flag so quoting,
/// pipes, redirection, and `&&`/`;` chaining keep working exactly as the model
/// expects:
///
/// - **Unix**: `bash -c "<command>"` — unchanged from the original hard-coded
///   `Command::new("bash").arg("-c").arg(cmd)`, so behavior is byte-for-byte
///   identical (same shell, same single-string argument, same word-splitting).
/// - **Windows**: `cmd.exe /C "<command>"`. `cmd.exe` is always present on a
///   stock Windows host (unlike bash) and `/C` carries the entire remaining
///   command line through to the interpreter. The Rust `std`/`tokio` Windows
///   `CreateProcess` argument joiner already applies the documented cmd-style
///   quoting when it reassembles `args` into a command line, so a command
///   string containing spaces or quotes is passed through intact rather than
///   being re-split.
///
/// Returns `(program, args)`; the caller builds the `Command`, sets cwd/env,
/// and wires stdio so that the exec semantics (working dir, env scrubbing,
/// stdout/stderr capture, exit code, log files) stay identical across both
/// arms.
pub fn agent_shell_command(command: &str) -> (&'static str, Vec<String>) {
    #[cfg(windows)]
    {
        ("cmd.exe", vec!["/C".to_string(), command.to_string()])
    }
    #[cfg(not(windows))]
    {
        ("bash", vec!["-c".to_string(), command.to_string()])
    }
}

/// Select the interactive shell program and argument vector for a PTY-backed
/// session (the `execPty` path in the runtime).
///
/// Unlike [`agent_shell_command`], a PTY shell is spawned *without* a command
/// argument: the runtime then writes command lines into the PTY's stdin and
/// scrapes output between sentinel markers. So this returns the shell to run
/// interactively plus the flags that suppress per-user startup files (keeping
/// the prompt and environment predictable for marker scraping):
///
/// - **Unix**: `bash --norc --noprofile` — unchanged from the original
///   hard-coded `PtyCommandBuilder::new("bash")` + `args(["--norc",
///   "--noprofile"])`.
/// - **Windows**: `powershell.exe -NoLogo -NoProfile` (PowerShell ships on
///   every supported Windows release; `cmd.exe` is the fallback if PowerShell
///   is unavailable — see [`pty_shell_fallback`]). `-NoProfile` is the
///   analogue of `--norc --noprofile`: it skips profile scripts so the prompt
///   and env are deterministic.
///
/// Returns `(program, args)`.
pub fn pty_shell_command() -> (&'static str, Vec<String>) {
    #[cfg(windows)]
    {
        (
            "powershell.exe",
            vec!["-NoLogo".to_string(), "-NoProfile".to_string()],
        )
    }
    #[cfg(not(windows))]
    {
        (
            "bash",
            vec!["--norc".to_string(), "--noprofile".to_string()],
        )
    }
}

/// Windows-only fallback PTY shell, used when [`pty_shell_command`]'s primary
/// choice (`powershell.exe`) cannot be spawned. `cmd.exe` is guaranteed to be
/// present on every Windows host. Returns `None` on non-Windows because the
/// Unix primary (`bash`) has no separate fallback — its absence is a genuine
/// configuration error there, not a routine condition.
#[allow(dead_code)]
pub fn pty_shell_fallback() -> Option<(&'static str, Vec<String>)> {
    #[cfg(windows)]
    {
        Some(("cmd.exe", Vec::new()))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Line terminator to write into a PTY to *submit* a typed command line.
///
/// A PTY models a terminal, and the byte that means "Enter was pressed" is the
/// carriage return `\r` (0x0D), not the newline `\n` (0x0A). On Unix the line
/// discipline maps the incoming `\r` to `\n` for the shell, and historically
/// the runtime wrote `\n` directly, which bash also accepts — so `\n` worked
/// there. Windows ConPTY does *not* perform that translation: cmd.exe and
/// PowerShell only treat `\r` as line submission, and a `\n` leaves the
/// injected command unsubmitted (the shell just keeps buffering). Use `\r` on
/// Windows; keep `\n` on Unix to preserve the exact pre-existing byte stream.
pub fn pty_line_ending() -> &'static str {
    #[cfg(windows)]
    {
        "\r"
    }
    #[cfg(not(windows))]
    {
        "\n"
    }
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
    fn agent_shell_command_passes_whole_command_string() {
        let (program, args) = agent_shell_command("echo a && echo b");
        // The command must arrive as a single argument so the shell — not the
        // process spawner — does the word-splitting.
        assert_eq!(args.last().map(String::as_str), Some("echo a && echo b"));
        #[cfg(windows)]
        {
            assert_eq!(program, "cmd.exe");
            assert_eq!(args[0], "/C");
        }
        #[cfg(not(windows))]
        {
            assert_eq!(program, "bash");
            assert_eq!(args[0], "-c");
        }
    }

    #[test]
    fn pty_shell_command_suppresses_startup_files() {
        let (program, args) = pty_shell_command();
        #[cfg(windows)]
        {
            assert_eq!(program, "powershell.exe");
            assert!(args.iter().any(|a| a == "-NoProfile"));
        }
        #[cfg(not(windows))]
        {
            assert_eq!(program, "bash");
            assert!(args.iter().any(|a| a == "--norc"));
            assert!(args.iter().any(|a| a == "--noprofile"));
        }
    }

    #[test]
    fn pty_shell_fallback_is_windows_only() {
        #[cfg(windows)]
        {
            let (program, _) = pty_shell_fallback().expect("windows has a fallback");
            assert_eq!(program, "cmd.exe");
        }
        #[cfg(not(windows))]
        {
            assert!(pty_shell_fallback().is_none());
        }
    }

    #[test]
    fn pty_line_ending_is_cr_on_windows_lf_elsewhere() {
        #[cfg(windows)]
        assert_eq!(pty_line_ending(), "\r");
        #[cfg(not(windows))]
        assert_eq!(pty_line_ending(), "\n");
    }
}
