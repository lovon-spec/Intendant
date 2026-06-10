//! HUD overlay: 2D canvas panels, draw primitives, and the memoized
//! style wrapper.

use std::cell::RefCell;
use std::collections::HashMap;

use web_sys::CanvasRenderingContext2d;

use crate::input::{HitAction, HitZone};
use crate::model::activity_retained_count;
use crate::scene::{ndc_to_screen, LayoutName, Mood, NodeKind, ProjectedNode, Vec2};
use crate::util::{
    css_rgba, hex_color, level_color_css, nonempty, pct_label, percent, pressure_color, truncate,
    Color, C_BLUE, C_BLUE_CSS, C_GREEN_CSS, C_LAVENDER, C_LAVENDER_CSS, C_MAUVE_CSS, C_OVERLAY1,
    C_OVERLAY1_CSS, C_PEACH, C_PEACH_CSS, C_RED_CSS, C_SUBTEXT0_CSS, C_TEAL, C_TEAL_CSS,
    C_TEXT_CSS, C_YELLOW_CSS,
};
use crate::StationInner;

/// The HUD 2D context plus memoized style state. Canvas style setters are
/// expensive to spam and the HUD repeats the same handful of fills, strokes,
/// and fonts hundreds of times per frame, so each setter only touches the
/// context when the value actually changes. Font strings are interned per
/// (size, weight). Interior mutability keeps the draw helpers callable
/// through `&self`.
pub(crate) struct Hud {
    pub(crate) ctx: CanvasRenderingContext2d,
    pub(crate) style: RefCell<HudStyle>,
}

#[derive(Default)]
pub(crate) struct HudStyle {
    pub(crate) fill: String,
    pub(crate) stroke: String,
    pub(crate) font: (u32, bool),
    pub(crate) fonts: HashMap<(u32, bool), String>,
    pub(crate) vignette: Option<Vignette>,
}

pub(crate) struct Vignette {
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) mood: Mood,
    pub(crate) gradient: web_sys::CanvasGradient,
}

impl Hud {
    pub(crate) fn new(ctx: CanvasRenderingContext2d) -> Self {
        Self {
            ctx,
            style: RefCell::new(HudStyle::default()),
        }
    }

    pub(crate) fn set_fill(&self, css: &str) {
        let mut style = self.style.borrow_mut();
        if style.fill != css {
            style.fill.clear();
            style.fill.push_str(css);
            self.ctx.set_fill_style_str(css);
        }
    }

    pub(crate) fn set_stroke(&self, css: &str) {
        let mut style = self.style.borrow_mut();
        if style.stroke != css {
            style.stroke.clear();
            style.stroke.push_str(css);
            self.ctx.set_stroke_style_str(css);
        }
    }

    pub(crate) fn set_font(&self, px: f32, bold: bool) {
        let key = ((px * 10.0).round() as u32, bold);
        let mut style = self.style.borrow_mut();
        if style.font == key {
            return;
        }
        style.font = key;
        let font = style.fonts.entry(key).or_insert_with(|| {
            format!(
                "{} {px}px 'SF Mono', Menlo, Consolas, monospace",
                if bold { "bold" } else { "normal" }
            )
        });
        self.ctx.set_font(font);
    }

    /// The fill was set to a non-string paint (e.g. a gradient) behind the
    /// memo's back; force the next `set_fill` through.
    pub(crate) fn note_fill_unknown(&self) {
        self.style.borrow_mut().fill.clear();
    }

    /// Radial vignette gradient, rebuilt only when the size or mood changes.
    pub(crate) fn vignette(&self, w: f32, h: f32, mood: Mood) -> Option<web_sys::CanvasGradient> {
        let mut style = self.style.borrow_mut();
        if let Some(v) = style.vignette.as_ref() {
            if v.width == w && v.height == h && v.mood == mood {
                return Some(v.gradient.clone());
            }
        }
        let gradient = self
            .ctx
            .create_radial_gradient(
                (w / 2.0) as f64,
                (h / 2.0) as f64,
                20.0,
                (w / 2.0) as f64,
                (h / 2.0) as f64,
                (w.max(h) * 0.72) as f64,
            )
            .ok()?;
        for (offset, color) in mood.vignette_stops() {
            let _ = gradient.add_color_stop(offset as f32, color);
        }
        style.vignette = Some(Vignette {
            width: w,
            height: h,
            mood,
            gradient: gradient.clone(),
        });
        Some(gradient)
    }

    pub(crate) fn invalidate_vignette(&self) {
        self.style.borrow_mut().vignette = None;
    }

    /// Forget memoized style state after the real context state was reset
    /// (canvas resize) or mutated outside the memo (scene underlay).
    pub(crate) fn invalidate_styles(&self) {
        let mut style = self.style.borrow_mut();
        style.fill.clear();
        style.stroke.clear();
        style.font = (0, false);
    }

    /// Full reset: styles and the size-dependent vignette.
    pub(crate) fn invalidate(&self) {
        self.invalidate_styles();
        self.invalidate_vignette();
    }
}

impl StationInner {
    pub(crate) fn draw_hud(&mut self, time_ms: f64) {
        self.hud
            .ctx
            .set_transform(self.dpr, 0.0, 0.0, self.dpr, 0.0, 0.0)
            .ok();
        let w = self.css_width();
        let h = self.css_height();
        self.hud.ctx.clear_rect(0.0, 0.0, w as f64, h as f64);
        self.hit_zones.clear();

        if self.gpu.is_none() && self.scene_ctx.is_none() {
            // Runtime WebGPU failure with a consumed scene canvas: paint the
            // wireframe under the HUD. The identity transform matches the
            // device-pixel coordinates draw_scene_lines expects.
            self.hud.ctx.save();
            self.hud
                .ctx
                .set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0)
                .ok();
            self.draw_scene_lines(&self.hud.ctx);
            self.hud.ctx.restore();
            self.hud.invalidate_styles();
        }

        self.draw_vignette(w, h);
        self.draw_display_thumbnails();
        self.draw_station_header(w);
        self.draw_station_control_center(w, h, time_ms);
        self.draw_corners(w, h);
        self.draw_compass(w, h);
        if let Some(id) = self.selected_id.clone() {
            self.draw_station_focus_detail(&id, w, h);
        }
    }

    pub(crate) fn draw_vignette(&self, w: f32, h: f32) {
        if let Some(gradient) = self.hud.vignette(w, h, self.mood) {
            self.hud.ctx.set_fill_style_canvas_gradient(&gradient);
            self.hud.note_fill_unknown();
            self.hud.ctx.fill_rect(0.0, 0.0, w as f64, h as f64);
        }
    }

    /// Thumbnail frame rect (CSS px) for a display source anchored at the
    /// projected host position. Shared by the full HUD paint and the
    /// video-only partial repaint so the two can never drift apart.
    pub(crate) fn thumbnail_rect(css: Vec2, css_width: f32) -> (f32, f32, f32, f32) {
        let tw = 164.0_f32.min(css_width * 0.28).max(98.0);
        let th = tw * 0.5625;
        let x = css.x - tw / 2.0;
        let y = css.y - 118.0 - th * 0.2;
        (x, y, tw, th)
    }

    pub(crate) fn draw_display_thumbnails(&self) {
        if self.display_sources.is_empty() {
            return;
        }
        let by_host: HashMap<&str, &ProjectedNode> = self
            .frame
            .projected_nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Host)
            .map(|n| (n.id.strip_prefix("host:").unwrap_or(n.id.as_str()), n))
            .collect();

        for source in self.display_sources.values() {
            let Some(node) = by_host.get(source.host_id.as_str()) else {
                continue;
            };
            let center = ndc_to_screen([node.ndc.x, node.ndc.y], self.width, self.height);
            let css = Vec2::new(center.x / self.dpr as f32, center.y / self.dpr as f32);
            let (x, y, tw, th) = Self::thumbnail_rect(css, self.css_width());
            self.glass_panel(x, y, tw, th, 6.0, C_PEACH, 1.2, 1.15);
            let video_ready = source.video.video_width() > 0 && source.video.video_height() > 0;
            if video_ready {
                let _ = self
                    .hud
                    .ctx
                    .draw_image_with_html_video_element_and_dw_and_dh(
                        &source.video,
                        (x + 3.0) as f64,
                        (y + 3.0) as f64,
                        (tw - 6.0) as f64,
                        (th - 6.0) as f64,
                    );
            } else {
                self.hud.set_fill("rgba(49,50,68,0.55)");
                self.hud.ctx.fill_rect(
                    (x + 3.0) as f64,
                    (y + 3.0) as f64,
                    (tw - 6.0) as f64,
                    (th - 6.0) as f64,
                );
                self.text(
                    "linking display",
                    x + 12.0,
                    y + th / 2.0,
                    10.0,
                    C_OVERLAY1_CSS,
                    "normal",
                );
            }
            self.text(
                &source.label,
                x + 7.0,
                y + th + 12.0,
                10.0,
                C_PEACH_CSS,
                "normal",
            );
        }
    }

    pub(crate) fn draw_station_header(&mut self, w: f32) {
        let ctx = &self.hud.ctx;
        let a = self.mood.glass();
        // Full-bleed glass strip: translucent gradient body, top sheen,
        // luminous bottom edge.
        let body = ctx.create_linear_gradient(0.0, 0.0, 0.0, 42.0);
        let _ = body.add_color_stop(0.0, "rgba(16,17,28,0.92)");
        let _ = body.add_color_stop(1.0, "rgba(11,11,19,0.62)");
        ctx.set_fill_style_canvas_gradient(&body);
        self.hud.note_fill_unknown();
        ctx.fill_rect(0.0, 0.0, w as f64, 42.0);
        self.hud.set_stroke(&css_rgba([0.93, 0.95, 1.0, 0.06 * a]));
        self.line(0.0, 1.0, w, 1.0);
        self.hud
            .set_stroke(&css_rgba(C_BLUE.with_alpha(0.30 * a).into()));
        self.line(0.0, 42.0, w, 42.0);
        self.text("STATION", 24.0, 26.0, 11.0, C_TEXT_CSS, "bold");
        self.pill_button(
            96.0,
            10.0,
            78.0,
            23.0,
            "orbital",
            self.layout == LayoutName::Orbital,
            HitAction::Layout(LayoutName::Orbital),
        );
        self.pill_button(
            182.0,
            10.0,
            116.0,
            23.0,
            "constellation",
            self.layout == LayoutName::Constellation,
            HitAction::Layout(LayoutName::Constellation),
        );

        let active_agents = self
            .snapshot
            .agents
            .iter()
            .filter(|agent| agent.status == "in_progress")
            .count();
        let pending = self
            .snapshot
            .agents
            .iter()
            .filter(|agent| agent.needs_approval)
            .count();
        let right = format!(
            "{} hosts / {} active / {} approvals / renderer {}",
            self.snapshot.hosts.len(),
            active_agents,
            pending,
            if self.gpu.is_some() {
                "WebGPU"
            } else {
                "Canvas"
            },
        );
        self.text(
            &truncate(&right, ((w - 330.0) / 7.0).max(22.0) as usize),
            318.0,
            26.0,
            10.0,
            if pending > 0 {
                C_YELLOW_CSS
            } else {
                C_SUBTEXT0_CSS
            },
            "normal",
        );
    }

    pub(crate) fn draw_station_control_center(&mut self, w: f32, h: f32, time_ms: f64) {
        if w < 360.0 || h < 320.0 {
            return;
        }
        if w < 820.0 {
            self.draw_station_compact_surface(w, h);
            return;
        }

        let margin = 24.0;
        let top_y = 58.0;
        let gap = 14.0;
        let available_w = (w - margin * 2.0).max(760.0);
        let available_h = (h - top_y - 24.0).max(420.0);
        let command_h = if h < 640.0 { 78.0 } else { 92.0 };
        let lane_h = if h < 640.0 { 68.0 } else { 78.0 };
        let main_h = (available_h - command_h - lane_h - gap * 2.0).max(250.0);

        let center_x = margin;
        let center_w = available_w;
        let main_y = top_y + command_h + gap;

        self.draw_station_command_deck(margin, top_y, available_w, command_h);
        self.draw_station_scene_core(center_x, main_y, center_w, main_h, time_ms);
        self.draw_station_activity_lane(margin, h, available_w);
    }

    pub(crate) fn draw_station_command_deck(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.glass_panel(x - 6.0, y - 8.0, w + 12.0, h + 14.0, 12.0, C_BLUE, 0.9, 0.9);
        self.hud.set_fill(C_BLUE_CSS);
        self.hud
            .ctx
            .fill_rect(x as f64, (y + 15.0) as f64, 3.0, 38.0);
        self.text(
            "CONTROL CENTER",
            x + 18.0,
            y + 24.0,
            12.0,
            C_BLUE_CSS,
            "bold",
        );
        self.text(
            &truncate(
                &self.station_target_label(),
                ((w * 0.44) / 7.0).max(38.0) as usize,
            ),
            x + 18.0,
            y + 48.0,
            14.0,
            C_TEXT_CSS,
            "bold",
        );

        let controls = &self.snapshot.controls;
        let session_state = if controls.session_detached {
            "detached"
        } else if controls.session_active {
            "active"
        } else if controls.session_id.is_empty() {
            "no target"
        } else {
            "idle"
        };
        let session_line = format!(
            "{} / {} / {} / {}",
            nonempty(&controls.backend, "agent"),
            if controls.direct_mode {
                "direct"
            } else {
                "presence"
            },
            nonempty(&controls.approval_policy, "approval"),
            session_state
        );
        self.text(
            &truncate(&session_line, ((w * 0.46) / 6.2).max(42.0) as usize),
            x + 18.0,
            y + 68.0,
            10.0,
            C_SUBTEXT0_CSS,
            "normal",
        );

        let context_pct = percent(
            self.snapshot.context.tokens,
            self.snapshot.context.effective_window,
        );
        let managed_pct = percent(
            self.snapshot.managed.used_tokens,
            self.snapshot.managed.effective_window,
        );
        let metric_w = ((w * 0.42) - 24.0).max(300.0) / 3.0;
        let metric_x = x + w - metric_w * 3.0 - 18.0;
        let metric_y = y + 15.0;
        let metrics = [
            (
                "Context",
                pct_label(context_pct),
                pressure_color(context_pct),
            ),
            (
                "Managed",
                nonempty(&self.snapshot.managed.status, "unknown"),
                pressure_color(managed_pct),
            ),
            (
                "Changes",
                if self.snapshot.changes.count > 0 {
                    format!("{} files", self.snapshot.changes.count)
                } else {
                    nonempty(&self.snapshot.changes.status, "clean")
                },
                if self.snapshot.changes.count > 0 {
                    C_YELLOW_CSS
                } else {
                    C_GREEN_CSS
                },
            ),
        ];
        for (idx, (label, value, color)) in metrics.into_iter().enumerate() {
            let mx = metric_x + idx as f32 * metric_w;
            self.text(
                label,
                mx + 10.0,
                metric_y + 15.0,
                8.5,
                C_OVERLAY1_CSS,
                "bold",
            );
            self.text(
                &truncate(&value, ((metric_w - 22.0) / 6.0).max(8.0) as usize),
                mx + 10.0,
                metric_y + 32.0,
                10.0,
                color,
                "bold",
            );
            let pct = if label == "Context" {
                context_pct
            } else if label == "Managed" {
                managed_pct
            } else if self.snapshot.changes.count > 0 {
                1.0
            } else {
                0.0
            };
            self.meter(mx + 10.0, metric_y + 39.0, metric_w - 28.0, pct, color);
        }

        let mut ax = x + w - 18.0;
        let ay = y + h - 34.0;
        for action in self.station_primary_actions().into_iter().rev().take(7) {
            ax -= action.width;
            if ax < x + w * 0.48 {
                break;
            }
            self.pill_at(ax, ay, action.width, 23.0, action.label, action.color, false);
            self.hit_zones
                .push(HitZone::new(ax, ay, action.width, 23.0, action.hit));
            ax -= 8.0;
        }
    }

    pub(crate) fn draw_station_compact_surface(&mut self, w: f32, h: f32) {
        let x = 18.0;
        let y = 64.0;
        let panel_w = w - 36.0;
        let panel_h = (h - 92.0).max(180.0);
        self.glass_panel(x, y, panel_w, panel_h, 10.0, C_BLUE, 1.0, 1.0);
        self.text(
            "CONTROL CENTER",
            x + 16.0,
            y + 24.0,
            12.0,
            C_BLUE_CSS,
            "bold",
        );
        self.text(
            &truncate(&self.station_target_label(), 48),
            x + 16.0,
            y + 46.0,
            11.0,
            C_TEXT_CSS,
            "normal",
        );

        let targets = std::mem::take(&mut self.system_targets);
        let tile_w = (panel_w - 44.0) * 0.5;
        let mut tx = x + 14.0;
        let mut ty = y + 66.0;
        for (idx, target) in targets.iter().take(8).enumerate() {
            if idx > 0 && idx % 2 == 0 {
                tx = x + 14.0;
                ty += 58.0;
            }
            self.station_focus_button(tx, ty, tile_w, 48.0, target);
            tx += tile_w + 16.0;
        }
        self.system_targets = targets;
    }

    pub(crate) fn draw_station_scene_core(&mut self, x: f32, y: f32, w: f32, h: f32, time_ms: f64) {
        let core_h = h.clamp(330.0, 560.0);
        if core_h < 150.0 {
            return;
        }
        // Clear glass: low tint so the 3D scene stays visible through it.
        self.glass_panel(x, y, w, core_h, 12.0, C_LAVENDER, 0.5, 0.28);
        let cx = x + w * 0.5;
        let cy = y + core_h * 0.52;
        let ring_scale = (core_h * 0.42).clamp(132.0, 230.0);
        self.hud.set_stroke(match self.mood {
            Mood::Cockpit => "rgba(137,180,250,0.28)",
            Mood::Calm => "rgba(137,180,250,0.18)",
        });
        let breathe = (time_ms as f32 * 0.001).sin() * 2.0 * self.mood.pulse();
        for radius in [ring_scale * 0.36, ring_scale * 0.62, ring_scale] {
            self.hud.ctx.begin_path();
            let _ = self.hud.ctx.arc(
                cx as f64,
                cy as f64,
                (radius + breathe) as f64,
                0.0,
                std::f64::consts::TAU,
            );
            self.hud.ctx.stroke();
        }
        self.text(
            "LIVE STATE",
            x + 18.0,
            y + 24.0,
            10.0,
            C_OVERLAY1_CSS,
            "bold",
        );
        let targets = std::mem::take(&mut self.system_targets);
        let selected = self
            .selected_id
            .as_deref()
            .and_then(|id| targets.iter().find(|target| target.id == id));
        if let Some(target) = selected {
            self.text(
                target.title,
                x + 118.0,
                y + 24.0,
                10.0,
                target.color,
                "bold",
            );
            self.text(
                &truncate(&target.detail, ((w - 260.0) / 6.0).max(24.0) as usize),
                x + 210.0,
                y + 24.0,
                9.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
        }
        self.text(
            &format!(
                "{} events / {} sessions / {} peers",
                self.snapshot.events.len(),
                self.snapshot.sessions.total,
                self.snapshot.hosts.len().saturating_sub(1),
            ),
            x + 18.0,
            y + 43.0,
            11.0,
            C_TEXT_CSS,
            "normal",
        );

        let node_w = (w * 0.20).clamp(158.0, 230.0);
        let node_h = 58.0;
        let node_specs = [
            (
                "system:activity",
                cx - ring_scale - node_w - 26.0,
                cy - 30.0,
            ),
            (
                "system:context",
                cx - ring_scale * 0.72 - node_w,
                cy + ring_scale * 0.62,
            ),
            ("system:managed", cx + ring_scale + 26.0, cy - 30.0),
            (
                "system:controls",
                cx + ring_scale * 0.58,
                cy + ring_scale * 0.66,
            ),
            ("system:peers", cx - node_w * 0.72, cy - ring_scale - 86.0),
            ("system:view", cx - node_w * 0.5, cy + ring_scale + 34.0),
        ];
        for (id, nx, ny) in node_specs {
            if let Some(target) = targets.iter().find(|target| target.id == id) {
                let node_w = if id == "system:peers" {
                    (node_w * 1.45).min(330.0)
                } else {
                    node_w
                };
                let node_h = if id == "system:peers" {
                    node_h + 16.0
                } else {
                    node_h
                };
                self.station_orbital_node(
                    cx,
                    cy,
                    nx.clamp(x + 20.0, x + w - node_w - 20.0),
                    ny.clamp(y + 58.0, y + core_h - node_h - 20.0),
                    node_w,
                    node_h,
                    target,
                );
            }
        }

        self.system_targets = targets;

        let row_y = y + core_h - 118.0;
        let matrix_ids = [
            "system:activity",
            "system:context",
            "system:managed",
            "system:sessions",
            "system:peers",
            "system:changes",
            "system:worktrees",
            "system:controls",
            "system:view",
        ];
        let matrix_w = (w - 72.0) / 3.0;
        for (idx, id) in matrix_ids.into_iter().enumerate() {
            let col = idx % 3;
            let row = idx / 3;
            self.hit_zones.push(HitZone::new(
                x + 30.0 + col as f32 * matrix_w,
                row_y + 25.0 + row as f32 * 31.0,
                matrix_w - 8.0,
                25.0,
                HitAction::Select(id.to_string()),
            ));
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn station_orbital_node(
        &mut self,
        cx: f32,
        cy: f32,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        target: &SystemTarget,
    ) {
        let selected = self.selected_id.as_deref() == Some(target.id);
        let hovered = self.hover_xy.is_some_and(|(hx, hy)| {
            hx >= x - 8.0 && hx <= x + w + 8.0 && hy >= y - 8.0 && hy <= y + h + 8.0
        });
        let is_display = target.id == "system:peers";
        let anchor_x = if x + w * 0.5 < cx { x + w } else { x };
        let anchor_y = y + h * 0.5;
        self.hud.set_stroke(if selected {
            target.color
        } else {
            "rgba(137,180,250,0.22)"
        });
        self.line(cx, cy, anchor_x, anchor_y);
        self.hud.set_fill(target.color);
        self.hud.ctx.begin_path();
        let _ = self.hud.ctx.arc(
            anchor_x as f64,
            anchor_y as f64,
            4.0,
            0.0,
            std::f64::consts::TAU,
        );
        self.hud.ctx.fill();
        self.hud.set_stroke(target.color);
        self.hud.ctx.begin_path();
        let _ = self.hud.ctx.arc(
            anchor_x as f64,
            anchor_y as f64,
            13.0,
            0.0,
            std::f64::consts::TAU,
        );
        self.hud.ctx.stroke();
        // Light glass chip behind the node text so it reads over the scene.
        self.glass_panel(
            x - 12.0,
            y - 4.0,
            w + 18.0,
            h + 8.0,
            9.0,
            hex_color(target.color).unwrap_or(C_BLUE),
            if selected {
                1.6
            } else if hovered {
                1.1
            } else {
                0.55
            },
            if selected { 0.95 } else { 0.62 },
        );
        if is_display {
            self.hud.set_stroke("rgba(250,179,135,0.58)");
            let aperture_w = (w * 0.34).max(92.0);
            let aperture_cx = x + aperture_w * 0.5;
            let aperture_cy = y + 29.0;
            for radius in [aperture_w * 0.22, aperture_w * 0.34] {
                self.hud.ctx.begin_path();
                let _ = self.hud.ctx.arc(
                    aperture_cx as f64,
                    aperture_cy as f64,
                    radius as f64,
                    0.0,
                    std::f64::consts::TAU,
                );
                self.hud.ctx.stroke();
            }
            self.text(
                target.kicker,
                x + aperture_w + 10.0,
                y + 15.0,
                8.0,
                C_OVERLAY1_CSS,
                "bold",
            );
            self.text(
                target.title,
                x + aperture_w + 10.0,
                y + 36.0,
                14.0,
                target.color,
                "bold",
            );
            self.text(
                &truncate(
                    &target.value,
                    ((w - aperture_w - 18.0) / 6.2).max(18.0) as usize,
                ),
                x + aperture_w + 10.0,
                y + 55.0,
                10.0,
                C_TEXT_CSS,
                "normal",
            );
            self.hit_zones.push(HitZone::new(
                x - 8.0,
                y - 8.0,
                w + 16.0,
                h + 16.0,
                HitAction::Select(target.id.to_string()),
            ));
            return;
        }
        self.text(target.kicker, x, y + 12.0, 8.0, C_OVERLAY1_CSS, "bold");
        self.text(target.title, x, y + 30.0, 12.0, target.color, "bold");
        self.text(
            &truncate(&target.value, ((w - 10.0) / 6.2).max(18.0) as usize),
            x,
            y + 47.0,
            10.0,
            C_TEXT_CSS,
            "normal",
        );
        if selected {
            self.text(
                &truncate(&target.detail, ((w - 10.0) / 6.4).max(18.0) as usize),
                x,
                y + h + 12.0,
                9.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
        }
        self.hit_zones.push(HitZone::new(
            x - 8.0,
            y - 8.0,
            w + 16.0,
            h + 16.0,
            HitAction::Select(target.id.to_string()),
        ));
    }

    pub(crate) fn draw_station_activity_lane(&mut self, x: f32, h: f32, w: f32) {
        let lane_h = 78.0;
        let y = (h - lane_h - 24.0).max(282.0);
        self.glass_panel(x - 6.0, y, w + 12.0, lane_h + 10.0, 12.0, C_TEAL, 0.9, 0.9);
        self.hud.set_fill(C_TEAL_CSS);
        self.hud
            .ctx
            .fill_rect((x + 1.0) as f64, (y + 18.0) as f64, 3.0, 34.0);
        self.text(
            "ACTIVITY RUNWAY",
            x + 18.0,
            y + 24.0,
            10.0,
            C_TEAL_CSS,
            "bold",
        );
        let latest = self
            .snapshot
            .events
            .iter()
            .rev()
            .take(3)
            .collect::<Vec<_>>();
        if latest.is_empty() {
            self.text(
                "Waiting for retained activity",
                x + 18.0,
                y + 56.0,
                11.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
        } else {
            for (idx, event) in latest.into_iter().enumerate() {
                let row_y = y + 43.0 + idx as f32 * 18.0;
                let color = level_color_css(&event.level);
                self.hud.set_fill(color);
                self.hud
                    .ctx
                    .fill_rect((x + 19.0) as f64, (row_y - 9.0) as f64, 4.0, 14.0);
                self.text(
                    &truncate(&nonempty(&event.ts, "--"), 10),
                    x + 33.0,
                    row_y,
                    9.0,
                    C_OVERLAY1_CSS,
                    "normal",
                );
                self.text(
                    &truncate(&event.level, 8),
                    x + 96.0,
                    row_y,
                    9.0,
                    color,
                    "bold",
                );
                self.text(
                    &truncate(&event.msg, ((w - 190.0) / 6.4).max(28.0) as usize),
                    x + 154.0,
                    row_y,
                    9.0,
                    C_SUBTEXT0_CSS,
                    "normal",
                );
            }
        }
        let actions = [
            LaneAction::activity("latest", "bottom", 68.0, C_TEAL_CSS),
            LaneAction::activity("copy", "copy-visible", 56.0, C_BLUE_CSS),
            LaneAction::select("activity", "system:activity", 76.0, C_OVERLAY1_CSS),
        ];
        let mut ax = x + w - 18.0;
        for action in actions.into_iter().rev() {
            ax -= action.width;
            self.pill_at(
                ax,
                y + 13.0,
                action.width,
                22.0,
                action.label,
                action.color,
                false,
            );
            self.hit_zones
                .push(HitZone::new(ax, y + 13.0, action.width, 22.0, action.hit));
            ax -= 8.0;
        }
    }

    pub(crate) fn draw_station_focus_detail(&mut self, id: &str, w: f32, h: f32) {
        if id.starts_with("system:") {
            return;
        }
        let panel_w = 370.0_f32.min(w - 48.0).max(280.0);
        let panel_h = 112.0;
        let x = (w - panel_w - 24.0).max(24.0);
        let activity_lane_y = (h - 126.0 - 24.0).max(282.0);
        let y = (activity_lane_y - panel_h - 12.0).max(58.0);
        let (title, value, detail, color) =
            match self.system_targets.iter().find(|target| target.id == id) {
                Some(target) => (
                    target.title,
                    truncate(&target.value, 52),
                    truncate(&target.detail, 58),
                    target.color,
                ),
                None => (
                    "Selection",
                    truncate(id, 42),
                    "scene node selected".to_string(),
                    C_BLUE_CSS,
                ),
            };
        self.glass_panel(
            x,
            y,
            panel_w,
            panel_h,
            10.0,
            hex_color(color).unwrap_or(C_BLUE),
            1.5,
            1.1,
        );
        self.hit_zones
            .push(HitZone::new(x, y, panel_w, panel_h, HitAction::Noop));
        self.text("FOCUS", x + 16.0, y + 23.0, 10.0, C_OVERLAY1_CSS, "bold");
        self.text(title, x + 16.0, y + 47.0, 14.0, color, "bold");
        self.text(&value, x + 16.0, y + 68.0, 11.0, C_TEXT_CSS, "normal");
        self.text(&detail, x + 16.0, y + 88.0, 10.0, C_SUBTEXT0_CSS, "normal");
        self.pill_at(
            x + panel_w - 70.0,
            y + 13.0,
            50.0,
            23.0,
            "close",
            C_OVERLAY1_CSS,
            false,
        );
        self.hit_zones.push(HitZone::new(
            x + panel_w - 70.0,
            y + 13.0,
            50.0,
            23.0,
            HitAction::ClosePanel,
        ));
    }

    pub(crate) fn station_focus_button(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        target: &SystemTarget,
    ) {
        let SystemTarget {
            id,
            kicker,
            title,
            color,
            ..
        } = *target;
        let value = &target.value;
        let detail = &target.detail;
        let selected = self.selected_id.as_deref() == Some(id);
        let hovered = self
            .hover_xy
            .is_some_and(|(hx, hy)| hx >= x && hx <= x + w && hy >= y && hy <= y + h);
        self.glass_panel(
            x,
            y,
            w,
            h,
            8.0,
            hex_color(color).unwrap_or(C_OVERLAY1),
            if selected {
                1.7
            } else if hovered {
                1.2
            } else {
                0.7
            },
            if selected { 1.1 } else { 0.85 },
        );
        self.hud.set_fill(color);
        self.hud
            .ctx
            .fill_rect((x + 9.0) as f64, (y + 10.0) as f64, 4.0, (h - 20.0) as f64);
        let max_chars = ((w - 34.0) / 6.2).max(12.0) as usize;
        if h < 38.0 {
            self.text(
                &truncate(title, max_chars),
                x + 20.0,
                y + h * 0.5 + 4.0,
                9.0,
                color,
                "bold",
            );
        } else if h < 58.0 {
            self.text(title, x + 20.0, y + 18.0, 10.0, color, "bold");
            self.text(
                &truncate(value, max_chars),
                x + 20.0,
                y + 35.0,
                9.5,
                C_TEXT_CSS,
                "normal",
            );
        } else if h < 72.0 {
            if !kicker.is_empty() {
                self.text(kicker, x + 20.0, y + 15.0, 7.5, C_OVERLAY1_CSS, "bold");
            }
            self.text(
                title,
                x + 20.0,
                y + if kicker.is_empty() { 21.0 } else { 29.0 },
                10.5,
                color,
                "bold",
            );
            self.text(
                &truncate(value, max_chars),
                x + 20.0,
                y + if detail.is_empty() {
                    h - 13.0
                } else {
                    h - 25.0
                },
                9.5,
                C_TEXT_CSS,
                "normal",
            );
            if !detail.is_empty() {
                self.text(
                    &truncate(detail, max_chars),
                    x + 20.0,
                    y + h - 11.0,
                    8.0,
                    C_SUBTEXT0_CSS,
                    "normal",
                );
            }
        } else {
            if !kicker.is_empty() {
                self.text(kicker, x + 20.0, y + 16.0, 8.0, C_OVERLAY1_CSS, "bold");
            }
            self.text(
                title,
                x + 20.0,
                y + if kicker.is_empty() { 24.0 } else { 34.0 },
                11.0,
                color,
                "bold",
            );
            self.text(
                &truncate(value, max_chars),
                x + 20.0,
                y + h - if detail.is_empty() { 15.0 } else { 29.0 },
                10.0,
                C_TEXT_CSS,
                "normal",
            );
            if !detail.is_empty() {
                self.text(
                    &truncate(detail, max_chars),
                    x + 20.0,
                    y + h - 12.0,
                    8.5,
                    C_SUBTEXT0_CSS,
                    "normal",
                );
            }
        }
        self.hit_zones
            .push(HitZone::new(x, y, w, h, HitAction::Select(id.to_string())));
    }

    pub(crate) fn station_target_label(&self) -> String {
        let controls = &self.snapshot.controls;
        nonempty(
            &controls.session_label,
            &nonempty(
                &controls.session_selection,
                &nonempty(&controls.command, "No active command target"),
            ),
        )
    }

    pub(crate) fn station_primary_actions(&self) -> Vec<LaneAction> {
        let controls = &self.snapshot.controls;
        let mut actions = vec![
            LaneAction::activity(
                if controls.prompt_mode == "steer" {
                    "steer"
                } else {
                    "send"
                },
                "send",
                72.0,
                C_BLUE_CSS,
            ),
            LaneAction::activity("new session", "new-session", 112.0, C_TEAL_CSS),
        ];
        if controls.session_can_focus {
            actions.push(LaneAction::activity("focus", "target", 72.0, C_PEACH_CSS));
        }
        if controls.session_can_interrupt {
            actions.push(LaneAction::activity("stop", "stop", 60.0, C_RED_CSS));
        }
        if controls.shared_view_can_take_input {
            actions.push(LaneAction::controls(
                "take input",
                "shared-view-take-input",
                102.0,
                C_GREEN_CSS,
            ));
        }
        actions.extend([
            LaneAction::select("context", "system:context", 82.0, C_BLUE_CSS),
            LaneAction::select("managed", "system:managed", 88.0, C_MAUVE_CSS),
            LaneAction::select("sessions", "system:sessions", 90.0, C_TEAL_CSS),
            LaneAction::select("controls", "system:controls", 88.0, C_MAUVE_CSS),
        ]);
        actions
    }

    pub(crate) fn compute_system_targets(&self) -> Vec<SystemTarget> {
        let latest_event = self.snapshot.events.last();
        let ctx_pct = percent(
            self.snapshot.context.tokens,
            self.snapshot.context.effective_window,
        );
        let managed_pct = percent(
            self.snapshot.managed.used_tokens,
            self.snapshot.managed.effective_window,
        );
        let changes = &self.snapshot.changes;
        let controls = &self.snapshot.controls;
        let peer_count = self.snapshot.hosts.len().saturating_sub(1);
        vec![
            SystemTarget {
                id: "system:activity",
                kicker: "signal",
                title: "Activity",
                value: format!("{} retained", activity_retained_count(&self.snapshot)),
                detail: latest_event
                    .map(|event| truncate(&format!("{} {}", event.level, event.msg), 30))
                    .unwrap_or_else(|| "waiting for events".to_string()),
                color: latest_event
                    .map(|event| level_color_css(&event.level))
                    .unwrap_or(C_TEAL_CSS),
            },
            SystemTarget {
                id: "system:context",
                kicker: "memory",
                title: "Context",
                value: if self.snapshot.context.available {
                    format!(
                        "{} / {} items",
                        pct_label(ctx_pct),
                        self.snapshot.context.item_count
                    )
                } else {
                    "waiting".to_string()
                },
                detail: truncate(
                    &format!(
                        "{} {}",
                        nonempty(&self.snapshot.context.source, "snapshot"),
                        nonempty(&self.snapshot.context.turn, "")
                    ),
                    30,
                ),
                color: pressure_color(ctx_pct),
            },
            SystemTarget {
                id: "system:managed",
                kicker: "lineage",
                title: "Managed",
                value: format!(
                    "{} / {}",
                    nonempty(&self.snapshot.managed.mode, "managed"),
                    nonempty(&self.snapshot.managed.status, "unknown")
                ),
                detail: format!(
                    "{} records / {} anchors",
                    self.snapshot.managed.records, self.snapshot.managed.anchors
                ),
                color: pressure_color(managed_pct),
            },
            SystemTarget {
                id: "system:controls",
                kicker: "operator",
                title: "Controls",
                value: truncate(
                    &format!(
                        "{} / {}",
                        nonempty(&controls.backend, "agent"),
                        nonempty(&controls.sandbox, "sandbox")
                    ),
                    32,
                ),
                detail: truncate(
                    &format!(
                        "{} / managed {}",
                        nonempty(&controls.approval_policy, "approval"),
                        nonempty(&controls.managed_context, "unknown")
                    ),
                    34,
                ),
                color: C_MAUVE_CSS,
            },
            SystemTarget {
                id: "system:sessions",
                kicker: "work",
                title: "Sessions",
                value: format!(
                    "{} total / {} active",
                    self.snapshot.sessions.total, self.snapshot.sessions.active
                ),
                detail: truncate(
                    &nonempty(&self.snapshot.sessions.latest_task, "launch history"),
                    32,
                ),
                color: if self.snapshot.sessions.active > 0 {
                    C_TEAL_CSS
                } else {
                    C_BLUE_CSS
                },
            },
            SystemTarget {
                id: "system:peers",
                kicker: "display",
                title: "Peers",
                value: format!(
                    "{peer_count} peers / {} streams",
                    self.display_sources.len()
                ),
                detail: truncate(
                    &format!(
                        "{} / {}",
                        nonempty(&controls.display_access, "display"),
                        nonempty(&controls.cu_backend, "computer use")
                    ),
                    34,
                ),
                color: C_PEACH_CSS,
            },
            SystemTarget {
                id: "system:changes",
                kicker: "tree",
                title: "Changes",
                value: if changes.count > 0 {
                    format!(
                        "{} files / +{} -{}",
                        changes.count, changes.total_added, changes.total_removed
                    )
                } else {
                    nonempty(&changes.status, "clean")
                },
                detail: truncate(&nonempty(&changes.latest_path, "working tree clean"), 34),
                color: if changes.count > 0 || changes.status == "mismatch" {
                    C_YELLOW_CSS
                } else {
                    C_GREEN_CSS
                },
            },
            SystemTarget {
                id: "system:worktrees",
                kicker: "project",
                title: "Worktrees",
                value: format!(
                    "{} scanned / {} active",
                    self.snapshot.sessions.worktrees, self.snapshot.sessions.worktree_active
                ),
                detail: format!(
                    "{} dirty / {} unmerged",
                    self.snapshot.sessions.worktree_dirty, self.snapshot.sessions.worktree_unmerged
                ),
                color: if self.snapshot.sessions.worktree_dirty > 0
                    || self.snapshot.sessions.worktree_unmerged > 0
                {
                    C_YELLOW_CSS
                } else {
                    C_BLUE_CSS
                },
            },
            SystemTarget {
                id: "system:view",
                kicker: "scene",
                title: "View",
                value: format!("{} / {}", self.layout.label(), self.mood.label()),
                detail: format!(
                    "{} fov / {:.1} density",
                    self.fov_deg.round() as i32,
                    self.density
                ),
                color: C_LAVENDER_CSS,
            },
        ]
    }

    pub(crate) fn draw_corners(&self, w: f32, h: f32) {
        let a = self.mood.glass();
        self.hud
            .set_stroke(&css_rgba(C_LAVENDER.with_alpha(0.34 * a).into()));
        let len = 26.0;
        for (x, y, sx, sy) in [
            (11.0, 50.0, 1.0, 1.0),
            (w - 11.0, 50.0, -1.0, 1.0),
            (11.0, h - 11.0, 1.0, -1.0),
            (w - 11.0, h - 11.0, -1.0, -1.0),
        ] {
            self.line(x, y, x + sx * len, y);
            self.line(x, y, x, y + sy * len);
        }
    }

    pub(crate) fn draw_compass(&self, w: f32, h: f32) {
        let cx = w - 71.0;
        let cy = h - 33.0;
        // Small glass disc so the dial reads over any scene behind it.
        self.hud.ctx.begin_path();
        let _ = self
            .hud
            .ctx
            .arc(cx as f64, cy as f64, 18.0, 0.0, std::f64::consts::TAU);
        self.hud.set_fill("rgba(13,14,24,0.55)");
        self.hud.ctx.fill();
        self.hud.set_stroke(&css_rgba(
            C_LAVENDER
                .with_alpha(0.40 * self.mood.glass())
                .into(),
        ));
        self.hud.ctx.stroke();
        let angle = -self.yaw as f64;
        self.hud.set_stroke(C_BLUE_CSS);
        self.hud.ctx.begin_path();
        self.hud.ctx.move_to(cx as f64, cy as f64);
        self.hud.ctx.line_to(
            cx as f64 + angle.sin() * 14.0,
            cy as f64 - angle.cos() * 14.0,
        );
        self.hud.ctx.stroke();
        self.text("N", cx + 27.0, cy + 4.0, 10.0, C_OVERLAY1_CSS, "bold");
    }

    pub(crate) fn meter(&self, x: f32, y: f32, w: f32, pct: f32, color: &str) {
        let pct = pct.clamp(0.0, 1.0);
        self.hud.set_fill("rgba(49,50,68,0.92)");
        self.hud
            .ctx
            .fill_rect(x as f64, (y - 6.0) as f64, w as f64, 5.0);
        self.hud.set_fill(color);
        self.hud
            .ctx
            .fill_rect(x as f64, (y - 6.0) as f64, (w * pct) as f64, 5.0);
        self.hud.set_stroke("rgba(127,132,156,0.5)");
        self.hud
            .ctx
            .stroke_rect(x as f64, (y - 6.0) as f64, w as f64, 5.0);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pill_button(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        label: &str,
        active: bool,
        action: HitAction,
    ) {
        self.pill_at(
            x,
            y,
            w,
            h,
            label,
            if active { C_BLUE_CSS } else { C_OVERLAY1_CSS },
            active,
        );
        self.hit_zones.push(HitZone::new(x, y, w, h, action));
    }

    /// Capsule pill with the glass treatment. `active` (selected) and
    /// hovered pills are lit from within: an accent gradient swelling from
    /// the capsule's middle plus a brighter luminous border.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pill_at(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        label: &str,
        color: &str,
        active: bool,
    ) {
        let ctx = &self.hud.ctx;
        let a = self.mood.glass();
        let accent = hex_color(color).unwrap_or(C_OVERLAY1);
        let hovered = self
            .hover_xy
            .is_some_and(|(hx, hy)| hx >= x && hx <= x + w && hy >= y && hy <= y + h);
        let r = (h * 0.5).min(11.0);
        // Dark translucent capsule base.
        self.rounded_path(x, y, w, h, r);
        let base = ctx.create_linear_gradient(x as f64, y as f64, x as f64, (y + h) as f64);
        let _ = base.add_color_stop(0.0, &css_rgba(Color::rgb(42, 44, 66).with_alpha(0.52).into()));
        let _ = base.add_color_stop(1.0, &css_rgba(Color::rgb(13, 14, 24).with_alpha(0.68).into()));
        ctx.set_fill_style_canvas_gradient(&base);
        self.hud.note_fill_unknown();
        ctx.fill();
        if active || hovered {
            let lit = (if active { 0.30 } else { 0.20 }) * a;
            let inner = ctx.create_linear_gradient(x as f64, y as f64, x as f64, (y + h) as f64);
            let _ = inner.add_color_stop(0.0, &css_rgba(accent.with_alpha(lit * 0.35).into()));
            let _ = inner.add_color_stop(0.5, &css_rgba(accent.with_alpha(lit).into()));
            let _ = inner.add_color_stop(1.0, &css_rgba(accent.with_alpha(lit * 0.45).into()));
            ctx.set_fill_style_canvas_gradient(&inner);
            ctx.fill();
        }
        // Gentle top highlight, then the luminous border.
        self.hud.set_stroke(&css_rgba([0.93, 0.95, 1.0, 0.07 * a]));
        self.line(x + r, y + 1.0, x + w - r, y + 1.0);
        let border = if active {
            0.85
        } else if hovered {
            0.62
        } else {
            0.38
        } * a;
        self.rounded_path(x, y, w, h, r);
        self.hud.set_stroke(&css_rgba(accent.with_alpha(border).into()));
        ctx.stroke();
        self.text(label, x + 8.0, y + h * 0.65, 10.0, color, "bold");
    }

    /// Trace a rounded-rect path on the HUD context (no fill/stroke).
    pub(crate) fn rounded_path(&self, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let ctx = &self.hud.ctx;
        let r = r.min(w * 0.5).min(h * 0.5).max(0.0);
        ctx.begin_path();
        ctx.move_to((x + r) as f64, y as f64);
        ctx.line_to((x + w - r) as f64, y as f64);
        ctx.quadratic_curve_to((x + w) as f64, y as f64, (x + w) as f64, (y + r) as f64);
        ctx.line_to((x + w) as f64, (y + h - r) as f64);
        ctx.quadratic_curve_to(
            (x + w) as f64,
            (y + h) as f64,
            (x + w - r) as f64,
            (y + h) as f64,
        );
        ctx.line_to((x + r) as f64, (y + h) as f64);
        ctx.quadratic_curve_to(x as f64, (y + h) as f64, x as f64, (y + h - r) as f64);
        ctx.line_to(x as f64, (y + r) as f64);
        ctx.quadratic_curve_to(x as f64, y as f64, (x + r) as f64, y as f64);
        ctx.close_path();
    }

    /// Frosted-glass panel, canvas-native: a soft outer shadow, layered
    /// translucent body gradient, a top-edge specular sheen, a faint inner
    /// highlight, and a 1px luminous border with corner glow. Everything is
    /// plain gradient/alpha layering — no `ctx.filter` blur, which would be
    /// far too slow to repaint per frame.
    ///
    /// `emphasis` scales the accent (border/corner) luminosity — ~1.0 for
    /// resting panels, higher for selected/featured ones. `tint` scales the
    /// body opacity — 1.0 for solid panels, low values for see-through
    /// surfaces that must not hide the 3D scene behind them. The calm mood
    /// additionally dims all accents via [`Mood::glass`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn glass_panel(
        &self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        accent: Color,
        emphasis: f32,
        tint: f32,
    ) {
        let ctx = &self.hud.ctx;
        let a = self.mood.glass();
        // Soft outer shadow: one slightly enlarged, downward-biased dark
        // fill fakes a blurred drop shadow.
        self.rounded_path(x - 2.0, y - 1.0, w + 4.0, h + 5.0, r + 3.0);
        self.hud.set_fill("rgba(2,3,9,0.30)");
        ctx.fill();
        // Body: deep dark vertical gradient (lighter up top, denser below).
        self.rounded_path(x, y, w, h, r);
        let body = ctx.create_linear_gradient(x as f64, y as f64, x as f64, (y + h) as f64);
        let _ = body.add_color_stop(0.0, &css_rgba(Color::rgb(38, 40, 60).with_alpha(0.62 * tint).into()));
        let _ = body.add_color_stop(0.45, &css_rgba(Color::rgb(21, 22, 34).with_alpha(0.74 * tint).into()));
        let _ = body.add_color_stop(1.0, &css_rgba(Color::rgb(12, 12, 20).with_alpha(0.85 * tint).into()));
        ctx.set_fill_style_canvas_gradient(&body);
        self.hud.note_fill_unknown();
        ctx.fill();
        // Top-edge specular sheen; the body path is still current.
        let sheen_h = (h * 0.42).clamp(8.0, 30.0);
        let sheen = ctx.create_linear_gradient(x as f64, y as f64, x as f64, (y + sheen_h) as f64);
        let _ = sheen.add_color_stop(0.0, &css_rgba([0.92, 0.95, 1.0, 0.10 * a]));
        let _ = sheen.add_color_stop(1.0, "rgba(235,242,255,0)");
        ctx.set_fill_style_canvas_gradient(&sheen);
        ctx.fill();
        // Gentle inner highlight stroke, inset 1px.
        self.rounded_path(
            x + 1.0,
            y + 1.0,
            (w - 2.0).max(1.0),
            (h - 2.0).max(1.0),
            (r - 1.0).max(1.5),
        );
        self.hud.set_stroke(&css_rgba([0.93, 0.95, 1.0, 0.05 * a]));
        ctx.stroke();
        // 1px luminous border.
        let border = ((0.26 + 0.26 * emphasis) * a).min(0.92);
        self.rounded_path(x, y, w, h, r);
        self.hud.set_stroke(&css_rgba(accent.with_alpha(border).into()));
        ctx.stroke();
        // Corner glow: brighter quarter-arcs hugging each rounded corner.
        let glow = (0.55 * emphasis * a).min(0.95);
        self.hud.set_stroke(&css_rgba(accent.with_alpha(glow).into()));
        let cr = r.max(2.0).min(w * 0.5).min(h * 0.5) as f64;
        let half_pi = std::f64::consts::FRAC_PI_2;
        for (cx, cy, start) in [
            (x + cr as f32, y + cr as f32, std::f64::consts::PI),
            (x + w - cr as f32, y + cr as f32, 1.5 * std::f64::consts::PI),
            (x + w - cr as f32, y + h - cr as f32, 0.0),
            (x + cr as f32, y + h - cr as f32, half_pi),
        ] {
            ctx.begin_path();
            let _ = ctx.arc(cx as f64, cy as f64, cr, start, start + half_pi);
            ctx.stroke();
        }
    }

    pub(crate) fn text(&self, text: &str, x: f32, y: f32, px: f32, color: &str, weight: &str) {
        self.hud.set_fill(color);
        self.hud.set_font(px, weight == "bold");
        let _ = self.hud.ctx.fill_text(text, x as f64, y as f64);
    }

    pub(crate) fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32) {
        self.hud.ctx.begin_path();
        self.hud.ctx.move_to(x1 as f64, y1 as f64);
        self.hud.ctx.line_to(x2 as f64, y2 as f64);
        self.hud.ctx.stroke();
    }

    pub(crate) fn css_width(&self) -> f32 {
        self.width as f32 / self.dpr as f32
    }

    pub(crate) fn css_height(&self) -> f32 {
        self.height as f32 / self.dpr as f32
    }
}

pub(crate) struct LaneAction {
    pub(crate) label: &'static str,
    pub(crate) width: f32,
    pub(crate) color: &'static str,
    pub(crate) hit: HitAction,
}

/// One control-center summary tile, derived from the snapshot. Rebuilt
/// only when the underlying state changes, then reused across frames.
pub(crate) struct SystemTarget {
    pub(crate) id: &'static str,
    pub(crate) kicker: &'static str,
    pub(crate) title: &'static str,
    pub(crate) value: String,
    pub(crate) detail: String,
    pub(crate) color: &'static str,
}

impl LaneAction {
    pub(crate) fn select(
        label: &'static str,
        id: &'static str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::Select(id.to_string()),
        }
    }

    pub(crate) fn activity(
        label: &'static str,
        action: &'static str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::ActivityAction {
                action: action.to_string(),
                id: String::new(),
            },
        }
    }

    pub(crate) fn controls(
        label: &'static str,
        action: &'static str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::ControlsAction {
                action: action.to_string(),
            },
        }
    }
}
