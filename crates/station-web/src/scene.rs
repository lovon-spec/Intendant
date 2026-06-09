//! 3D scene: math primitives, camera, node layout, and frame building.

use std::collections::HashMap;
use std::f32::consts::PI;

use web_sys::CanvasRenderingContext2d;

use crate::gpu::GpuFrame;
use crate::model::{StationAgent, StationHost, StationSnapshot};
use crate::util::phase_color;
use crate::util::{
    css_rgba, role_color, stable_angle, stable_unit, Color, C_BLUE, C_GREEN, C_MAUVE, C_PEACH,
    C_RED, C_SAPPHIRE, C_SURFACE0, C_TEAL, C_YELLOW,
};
use crate::StationInner;

impl StationInner {
    /// Refill `self.frame` for this frame, reusing its buffers. `anim_ms`
    /// drives ambient animation phases (frozen at motion 0); `time_ms` is
    /// real time, used for self-expiring event particles.
    pub(crate) fn build_frame(&mut self, anim_ms: f64, time_ms: f64) {
        let mut frame = std::mem::take(&mut self.frame);
        frame.clear();
        let camera = self.camera();
        let aspect = self.width as f32 / self.height.max(1) as f32;
        let fov_deg = self.fov_deg;
        let density = self.density;

        let mut project = move |p: Vec3| camera.project(p, aspect, fov_deg);

        let star_alpha = self.mood.starfield_alpha();
        for star in self.starfield.iter().step_by(self.mood.starfield_stride()) {
            if let Some(p) = project(*star) {
                let s = 0.0045 * density;
                frame.add_quad_ndc(p.x, p.y, s, [0.35, 0.36, 0.44, star_alpha]);
            }
        }

        self.add_grid(&mut frame, &mut project);
        self.add_operator(&mut frame, &mut project, anim_ms);

        for host in &self.snapshot.hosts {
            let id = format!("host:{}", host.id);
            if let Some(pos) = self.layout_cache.get(&id).copied() {
                self.add_host(&mut frame, host, pos, &mut project, anim_ms);
            }
        }
        for agent in &self.snapshot.agents {
            if let Some(pos) = self.layout_cache.get(&agent.id).copied() {
                self.add_agent(&mut frame, agent, pos, &mut project, anim_ms);
            }
        }

        let layout = &self.layout_cache;
        for agent in &self.snapshot.agents {
            let Some(a_pos) = layout.get(&agent.id).copied() else {
                continue;
            };
            let host_id = format!("host:{}", agent.host_id);
            if let Some(parent_id) = agent.parent_id.as_ref().filter(|p| !p.is_empty()) {
                if let Some(p_pos) = layout.get(parent_id).copied() {
                    frame.add_line_projected(
                        &mut project,
                        p_pos,
                        a_pos,
                        role_color(&agent.role).with_alpha(0.54),
                    );
                    continue;
                }
            }
            if let Some(h_pos) = layout.get(&host_id).copied() {
                frame.add_line_projected(
                    &mut project,
                    h_pos,
                    a_pos,
                    role_color(&agent.role).with_alpha(0.42),
                );
            }
        }
        for host in &self.snapshot.hosts {
            let id = format!("host:{}", host.id);
            if let Some(pos) = layout.get(&id).copied() {
                frame.add_line_projected(&mut project, Vec3::ZERO, pos, C_BLUE.with_alpha(0.26));
            }
        }

        self.particles.retain(|particle| {
            let t = ((time_ms - particle.born_ms) as f32 / particle.ttl_ms as f32).clamp(0.0, 1.0);
            if t >= 1.0 {
                return false;
            }
            let lifted =
                particle.start.lerp(particle.end, t) + Vec3::new(0.0, (t * PI).sin() * 0.6, 0.0);
            if let Some(p) = project(lifted) {
                let size = (0.026 * (1.0 - t) + 0.006) * density;
                frame.add_quad_ndc(
                    p.x,
                    p.y,
                    size,
                    particle.color.with_alpha(0.88 * (1.0 - t)).into(),
                );
            }
            true
        });

        self.frame = frame;
    }

    pub(crate) fn add_grid(
        &self,
        frame: &mut GpuFrame,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
    ) {
        let grid = 9;
        for i in -grid..=grid {
            let v = i as f32;
            let alpha = if i == 0 { 0.33 } else { 0.16 };
            frame.add_line_projected(
                project,
                Vec3::new(-9.0, -1.8, v),
                Vec3::new(9.0, -1.8, v),
                C_SURFACE0.with_alpha(alpha),
            );
            frame.add_line_projected(
                project,
                Vec3::new(v, -1.8, -9.0),
                Vec3::new(v, -1.8, 9.0),
                C_SURFACE0.with_alpha(alpha),
            );
        }
    }

    pub(crate) fn add_operator(
        &self,
        frame: &mut GpuFrame,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        time_ms: f64,
    ) {
        let pos = self.layout_cache.get("op").copied().unwrap_or(Vec3::ZERO);
        let spin = time_ms as f32 * 0.00032 * self.motion;
        let glow = self.mood.glow();
        frame.add_wire_octa(project, pos, 0.48, spin, C_BLUE.with_alpha(0.95));
        frame.add_ring(
            project,
            pos,
            0.82,
            C_SAPPHIRE.with_alpha(0.55 * glow),
            Plane::XZ,
        );
        frame.add_ring(
            project,
            pos,
            1.18,
            C_BLUE.with_alpha(0.18 * glow),
            Plane::XZ,
        );
        if let Some(p) = project(pos) {
            frame.projected_nodes.push(ProjectedNode::new(
                "op",
                NodeKind::Operator,
                p,
                18.0 * self.density,
            ));
        }
    }

    pub(crate) fn add_host(
        &self,
        frame: &mut GpuFrame,
        host: &StationHost,
        pos: Vec3,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        time_ms: f64,
    ) {
        let id = format!("host:{}", host.id);
        let spin = time_ms as f32 * 0.00011 * self.motion + stable_angle(&host.id);
        frame.add_wire_hex(
            project,
            pos,
            0.58,
            0.28,
            spin,
            C_PEACH.with_alpha(if host.connected { 0.9 } else { 0.38 }),
        );
        frame.add_ring(
            project,
            pos + Vec3::new(0.0, -0.17, 0.0),
            0.82 + (time_ms as f32 * 0.003).sin() * 0.035 * self.mood.pulse(),
            C_PEACH.with_alpha(0.28 * self.mood.glow()),
            Plane::XZ,
        );
        if let Some(p) = project(pos) {
            frame.projected_nodes.push(ProjectedNode::new(
                &id,
                NodeKind::Host,
                p,
                21.0 * self.density,
            ));
        }
    }

    pub(crate) fn add_agent(
        &self,
        frame: &mut GpuFrame,
        agent: &StationAgent,
        pos: Vec3,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        time_ms: f64,
    ) {
        let role = role_color(&agent.role);
        let phase = phase_color(&agent.phase);
        let spin = time_ms as f32 * 0.0005 * self.motion + stable_angle(&agent.id);
        match agent.role.as_str() {
            "orchestrator" => frame.add_wire_octa(project, pos, 0.34, spin, role.with_alpha(0.96)),
            "sub-agent" => frame.add_wire_tetra(project, pos, 0.31, spin, role.with_alpha(0.95)),
            _ => frame.add_wire_icosa(project, pos, 0.31, spin, role.with_alpha(0.95)),
        }
        let pct = if agent.token_cap > 0.0 {
            (agent.tokens / agent.token_cap).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let budget = if pct < 0.5 {
            C_GREEN
        } else if pct < 0.85 {
            C_YELLOW
        } else {
            C_RED
        };
        frame.add_ring(project, pos, 0.56, budget.with_alpha(0.66), Plane::XY);
        frame.add_ring(project, pos, 0.38, phase.with_alpha(0.2), Plane::YZ);
        if agent.status == "in_progress" || agent.phase == "running" {
            frame.add_ring(
                project,
                pos,
                0.72 + (time_ms as f32 * 0.004).sin() * 0.05 * self.mood.pulse(),
                C_TEAL.with_alpha(0.22 * self.mood.glow()),
                Plane::XY,
            );
        }
        if agent.needs_approval {
            frame.add_ring(
                project,
                pos,
                0.84 + (time_ms as f32 * 0.006).sin() * 0.07 * self.mood.pulse(),
                C_YELLOW.with_alpha(0.58),
                Plane::XY,
            );
        }
        if self.selected_id.as_deref() == Some(&agent.id) {
            frame.add_ring(project, pos, 0.96, C_BLUE.with_alpha(0.84), Plane::XY);
        }
        if let Some(parent_id) = agent.parent_id.as_ref().filter(|s| !s.is_empty()) {
            if let Some(parent) = self.layout_cache.get(parent_id).copied() {
                frame.add_line_projected(project, parent, pos, C_MAUVE.with_alpha(0.5));
            }
        }
        if let Some(p) = project(pos) {
            frame.projected_nodes.push(ProjectedNode::new(
                &agent.id,
                NodeKind::Agent,
                p,
                15.0 * self.density,
            ));
        }
    }

    /// Stroke the frame's projected line list into a 2D context: the scene
    /// canvas when WebGPU is off, or the HUD canvas (as an underlay) when the
    /// scene canvas was consumed by a failed WebGPU init. Consecutive
    /// same-color segments share one path, and the stroke style is only
    /// touched when the color changes.
    pub(crate) fn draw_scene_lines(&self, ctx: &CanvasRenderingContext2d) {
        ctx.set_fill_style_str("rgba(17,17,27,0.94)");
        ctx.fill_rect(0.0, 0.0, self.width as f64, self.height as f64);
        let mut current: Option<[f32; 4]> = None;
        let mut open = false;
        for pair in self.frame.line_vertices.chunks_exact(2) {
            if current != Some(pair[0].color) {
                if open {
                    ctx.stroke();
                }
                ctx.set_stroke_style_str(&css_rgba(pair[0].color));
                ctx.begin_path();
                current = Some(pair[0].color);
                open = true;
            }
            let a = ndc_to_screen(pair[0].pos, self.width, self.height);
            let b = ndc_to_screen(pair[1].pos, self.width, self.height);
            ctx.move_to(a.x as f64, a.y as f64);
            ctx.line_to(b.x as f64, b.y as f64);
        }
        if open {
            ctx.stroke();
        }
    }

    pub(crate) fn camera(&self) -> Camera {
        let parallax = Vec3::new(
            self.ar_x * self.ar_strength,
            self.ar_y * self.ar_strength * 0.5,
            0.0,
        );
        let cp = self.pitch.cos();
        let eye = Vec3::new(
            self.yaw.sin() * cp * self.distance,
            self.pitch.sin() * self.distance + 3.2,
            self.yaw.cos() * cp * self.distance,
        ) + parallax;
        Camera::look_at(eye, Vec3::new(0.0, 0.25, 0.0), Vec3::Y)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LayoutName {
    Orbital,
    Constellation,
}

impl LayoutName {
    pub(crate) fn from_str(s: &str) -> Self {
        match s {
            "constellation" => Self::Constellation,
            _ => Self::Orbital,
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Orbital => "orbital",
            Self::Constellation => "constellation",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mood {
    Cockpit,
    Calm,
}

impl Mood {
    pub(crate) fn from_str(s: &str) -> Self {
        match s {
            "calm" => Self::Calm,
            _ => Self::Cockpit,
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Cockpit => "cockpit",
            Self::Calm => "calm",
        }
    }

    /// Starfield quad alpha: calm dims the backdrop.
    pub(crate) fn starfield_alpha(self) -> f32 {
        match self {
            Self::Cockpit => 0.55,
            Self::Calm => 0.32,
        }
    }

    /// Starfield sampling stride: calm draws every other star.
    pub(crate) fn starfield_stride(self) -> usize {
        match self {
            Self::Cockpit => 1,
            Self::Calm => 2,
        }
    }

    /// Amplitude scale for breathing/pulse animations.
    pub(crate) fn pulse(self) -> f32 {
        match self {
            Self::Cockpit => 1.0,
            Self::Calm => 0.45,
        }
    }

    /// Alpha scale for decorative (non-semantic) glow rings.
    pub(crate) fn glow(self) -> f32 {
        match self {
            Self::Cockpit => 1.0,
            Self::Calm => 0.65,
        }
    }

    /// Radial vignette color stops; calm is softer and less saturated.
    pub(crate) fn vignette_stops(self) -> [(f64, &'static str); 3] {
        match self {
            Self::Cockpit => [
                (0.0, "rgba(30,30,46,0.04)"),
                (0.75, "rgba(17,17,27,0.16)"),
                (1.0, "rgba(4,4,9,0.48)"),
            ],
            Self::Calm => [
                (0.0, "rgba(30,30,46,0.03)"),
                (0.75, "rgba(17,17,27,0.10)"),
                (1.0, "rgba(4,4,9,0.36)"),
            ],
        }
    }
}

pub(crate) struct Particle {
    pub(crate) start: Vec3,
    pub(crate) end: Vec3,
    pub(crate) born_ms: f64,
    pub(crate) ttl_ms: f64,
    pub(crate) color: Color,
}

#[derive(Clone)]
pub(crate) struct ProjectedNode {
    pub(crate) id: String,
    pub(crate) kind: NodeKind,
    pub(crate) ndc: Vec2,
    pub(crate) radius: f32,
}

impl ProjectedNode {
    pub(crate) fn new(id: &str, kind: NodeKind, ndc: Vec2, radius: f32) -> Self {
        Self {
            id: id.to_string(),
            kind,
            ndc,
            radius,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeKind {
    Operator,
    Host,
    Agent,
}

#[derive(Clone, Copy)]
pub(crate) enum Plane {
    XY,
    XZ,
    YZ,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Vec2 {
    pub(crate) x: f32,
    pub(crate) y: f32,
}

impl Vec2 {
    pub(crate) fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Vec3 {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) z: f32,
}

impl Vec3 {
    pub(crate) const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
    pub(crate) const Y: Self = Self {
        x: 0.0,
        y: 1.0,
        z: 0.0,
    };

    pub(crate) fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    pub(crate) fn dot(self, rhs: Self) -> f32 {
        self.x * rhs.x + self.y * rhs.y + self.z * rhs.z
    }

    pub(crate) fn cross(self, rhs: Self) -> Self {
        Self {
            x: self.y * rhs.z - self.z * rhs.y,
            y: self.z * rhs.x - self.x * rhs.z,
            z: self.x * rhs.y - self.y * rhs.x,
        }
    }

    pub(crate) fn len(self) -> f32 {
        self.dot(self).sqrt()
    }

    pub(crate) fn normalized(self) -> Self {
        let len = self.len();
        if len < 0.0001 {
            Self::ZERO
        } else {
            self * (1.0 / len)
        }
    }

    pub(crate) fn lerp(self, rhs: Self, t: f32) -> Self {
        self * (1.0 - t) + rhs * t
    }
}

impl std::ops::Add for Vec3 {
    type Output = Self;
    fn add(self, rhs: Self) -> Self::Output {
        Self::new(self.x + rhs.x, self.y + rhs.y, self.z + rhs.z)
    }
}

impl std::ops::Sub for Vec3 {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self::Output {
        Self::new(self.x - rhs.x, self.y - rhs.y, self.z - rhs.z)
    }
}

impl std::ops::Mul<f32> for Vec3 {
    type Output = Self;
    fn mul(self, rhs: f32) -> Self::Output {
        Self::new(self.x * rhs, self.y * rhs, self.z * rhs)
    }
}

pub(crate) struct Camera {
    pub(crate) eye: Vec3,
    pub(crate) right: Vec3,
    pub(crate) up: Vec3,
    pub(crate) forward: Vec3,
}

impl Camera {
    pub(crate) fn look_at(eye: Vec3, target: Vec3, world_up: Vec3) -> Self {
        let forward = (target - eye).normalized();
        let right = forward.cross(world_up).normalized();
        let up = right.cross(forward).normalized();
        Self {
            eye,
            right,
            up,
            forward,
        }
    }

    pub(crate) fn project(&self, world: Vec3, aspect: f32, fov_deg: f32) -> Option<Vec2> {
        let p = world - self.eye;
        let z = p.dot(self.forward);
        if z <= 0.12 {
            return None;
        }
        let x = p.dot(self.right);
        let y = p.dot(self.up);
        let f = 1.0 / (fov_deg.to_radians() * 0.5).tan();
        let ndc_x = (x * f / aspect) / z;
        let ndc_y = (y * f) / z;
        if ndc_x.abs() > 2.2 || ndc_y.abs() > 2.2 {
            return None;
        }
        Some(Vec2::new(ndc_x, ndc_y))
    }
}

pub(crate) fn rotate_y(v: Vec3, a: f32) -> Vec3 {
    let (s, c) = a.sin_cos();
    Vec3::new(v.x * c + v.z * s, v.y, -v.x * s + v.z * c)
}

pub(crate) fn rotate_x(v: Vec3, a: f32) -> Vec3 {
    let (s, c) = a.sin_cos();
    Vec3::new(v.x, v.y * c - v.z * s, v.y * s + v.z * c)
}

pub(crate) fn ndc_to_screen(pos: [f32; 2], width: u32, height: u32) -> Vec2 {
    Vec2::new(
        (pos[0] * 0.5 + 0.5) * width as f32,
        (0.5 - pos[1] * 0.5) * height as f32,
    )
}

/// World position per node id ("op", "host:<id>", agent ids) for the given
/// layout. Pure: depends only on the snapshot and layout, so callers cache
/// the result per (snapshot, layout) change.
pub(crate) fn layout_positions(
    snapshot: &StationSnapshot,
    layout: LayoutName,
) -> HashMap<String, Vec3> {
    let mut map = HashMap::new();
    map.insert("op".to_string(), Vec3::ZERO);
    let host_count = snapshot.hosts.len().max(1);
    for (i, host) in snapshot.hosts.iter().enumerate() {
        let t = i as f32 / host_count as f32;
        let pos = match layout {
            LayoutName::Orbital => {
                let angle = t * PI * 2.0 + PI * 0.08;
                let radius = 4.2 + (host_count as f32 * 0.18).min(1.3);
                Vec3::new(angle.cos() * radius, 0.0, angle.sin() * radius)
            }
            LayoutName::Constellation => {
                let spread = (host_count as f32 - 1.0).max(1.0);
                let x = (i as f32 - spread * 0.5) * 3.2;
                let z = -1.3 + (stable_unit(&host.id) - 0.5) * 2.3;
                Vec3::new(
                    x,
                    -0.05 + (stable_unit(&(host.id.clone() + "y")) - 0.5) * 0.8,
                    z,
                )
            }
        };
        map.insert(format!("host:{}", host.id), pos);
    }
    let mut by_host: HashMap<&str, Vec<&StationAgent>> = HashMap::new();
    for agent in &snapshot.agents {
        by_host
            .entry(agent.host_id.as_str())
            .or_default()
            .push(agent);
    }
    for host in &snapshot.hosts {
        let host_pos = map
            .get(&format!("host:{}", host.id))
            .copied()
            .unwrap_or(Vec3::ZERO);
        let agents = by_host.get(host.id.as_str()).cloned().unwrap_or_default();
        let count = agents.len().max(1);
        for (idx, agent) in agents.into_iter().enumerate() {
            let pos = match layout {
                LayoutName::Orbital => {
                    let angle = idx as f32 / count as f32 * PI * 2.0 + stable_angle(&agent.id);
                    let ring = if agent.role == "sub-agent" {
                        1.55
                    } else {
                        1.18
                    };
                    host_pos
                        + Vec3::new(
                            angle.cos() * ring,
                            0.55 + (idx % 3) as f32 * 0.28,
                            angle.sin() * ring * 0.72,
                        )
                }
                LayoutName::Constellation => {
                    let u = stable_unit(&agent.id);
                    let v = stable_unit(&(agent.id.clone() + "v"));
                    host_pos
                        + Vec3::new(
                            (u - 0.5) * 2.9,
                            0.7 + v * 1.8,
                            (stable_unit(&(agent.id.clone() + "z")) - 0.5) * 2.0,
                        )
                }
            };
            map.insert(agent.id.clone(), pos);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StationAgent;

    fn snapshot() -> StationSnapshot {
        StationSnapshot {
            hosts: vec![
                StationHost {
                    id: "alpha".into(),
                    ..Default::default()
                },
                StationHost {
                    id: "beta".into(),
                    ..Default::default()
                },
            ],
            agents: vec![
                StationAgent {
                    id: "agent-1".into(),
                    host_id: "alpha".into(),
                    ..Default::default()
                },
                StationAgent {
                    id: "agent-2".into(),
                    host_id: "alpha".into(),
                    role: "sub-agent".into(),
                    ..Default::default()
                },
                StationAgent {
                    id: "agent-3".into(),
                    host_id: "beta".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn assert_same_positions(a: &HashMap<String, Vec3>, b: &HashMap<String, Vec3>) {
        assert_eq!(a.len(), b.len());
        for (key, pa) in a {
            let pb = b.get(key).unwrap_or_else(|| panic!("missing key {key}"));
            assert_eq!(
                (pa.x.to_bits(), pa.y.to_bits(), pa.z.to_bits()),
                (pb.x.to_bits(), pb.y.to_bits(), pb.z.to_bits()),
                "position differs for {key}"
            );
        }
    }

    #[test]
    fn layout_positions_is_deterministic() {
        let snapshot = snapshot();
        for layout in [LayoutName::Orbital, LayoutName::Constellation] {
            let a = layout_positions(&snapshot, layout);
            let b = layout_positions(&snapshot, layout);
            assert_same_positions(&a, &b);
        }
    }

    #[test]
    fn layout_positions_covers_every_node() {
        let snapshot = snapshot();
        let map = layout_positions(&snapshot, LayoutName::Orbital);
        assert!(map.contains_key("op"));
        assert!(map.contains_key("host:alpha"));
        assert!(map.contains_key("host:beta"));
        for agent in &snapshot.agents {
            assert!(map.contains_key(&agent.id), "missing {}", agent.id);
        }
        assert_eq!(map.len(), 1 + snapshot.hosts.len() + snapshot.agents.len());
    }

    #[test]
    fn layouts_actually_differ() {
        let snapshot = snapshot();
        let orbital = layout_positions(&snapshot, LayoutName::Orbital);
        let constellation = layout_positions(&snapshot, LayoutName::Constellation);
        let a = orbital.get("host:alpha").unwrap();
        let b = constellation.get("host:alpha").unwrap();
        assert!(
            (a.x - b.x).abs() > 1e-6 || (a.y - b.y).abs() > 1e-6 || (a.z - b.z).abs() > 1e-6,
            "orbital and constellation should place hosts differently"
        );
    }

    #[test]
    fn camera_projects_target_near_center_and_culls_behind() {
        let eye = Vec3::new(0.0, 0.0, 10.0);
        let camera = Camera::look_at(eye, Vec3::ZERO, Vec3::Y);
        let center = camera.project(Vec3::ZERO, 16.0 / 9.0, 55.0).unwrap();
        assert!(center.x.abs() < 1e-5 && center.y.abs() < 1e-5);
        // A point behind the camera must be culled.
        assert!(camera
            .project(Vec3::new(0.0, 0.0, 20.0), 16.0 / 9.0, 55.0)
            .is_none());
    }

    #[test]
    fn vec3_math_basics() {
        let v = Vec3::new(3.0, 0.0, 4.0);
        assert_eq!(v.len(), 5.0);
        let n = v.normalized();
        assert!((n.len() - 1.0).abs() < 1e-6);
        assert_eq!(Vec3::ZERO.normalized().len(), 0.0);
        let lerped = Vec3::ZERO.lerp(Vec3::new(2.0, 2.0, 2.0), 0.5);
        assert_eq!((lerped.x, lerped.y, lerped.z), (1.0, 1.0, 1.0));
        let cross = Vec3::new(1.0, 0.0, 0.0).cross(Vec3::new(0.0, 1.0, 0.0));
        assert_eq!((cross.x, cross.y, cross.z), (0.0, 0.0, 1.0));
    }

    #[test]
    fn rotations_preserve_length() {
        let v = Vec3::new(1.0, 2.0, 3.0);
        assert!((rotate_y(v, 1.3).len() - v.len()).abs() < 1e-5);
        assert!((rotate_x(v, -0.7).len() - v.len()).abs() < 1e-5);
    }

    #[test]
    fn ndc_to_screen_maps_corners() {
        let top_left = ndc_to_screen([-1.0, 1.0], 200, 100);
        assert_eq!((top_left.x, top_left.y), (0.0, 0.0));
        let bottom_right = ndc_to_screen([1.0, -1.0], 200, 100);
        assert_eq!((bottom_right.x, bottom_right.y), (200.0, 100.0));
        let center = ndc_to_screen([0.0, 0.0], 200, 100);
        assert_eq!((center.x, center.y), (100.0, 50.0));
    }

    #[test]
    fn mood_parsing_and_factors() {
        assert_eq!(Mood::from_str("calm"), Mood::Calm);
        assert_eq!(Mood::from_str("anything"), Mood::Cockpit);
        assert!(Mood::Calm.starfield_alpha() < Mood::Cockpit.starfield_alpha());
        assert!(Mood::Calm.pulse() < Mood::Cockpit.pulse());
        assert!(Mood::Calm.glow() < Mood::Cockpit.glow());
        assert_eq!(Mood::Calm.starfield_stride(), 2);
        assert_eq!(
            LayoutName::from_str("constellation"),
            LayoutName::Constellation
        );
        assert_eq!(LayoutName::from_str("bogus"), LayoutName::Orbital);
    }
}
