//! Cross-platform process and system utilities.
//!
//! Replaces Linux-specific `/proc` filesystem access with POSIX-compatible
//! implementations that work on both Linux and macOS.

/// Check whether a process with the given PID is currently running.
///
/// Uses POSIX `kill(pid, 0)` which checks process existence without
/// sending a signal.
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

/// Get the UID of the current process. POSIX `getuid()`.
pub fn current_uid() -> u32 {
    unsafe { libc::getuid() }
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
}
