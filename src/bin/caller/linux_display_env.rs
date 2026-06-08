//! Linux GUI session environment discovery for daemon and managed-agent runs.
//!
//! Intendant is often launched from a tty, SSH session, or a systemd user
//! service while the graphical session is already active. In that shape the
//! process may have no GUI environment even though systemd's user manager does.
//! Adopt only the small set of display/session variables needed for portals,
//! X11 auth, screenshots, and input tools.

#[cfg(target_os = "linux")]
use std::collections::BTreeMap;

#[cfg(target_os = "linux")]
const GUI_ENV_KEYS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "DESKTOP_SESSION",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub struct GuiEnvAdoption {
    pub adopted: Vec<String>,
    pub skipped: Vec<String>,
    pub source: Option<String>,
}

#[cfg(target_os = "linux")]
pub fn ensure_gui_session_env(context: &str) -> GuiEnvAdoption {
    let Some(systemd_env) = read_systemd_user_environment() else {
        return GuiEnvAdoption::default();
    };
    let report = adopt_from_map(&systemd_env);
    if !report.adopted.is_empty() {
        eprintln!(
            "[linux_display_env] {context}: adopted GUI env from {}: {}",
            report.source.as_deref().unwrap_or("systemd --user"),
            report.adopted.join(", ")
        );
    }
    if !report.skipped.is_empty() {
        eprintln!(
            "[linux_display_env] {context}: skipped untrusted GUI env from {}: {}",
            report.source.as_deref().unwrap_or("systemd --user"),
            report.skipped.join(", ")
        );
    }
    report
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub fn ensure_gui_session_env(_context: &str) -> GuiEnvAdoption {
    GuiEnvAdoption::default()
}

#[cfg(target_os = "linux")]
pub fn apply_to_tokio_command(command: &mut tokio::process::Command) {
    ensure_gui_session_env("child process spawn");
    for key in GUI_ENV_KEYS {
        if let Ok(value) = std::env::var(key) {
            command.env(key, value);
        }
    }
    if let Ok(value) = std::env::var("INTENDANT_USER_DISPLAY") {
        command.env("INTENDANT_USER_DISPLAY", value);
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub fn apply_to_tokio_command(_command: &mut tokio::process::Command) {}

#[allow(dead_code)]
pub fn diagnostic_summary() -> String {
    #[cfg(target_os = "linux")]
    {
        let keys = [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XAUTHORITY",
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_SESSION_TYPE",
        ];
        let mut parts = Vec::new();
        for key in keys {
            match std::env::var(key) {
                Ok(value) if !value.is_empty() => {
                    if key == "DBUS_SESSION_BUS_ADDRESS" {
                        parts.push(format!("{key}=set"));
                    } else {
                        parts.push(format!("{key}={value}"));
                    }
                }
                _ => parts.push(format!("{key}=missing")),
            }
        }
        format!(
            "{}. To refresh a GNOME/Wayland shell, run: systemctl --user import-environment DISPLAY WAYLAND_DISPLAY XAUTHORITY XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS XDG_SESSION_TYPE",
            parts.join(", ")
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        "GUI environment adoption is Linux-only".to_string()
    }
}

#[cfg(target_os = "linux")]
fn read_systemd_user_environment() -> Option<BTreeMap<String, String>> {
    let uid = crate::platform::current_uid();
    let runtime_dir = format!("/run/user/{uid}");
    let bus_path = format!("{runtime_dir}/bus");
    let mut cmd = std::process::Command::new("systemctl");
    cmd.args(["--user", "show-environment"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    if std::path::Path::new(&runtime_dir).is_dir() {
        cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    }
    if std::path::Path::new(&bus_path).exists() {
        cmd.env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={bus_path}"));
    }

    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let parsed = parse_systemd_environment(&text);
    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

#[cfg(target_os = "linux")]
fn adopt_from_map(values: &BTreeMap<String, String>) -> GuiEnvAdoption {
    let mut report = GuiEnvAdoption {
        source: Some("systemd --user show-environment".to_string()),
        ..GuiEnvAdoption::default()
    };

    for key in GUI_ENV_KEYS {
        let Some(value) = values.get(*key) else {
            continue;
        };
        if std::env::var_os(key).is_some() {
            continue;
        }
        if trusted_env_value(key, value) {
            std::env::set_var(key, value);
            report.adopted.push((*key).to_string());
        } else {
            report.skipped.push((*key).to_string());
        }
    }

    if std::env::var_os("INTENDANT_USER_DISPLAY").is_none() {
        if let Some(display) = values
            .get("DISPLAY")
            .filter(|v| trusted_env_value("DISPLAY", v))
        {
            std::env::set_var("INTENDANT_USER_DISPLAY", display);
            report.adopted.push("INTENDANT_USER_DISPLAY".to_string());
        }
    }

    report
}

#[cfg(target_os = "linux")]
fn parse_systemd_environment(text: &str) -> BTreeMap<String, String> {
    let mut parsed = BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if GUI_ENV_KEYS.contains(&key) && !value.contains('\0') {
            parsed.insert(key.to_string(), value.to_string());
        }
    }
    parsed
}

#[cfg(target_os = "linux")]
fn trusted_env_value(key: &str, value: &str) -> bool {
    if value.is_empty() || value.contains('\0') || value.contains('\n') {
        return false;
    }

    let uid = crate::platform::current_uid();
    trusted_env_value_for_uid(key, value, uid, std::path::Path::exists)
}

#[cfg(target_os = "linux")]
fn trusted_env_value_for_uid<F>(key: &str, value: &str, uid: u32, exists: F) -> bool
where
    F: Fn(&std::path::Path) -> bool,
{
    let runtime_dir = format!("/run/user/{uid}");
    match key {
        "XDG_RUNTIME_DIR" => value == runtime_dir && exists(std::path::Path::new(value)),
        "DBUS_SESSION_BUS_ADDRESS" => {
            let expected = format!("unix:path={runtime_dir}/bus");
            value == expected && exists(std::path::Path::new(&format!("{runtime_dir}/bus")))
        }
        "DISPLAY" => {
            let Some(display_num) = parse_local_display_number(value) else {
                return false;
            };
            exists(std::path::Path::new(&format!(
                "/tmp/.X11-unix/X{display_num}"
            )))
        }
        "WAYLAND_DISPLAY" => {
            if value.contains('/') || value == "." || value == ".." {
                return false;
            }
            exists(std::path::Path::new(&runtime_dir).join(value).as_path())
        }
        "XAUTHORITY" => {
            let path = std::path::Path::new(value);
            path.is_absolute() && exists(path)
        }
        "XDG_SESSION_TYPE" => matches!(value, "wayland" | "x11"),
        "XDG_CURRENT_DESKTOP" | "DESKTOP_SESSION" => value.len() <= 128,
        _ => false,
    }
}

#[cfg(target_os = "linux")]
fn parse_local_display_number(display: &str) -> Option<u32> {
    let rest = display.strip_prefix(':')?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;

    #[test]
    fn parses_systemd_show_environment() {
        let parsed = parse_systemd_environment(
            "DISPLAY=:0\nWAYLAND_DISPLAY=wayland-0\nIGNORED=value\nXDG_SESSION_TYPE=wayland\n",
        );
        assert_eq!(parsed.get("DISPLAY").map(String::as_str), Some(":0"));
        assert_eq!(
            parsed.get("WAYLAND_DISPLAY").map(String::as_str),
            Some("wayland-0")
        );
        assert!(!parsed.contains_key("IGNORED"));
    }

    #[test]
    fn trusts_same_user_session_paths() {
        let exists = |path: &std::path::Path| {
            matches!(
                path.to_str(),
                Some("/run/user/1000")
                    | Some("/run/user/1000/bus")
                    | Some("/run/user/1000/wayland-0")
                    | Some("/run/user/1000/.mutter-Xwaylandauth.ABC")
                    | Some("/tmp/.X11-unix/X0")
            )
        };
        assert!(trusted_env_value_for_uid(
            "XDG_RUNTIME_DIR",
            "/run/user/1000",
            1000,
            exists
        ));
        assert!(trusted_env_value_for_uid(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/run/user/1000/bus",
            1000,
            exists
        ));
        assert!(trusted_env_value_for_uid(
            "WAYLAND_DISPLAY",
            "wayland-0",
            1000,
            exists
        ));
        assert!(trusted_env_value_for_uid("DISPLAY", ":0", 1000, exists));
        assert!(trusted_env_value_for_uid(
            "XAUTHORITY",
            "/run/user/1000/.mutter-Xwaylandauth.ABC",
            1000,
            exists
        ));
    }

    #[test]
    fn rejects_foreign_or_nonlocal_values() {
        let exists = |_path: &std::path::Path| true;
        assert!(!trusted_env_value_for_uid(
            "XDG_RUNTIME_DIR",
            "/run/user/1001",
            1000,
            exists
        ));
        assert!(!trusted_env_value_for_uid(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/run/user/1001/bus",
            1000,
            exists
        ));
        assert!(!trusted_env_value_for_uid(
            "DISPLAY", "remote:0", 1000, exists
        ));
        assert!(!trusted_env_value_for_uid(
            "WAYLAND_DISPLAY",
            "../wayland-0",
            1000,
            exists
        ));
        assert!(!trusted_env_value_for_uid(
            "XAUTHORITY",
            "relative-auth",
            1000,
            exists
        ));
    }
}
