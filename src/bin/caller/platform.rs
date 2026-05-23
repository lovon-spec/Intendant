//! Cross-platform process and system utilities.
//!
//! Replaces Linux-specific `/proc` filesystem access with POSIX-compatible
//! implementations that work on both Linux and macOS.

/// Ensure platform tool directories are in PATH.
///
/// On macOS, Homebrew installs to `/opt/homebrew/bin` (Apple Silicon) or
/// `/usr/local/bin` (Intel), but these may not be in PATH when launched
/// from SSH, launchd, or other non-login contexts. This ensures tools
/// like ffmpeg, cliclick, and wasm-pack are discoverable.
pub fn ensure_tool_paths() {
    #[cfg(target_os = "macos")]
    {
        let path = std::env::var("PATH").unwrap_or_default();
        let mut additions = Vec::new();
        for dir in &["/opt/homebrew/bin", "/usr/local/bin"] {
            if !path.contains(dir) && std::path::Path::new(dir).is_dir() {
                additions.push(*dir);
            }
        }
        if !additions.is_empty() {
            let new_path = format!("{}:{}", additions.join(":"), path);
            std::env::set_var("PATH", &new_path);
        }
    }
}

/// Check whether a process with the given PID is currently running.
///
/// Uses POSIX `kill(pid, 0)` which checks process existence without
/// sending a signal.
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    // pid_t is i32; values > i32::MAX overflow to negative which have
    // special semantics in kill() (e.g. -1 = all processes). Reject them.
    let pid = match libc::pid_t::try_from(pid) {
        Ok(p) if p > 0 => p,
        _ => return false,
    };
    // Safety: kill with signal 0 is a standard POSIX existence check.
    let ret = unsafe { libc::kill(pid, 0) };
    if ret == 0 {
        return true;
    }
    // EPERM means the process exists but we can't signal it — still alive
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Check whether a process with the given PID is currently running.
///
/// Windows has no `kill(pid, 0)` equivalent, so we `OpenProcess` for the
/// minimal `PROCESS_QUERY_LIMITED_INFORMATION` right and ask for the
/// exit code: a live process reports `STILL_ACTIVE` (259), an exited one
/// reports its real code. A failed `OpenProcess` (handle null) means the
/// PID isn't a process we can see — treated as not alive.
///
/// Caveat shared with the POSIX path: a recently-exited PID can be
/// reused, and the rare process that legitimately exits with code 259 is
/// indistinguishable from a running one. Both are acceptable for the
/// liveness heuristics this powers (stale-session / orphan detection).
#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // Safety: OpenProcess with a query-only access right is a read-only
    // probe; we always close the handle if one was returned.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code) != 0;
        CloseHandle(handle);
        // STILL_ACTIVE (259) == process has not exited. If the query
        // itself failed, fall back to "exists" — the handle opened, so
        // there is a process there.
        !ok || code == STILL_ACTIVE as u32
    }
}

/// Read the command line of a process by PID.
///
/// Returns the full command line as a single string (arguments separated
/// by spaces), or `None` if the process doesn't exist or can't be read.
#[cfg(target_os = "linux")]
pub fn process_cmdline(pid: u32) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{}/cmdline", pid)).ok()?;
    if bytes.is_empty() {
        return None;
    }
    // /proc/pid/cmdline is NUL-separated; replace NULs with spaces
    Some(String::from_utf8_lossy(&bytes).replace('\0', " "))
}

/// Read the command line of a process by PID.
///
/// Uses `sysctl(KERN_PROCARGS2)` on macOS.
#[cfg(target_os = "macos")]
pub fn process_cmdline(pid: u32) -> Option<String> {
    let mut mib: [libc::c_int; 3] = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];

    // First call: get buffer size
    let mut size: libc::size_t = 0;
    let ret = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 || size == 0 {
        return None;
    }

    // Second call: read the data
    let mut buf: Vec<u8> = vec![0u8; size];
    let ret = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 {
        return None;
    }
    buf.truncate(size);

    // KERN_PROCARGS2 layout: argc (i32) | exec_path\0 | padding\0* | argv[0]\0 argv[1]\0 ...
    if buf.len() < std::mem::size_of::<i32>() {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;

    let rest = &buf[4..];
    // Skip executable path (first NUL-terminated string)
    let exec_end = rest.iter().position(|&b| b == 0)?;
    let mut pos = exec_end;
    // Skip NUL padding between exec path and argv
    while pos < rest.len() && rest[pos] == 0 {
        pos += 1;
    }

    // Collect argc arguments
    let args: Vec<&str> = rest[pos..]
        .split(|&b| b == 0)
        .take(argc)
        .filter_map(|s| std::str::from_utf8(s).ok())
        .filter(|s| !s.is_empty())
        .collect();

    if args.is_empty() {
        None
    } else {
        Some(args.join(" "))
    }
}

/// Read the command line of a process by PID.
///
/// Windows has no `/proc` and no `KERN_PROCARGS2`. We take a two-tier,
/// best-effort approach:
///
/// 1. **Full command line** via `NtQueryInformationProcess` with the
///    `ProcessCommandLineInformation` class (Windows 8.1+). This returns
///    the *exact* command line the process was launched with — the closest
///    analogue to Linux `/proc/<pid>/cmdline`. The result is a
///    `UNICODE_STRING` header immediately followed by the UTF-16 buffer it
///    points at, so we copy the whole blob and read the buffer at its
///    offset. We use the standard two-call size-probe: the first call
///    returns `STATUS_INFO_LENGTH_MISMATCH` with the needed length.
/// 2. **Executable path** via `QueryFullProcessImageNameW` as a fallback
///    when the NT call is unavailable (pre-8.1) or denied. This loses the
///    arguments but the full image path is still strictly more useful than
///    `None` for the liveness / process-identification heuristics this
///    powers.
///
/// Both tiers open the process with only `PROCESS_QUERY_LIMITED_INFORMATION`
/// (the same minimal right [`process_alive`] uses), so this works against
/// other users' processes that we'd be allowed to query at all without
/// needing `PROCESS_VM_READ`. Returns `None` only if the PID can't be opened
/// or both queries fail.
///
/// Limitation: `ProcessCommandLineInformation` is a documented-but-NT
/// information class — it's stable across all supported Windows releases
/// (8.1 and up, which covers every target this port runs on) but is not a
/// kernel32 export, hence the ntdll path. The exe-path fallback keeps the
/// function useful even where the NT class is refused.
#[cfg(windows)]
pub fn process_cmdline(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // Safety: every raw pointer below is into a buffer we own and size
    // ourselves; the handle is always closed before returning.
    unsafe {
        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }

        let result = query_cmdline_nt(handle).or_else(|| query_image_path(handle));

        CloseHandle(handle);
        result
    }
}

/// Tier 1: full command line via `NtQueryInformationProcess`.
///
/// # Safety
/// `handle` must be a live process handle opened with at least
/// `PROCESS_QUERY_LIMITED_INFORMATION`.
#[cfg(windows)]
unsafe fn query_cmdline_nt(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<String> {
    use windows_sys::Wdk::System::Threading::{
        NtQueryInformationProcess, ProcessCommandLineInformation,
    };
    use windows_sys::Win32::Foundation::{STATUS_INFO_LENGTH_MISMATCH, UNICODE_STRING};

    // First call: probe the required buffer length. Passing a zero-length
    // buffer makes ntdll report the size via `ret_len` and return
    // STATUS_INFO_LENGTH_MISMATCH.
    let mut needed: u32 = 0;
    let status = NtQueryInformationProcess(
        handle,
        ProcessCommandLineInformation,
        std::ptr::null_mut(),
        0,
        &mut needed,
    );
    // Any status other than the length-mismatch sentinel (e.g. invalid info
    // class on a pre-8.1 kernel, or access denied) means we can't use this
    // path — let the caller fall back to the image path.
    if status != STATUS_INFO_LENGTH_MISMATCH
        || (needed as usize) < std::mem::size_of::<UNICODE_STRING>()
    {
        return None;
    }

    // Allocate a u16-aligned buffer so the trailing UTF-16 command-line text
    // (which the UNICODE_STRING.Buffer points into) is correctly aligned.
    let cap_u16 = (needed as usize).div_ceil(std::mem::size_of::<u16>());
    let mut buf: Vec<u16> = vec![0u16; cap_u16];
    let byte_cap = (cap_u16 * std::mem::size_of::<u16>()) as u32;

    let mut written: u32 = 0;
    let status = NtQueryInformationProcess(
        handle,
        ProcessCommandLineInformation,
        buf.as_mut_ptr() as *mut core::ffi::c_void,
        byte_cap,
        &mut written,
    );
    // NTSTATUS success codes are >= 0 (the sign bit flags errors).
    if status < 0 {
        return None;
    }

    // The blob begins with a UNICODE_STRING whose Buffer points somewhere
    // inside the same allocation (right after the header). Read the header
    // by copy to avoid an unaligned struct reference into the u16 buffer.
    let header = std::ptr::read_unaligned(buf.as_ptr() as *const UNICODE_STRING);
    let len_bytes = header.Length as usize;
    if len_bytes == 0 || header.Buffer.is_null() {
        // Empty command line is possible but useless; treat as "no cmdline"
        // so the caller falls through to the image path.
        return None;
    }

    // `Length` is in bytes; the text is `Length / 2` UTF-16 code units at
    // `Buffer`. Buffer lies within our own allocation, so reading
    // `len_bytes` from it is in-bounds.
    let units = len_bytes / std::mem::size_of::<u16>();
    let slice = std::slice::from_raw_parts(header.Buffer as *const u16, units);
    let s = String::from_utf16_lossy(slice);
    let s = s.trim_end_matches('\0').to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Tier 2 fallback: full executable image path via
/// `QueryFullProcessImageNameW`. Arguments are lost, but the path alone is
/// more useful than nothing.
///
/// # Safety
/// `handle` must be a live process handle opened with at least
/// `PROCESS_QUERY_LIMITED_INFORMATION`.
#[cfg(windows)]
unsafe fn query_image_path(handle: windows_sys::Win32::Foundation::HANDLE) -> Option<String> {
    use windows_sys::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_WIN32};

    // MAX_PATH is a soft limit on Windows; long paths can exceed it, so size
    // generously and let the call report the actual length back.
    let mut buf: Vec<u16> = vec![0u16; 4096];
    let mut size: u32 = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut size);
    if ok == 0 || size == 0 {
        return None;
    }
    // On success `size` is the length in code units, excluding the NUL.
    let s = String::from_utf16_lossy(&buf[..size as usize]);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Get the UID of the current process. POSIX `getuid()`.
#[cfg(unix)]
pub fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

/// Get the UID of the current process.
///
/// Windows has no numeric UID model (it uses SIDs). Tier-0 returns 0 —
/// the only caller is the control-socket peer-credential check, which is
/// itself `#[cfg(unix)]`-gated, so this value is never compared against a
/// real peer UID on Windows.
#[cfg(windows)]
pub fn current_uid() -> u32 {
    0
}

/// Select the interactive shell program and argument vector for the web
/// terminal's PTY-backed session.
///
/// The web terminal spawns a *login-style interactive* shell (the user types
/// into it via xterm.js), so it wants the user's full environment (PATH,
/// aliases, prompt) set up — the opposite of the runtime's marker-scraping PTY
/// which suppresses startup files.
///
/// - **Unix**: `$SHELL -l` (falling back to `/bin/bash -l`) — unchanged from
///   the original hard-coded behavior. `-l` loads the login-time environment.
/// - **Windows**: `powershell.exe -NoLogo` (profile *enabled* so the user's
///   PATH/prompt are configured, the Windows analogue of `-l`). There is no
///   `$SHELL` convention on Windows. `cmd.exe` is the fallback when PowerShell
///   can't be launched — see [`interactive_pty_shell_fallback`].
///
/// Returns `(program, args)`. The caller wires cwd, the `TERM` env, and stdio.
#[allow(dead_code)]
pub fn interactive_pty_shell() -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        ("powershell.exe".to_string(), vec!["-NoLogo".to_string()])
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        (shell, vec!["-l".to_string()])
    }
}

/// Windows-only fallback for [`interactive_pty_shell`]: `cmd.exe`, which is
/// always present. `None` on non-Windows (the Unix `$SHELL`/`bash` primary has
/// no routine fallback).
#[allow(dead_code)]
pub fn interactive_pty_shell_fallback() -> Option<(String, Vec<String>)> {
    #[cfg(windows)]
    {
        Some(("cmd.exe".to_string(), Vec::new()))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Launch a detached controller-restart command and return its PID.
///
/// The restarted controller must outlive the process that spawns it (the
/// current controller is about to exit / `exec()`), so it is started in its
/// own process group/session with stdio detached.
///
/// - **Unix**: unchanged — `nohup setsid bash -lc "$cmd"` (or `nohup bash -lc`
///   when `setsid` is absent), with the command passed via the
///   `INTENDANT_RESTART_COMMAND` env var so no shell-quoting of the
///   user-supplied command is needed. Returns the backgrounded PID via
///   `echo $!`.
/// - **Windows**: spawn a detached, window-less child via the Win32
///   `CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`
///   creation flags (the documented analogue of `nohup`/`setsid`: no console
///   window, detached from the parent's console, own process group so a
///   Ctrl-Break to the parent group won't reach it). The command runs under
///   `cmd.exe /C` and the child's PID is returned directly from the spawn — no
///   `echo $!` round-trip.
#[cfg(not(windows))]
pub async fn spawn_detached_restart(cmd: &str) -> Result<u32, String> {
    use std::process::Stdio;
    use tokio::process::Command;

    // Use setsid when available to separate process group/session so parent
    // shutdown doesn't tear down the restarted controller process.
    let wrapper = r#"
if command -v setsid >/dev/null 2>&1; then
  nohup setsid bash -lc "$INTENDANT_RESTART_COMMAND" </dev/null >/dev/null 2>&1 &
else
  nohup bash -lc "$INTENDANT_RESTART_COMMAND" </dev/null >/dev/null 2>&1 &
fi
echo $!
"#;

    let output = Command::new("bash")
        .args(["-lc", wrapper])
        .env("INTENDANT_RESTART_COMMAND", cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("Failed to launch detached restart command: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "Failed to launch detached restart command (exit={})",
            output.status
        ));
    }

    let pid_text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    pid_text
        .parse::<u32>()
        .map_err(|e| format!("Failed to parse detached restart pid '{}': {}", pid_text, e))
}

/// Windows implementation of [`spawn_detached_restart`]. See the non-Windows
/// doc comment for the cross-platform contract.
#[cfg(windows)]
pub async fn spawn_detached_restart(cmd: &str) -> Result<u32, String> {
    use std::process::Stdio;
    // `creation_flags` is an inherent method on tokio's Command (it mirrors
    // the std `CommandExt` Windows extension), so no trait import is needed.
    use tokio::process::Command;

    // Win32 process-creation flags (values from winbase.h, stable ABI):
    //   CREATE_NO_WINDOW          0x08000000 — no console window for the child
    //   DETACHED_PROCESS          0x00000008 — don't inherit the parent console
    //   CREATE_NEW_PROCESS_GROUP  0x00000200 — own group; isolates Ctrl-Break
    // Together these are the Windows analogue of `nohup` + `setsid`.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    // `cmd.exe /C` interprets the restart command line; the command is passed
    // as a single argument so cmd does the splitting (mirrors `bash -lc`).
    let child = Command::new("cmd.exe")
        .args(["/C", cmd])
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to launch detached restart command: {}", e))?;

    child
        .id()
        .ok_or_else(|| "Detached restart child has no PID".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_alive() {
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn dead_pid_is_not_alive() {
        // PID u32::MAX is effectively impossible
        assert!(!process_alive(u32::MAX));
    }

    #[test]
    fn current_uid_returns_value() {
        // Just verify it doesn't panic and returns a sane value
        let uid = current_uid();
        // In test environments, UID is typically > 0 for non-root, or 0 for root
        assert!(uid < 100_000);
    }

    #[test]
    fn cmdline_of_current_process() {
        let cmdline = process_cmdline(std::process::id());
        assert!(cmdline.is_some(), "should be able to read own cmdline");
    }

    #[test]
    fn cmdline_of_dead_pid() {
        assert!(process_cmdline(u32::MAX).is_none());
    }

    #[test]
    fn interactive_pty_shell_picks_platform_shell() {
        let (program, args) = interactive_pty_shell();
        #[cfg(windows)]
        {
            assert_eq!(program, "powershell.exe");
            assert!(args.iter().any(|a| a == "-NoLogo"));
            assert!(interactive_pty_shell_fallback().is_some());
        }
        #[cfg(not(windows))]
        {
            // Defaults to $SHELL or /bin/bash, always a login shell.
            assert!(!program.is_empty());
            assert_eq!(args, vec!["-l".to_string()]);
            assert!(interactive_pty_shell_fallback().is_none());
        }
    }

    #[tokio::test]
    async fn spawn_detached_restart_yields_live_pid() {
        // Long-lived per platform so the PID is still alive when we probe.
        #[cfg(windows)]
        let long_running = "timeout /T 30 /NOBREAK";
        #[cfg(not(windows))]
        let long_running = "sleep 30";

        let pid = spawn_detached_restart(long_running)
            .await
            .expect("detached spawn should succeed");
        assert!(pid > 1);
        assert!(process_alive(pid), "detached child should be alive");

        // Best-effort cleanup.
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F", "/T"])
                .status();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }

    // Windows-specific: the real implementation must yield a non-empty
    // string for our own PID, and (whether it came from the NT command-line
    // class or the QueryFullProcessImageNameW exe-path fallback) it should
    // reference the running test binary — i.e. contain an `.exe` token.
    #[cfg(windows)]
    #[test]
    fn cmdline_of_current_process_is_nonempty_and_exe_like() {
        let cmdline =
            process_cmdline(std::process::id()).expect("own cmdline should be readable on Windows");
        assert!(!cmdline.trim().is_empty(), "cmdline should not be blank");
        assert!(
            cmdline.to_ascii_lowercase().contains(".exe"),
            "cmdline should reference the test executable, got: {cmdline:?}"
        );
    }
}
