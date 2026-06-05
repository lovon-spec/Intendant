use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::f32::consts::PI;
use std::rc::Rc;

use bytemuck::{Pod, Zeroable};
use serde::Deserialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{
    CanvasRenderingContext2d, DeviceOrientationEvent, Event, HtmlCanvasElement, HtmlVideoElement,
    KeyboardEvent, PointerEvent, WheelEvent,
};
use wgpu::util::DeviceExt;

#[wasm_bindgen]
pub struct StationWeb {
    inner: Rc<RefCell<StationInner>>,
}

#[wasm_bindgen]
impl StationWeb {
    #[wasm_bindgen(constructor)]
    pub fn new(scene_canvas: HtmlCanvasElement, hud_canvas: HtmlCanvasElement) -> Result<StationWeb, JsValue> {
        console_error_panic_hook::set_once();
        let ctx = hud_canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("Station HUD canvas has no 2D context"))?
            .dyn_into::<CanvasRenderingContext2d>()?;
        let scene_ctx = scene_canvas
            .get_context("2d")?
            .and_then(|ctx| ctx.dyn_into::<CanvasRenderingContext2d>().ok());

        let inner = Rc::new(RefCell::new(StationInner::new(scene_canvas, hud_canvas, ctx, scene_ctx)));
        StationInner::install_events(inner.clone())?;
        StationInner::start_gpu(inner.clone());
        StationInner::start_loop(inner.clone());
        Ok(Self { inner })
    }

    pub fn set_active(&self, active: bool) {
        self.inner.borrow_mut().active = active;
    }

    pub fn set_action_callback(&self, callback: js_sys::Function) {
        self.inner.borrow_mut().action_callback = Some(callback);
    }

    pub fn resize(&self) {
        self.inner.borrow_mut().resize();
    }

    pub fn update_snapshot(&self, snapshot: JsValue) -> Result<(), JsValue> {
        let snapshot: StationSnapshot = serde_wasm_bindgen::from_value(snapshot)?;
        self.inner.borrow_mut().apply_snapshot(snapshot);
        Ok(())
    }

    pub fn register_display_source(
        &self,
        source_id: String,
        host_id: String,
        display_id: String,
        label: String,
        kind: String,
        video: HtmlVideoElement,
    ) {
        self.inner.borrow_mut().display_sources.insert(
            source_id,
            DisplaySource {
                host_id,
                display_id,
                label,
                kind,
                video,
            },
        );
    }

    pub fn unregister_display_source(&self, source_id: String) {
        self.inner.borrow_mut().display_sources.remove(&source_id);
    }

    pub fn set_layout(&self, layout: String) {
        self.inner.borrow_mut().layout = LayoutName::from_str(&layout);
    }

    pub fn select_by_id(&self, id: Option<String>) {
        let mut inner = self.inner.borrow_mut();
        inner.selected_id = id;
        inner.panel_scroll = 0.0;
    }

    pub fn focus_on(&self, id: String) {
        self.inner.borrow_mut().focus_id = Some(id);
    }

    pub fn debug_state(&self) -> String {
        let inner = self.inner.borrow();
        format!(
            "station hosts={} agents={} events={} displays={} gpu={}",
            inner.snapshot.hosts.len(),
            inner.snapshot.agents.len(),
            inner.snapshot.events.len(),
            inner.display_sources.len(),
            inner.gpu.is_some(),
        )
    }
}

struct StationInner {
    scene_canvas: HtmlCanvasElement,
    hud_canvas: HtmlCanvasElement,
    ctx: CanvasRenderingContext2d,
    scene_ctx: Option<CanvasRenderingContext2d>,
    gpu: Option<GpuState>,
    active: bool,
    width: u32,
    height: u32,
    dpr: f64,
    snapshot: StationSnapshot,
    display_sources: HashMap<String, DisplaySource>,
    particles: Vec<Particle>,
    seen_events: HashSet<String>,
    starfield: Vec<Vec3>,
    layout: LayoutName,
    mood: Mood,
    fov_deg: f32,
    motion: f32,
    ar_strength: f32,
    density: f32,
    yaw: f32,
    pitch: f32,
    distance: f32,
    auto_orbit: bool,
    last_input_ms: f64,
    selected_id: Option<String>,
    hovered_id: Option<String>,
    focus_id: Option<String>,
    pointer_down: Option<PointerDrag>,
    drag_slider: Option<SliderKind>,
    ar_x: f32,
    ar_y: f32,
    panel_scroll: f32,
    projected_nodes: Vec<ProjectedNode>,
    hit_zones: Vec<HitZone>,
    action_callback: Option<js_sys::Function>,
    _events: Vec<Closure<dyn FnMut(Event)>>,
    _raf: Option<Closure<dyn FnMut(f64)>>,
    boot_started_ms: f64,
}

impl StationInner {
    fn new(
        scene_canvas: HtmlCanvasElement,
        hud_canvas: HtmlCanvasElement,
        ctx: CanvasRenderingContext2d,
        scene_ctx: Option<CanvasRenderingContext2d>,
    ) -> Self {
        let mut starfield = Vec::with_capacity(480);
        let mut seed = 0x51a7_10cdu32;
        for _ in 0..480 {
            seed = lcg(seed);
            let th = unit(seed) * PI * 2.0;
            seed = lcg(seed);
            let ph = (2.0 * unit(seed) - 1.0).acos();
            seed = lcg(seed);
            let r = 18.0 + unit(seed) * 16.0;
            starfield.push(Vec3::new(
                r * ph.sin() * th.cos(),
                r * ph.cos() * 0.62,
                r * ph.sin() * th.sin(),
            ));
        }

        let mut inner = Self {
            scene_canvas,
            hud_canvas,
            ctx,
            scene_ctx,
            gpu: None,
            active: false,
            width: 1,
            height: 1,
            dpr: 1.0,
            snapshot: StationSnapshot::default(),
            display_sources: HashMap::new(),
            particles: Vec::new(),
            seen_events: HashSet::new(),
            starfield,
            layout: LayoutName::Orbital,
            mood: Mood::Cockpit,
            fov_deg: 55.0,
            motion: 1.0,
            ar_strength: 0.45,
            density: 1.0,
            yaw: 0.58,
            pitch: 0.42,
            distance: 11.0,
            auto_orbit: true,
            last_input_ms: now_ms(),
            selected_id: None,
            hovered_id: None,
            focus_id: None,
            pointer_down: None,
            drag_slider: None,
            ar_x: 0.0,
            ar_y: 0.0,
            panel_scroll: 0.0,
            projected_nodes: Vec::new(),
            hit_zones: Vec::new(),
            action_callback: None,
            _events: Vec::new(),
            _raf: None,
            boot_started_ms: now_ms(),
        };
        inner.resize();
        inner
    }

    fn install_events(inner: Rc<RefCell<Self>>) -> Result<(), JsValue> {
        let target: web_sys::EventTarget = inner.borrow().hud_canvas.clone().into();
        let window = web_sys::window().ok_or_else(|| JsValue::from_str("window unavailable"))?;

        let down_inner = inner.clone();
        let down = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<PointerEvent>() else { return; };
            e.prevent_default();
            let mut s = down_inner.borrow_mut();
            s.mark_input();
            let (x, y) = s.event_xy(e.client_x() as f64, e.client_y() as f64);
            if let Some(action) = s.hit_action_at(x, y) {
                match action {
                    HitAction::Slider(kind) => {
                        s.drag_slider = Some(kind);
                        s.apply_slider_at(kind, x);
                    }
                    _ => {
                        s.pointer_down = Some(PointerDrag {
                            x,
                            y,
                            last_x: x,
                            last_y: y,
                            moved: false,
                            pending_action: Some(action),
                        });
                    }
                }
                return;
            }
            s.pointer_down = Some(PointerDrag {
                x,
                y,
                last_x: x,
                last_y: y,
                moved: false,
                pending_action: None,
            });
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("pointerdown", down.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(down);

        let move_inner = inner.clone();
        let mv = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<PointerEvent>() else { return; };
            let mut s = move_inner.borrow_mut();
            let (x, y) = s.event_xy(e.client_x() as f64, e.client_y() as f64);
            if let Some(kind) = s.drag_slider {
                s.apply_slider_at(kind, x);
                return;
            }
            if let Some(drag) = s.pointer_down.as_mut() {
                let dx = x - drag.last_x;
                let dy = y - drag.last_y;
                drag.last_x = x;
                drag.last_y = y;
                if (x - drag.x).abs() + (y - drag.y).abs() > 4.0 {
                    drag.moved = true;
                    drag.pending_action = None;
                }
                s.yaw -= dx * 0.006;
                s.pitch = (s.pitch + dy * 0.005).clamp(-1.05, 1.05);
                s.mark_input();
            } else {
                s.hovered_id = s.pick_node(x, y);
            }
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("pointermove", mv.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(mv);

        let up_inner = inner.clone();
        let up = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<PointerEvent>() else { return; };
            e.prevent_default();
            let mut s = up_inner.borrow_mut();
            let (x, y) = s.event_xy(e.client_x() as f64, e.client_y() as f64);
            if s.drag_slider.take().is_some() {
                return;
            }
            if let Some(drag) = s.pointer_down.take() {
                if let Some(action) = drag.pending_action {
                    s.dispatch_hit(action, x, y);
                } else if !drag.moved {
                    s.selected_id = s.pick_node(x, y);
                    s.panel_scroll = 0.0;
                }
            }
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("pointerup", up.as_ref().unchecked_ref())?;
        target.add_event_listener_with_callback("pointercancel", up.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(up);

        let wheel_inner = inner.clone();
        let wheel = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<WheelEvent>() else { return; };
            e.prevent_default();
            let mut s = wheel_inner.borrow_mut();
            s.mark_input();
            if s.panel_hit(e.client_x() as f64, e.client_y() as f64) {
                s.panel_scroll = (s.panel_scroll + e.delta_y() as f32).max(0.0);
            } else {
                s.distance = (s.distance + e.delta_y() as f32 * 0.014).clamp(4.2, 25.0);
            }
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("wheel", wheel.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(wheel);

        let key_inner = inner.clone();
        let key = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<KeyboardEvent>() else { return; };
            let mut s = key_inner.borrow_mut();
            if !s.active {
                return;
            }
            let mut used = true;
            match e.key().as_str() {
                "ArrowLeft" | "a" | "A" => s.yaw += 0.08,
                "ArrowRight" | "d" | "D" => s.yaw -= 0.08,
                "ArrowUp" | "w" | "W" => s.pitch = (s.pitch - 0.06).clamp(-1.05, 1.05),
                "ArrowDown" | "s" | "S" => s.pitch = (s.pitch + 0.06).clamp(-1.05, 1.05),
                "+" | "=" => s.distance = (s.distance - 0.6).clamp(4.2, 25.0),
                "-" | "_" => s.distance = (s.distance + 0.6).clamp(4.2, 25.0),
                "Escape" => s.selected_id = None,
                "1" => s.layout = LayoutName::Orbital,
                "2" => s.layout = LayoutName::Constellation,
                _ => used = false,
            }
            if used {
                e.prevent_default();
                s.mark_input();
            }
        }) as Box<dyn FnMut(_)>);
        window.add_event_listener_with_callback("keydown", key.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(key);

        let orientation_inner = inner.clone();
        let orientation = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<DeviceOrientationEvent>() else { return; };
            let mut s = orientation_inner.borrow_mut();
            let gamma = e.gamma().unwrap_or(0.0) as f32;
            let beta = e.beta().unwrap_or(0.0) as f32;
            s.ar_x = (gamma / 45.0).clamp(-1.0, 1.0);
            s.ar_y = (beta / 60.0).clamp(-1.0, 1.0);
        }) as Box<dyn FnMut(_)>);
        window.add_event_listener_with_callback("deviceorientation", orientation.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(orientation);

        let resize_inner = inner.clone();
        let resize = Closure::wrap(Box::new(move |_event: Event| {
            resize_inner.borrow_mut().resize();
        }) as Box<dyn FnMut(_)>);
        window.add_event_listener_with_callback("resize", resize.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(resize);

        Ok(())
    }

    fn start_gpu(inner: Rc<RefCell<Self>>) {
        let canvas = inner.borrow().scene_canvas.clone();
        spawn_local(async move {
            match GpuState::new(canvas).await {
                Ok(gpu) => {
                    inner.borrow_mut().gpu = Some(gpu);
                }
                Err(err) => {
                    web_sys::console::warn_1(&JsValue::from_str(&format!(
                        "Station WebGPU unavailable, using HUD canvas fallback: {err:?}"
                    )));
                }
            }
        });
    }

    fn start_loop(inner: Rc<RefCell<Self>>) {
        let loop_inner = inner.clone();
        let cb = Closure::wrap(Box::new(move |time_ms: f64| {
            {
                let mut s = loop_inner.borrow_mut();
                s.render(time_ms);
            }
            let next = loop_inner.borrow()._raf.as_ref().map(|cb| {
                cb.as_ref().unchecked_ref::<js_sys::Function>().clone()
            });
            if let Some(next) = next {
                if let Some(window) = web_sys::window() {
                    let _ = window.request_animation_frame(&next);
                }
            }
        }) as Box<dyn FnMut(f64)>);

        request_animation_frame(&cb);
        inner.borrow_mut()._raf = Some(cb);
    }

    fn apply_snapshot(&mut self, snapshot: StationSnapshot) {
        for event in &snapshot.events {
            if self.seen_events.insert(event.id.clone()) {
                let positions = self.layout_positions();
                let start = event
                    .agent_id
                    .as_ref()
                    .and_then(|id| positions.get(id))
                    .copied()
                    .or_else(|| positions.get(&format!("host:{}", event.host_id)).copied())
                    .unwrap_or(Vec3::ZERO);
                let end = event
                    .host_id
                    .is_empty()
                    .then_some(Vec3::ZERO)
                    .or_else(|| positions.get(&format!("host:{}", event.host_id)).copied())
                    .unwrap_or(Vec3::ZERO);
                self.particles.push(Particle {
                    start,
                    end,
                    born_ms: now_ms(),
                    ttl_ms: 1700.0,
                    color: level_color(&event.level),
                });
            }
        }
        self.snapshot = snapshot;
        if self.selected_id.as_ref().is_some_and(|id| !self.node_exists(id)) {
            self.selected_id = None;
        }
    }

    fn node_exists(&self, id: &str) -> bool {
        id == "op"
            || self.snapshot.hosts.iter().any(|h| format!("host:{}", h.id) == id)
            || self.snapshot.agents.iter().any(|a| a.id == id)
    }

    fn resize(&mut self) {
        let dpr = web_sys::window()
            .and_then(|w| Some(w.device_pixel_ratio()))
            .unwrap_or(1.0)
            .clamp(1.0, 2.0);
        let css_w = self.hud_canvas.client_width().max(1) as f64;
        let css_h = self.hud_canvas.client_height().max(1) as f64;
        let width = (css_w * dpr).round().max(1.0) as u32;
        let height = (css_h * dpr).round().max(1.0) as u32;
        self.dpr = dpr;
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        self.scene_canvas.set_width(width);
        self.scene_canvas.set_height(height);
        self.hud_canvas.set_width(width);
        self.hud_canvas.set_height(height);
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.resize(width, height);
        }
    }

    fn render(&mut self, time_ms: f64) {
        self.resize();
        if !self.active {
            return;
        }
        let idle_ms = time_ms - self.last_input_ms;
        if self.auto_orbit && idle_ms > 2400.0 {
            self.yaw -= 0.000055 * self.motion * (idle_ms.min(5000.0) as f32 / 1000.0);
        }
        if let Some(focus_id) = self.focus_id.take() {
            if let Some(pos) = self.layout_positions().get(&focus_id).copied() {
                let dir = pos.normalized();
                if dir.len() > 0.001 {
                    self.yaw = dir.x.atan2(dir.z);
                    self.pitch = (-dir.y * 0.22).clamp(-0.75, 0.75);
                    self.distance = 8.0;
                }
            }
        }

        let frame = self.build_frame(time_ms);
        if let Some(gpu) = self.gpu.as_mut() {
            if let Err(err) = gpu.render(&frame) {
                web_sys::console::warn_1(&JsValue::from_str(&format!("Station GPU render failed: {err:?}")));
            }
        } else {
            self.draw_fallback_scene(&frame);
        }
        self.draw_hud(&frame, time_ms);
        self.projected_nodes = frame.projected_nodes;
    }

    fn build_frame(&mut self, time_ms: f64) -> GpuFrame {
        let mut frame = GpuFrame::default();
        let layout = self.layout_positions();
        let camera = self.camera();
        let aspect = self.width as f32 / self.height.max(1) as f32;

        let mut project = |p: Vec3| camera.project(p, aspect, self.fov_deg);

        for star in &self.starfield {
            if let Some(p) = project(*star) {
                let s = 0.0045 * self.density;
                frame.add_quad_ndc(p.x, p.y, s, [0.35, 0.36, 0.44, 0.55]);
            }
        }

        self.add_grid(&mut frame, &mut project);
        self.add_operator(&mut frame, &layout, &mut project, time_ms);

        for host in &self.snapshot.hosts {
            let id = format!("host:{}", host.id);
            if let Some(pos) = layout.get(&id).copied() {
                self.add_host(&mut frame, host, pos, &mut project, time_ms);
            }
        }
        for agent in &self.snapshot.agents {
            if let Some(pos) = layout.get(&agent.id).copied() {
                self.add_agent(&mut frame, agent, pos, &layout, &mut project, time_ms);
            }
        }

        for agent in &self.snapshot.agents {
            let Some(a_pos) = layout.get(&agent.id).copied() else { continue; };
            let host_id = format!("host:{}", agent.host_id);
            if let Some(parent_id) = agent.parent_id.as_ref().filter(|p| !p.is_empty()) {
                if let Some(p_pos) = layout.get(parent_id).copied() {
                    frame.add_line_projected(&mut project, p_pos, a_pos, role_color(&agent.role).with_alpha(0.54));
                    continue;
                }
            }
            if let Some(h_pos) = layout.get(&host_id).copied() {
                frame.add_line_projected(&mut project, h_pos, a_pos, role_color(&agent.role).with_alpha(0.42));
            }
        }
        for host in &self.snapshot.hosts {
            let id = format!("host:{}", host.id);
            if let Some(pos) = layout.get(&id).copied() {
                frame.add_line_projected(&mut project, Vec3::ZERO, pos, C_BLUE.with_alpha(0.26));
            }
        }

        let mut live_particles = Vec::with_capacity(self.particles.len());
        for particle in self.particles.drain(..) {
            let t = ((time_ms - particle.born_ms) as f32 / particle.ttl_ms as f32).clamp(0.0, 1.0);
            if t < 1.0 {
                let lifted = particle.start.lerp(particle.end, t) + Vec3::new(0.0, (t * PI).sin() * 0.6, 0.0);
                if let Some(p) = project(lifted) {
                    let size = (0.026 * (1.0 - t) + 0.006) * self.density;
                    frame.add_quad_ndc(p.x, p.y, size, particle.color.with_alpha(0.88 * (1.0 - t)).into());
                }
                live_particles.push(particle);
            }
        }
        self.particles = live_particles;

        frame
    }

    fn add_grid(&self, frame: &mut GpuFrame, project: &mut impl FnMut(Vec3) -> Option<Vec2>) {
        let grid = 9;
        for i in -grid..=grid {
            let v = i as f32;
            let alpha = if i == 0 { 0.33 } else { 0.16 };
            frame.add_line_projected(project, Vec3::new(-9.0, -1.8, v), Vec3::new(9.0, -1.8, v), C_SURFACE0.with_alpha(alpha));
            frame.add_line_projected(project, Vec3::new(v, -1.8, -9.0), Vec3::new(v, -1.8, 9.0), C_SURFACE0.with_alpha(alpha));
        }
    }

    fn add_operator(
        &self,
        frame: &mut GpuFrame,
        layout: &HashMap<String, Vec3>,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        time_ms: f64,
    ) {
        let pos = layout.get("op").copied().unwrap_or(Vec3::ZERO);
        let spin = time_ms as f32 * 0.00032 * self.motion;
        frame.add_wire_octa(project, pos, 0.48, spin, C_BLUE.with_alpha(0.95));
        frame.add_ring(project, pos, 0.82, C_SAPPHIRE.with_alpha(0.55), Plane::XZ);
        frame.add_ring(project, pos, 1.18, C_BLUE.with_alpha(0.18), Plane::XZ);
        if let Some(p) = project(pos) {
            frame.projected_nodes.push(ProjectedNode::new(
                "op",
                "you",
                NodeKind::Operator,
                p,
                18.0 * self.density,
            ));
        }
    }

    fn add_host(
        &self,
        frame: &mut GpuFrame,
        host: &StationHost,
        pos: Vec3,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        time_ms: f64,
    ) {
        let id = format!("host:{}", host.id);
        let spin = time_ms as f32 * 0.00011 * self.motion + stable_angle(&host.id);
        frame.add_wire_hex(project, pos, 0.58, 0.28, spin, C_PEACH.with_alpha(if host.connected { 0.9 } else { 0.38 }));
        frame.add_ring(project, pos + Vec3::new(0.0, -0.17, 0.0), 0.82 + (time_ms as f32 * 0.003).sin() * 0.035, C_PEACH.with_alpha(0.28), Plane::XZ);
        if let Some(p) = project(pos) {
            frame.projected_nodes.push(ProjectedNode::new(
                &id,
                &host.name,
                NodeKind::Host,
                p,
                21.0 * self.density,
            ));
        }
    }

    fn add_agent(
        &self,
        frame: &mut GpuFrame,
        agent: &StationAgent,
        pos: Vec3,
        layout: &HashMap<String, Vec3>,
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
        let budget = if pct < 0.5 { C_GREEN } else if pct < 0.85 { C_YELLOW } else { C_RED };
        frame.add_ring(project, pos, 0.56, budget.with_alpha(0.66), Plane::XY);
        frame.add_ring(project, pos, 0.38, phase.with_alpha(0.2), Plane::YZ);
        if agent.status == "in_progress" || agent.phase == "running" {
            frame.add_ring(project, pos, 0.72 + (time_ms as f32 * 0.004).sin() * 0.05, C_TEAL.with_alpha(0.22), Plane::XY);
        }
        if agent.needs_approval {
            frame.add_ring(project, pos, 0.84 + (time_ms as f32 * 0.006).sin() * 0.07, C_YELLOW.with_alpha(0.58), Plane::XY);
        }
        if self.selected_id.as_deref() == Some(&agent.id) {
            frame.add_ring(project, pos, 0.96, C_BLUE.with_alpha(0.84), Plane::XY);
        }
        if let Some(parent_id) = agent.parent_id.as_ref().filter(|s| !s.is_empty()) {
            if let Some(parent) = layout.get(parent_id).copied() {
                frame.add_line_projected(project, parent, pos, C_MAUVE.with_alpha(0.5));
            }
        }
        if let Some(p) = project(pos) {
            frame.projected_nodes.push(ProjectedNode::new(
                &agent.id,
                &agent.id,
                NodeKind::Agent,
                p,
                15.0 * self.density,
            ));
        }
    }

    fn draw_fallback_scene(&self, frame: &GpuFrame) {
        let Some(ctx) = self.scene_ctx.as_ref() else {
            return;
        };
        ctx.save();
        ctx.set_global_alpha(1.0);
        ctx.set_fill_style(&JsValue::from_str("rgba(17,17,27,0.94)"));
        ctx.fill_rect(0.0, 0.0, self.width as f64, self.height as f64);
        for pair in frame.line_vertices.chunks_exact(2) {
            let a = ndc_to_screen(pair[0].pos, self.width, self.height);
            let b = ndc_to_screen(pair[1].pos, self.width, self.height);
            let color = css_rgba(pair[0].color);
            ctx.set_stroke_style(&JsValue::from_str(&color));
            ctx.begin_path();
            ctx.move_to(a.x as f64, a.y as f64);
            ctx.line_to(b.x as f64, b.y as f64);
            ctx.stroke();
        }
        ctx.restore();
    }

    fn draw_hud(&mut self, frame: &GpuFrame, time_ms: f64) {
        self.ctx.save();
        self.ctx.set_transform(self.dpr, 0.0, 0.0, self.dpr, 0.0, 0.0).ok();
        let w = self.css_width();
        let h = self.css_height();
        self.ctx.clear_rect(0.0, 0.0, w as f64, h as f64);
        self.hit_zones.clear();

        self.draw_vignette(w, h);
        self.draw_display_thumbnails(frame);
        self.draw_toolbar(w);
        self.draw_tweaks_panel();
        self.draw_corners(w, h);
        self.draw_readout(h);
        self.draw_compass(w, h);
        self.draw_ticker(w, h);
        self.draw_legend(w, h);
        if let Some(id) = self.selected_id.clone() {
            self.draw_info_panel(&id, w, h, time_ms);
        }
        if time_ms - self.boot_started_ms < 1150.0 {
            self.draw_boot_splash(w, h, time_ms);
        }
        self.ctx.restore();
    }

    fn draw_vignette(&self, w: f32, h: f32) {
        if let Ok(gradient) = self.ctx.create_radial_gradient(
            (w / 2.0) as f64,
            (h / 2.0) as f64,
            20.0,
            (w / 2.0) as f64,
            (h / 2.0) as f64,
            (w.max(h) * 0.72) as f64,
        ) {
            let _ = gradient.add_color_stop(0.0, "rgba(30,30,46,0.04)");
            let _ = gradient.add_color_stop(0.75, "rgba(17,17,27,0.16)");
            let _ = gradient.add_color_stop(1.0, "rgba(4,4,9,0.48)");
            self.ctx.set_fill_style(&gradient.into());
            self.ctx.fill_rect(0.0, 0.0, w as f64, h as f64);
        }
    }

    fn draw_display_thumbnails(&self, frame: &GpuFrame) {
        let by_host: HashMap<&str, &ProjectedNode> = frame
            .projected_nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Host)
            .map(|n| (n.id.strip_prefix("host:").unwrap_or(n.id.as_str()), n))
            .collect();

        for source in self.display_sources.values() {
            let Some(node) = by_host.get(source.host_id.as_str()) else { continue; };
            let center = ndc_to_screen([node.ndc.x, node.ndc.y], self.width, self.height);
            let css = Vec2::new(center.x / self.dpr as f32, center.y / self.dpr as f32);
            let tw = 164.0_f32.min(self.css_width() * 0.28).max(98.0);
            let th = tw * 0.5625;
            let x = css.x - tw / 2.0;
            let y = css.y - 118.0 - th * 0.2;
            self.round_rect(x, y, tw, th, 5.0, "rgba(17,17,27,0.86)", "rgba(250,179,135,0.82)");
            let video_ready = source.video.video_width() > 0 && source.video.video_height() > 0;
            if video_ready {
                let _ = self.ctx.draw_image_with_html_video_element_and_dw_and_dh(
                    &source.video,
                    (x + 3.0) as f64,
                    (y + 3.0) as f64,
                    (tw - 6.0) as f64,
                    (th - 6.0) as f64,
                );
            } else {
                self.ctx.set_fill_style(&JsValue::from_str("rgba(49,50,68,0.55)"));
                self.ctx.fill_rect((x + 3.0) as f64, (y + 3.0) as f64, (tw - 6.0) as f64, (th - 6.0) as f64);
                self.text("linking display", x + 12.0, y + th / 2.0, 10.0, C_OVERLAY1_CSS, "normal");
            }
            self.text(&source.label, x + 7.0, y + th + 12.0, 10.0, C_PEACH_CSS, "normal");
        }
    }

    fn draw_toolbar(&mut self, w: f32) {
        self.ctx.set_fill_style(&JsValue::from_str("rgba(24,24,37,0.88)"));
        self.ctx.fill_rect(0.0, 0.0, w as f64, 39.0);
        self.ctx.set_stroke_style(&JsValue::from_str("rgba(49,50,68,0.92)"));
        self.line(0.0, 39.0, w, 39.0);
        self.text("STATION", 13.0, 24.0, 10.0, C_OVERLAY1_CSS, "bold");
        let mut x = 86.0;
        self.pill_button(x, 9.0, 66.0, 22.0, "orbital", self.layout == LayoutName::Orbital, HitAction::Layout(LayoutName::Orbital));
        x += 72.0;
        self.pill_button(x, 9.0, 102.0, 22.0, "constellation", self.layout == LayoutName::Constellation, HitAction::Layout(LayoutName::Constellation));
        x += 124.0;
        let active_agents = self.snapshot.agents.iter().filter(|a| a.status == "in_progress").count();
        self.text(
            &format!("{} hosts · {} active agents", self.snapshot.hosts.len(), active_agents),
            x,
            24.0,
            11.0,
            C_SUBTEXT0_CSS,
            "normal",
        );
        let pending = self.snapshot.agents.iter().filter(|a| a.needs_approval).count();
        if pending > 0 {
            self.pill(x + 178.0, 9.0, 118.0, 22.0, &format!("{pending} approval{}", if pending == 1 { "" } else { "s" }), C_YELLOW_CSS);
        }
        let role_counts = [
            ("orch", "orchestrator", C_BLUE_CSS),
            ("direct", "direct", C_TEAL_CSS),
            ("sub", "sub-agent", C_MAUVE_CSS),
        ];
        let mut rx = w - 225.0;
        for (label, role, color) in role_counts {
            let count = self.snapshot.agents.iter().filter(|a| a.role == role).count();
            self.pill(rx, 9.0, 66.0, 22.0, &format!("{label} {count}"), color);
            rx += 72.0;
        }
    }

    fn draw_tweaks_panel(&mut self) {
        let x = 12.0;
        let y = 52.0;
        let w = 222.0;
        self.round_rect(x, y, w, 178.0, 6.0, "rgba(24,24,37,0.78)", "rgba(69,71,90,0.74)");
        self.text("TWEAKS", x + 11.0, y + 19.0, 10.0, C_OVERLAY1_CSS, "bold");
        self.pill_button(x + 82.0, y + 7.0, 58.0, 22.0, "cockpit", self.mood == Mood::Cockpit, HitAction::Mood(Mood::Cockpit));
        self.pill_button(x + 145.0, y + 7.0, 48.0, 22.0, "calm", self.mood == Mood::Calm, HitAction::Mood(Mood::Calm));
        self.slider(x + 12.0, y + 47.0, 190.0, "fov", self.fov_deg, 35.0, 85.0, SliderKind::Fov);
        self.slider(x + 12.0, y + 78.0, 190.0, "motion", self.motion, 0.0, 2.0, SliderKind::Motion);
        self.slider(x + 12.0, y + 109.0, 190.0, "ar", self.ar_strength, 0.0, 1.0, SliderKind::Ar);
        self.slider(x + 12.0, y + 140.0, 190.0, "density", self.density, 0.5, 1.8, SliderKind::Density);
    }

    fn draw_corners(&self, w: f32, h: f32) {
        let c = "rgba(69,71,90,0.8)";
        self.ctx.set_stroke_style(&JsValue::from_str(c));
        let len = 26.0;
        for (x, y, sx, sy) in [(11.0, 50.0, 1.0, 1.0), (w - 11.0, 50.0, -1.0, 1.0), (11.0, h - 11.0, 1.0, -1.0), (w - 11.0, h - 11.0, -1.0, -1.0)] {
            self.line(x, y, x + sx * len, y);
            self.line(x, y, x, y + sy * len);
        }
    }

    fn draw_readout(&self, h: f32) {
        let tokens: f64 = self.snapshot.agents.iter().map(|a| a.tokens as f64).sum();
        let cost: f64 = self.snapshot.agents.iter().map(|a| a.cost).sum();
        let mut y = h - 58.0;
        for (k, v, color) in [
            ("cam", if self.auto_orbit { "orbit · auto".to_string() } else { "orbit".to_string() }, C_SUBTEXT0_CSS),
            ("tokens", format!("{tokens:.0}"), C_BLUE_CSS),
            ("cost", format!("${cost:.2}"), C_SUBTEXT0_CSS),
        ] {
            self.text(k, 44.0, y, 10.0, C_OVERLAY0_CSS, "bold");
            self.text(&v, 104.0, y, 10.0, color, "normal");
            y += 15.0;
        }
        if let Some(id) = &self.hovered_id {
            self.text("hov", 44.0, y, 10.0, C_OVERLAY0_CSS, "bold");
            self.text(id, 104.0, y, 10.0, C_BLUE_CSS, "normal");
        }
    }

    fn draw_compass(&self, w: f32, h: f32) {
        let cx = w - 71.0;
        let cy = h - 33.0;
        self.ctx.set_stroke_style(&JsValue::from_str("rgba(69,71,90,0.9)"));
        self.ctx.begin_path();
        let _ = self.ctx.arc(cx as f64, cy as f64, 18.0, 0.0, std::f64::consts::TAU);
        self.ctx.stroke();
        let angle = -self.yaw as f64;
        self.ctx.set_stroke_style(&JsValue::from_str(C_BLUE_CSS));
        self.ctx.begin_path();
        self.ctx.move_to(cx as f64, cy as f64);
        self.ctx.line_to(cx as f64 + angle.sin() * 14.0, cy as f64 - angle.cos() * 14.0);
        self.ctx.stroke();
        self.text("N", cx + 27.0, cy + 4.0, 10.0, C_OVERLAY1_CSS, "bold");
    }

    fn draw_ticker(&self, w: f32, h: f32) {
        let events = self.snapshot.events.iter().rev().take(5).collect::<Vec<_>>();
        let row_h = 16.0;
        let x = 250.0;
        let mut y = h - row_h * events.len() as f32 - 13.0;
        for ev in events.into_iter().rev() {
            self.ctx.set_fill_style(&JsValue::from_str("rgba(17,17,27,0.55)"));
            self.ctx.fill_rect(x as f64, (y - 11.0) as f64, (w - x - 245.0).max(280.0) as f64, 15.0);
            self.text(&ev.ts, x + 6.0, y, 10.0, C_OVERLAY1_CSS, "normal");
            self.text(&ev.level, x + 72.0, y, 10.0, level_color_css(&ev.level), "bold");
            self.text(&truncate(&ev.msg, 96), x + 130.0, y, 10.0, C_SUBTEXT0_CSS, "normal");
            y += row_h;
        }
    }

    fn draw_legend(&self, w: f32, h: f32) {
        let items = [
            ("orchestrator", C_BLUE_CSS),
            ("direct", C_TEAL_CSS),
            ("sub-agent", C_MAUVE_CSS),
            ("host", C_PEACH_CSS),
            ("approval", C_YELLOW_CSS),
        ];
        let mut x = (w * 0.5 - 270.0).max(246.0);
        let y = h - 17.0;
        for (label, color) in items {
            self.ctx.set_fill_style(&JsValue::from_str(color));
            self.ctx.fill_rect(x as f64, (y - 7.0) as f64, 8.0, 8.0);
            self.text(label, x + 14.0, y, 10.0, C_OVERLAY1_CSS, "normal");
            x += label.len() as f32 * 7.1 + 28.0;
        }
    }

    fn draw_boot_splash(&self, w: f32, h: f32, time_ms: f64) {
        let alpha = (1.0 - ((time_ms - self.boot_started_ms - 700.0) / 450.0).clamp(0.0, 1.0)) as f32;
        if alpha <= 0.0 {
            return;
        }
        self.ctx.set_global_alpha(alpha as f64);
        self.round_rect(w * 0.5 - 155.0, h * 0.5 - 34.0, 310.0, 68.0, 6.0, "rgba(24,24,37,0.92)", "rgba(69,71,90,0.88)");
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let idx = ((time_ms / 90.0) as usize) % frames.len();
        self.text(frames[idx], w * 0.5 - 95.0, h * 0.5 + 5.0, 21.0, C_BLUE_CSS, "normal");
        self.text("Initializing station · linking hosts", w * 0.5 - 58.0, h * 0.5 + 2.0, 12.0, C_SUBTEXT0_CSS, "normal");
        self.ctx.set_global_alpha(1.0);
    }

    fn draw_info_panel(&mut self, id: &str, w: f32, h: f32, time_ms: f64) {
        let panel_w = 350.0_f32.min(w - 28.0).max(280.0);
        let x = w - panel_w - 14.0;
        let y = 52.0;
        let panel_h = (h - 76.0).min(560.0);
        self.round_rect(x, y, panel_w, panel_h, 6.0, "rgba(24,24,37,0.94)", "rgba(69,71,90,0.92)");
        self.hit_zones.push(HitZone::new(x + panel_w - 31.0, y + 8.0, 22.0, 22.0, HitAction::ClosePanel));
        self.text("×", x + panel_w - 25.0, y + 24.0, 18.0, C_OVERLAY1_CSS, "normal");

        if id == "op" {
            self.text("operator", x + 12.0, y + 25.0, 10.0, C_BLUE_CSS, "bold");
            self.text("you", x + 86.0, y + 25.0, 13.0, C_TEXT_CSS, "bold");
            self.panel_row(x, y + 54.0, "mode", "station origin");
            self.panel_row(x, y + 76.0, "camera", "orbit / parallax");
            return;
        }

        if let Some(host) = self.snapshot.hosts.iter().find(|h| format!("host:{}", h.id) == id).cloned() {
            self.text("host", x + 12.0, y + 25.0, 10.0, C_PEACH_CSS, "bold");
            self.text(&host.name, x + 72.0, y + 25.0, 13.0, C_TEXT_CSS, "bold");
            let mut yy = y + 54.0 - self.panel_scroll;
            self.panel_row(x, yy, "platform", &host.platform); yy += 22.0;
            self.panel_row(x, yy, "region", &host.region); yy += 22.0;
            self.panel_row_color(x, yy, "cpu", &format!("{:.0}%", host.cpu), if host.cpu > 70.0 { C_YELLOW_CSS } else { C_TEXT_CSS }); yy += 22.0;
            self.panel_row_color(x, yy, "mem", &format!("{:.0}%", host.mem), if host.mem > 70.0 { C_YELLOW_CSS } else { C_TEXT_CSS }); yy += 30.0;
            self.section_title(x, yy, "Display · WebRTC"); yy += 18.0;
            self.round_rect(x + 12.0, yy, panel_w - 24.0, 120.0, 4.0, "rgba(17,17,27,0.86)", "rgba(49,50,68,0.86)");
            let source = self.display_sources.values().find(|s| s.host_id == host.id);
            if let Some(source) = source {
                if source.video.video_width() > 0 {
                    let _ = self.ctx.draw_image_with_html_video_element_and_dw_and_dh(
                        &source.video,
                        (x + 16.0) as f64,
                        (yy + 4.0) as f64,
                        (panel_w - 32.0) as f64,
                        112.0,
                    );
                }
                self.text(&format!("{} · live", source.display_id), x + 20.0, yy + 112.0, 10.0, C_GREEN_CSS, "normal");
            } else {
                self.text("no active stream", x + 25.0, yy + 62.0, 12.0, C_OVERLAY1_CSS, "normal");
            }
            self.hit_zones.push(HitZone::new(x + 188.0, yy + 92.0, 122.0, 22.0, HitAction::OpenDisplay(host.id.clone())));
            self.pill_at(x + 188.0, yy + 92.0, 122.0, 22.0, "open display", C_BLUE_CSS);
            yy += 148.0;
            self.section_title(x, yy, &format!("Agents on host ({})", self.snapshot.agents.iter().filter(|a| a.host_id == host.id).count()));
            yy += 20.0;
            for agent in self.snapshot.agents.iter().filter(|a| a.host_id == host.id).take(8) {
                self.text(&agent.role, x + 16.0, yy, 9.0, role_color_css(&agent.role), "bold");
                self.text(&agent.id, x + 88.0, yy, 10.0, C_OVERLAY1_CSS, "normal");
                self.text(&truncate(&agent.task, 32), x + 150.0, yy, 10.0, C_SUBTEXT0_CSS, "normal");
                self.hit_zones.push(HitZone::new(x + 12.0, yy - 13.0, panel_w - 24.0, 18.0, HitAction::Select(agent.id.clone())));
                yy += 20.0;
            }
            return;
        }

        if let Some(agent) = self.snapshot.agents.iter().find(|a| a.id == id).cloned() {
            self.text(&agent.role, x + 12.0, y + 25.0, 10.0, role_color_css(&agent.role), "bold");
            self.text(&agent.id, x + 102.0, y + 25.0, 13.0, C_TEXT_CSS, "bold");
            let spin = ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"][(time_ms as usize / 100) % 10];
            let mut yy = y + 54.0 - self.panel_scroll;
            self.panel_row(x, yy, "task", &agent.task); yy += 34.0;
            self.panel_row_color(x, yy, "host", &self.host_name(&agent.host_id), C_BLUE_CSS); yy += 22.0;
            self.panel_row_color(x, yy, "provider", &agent.provider, C_BLUE_CSS); yy += 22.0;
            self.panel_row_color(x, yy, "model", &agent.model, C_GREEN_CSS); yy += 22.0;
            self.panel_row_color(x, yy, "phase", &format!("{} {spin}", agent.phase), phase_color_css(&agent.phase)); yy += 22.0;
            self.panel_row(x, yy, "status", &agent.status.replace('_', " ")); yy += 22.0;
            self.panel_row(x, yy, "turns", &format!("{}/{}", agent.turns, agent.turn_cap)); yy += 22.0;
            self.panel_row(x, yy, "autonomy", &agent.autonomy); yy += 28.0;
            self.section_title(x, yy, "Token budget"); yy += 18.0;
            let pct = if agent.token_cap > 0.0 { (agent.tokens / agent.token_cap).clamp(0.0, 1.0) } else { 0.0 };
            let budget = if pct < 0.5 { C_GREEN_CSS } else if pct < 0.85 { C_YELLOW_CSS } else { C_RED_CSS };
            self.ctx.set_fill_style(&JsValue::from_str("rgba(49,50,68,0.85)"));
            self.ctx.fill_rect((x + 12.0) as f64, yy as f64, (panel_w - 24.0) as f64, 7.0);
            self.ctx.set_fill_style(&JsValue::from_str(budget));
            self.ctx.fill_rect((x + 12.0) as f64, yy as f64, ((panel_w - 24.0) * pct) as f64, 7.0);
            yy += 24.0;
            self.panel_row(x, yy, "prompt", &format!("{:.0}", agent.prompt)); yy += 20.0;
            self.panel_row(x, yy, "complete", &format!("{:.0}", agent.completion)); yy += 20.0;
            self.panel_row(x, yy, "cached", &format!("{:.0}", agent.cached)); yy += 20.0;
            self.panel_row_color(x, yy, "cost", &format!("${:.2}", agent.cost), C_GREEN_CSS); yy += 30.0;
            self.section_title(x, yy, "Recent events"); yy += 18.0;
            self.round_rect(x + 12.0, yy - 8.0, panel_w - 24.0, 92.0, 4.0, "rgba(17,17,27,0.88)", "rgba(49,50,68,0.88)");
            let mut ey = yy + 9.0;
            for ev in self.snapshot.events.iter().filter(|e| e.agent_id.as_deref() == Some(&agent.id) || e.host_id == agent.host_id).rev().take(5).collect::<Vec<_>>().into_iter().rev() {
                self.text(&ev.ts, x + 20.0, ey, 9.0, C_OVERLAY1_CSS, "normal");
                self.text(&truncate(&ev.msg, 42), x + 78.0, ey, 9.0, level_color_css(&ev.level), "normal");
                ey += 15.0;
            }
            yy += 108.0;
            if agent.needs_approval {
                self.section_title_color(x, yy, "Action needs approval", C_YELLOW_CSS); yy += 18.0;
                self.round_rect(x + 12.0, yy - 6.0, panel_w - 24.0, 42.0, 4.0, "rgba(17,17,27,0.9)", "rgba(49,50,68,0.88)");
                self.text(&truncate(&agent.approval_command, 56), x + 20.0, yy + 10.0, 10.0, C_SUBTEXT0_CSS, "normal");
                yy += 50.0;
                let approval_id = agent.approval_id.clone().unwrap_or_else(|| agent.id.clone());
                self.approval_button(x + 12.0, yy, 82.0, "Approve", &agent.host_id, &approval_id, "approve", C_GREEN_CSS);
                self.approval_button(x + 102.0, yy, 62.0, "Skip", &agent.host_id, &approval_id, "skip", C_OVERLAY1_CSS);
                self.approval_button(x + 172.0, yy, 62.0, "Deny", &agent.host_id, &approval_id, "deny", C_RED_CSS);
            }
        }
    }

    fn panel_row(&self, x: f32, y: f32, k: &str, v: &str) {
        self.panel_row_color(x, y, k, v, C_TEXT_CSS);
    }

    fn panel_row_color(&self, x: f32, y: f32, k: &str, v: &str, color: &str) {
        self.text(k, x + 14.0, y, 10.0, C_OVERLAY1_CSS, "bold");
        self.text(&truncate(v, 46), x + 94.0, y, 11.0, color, "normal");
    }

    fn section_title(&self, x: f32, y: f32, title: &str) {
        self.section_title_color(x, y, title, C_BLUE_CSS);
    }

    fn section_title_color(&self, x: f32, y: f32, title: &str, color: &str) {
        self.text(title, x + 14.0, y, 10.0, color, "bold");
    }

    fn approval_button(&mut self, x: f32, y: f32, w: f32, label: &str, host_id: &str, approval_id: &str, decision: &str, color: &str) {
        self.pill_at(x, y - 14.0, w, 24.0, label, color);
        self.hit_zones.push(HitZone::new(
            x,
            y - 14.0,
            w,
            24.0,
            HitAction::Approval {
                host_id: host_id.to_string(),
                approval_id: approval_id.to_string(),
                decision: decision.to_string(),
            },
        ));
    }

    fn slider(&mut self, x: f32, y: f32, w: f32, label: &str, value: f32, min: f32, max: f32, kind: SliderKind) {
        self.text(label, x, y, 10.0, C_OVERLAY1_CSS, "bold");
        let track_x = x + 66.0;
        let pct = ((value - min) / (max - min)).clamp(0.0, 1.0);
        self.ctx.set_fill_style(&JsValue::from_str("rgba(49,50,68,0.92)"));
        self.ctx.fill_rect(track_x as f64, (y - 6.0) as f64, (w - 82.0) as f64, 5.0);
        self.ctx.set_fill_style(&JsValue::from_str(C_BLUE_CSS));
        self.ctx.fill_rect(track_x as f64, (y - 6.0) as f64, ((w - 82.0) * pct) as f64, 5.0);
        self.ctx.begin_path();
        let _ = self.ctx.arc((track_x + (w - 82.0) * pct) as f64, (y - 3.5) as f64, 5.0, 0.0, std::f64::consts::TAU);
        self.ctx.fill();
        self.text(&format!("{value:.1}"), x + w - 26.0, y, 9.0, C_SUBTEXT0_CSS, "normal");
        self.hit_zones.push(HitZone::new(track_x - 6.0, y - 16.0, w - 70.0, 24.0, HitAction::Slider(kind)));
    }

    fn pill_button(&mut self, x: f32, y: f32, w: f32, h: f32, label: &str, active: bool, action: HitAction) {
        self.pill_at(x, y, w, h, label, if active { C_BLUE_CSS } else { C_OVERLAY1_CSS });
        self.hit_zones.push(HitZone::new(x, y, w, h, action));
    }

    fn pill(&self, x: f32, y: f32, w: f32, h: f32, label: &str, color: &str) {
        self.pill_at(x, y, w, h, label, color);
    }

    fn pill_at(&self, x: f32, y: f32, w: f32, h: f32, label: &str, color: &str) {
        self.round_rect(x, y, w, h, 4.0, "rgba(49,50,68,0.45)", color);
        self.text(label, x + 8.0, y + h * 0.65, 10.0, color, "bold");
    }

    fn round_rect(&self, x: f32, y: f32, w: f32, h: f32, r: f32, fill: &str, stroke: &str) {
        let ctx = &self.ctx;
        ctx.begin_path();
        ctx.move_to((x + r) as f64, y as f64);
        ctx.line_to((x + w - r) as f64, y as f64);
        let _ = ctx.quadratic_curve_to((x + w) as f64, y as f64, (x + w) as f64, (y + r) as f64);
        ctx.line_to((x + w) as f64, (y + h - r) as f64);
        let _ = ctx.quadratic_curve_to((x + w) as f64, (y + h) as f64, (x + w - r) as f64, (y + h) as f64);
        ctx.line_to((x + r) as f64, (y + h) as f64);
        let _ = ctx.quadratic_curve_to(x as f64, (y + h) as f64, x as f64, (y + h - r) as f64);
        ctx.line_to(x as f64, (y + r) as f64);
        let _ = ctx.quadratic_curve_to(x as f64, y as f64, (x + r) as f64, y as f64);
        ctx.close_path();
        ctx.set_fill_style(&JsValue::from_str(fill));
        ctx.fill();
        ctx.set_stroke_style(&JsValue::from_str(stroke));
        ctx.stroke();
    }

    fn text(&self, text: &str, x: f32, y: f32, px: f32, color: &str, weight: &str) {
        self.ctx.set_fill_style(&JsValue::from_str(color));
        self.ctx.set_font(&format!("{weight} {px}px 'SF Mono', Menlo, Consolas, monospace"));
        let _ = self.ctx.fill_text(text, x as f64, y as f64);
    }

    fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32) {
        self.ctx.begin_path();
        self.ctx.move_to(x1 as f64, y1 as f64);
        self.ctx.line_to(x2 as f64, y2 as f64);
        self.ctx.stroke();
    }

    fn layout_positions(&self) -> HashMap<String, Vec3> {
        let mut map = HashMap::new();
        map.insert("op".to_string(), Vec3::ZERO);
        let host_count = self.snapshot.hosts.len().max(1);
        for (i, host) in self.snapshot.hosts.iter().enumerate() {
            let t = i as f32 / host_count as f32;
            let pos = match self.layout {
                LayoutName::Orbital => {
                    let angle = t * PI * 2.0 + PI * 0.08;
                    let radius = 4.2 + (host_count as f32 * 0.18).min(1.3);
                    Vec3::new(angle.cos() * radius, 0.0, angle.sin() * radius)
                }
                LayoutName::Constellation => {
                    let spread = (host_count as f32 - 1.0).max(1.0);
                    let x = (i as f32 - spread * 0.5) * 3.2;
                    let z = -1.3 + (stable_unit(&host.id) - 0.5) * 2.3;
                    Vec3::new(x, -0.05 + (stable_unit(&(host.id.clone() + "y")) - 0.5) * 0.8, z)
                }
            };
            map.insert(format!("host:{}", host.id), pos);
        }
        let mut by_host: HashMap<&str, Vec<&StationAgent>> = HashMap::new();
        for agent in &self.snapshot.agents {
            by_host.entry(agent.host_id.as_str()).or_default().push(agent);
        }
        for host in &self.snapshot.hosts {
            let host_pos = map.get(&format!("host:{}", host.id)).copied().unwrap_or(Vec3::ZERO);
            let agents = by_host.get(host.id.as_str()).cloned().unwrap_or_default();
            let count = agents.len().max(1);
            for (idx, agent) in agents.into_iter().enumerate() {
                let pos = match self.layout {
                    LayoutName::Orbital => {
                        let angle = idx as f32 / count as f32 * PI * 2.0 + stable_angle(&agent.id);
                        let ring = if agent.role == "sub-agent" { 1.55 } else { 1.18 };
                        host_pos + Vec3::new(angle.cos() * ring, 0.55 + (idx % 3) as f32 * 0.28, angle.sin() * ring * 0.72)
                    }
                    LayoutName::Constellation => {
                        let u = stable_unit(&agent.id);
                        let v = stable_unit(&(agent.id.clone() + "v"));
                        host_pos + Vec3::new((u - 0.5) * 2.9, 0.7 + v * 1.8, (stable_unit(&(agent.id.clone() + "z")) - 0.5) * 2.0)
                    }
                };
                map.insert(agent.id.clone(), pos);
            }
        }
        map
    }

    fn camera(&self) -> Camera {
        let parallax = Vec3::new(self.ar_x * self.ar_strength, self.ar_y * self.ar_strength * 0.5, 0.0);
        let cp = self.pitch.cos();
        let eye = Vec3::new(
            self.yaw.sin() * cp * self.distance,
            self.pitch.sin() * self.distance + 3.2,
            self.yaw.cos() * cp * self.distance,
        ) + parallax;
        Camera::look_at(eye, Vec3::new(0.0, 0.25, 0.0), Vec3::Y)
    }

    fn pick_node(&self, x: f32, y: f32) -> Option<String> {
        let px = x * self.dpr as f32;
        let py = y * self.dpr as f32;
        self.projected_nodes
            .iter()
            .filter_map(|n| {
                let p = ndc_to_screen([n.ndc.x, n.ndc.y], self.width, self.height);
                let d = ((p.x - px).powi(2) + (p.y - py).powi(2)).sqrt();
                (d <= n.radius * self.dpr as f32 + 10.0).then(|| (d, n.id.clone()))
            })
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, id)| id)
    }

    fn dispatch_hit(&mut self, action: HitAction, x: f32, _y: f32) {
        match action {
            HitAction::Layout(layout) => self.layout = layout,
            HitAction::Mood(mood) => self.mood = mood,
            HitAction::ClosePanel => self.selected_id = None,
            HitAction::Select(id) => {
                self.selected_id = Some(id);
                self.panel_scroll = 0.0;
            }
            HitAction::Slider(kind) => self.apply_slider_at(kind, x),
            HitAction::Approval { host_id, approval_id, decision } => {
                self.emit_action(serde_json::json!({
                    "type": "approval",
                    "host_id": host_id,
                    "approval_id": approval_id,
                    "decision": decision,
                }));
            }
            HitAction::OpenDisplay(host_id) => {
                self.emit_action(serde_json::json!({
                    "type": "open_display",
                    "host_id": host_id,
                }));
            }
        }
    }

    fn emit_action(&self, action: serde_json::Value) {
        if let Some(cb) = &self.action_callback {
            if let Ok(value) = serde_wasm_bindgen::to_value(&action) {
                let _ = cb.call1(&JsValue::NULL, &value);
            }
        }
    }

    fn apply_slider_at(&mut self, kind: SliderKind, x: f32) {
        let track_x = 12.0 + 12.0 + 66.0;
        let track_w = 190.0 - 82.0;
        let pct = ((x - track_x) / track_w).clamp(0.0, 1.0);
        match kind {
            SliderKind::Fov => self.fov_deg = 35.0 + pct * 50.0,
            SliderKind::Motion => self.motion = pct * 2.0,
            SliderKind::Ar => self.ar_strength = pct,
            SliderKind::Density => self.density = 0.5 + pct * 1.3,
        }
    }

    fn hit_action_at(&self, x: f32, y: f32) -> Option<HitAction> {
        self.hit_zones
            .iter()
            .rev()
            .find(|z| x >= z.x && x <= z.x + z.w && y >= z.y && y <= z.y + z.h)
            .map(|z| z.action.clone())
    }

    fn panel_hit(&self, client_x: f64, client_y: f64) -> bool {
        let (x, y) = self.event_xy(client_x, client_y);
        let w = self.css_width();
        x > w - 380.0 && y > 48.0
    }

    fn event_xy(&self, client_x: f64, client_y: f64) -> (f32, f32) {
        let rect = self.hud_canvas.get_bounding_client_rect();
        ((client_x - rect.left()) as f32, (client_y - rect.top()) as f32)
    }

    fn mark_input(&mut self) {
        self.last_input_ms = now_ms();
    }

    fn css_width(&self) -> f32 {
        self.width as f32 / self.dpr as f32
    }

    fn css_height(&self) -> f32 {
        self.height as f32 / self.dpr as f32
    }

    fn host_name(&self, host_id: &str) -> String {
        self.snapshot
            .hosts
            .iter()
            .find(|h| h.id == host_id)
            .map(|h| h.name.clone())
            .unwrap_or_else(|| host_id.to_string())
    }
}

struct GpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    line_pipeline: wgpu::RenderPipeline,
    tri_pipeline: wgpu::RenderPipeline,
}

impl GpuState {
    async fn new(canvas: HtmlCanvasElement) -> Result<Self, JsValue> {
        let width = canvas.width().max(1);
        let height = canvas.height().max(1);
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::BROWSER_WEBGPU,
            dx12_shader_compiler: Default::default(),
            flags: wgpu::InstanceFlags::default(),
            gles_minor_version: wgpu::Gles3MinorVersion::Automatic,
        });
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
            .map_err(|e| JsValue::from_str(&format!("create WebGPU surface failed: {e:?}")))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| JsValue::from_str("no WebGPU adapter available"))?;
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("Intendant Station Device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_webgl2_defaults(),
                },
                None,
            )
            .await
            .map_err(|e| JsValue::from_str(&format!("request WebGPU device failed: {e:?}")))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Station Shader"),
            source: wgpu::ShaderSource::Wgsl(STATION_WGSL.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Station Pipeline Layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });
        let make_pipeline = |topology| {
            let vertex_layout = GpuVertex::layout();
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Station Render Pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: "vs_main",
                    buffers: &[vertex_layout],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: "fs_main",
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    unclipped_depth: false,
                    conservative: false,
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
            })
        };
        let line_pipeline = make_pipeline(wgpu::PrimitiveTopology::LineList);
        let tri_pipeline = make_pipeline(wgpu::PrimitiveTopology::TriangleList);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            line_pipeline,
            tri_pipeline,
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == self.config.width && height == self.config.height {
            return;
        }
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    fn render(&mut self, frame: &GpuFrame) -> Result<(), wgpu::SurfaceError> {
        let output = match self.surface.get_current_texture() {
            Ok(output) => output,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                self.surface.get_current_texture()?
            }
            Err(err) => return Err(err),
        };
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Station Encoder"),
        });

        let line_buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Station Lines"),
            contents: bytemuck::cast_slice(&frame.line_vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let tri_buffer = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Station Triangles"),
            contents: bytemuck::cast_slice(&frame.tri_vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Station Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.030,
                            g: 0.030,
                            b: 0.055,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if !frame.line_vertices.is_empty() {
                pass.set_pipeline(&self.line_pipeline);
                pass.set_vertex_buffer(0, line_buffer.slice(..));
                pass.draw(0..frame.line_vertices.len() as u32, 0..1);
            }
            if !frame.tri_vertices.is_empty() {
                pass.set_pipeline(&self.tri_pipeline);
                pass.set_vertex_buffer(0, tri_buffer.slice(..));
                pass.draw(0..frame.tri_vertices.len() as u32, 0..1);
            }
        }
        self.queue.submit(Some(encoder.finish()));
        output.present();
        Ok(())
    }
}

const STATION_WGSL: &str = r#"
struct VertexOut {
  @builtin(position) position: vec4<f32>,
  @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(@location(0) position: vec2<f32>, @location(1) color: vec4<f32>) -> VertexOut {
  var out: VertexOut;
  out.position = vec4<f32>(position, 0.0, 1.0);
  out.color = color;
  return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
  return in.color;
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuVertex {
    pos: [f32; 2],
    color: [f32; 4],
}

impl GpuVertex {
    const ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];

    fn layout<'a>() -> wgpu::VertexBufferLayout<'a> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GpuVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

#[derive(Default)]
struct GpuFrame {
    line_vertices: Vec<GpuVertex>,
    tri_vertices: Vec<GpuVertex>,
    projected_nodes: Vec<ProjectedNode>,
}

impl GpuFrame {
    fn add_line_ndc(&mut self, a: Vec2, b: Vec2, color: Color) {
        self.line_vertices.push(GpuVertex { pos: [a.x, a.y], color: color.into() });
        self.line_vertices.push(GpuVertex { pos: [b.x, b.y], color: color.into() });
    }

    fn add_line_projected(&mut self, project: &mut impl FnMut(Vec3) -> Option<Vec2>, a: Vec3, b: Vec3, color: Color) {
        if let (Some(pa), Some(pb)) = (project(a), project(b)) {
            self.add_line_ndc(pa, pb, color);
        }
    }

    fn add_quad_ndc(&mut self, x: f32, y: f32, size: f32, color: [f32; 4]) {
        let s = size;
        let verts = [
            [x - s, y - s],
            [x + s, y - s],
            [x + s, y + s],
            [x - s, y - s],
            [x + s, y + s],
            [x - s, y + s],
        ];
        for pos in verts {
            self.tri_vertices.push(GpuVertex { pos, color });
        }
    }

    fn add_ring(&mut self, project: &mut impl FnMut(Vec3) -> Option<Vec2>, center: Vec3, radius: f32, color: Color, plane: Plane) {
        let seg = 64;
        let mut prev = None;
        for i in 0..=seg {
            let t = i as f32 / seg as f32 * PI * 2.0;
            let local = match plane {
                Plane::XY => Vec3::new(t.cos() * radius, t.sin() * radius, 0.0),
                Plane::XZ => Vec3::new(t.cos() * radius, 0.0, t.sin() * radius),
                Plane::YZ => Vec3::new(0.0, t.cos() * radius, t.sin() * radius),
            };
            let p = center + local;
            if let Some(prev_p) = prev {
                self.add_line_projected(project, prev_p, p, color);
            }
            prev = Some(p);
        }
    }

    fn add_wire_octa(&mut self, project: &mut impl FnMut(Vec3) -> Option<Vec2>, center: Vec3, scale: f32, spin: f32, color: Color) {
        let verts = [
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
            Vec3::new(-1.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, -1.0),
            Vec3::new(0.0, -1.0, 0.0),
        ];
        let edges = [(0, 1), (0, 2), (0, 3), (0, 4), (5, 1), (5, 2), (5, 3), (5, 4), (1, 2), (2, 3), (3, 4), (4, 1)];
        self.add_edges(project, center, scale, spin, &verts, &edges, color);
    }

    fn add_wire_tetra(&mut self, project: &mut impl FnMut(Vec3) -> Option<Vec2>, center: Vec3, scale: f32, spin: f32, color: Color) {
        let verts = [
            Vec3::new(1.0, 1.0, 1.0),
            Vec3::new(-1.0, -1.0, 1.0),
            Vec3::new(-1.0, 1.0, -1.0),
            Vec3::new(1.0, -1.0, -1.0),
        ];
        let edges = [(0, 1), (0, 2), (0, 3), (1, 2), (2, 3), (3, 1)];
        self.add_edges(project, center, scale, spin, &verts, &edges, color);
    }

    fn add_wire_icosa(&mut self, project: &mut impl FnMut(Vec3) -> Option<Vec2>, center: Vec3, scale: f32, spin: f32, color: Color) {
        let phi = 1.618;
        let verts = [
            Vec3::new(-1.0, phi, 0.0), Vec3::new(1.0, phi, 0.0), Vec3::new(-1.0, -phi, 0.0), Vec3::new(1.0, -phi, 0.0),
            Vec3::new(0.0, -1.0, phi), Vec3::new(0.0, 1.0, phi), Vec3::new(0.0, -1.0, -phi), Vec3::new(0.0, 1.0, -phi),
            Vec3::new(phi, 0.0, -1.0), Vec3::new(phi, 0.0, 1.0), Vec3::new(-phi, 0.0, -1.0), Vec3::new(-phi, 0.0, 1.0),
        ];
        let edges = [
            (0, 1), (0, 5), (0, 7), (0, 10), (0, 11), (1, 5), (1, 7), (1, 8), (1, 9),
            (2, 3), (2, 4), (2, 6), (2, 10), (2, 11), (3, 4), (3, 6), (3, 8), (3, 9),
            (4, 5), (4, 9), (4, 11), (5, 9), (5, 11), (6, 7), (6, 8), (6, 10),
            (7, 8), (7, 10), (8, 9), (10, 11),
        ];
        self.add_edges(project, center, scale * 0.55, spin, &verts, &edges, color);
    }

    fn add_wire_hex(&mut self, project: &mut impl FnMut(Vec3) -> Option<Vec2>, center: Vec3, radius: f32, height: f32, spin: f32, color: Color) {
        let mut top = Vec::with_capacity(6);
        let mut bottom = Vec::with_capacity(6);
        for i in 0..6 {
            let a = i as f32 / 6.0 * PI * 2.0 + spin;
            top.push(center + Vec3::new(a.cos() * radius, height * 0.5, a.sin() * radius));
            bottom.push(center + Vec3::new(a.cos() * radius, -height * 0.5, a.sin() * radius));
        }
        for i in 0..6 {
            let n = (i + 1) % 6;
            self.add_line_projected(project, top[i], top[n], color);
            self.add_line_projected(project, bottom[i], bottom[n], color.with_alpha(color.a * 0.7));
            self.add_line_projected(project, top[i], bottom[i], color.with_alpha(color.a * 0.6));
        }
    }

    fn add_edges(&mut self, project: &mut impl FnMut(Vec3) -> Option<Vec2>, center: Vec3, scale: f32, spin: f32, verts: &[Vec3], edges: &[(usize, usize)], color: Color) {
        let transformed = verts
            .iter()
            .map(|v| center + rotate_y(rotate_x(*v * scale, spin * 0.7), spin))
            .collect::<Vec<_>>();
        for (a, b) in edges {
            self.add_line_projected(project, transformed[*a], transformed[*b], color);
        }
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct StationSnapshot {
    hosts: Vec<StationHost>,
    agents: Vec<StationAgent>,
    events: Vec<StationEvent>,
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationHost {
    id: String,
    name: String,
    platform: String,
    region: String,
    connected: bool,
    cpu: f32,
    mem: f32,
}

impl Default for StationHost {
    fn default() -> Self {
        Self {
            id: "local".into(),
            name: "local".into(),
            platform: "unknown".into(),
            region: "local".into(),
            connected: true,
            cpu: 0.0,
            mem: 0.0,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationAgent {
    id: String,
    host_id: String,
    role: String,
    phase: String,
    status: String,
    task: String,
    provider: String,
    model: String,
    tokens: f32,
    token_cap: f32,
    prompt: f32,
    completion: f32,
    cached: f32,
    cost: f64,
    turns: u32,
    turn_cap: u32,
    autonomy: String,
    worktree: String,
    parent_id: Option<String>,
    needs_approval: bool,
    approval_id: Option<String>,
    approval_command: String,
    approval_category: String,
}

impl Default for StationAgent {
    fn default() -> Self {
        Self {
            id: "agent".into(),
            host_id: "local".into(),
            role: "direct".into(),
            phase: "idle".into(),
            status: "idle".into(),
            task: "idle".into(),
            provider: "unknown".into(),
            model: "unknown".into(),
            tokens: 0.0,
            token_cap: 200_000.0,
            prompt: 0.0,
            completion: 0.0,
            cached: 0.0,
            cost: 0.0,
            turns: 0,
            turn_cap: 0,
            autonomy: "medium".into(),
            worktree: String::new(),
            parent_id: None,
            needs_approval: false,
            approval_id: None,
            approval_command: String::new(),
            approval_category: String::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationEvent {
    id: String,
    host_id: String,
    agent_id: Option<String>,
    ts: String,
    level: String,
    msg: String,
}

impl Default for StationEvent {
    fn default() -> Self {
        Self {
            id: "event".into(),
            host_id: "local".into(),
            agent_id: None,
            ts: String::new(),
            level: "info".into(),
            msg: String::new(),
        }
    }
}

struct DisplaySource {
    host_id: String,
    display_id: String,
    label: String,
    kind: String,
    video: HtmlVideoElement,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LayoutName {
    Orbital,
    Constellation,
}

impl LayoutName {
    fn from_str(s: &str) -> Self {
        match s {
            "constellation" => Self::Constellation,
            _ => Self::Orbital,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mood {
    Cockpit,
    Calm,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SliderKind {
    Fov,
    Motion,
    Ar,
    Density,
}

#[derive(Clone)]
enum HitAction {
    Layout(LayoutName),
    Mood(Mood),
    Slider(SliderKind),
    Select(String),
    ClosePanel,
    Approval { host_id: String, approval_id: String, decision: String },
    OpenDisplay(String),
}

struct HitZone {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    action: HitAction,
}

impl HitZone {
    fn new(x: f32, y: f32, w: f32, h: f32, action: HitAction) -> Self {
        Self { x, y, w, h, action }
    }
}

struct PointerDrag {
    x: f32,
    y: f32,
    last_x: f32,
    last_y: f32,
    moved: bool,
    pending_action: Option<HitAction>,
}

struct Particle {
    start: Vec3,
    end: Vec3,
    born_ms: f64,
    ttl_ms: f64,
    color: Color,
}

#[derive(Clone)]
struct ProjectedNode {
    id: String,
    label: String,
    kind: NodeKind,
    ndc: Vec2,
    radius: f32,
}

impl ProjectedNode {
    fn new(id: &str, label: &str, kind: NodeKind, ndc: Vec2, radius: f32) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            kind,
            ndc,
            radius,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Operator,
    Host,
    Agent,
}

#[derive(Clone, Copy)]
enum Plane {
    XY,
    XZ,
    YZ,
}

#[derive(Clone, Copy, Debug)]
struct Vec2 {
    x: f32,
    y: f32,
}

impl Vec2 {
    fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone, Copy, Debug)]
struct Vec3 {
    x: f32,
    y: f32,
    z: f32,
}

impl Vec3 {
    const ZERO: Self = Self { x: 0.0, y: 0.0, z: 0.0 };
    const Y: Self = Self { x: 0.0, y: 1.0, z: 0.0 };

    fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    fn dot(self, rhs: Self) -> f32 {
        self.x * rhs.x + self.y * rhs.y + self.z * rhs.z
    }

    fn cross(self, rhs: Self) -> Self {
        Self {
            x: self.y * rhs.z - self.z * rhs.y,
            y: self.z * rhs.x - self.x * rhs.z,
            z: self.x * rhs.y - self.y * rhs.x,
        }
    }

    fn len(self) -> f32 {
        self.dot(self).sqrt()
    }

    fn normalized(self) -> Self {
        let len = self.len();
        if len < 0.0001 {
            Self::ZERO
        } else {
            self * (1.0 / len)
        }
    }

    fn lerp(self, rhs: Self, t: f32) -> Self {
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

struct Camera {
    eye: Vec3,
    right: Vec3,
    up: Vec3,
    forward: Vec3,
}

impl Camera {
    fn look_at(eye: Vec3, target: Vec3, world_up: Vec3) -> Self {
        let forward = (target - eye).normalized();
        let right = forward.cross(world_up).normalized();
        let up = right.cross(forward).normalized();
        Self { eye, right, up, forward }
    }

    fn project(&self, world: Vec3, aspect: f32, fov_deg: f32) -> Option<Vec2> {
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

#[derive(Clone, Copy)]
struct Color {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

impl Color {
    const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self {
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            a: 1.0,
        }
    }

    fn with_alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }
}

impl From<Color> for [f32; 4] {
    fn from(value: Color) -> Self {
        [value.r, value.g, value.b, value.a]
    }
}

const C_SURFACE0: Color = Color::rgb(49, 50, 68);
const C_OVERLAY1: Color = Color::rgb(127, 132, 156);
const C_BLUE: Color = Color::rgb(137, 180, 250);
const C_LAVENDER: Color = Color::rgb(180, 190, 254);
const C_SAPPHIRE: Color = Color::rgb(116, 199, 236);
const C_TEAL: Color = Color::rgb(148, 226, 213);
const C_GREEN: Color = Color::rgb(166, 227, 161);
const C_YELLOW: Color = Color::rgb(249, 226, 175);
const C_PEACH: Color = Color::rgb(250, 179, 135);
const C_RED: Color = Color::rgb(243, 139, 168);
const C_MAUVE: Color = Color::rgb(203, 166, 247);

const C_TEXT_CSS: &str = "#cdd6f4";
const C_SUBTEXT0_CSS: &str = "#a6adc8";
const C_OVERLAY0_CSS: &str = "#6c7086";
const C_OVERLAY1_CSS: &str = "#7f849c";
const C_BLUE_CSS: &str = "#89b4fa";
const C_TEAL_CSS: &str = "#94e2d5";
const C_GREEN_CSS: &str = "#a6e3a1";
const C_YELLOW_CSS: &str = "#f9e2af";
const C_PEACH_CSS: &str = "#fab387";
const C_RED_CSS: &str = "#f38ba8";
const C_MAUVE_CSS: &str = "#cba6f7";

fn role_color(role: &str) -> Color {
    match role {
        "orchestrator" => C_BLUE,
        "sub-agent" => C_MAUVE,
        "direct" => C_TEAL,
        _ => C_TEAL,
    }
}

fn role_color_css(role: &str) -> &'static str {
    match role {
        "orchestrator" => C_BLUE_CSS,
        "sub-agent" => C_MAUVE_CSS,
        "direct" => C_TEAL_CSS,
        _ => C_TEAL_CSS,
    }
}

fn phase_color(phase: &str) -> Color {
    match phase {
        "thinking" => C_LAVENDER,
        "running" => C_TEAL,
        "waiting" => C_YELLOW,
        "done" => C_GREEN,
        _ => C_OVERLAY1,
    }
}

fn phase_color_css(phase: &str) -> &'static str {
    match phase {
        "thinking" => "#b4befe",
        "running" => C_TEAL_CSS,
        "waiting" => C_YELLOW_CSS,
        "done" => C_GREEN_CSS,
        _ => C_OVERLAY1_CSS,
    }
}

fn level_color(level: &str) -> Color {
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

fn level_color_css(level: &str) -> &'static str {
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

fn rotate_y(v: Vec3, a: f32) -> Vec3 {
    let (s, c) = a.sin_cos();
    Vec3::new(v.x * c + v.z * s, v.y, -v.x * s + v.z * c)
}

fn rotate_x(v: Vec3, a: f32) -> Vec3 {
    let (s, c) = a.sin_cos();
    Vec3::new(v.x, v.y * c - v.z * s, v.y * s + v.z * c)
}

fn ndc_to_screen(pos: [f32; 2], width: u32, height: u32) -> Vec2 {
    Vec2::new((pos[0] * 0.5 + 0.5) * width as f32, (0.5 - pos[1] * 0.5) * height as f32)
}

fn css_rgba(color: [f32; 4]) -> String {
    format!(
        "rgba({:.0},{:.0},{:.0},{:.3})",
        color[0] * 255.0,
        color[1] * 255.0,
        color[2] * 255.0,
        color[3]
    )
}

fn truncate(s: &str, max: usize) -> String {
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

fn stable_angle(s: &str) -> f32 {
    stable_unit(s) * PI * 2.0
}

fn stable_unit(s: &str) -> f32 {
    let mut h = 2166136261u32;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h as f32 / u32::MAX as f32).clamp(0.0, 1.0)
}

fn lcg(seed: u32) -> u32 {
    seed.wrapping_mul(1664525).wrapping_add(1013904223)
}

fn unit(seed: u32) -> f32 {
    seed as f32 / u32::MAX as f32
}

fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

fn request_animation_frame(f: &Closure<dyn FnMut(f64)>) {
    let _ = web_sys::window()
        .expect("window")
        .request_animation_frame(f.as_ref().unchecked_ref());
}
