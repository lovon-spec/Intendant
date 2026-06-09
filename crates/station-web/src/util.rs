//! Colors, palette, and small pure formatting/hashing helpers.

use std::f32::consts::PI;

#[derive(Clone, Copy)]
pub(crate) struct Color {
    pub(crate) r: f32,
    pub(crate) g: f32,
    pub(crate) b: f32,
    pub(crate) a: f32,
}

impl Color {
    pub(crate) const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: 1.0,
        }
    }

    pub(crate) fn with_alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }
}

impl From<Color> for [f32; 4] {
    fn from(value: Color) -> Self {
        [value.r, value.g, value.b, value.a]
    }
}

pub(crate) const C_SURFACE0: Color = Color::rgb(49, 50, 68);
pub(crate) const C_OVERLAY1: Color = Color::rgb(127, 132, 156);
pub(crate) const C_BLUE: Color = Color::rgb(137, 180, 250);
pub(crate) const C_LAVENDER: Color = Color::rgb(180, 190, 254);
pub(crate) const C_SAPPHIRE: Color = Color::rgb(116, 199, 236);
pub(crate) const C_TEAL: Color = Color::rgb(148, 226, 213);
pub(crate) const C_GREEN: Color = Color::rgb(166, 227, 161);
pub(crate) const C_YELLOW: Color = Color::rgb(249, 226, 175);
pub(crate) const C_PEACH: Color = Color::rgb(250, 179, 135);
pub(crate) const C_RED: Color = Color::rgb(243, 139, 168);
pub(crate) const C_MAUVE: Color = Color::rgb(203, 166, 247);

pub(crate) const C_TEXT_CSS: &str = "#cdd6f4";
pub(crate) const C_SUBTEXT0_CSS: &str = "#a6adc8";
pub(crate) const C_OVERLAY1_CSS: &str = "#7f849c";
pub(crate) const C_BLUE_CSS: &str = "#89b4fa";
pub(crate) const C_LAVENDER_CSS: &str = "#b4befe";
pub(crate) const C_TEAL_CSS: &str = "#94e2d5";
pub(crate) const C_GREEN_CSS: &str = "#a6e3a1";
pub(crate) const C_YELLOW_CSS: &str = "#f9e2af";
pub(crate) const C_PEACH_CSS: &str = "#fab387";
pub(crate) const C_RED_CSS: &str = "#f38ba8";
pub(crate) const C_MAUVE_CSS: &str = "#cba6f7";

pub(crate) fn role_color(role: &str) -> Color {
    match role {
        "orchestrator" => C_BLUE,
        "sub-agent" => C_MAUVE,
        "direct" => C_TEAL,
        _ => C_TEAL,
    }
}

pub(crate) fn phase_color(phase: &str) -> Color {
    match phase {
        "thinking" => C_LAVENDER,
        "running" => C_TEAL,
        "waiting" => C_YELLOW,
        "done" => C_GREEN,
        _ => C_OVERLAY1,
    }
}

pub(crate) fn level_color(level: &str) -> Color {
    match level {
        "error" => C_RED,
        "warn" => C_YELLOW,
        "model" => C_BLUE,
        "agent" => C_TEAL,
        "subagent" => C_MAUVE,
        "presence" => C_GREEN,
        _ => C_OVERLAY1,
    }
}

pub(crate) fn level_color_css(level: &str) -> &'static str {
    match level {
        "error" => C_RED_CSS,
        "warn" => C_YELLOW_CSS,
        "model" => C_BLUE_CSS,
        "agent" => C_TEAL_CSS,
        "subagent" => C_MAUVE_CSS,
        "presence" => C_GREEN_CSS,
        _ => C_OVERLAY1_CSS,
    }
}

pub(crate) fn css_rgba(color: [f32; 4]) -> String {
    format!(
        "rgba({:.0},{:.0},{:.0},{:.3})",
        color[0] * 255.0,
        color[1] * 255.0,
        color[2] * 255.0,
        color[3]
    )
}

pub(crate) fn percent(value: f32, max: f32) -> f32 {
    if max <= 0.0 {
        0.0
    } else {
        (value / max).clamp(0.0, 1.0)
    }
}

pub(crate) fn pct_label(pct: f32) -> String {
    format!("{:.0}%", pct.clamp(0.0, 1.0) * 100.0)
}

pub(crate) fn nonempty(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

pub(crate) fn pressure_color(pct: f32) -> &'static str {
    if pct >= 0.9 {
        C_RED_CSS
    } else if pct >= 0.72 {
        C_YELLOW_CSS
    } else if pct >= 0.5 {
        C_BLUE_CSS
    } else {
        C_GREEN_CSS
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

pub(crate) fn stable_angle(s: &str) -> f32 {
    stable_unit(s) * PI * 2.0
}

pub(crate) fn stable_unit(s: &str) -> f32 {
    let mut h = 2166136261u32;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h as f32 / u32::MAX as f32).clamp(0.0, 1.0)
}

pub(crate) fn lcg(seed: u32) -> u32 {
    seed.wrapping_mul(1664525).wrapping_add(1013904223)
}

pub(crate) fn unit(seed: u32) -> f32 {
    seed as f32 / u32::MAX as f32
}

pub(crate) fn station_enable_webgpu() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|document| document.url().ok())
        .is_none_or(|url| !url.contains("station_gpu=canvas") && !url.contains("station_gpu=off"))
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn now_ms() -> f64 {
    thread_local! {
        static PERFORMANCE: Option<web_sys::Performance> =
            web_sys::window().and_then(|w| w.performance());
    }
    PERFORMANCE.with(|p| p.as_ref().map_or(0.0, |p| p.now()))
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn now_ms() -> f64 {
    0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_passes_short_strings_through() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("", 4), "");
    }

    #[test]
    fn truncate_cuts_on_chars_not_bytes() {
        assert_eq!(truncate("hello", 3), "hel…");
        // Multi-byte characters count as one.
        assert_eq!(truncate("héllo wörld", 5), "héllo…");
    }

    #[test]
    fn nonempty_trims_and_falls_back() {
        assert_eq!(nonempty("  value  ", "fb"), "value");
        assert_eq!(nonempty("   ", "fb"), "fb");
        assert_eq!(nonempty("", "fb"), "fb");
    }

    #[test]
    fn percent_clamps_and_handles_empty_window() {
        assert_eq!(percent(50.0, 200.0), 0.25);
        assert_eq!(percent(500.0, 200.0), 1.0);
        assert_eq!(percent(-1.0, 200.0), 0.0);
        assert_eq!(percent(10.0, 0.0), 0.0);
        assert_eq!(percent(10.0, -5.0), 0.0);
    }

    #[test]
    fn pct_label_rounds_and_clamps() {
        assert_eq!(pct_label(0.0), "0%");
        assert_eq!(pct_label(0.254), "25%");
        assert_eq!(pct_label(1.7), "100%");
    }

    #[test]
    fn pressure_color_thresholds() {
        assert_eq!(pressure_color(0.1), C_GREEN_CSS);
        assert_eq!(pressure_color(0.5), C_BLUE_CSS);
        assert_eq!(pressure_color(0.72), C_YELLOW_CSS);
        assert_eq!(pressure_color(0.9), C_RED_CSS);
    }

    #[test]
    fn stable_unit_is_deterministic_and_in_range() {
        for id in ["", "agent-1", "host:alpha", "x"] {
            let a = stable_unit(id);
            assert_eq!(a, stable_unit(id));
            assert!((0.0..=1.0).contains(&a), "{id} -> {a}");
        }
        assert_ne!(stable_unit("agent-1"), stable_unit("agent-2"));
        assert_eq!(stable_angle("a"), stable_unit("a") * PI * 2.0);
    }

    #[test]
    fn lcg_and_unit_are_deterministic() {
        let s1 = lcg(1);
        assert_eq!(s1, lcg(1));
        assert!((0.0..=1.0).contains(&unit(s1)));
    }

    #[test]
    fn css_rgba_formats_components() {
        assert_eq!(css_rgba([1.0, 0.0, 0.5, 0.25]), "rgba(255,0,128,0.250)");
    }

    #[test]
    fn color_with_alpha_keeps_rgb() {
        let c = C_BLUE.with_alpha(0.5);
        let arr: [f32; 4] = c.into();
        assert_eq!(arr[3], 0.5);
        assert_eq!(arr[0], C_BLUE.r);
    }

    #[test]
    fn semantic_color_maps_cover_known_keys() {
        assert_eq!(level_color_css("error"), C_RED_CSS);
        assert_eq!(level_color_css("warn"), C_YELLOW_CSS);
        assert_eq!(level_color_css("unknown"), C_OVERLAY1_CSS);
        let err: [f32; 4] = level_color("error").into();
        let red: [f32; 4] = C_RED.into();
        assert_eq!(err, red);
        let orch: [f32; 4] = role_color("orchestrator").into();
        let blue: [f32; 4] = C_BLUE.into();
        assert_eq!(orch, blue);
        let think: [f32; 4] = phase_color("thinking").into();
        let lavender: [f32; 4] = C_LAVENDER.into();
        assert_eq!(think, lavender);
    }
}
