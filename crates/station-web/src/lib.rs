use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::f32::consts::PI;
use std::rc::Rc;

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::spawn_local;
use web_sys::{
    CanvasRenderingContext2d, DeviceOrientationEvent, Event, HtmlCanvasElement, HtmlVideoElement,
    KeyboardEvent, PointerEvent, WheelEvent,
};
#[cfg(target_arch = "wasm32")]
use wgpu::util::DeviceExt;

#[wasm_bindgen]
pub struct StationWeb {
    inner: Rc<RefCell<StationInner>>,
}

#[wasm_bindgen]
impl StationWeb {
    #[wasm_bindgen(constructor)]
    pub fn new(
        scene_canvas: HtmlCanvasElement,
        hud_canvas: HtmlCanvasElement,
    ) -> Result<StationWeb, JsValue> {
        console_error_panic_hook::set_once();
        let ctx = hud_canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("Station HUD canvas has no 2D context"))?
            .dyn_into::<CanvasRenderingContext2d>()?;
        let use_webgpu = station_enable_webgpu();
        let scene_ctx = if use_webgpu {
            None
        } else {
            scene_canvas
                .get_context("2d")?
                .and_then(|ctx| ctx.dyn_into::<CanvasRenderingContext2d>().ok())
        };

        let inner = Rc::new(RefCell::new(StationInner::new(
            scene_canvas,
            hud_canvas,
            ctx,
            scene_ctx,
        )));
        StationInner::install_events(inner.clone())?;
        if use_webgpu {
            StationInner::start_gpu(inner.clone());
        } else {
            web_sys::console::warn_1(&JsValue::from_str(
                "Station WebGPU disabled; add station_gpu=webgpu to enable it",
            ));
        }
        StationInner::start_loop(inner.clone());
        Ok(Self { inner })
    }

    pub fn set_active(&self, active: bool) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.active = active;
            inner.last_render_ms = 0.0;
        }
        StationInner::schedule_next_loop(&self.inner);
    }

    pub fn set_action_callback(&self, callback: js_sys::Function) {
        self.inner.borrow_mut().action_callback = Some(callback);
    }

    pub fn resize(&self) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.resize();
            inner.last_render_ms = 0.0;
        }
        StationInner::schedule_next_loop(&self.inner);
    }

    pub fn update_snapshot(&self, snapshot: JsValue) -> Result<(), JsValue> {
        let snapshot: StationSnapshot = serde_wasm_bindgen::from_value(snapshot)?;
        self.inner.borrow_mut().apply_snapshot(snapshot);
        StationInner::schedule_next_loop(&self.inner);
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
        {
            let mut inner = self.inner.borrow_mut();
            inner.display_sources.insert(
                source_id,
                DisplaySource {
                    host_id,
                    display_id,
                    label,
                    kind,
                    video,
                },
            );
            inner.last_render_ms = 0.0;
        }
        StationInner::schedule_next_loop(&self.inner);
    }

    pub fn unregister_display_source(&self, source_id: String) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.display_sources.remove(&source_id);
            inner.last_render_ms = 0.0;
        }
        StationInner::schedule_next_loop(&self.inner);
    }

    pub fn set_layout(&self, layout: String) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.layout = LayoutName::from_str(&layout);
            inner.last_render_ms = 0.0;
        }
        StationInner::schedule_next_loop(&self.inner);
    }

    pub fn set_visuals(
        &self,
        mood: String,
        fov_deg: f32,
        motion: f32,
        ar_strength: f32,
        density: f32,
    ) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.mood = Mood::from_str(&mood);
            inner.fov_deg = fov_deg.clamp(35.0, 85.0);
            inner.motion = motion.clamp(0.0, 2.0);
            inner.ar_strength = ar_strength.clamp(0.0, 1.0);
            inner.density = density.clamp(0.5, 1.8);
            inner.last_render_ms = 0.0;
        }
        StationInner::schedule_next_loop(&self.inner);
    }

    pub fn select_by_id(&self, id: Option<String>) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.selected_id = id;
            inner.panel_scroll = 0.0;
            inner.last_render_ms = 0.0;
        }
        StationInner::schedule_next_loop(&self.inner);
    }

    pub fn focus_on(&self, id: String) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.focus_id = Some(id);
            inner.last_render_ms = 0.0;
        }
        StationInner::schedule_next_loop(&self.inner);
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
    active_pointers: HashMap<i32, Vec2>,
    pinch_zoom: Option<PinchZoom>,
    drag_slider: Option<SliderKind>,
    ar_x: f32,
    ar_y: f32,
    panel_scroll: f32,
    projected_nodes: Vec<ProjectedNode>,
    hit_zones: Vec<HitZone>,
    action_callback: Option<js_sys::Function>,
    _events: Vec<Closure<dyn FnMut(Event)>>,
    _raf: Option<Closure<dyn FnMut(f64)>>,
    loop_pending: bool,
    boot_started_ms: f64,
    last_render_ms: f64,
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
            active_pointers: HashMap::new(),
            pinch_zoom: None,
            drag_slider: None,
            ar_x: 0.0,
            ar_y: 0.0,
            panel_scroll: 0.0,
            projected_nodes: Vec::new(),
            hit_zones: Vec::new(),
            action_callback: None,
            _events: Vec::new(),
            _raf: None,
            loop_pending: false,
            boot_started_ms: now_ms(),
            last_render_ms: 0.0,
        };
        inner.resize();
        inner
    }

    fn install_events(inner: Rc<RefCell<Self>>) -> Result<(), JsValue> {
        let target_canvas = inner.borrow().hud_canvas.clone();
        let target: web_sys::EventTarget = target_canvas.clone().into();
        let window = web_sys::window().ok_or_else(|| JsValue::from_str("window unavailable"))?;

        let down_inner = inner.clone();
        let down_canvas = target_canvas.clone();
        let down = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<PointerEvent>() else {
                return;
            };
            e.prevent_default();
            let _ = down_canvas.set_pointer_capture(e.pointer_id());
            let mut s = down_inner.borrow_mut();
            s.mark_input();
            let (x, y) = s.event_xy(e.client_x() as f64, e.client_y() as f64);
            s.active_pointers.insert(e.pointer_id(), Vec2::new(x, y));
            if s.active_pointers.len() >= 2 {
                s.begin_pinch();
                s.pointer_down = None;
                s.drag_slider = None;
                s.set_cursor("drag");
                return;
            }
            if let Some(action) = s.hit_action_at(x, y) {
                match action {
                    HitAction::Slider(kind) => {
                        s.drag_slider = Some(kind);
                        s.apply_slider_at(kind, x);
                        s.set_cursor("drag");
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
                        s.set_cursor("pointer");
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
            s.set_cursor("drag");
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("pointerdown", down.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(down);

        let move_inner = inner.clone();
        let mv = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<PointerEvent>() else {
                return;
            };
            let mut s = move_inner.borrow_mut();
            let (x, y) = s.event_xy(e.client_x() as f64, e.client_y() as f64);
            if s.active_pointers.contains_key(&e.pointer_id()) {
                s.active_pointers.insert(e.pointer_id(), Vec2::new(x, y));
            }
            if s.active_pointers.len() >= 2 {
                s.apply_pinch();
                s.mark_input();
                s.set_cursor("drag");
                return;
            }
            if let Some(kind) = s.drag_slider {
                s.apply_slider_at(kind, x);
                s.set_cursor("drag");
                return;
            }
            if let Some(drag) = s.pointer_down.as_mut() {
                let dx = x - drag.last_x;
                let dy = y - drag.last_y;
                drag.last_x = x;
                drag.last_y = y;
                let travel = (x - drag.x).abs() + (y - drag.y).abs();
                if drag.pending_action.is_some() && travel <= 12.0 {
                    s.mark_input();
                    return;
                }
                if travel > 4.0 {
                    drag.moved = true;
                    drag.pending_action = None;
                }
                s.yaw -= dx * 0.006;
                s.pitch = (s.pitch + dy * 0.005).clamp(-1.05, 1.05);
                s.mark_input();
                s.set_cursor("drag");
            } else {
                s.hovered_id = s.pick_node(x, y);
                if s.hit_action_at(x, y).is_some() || s.hovered_id.is_some() {
                    s.set_cursor("pointer");
                } else {
                    s.set_cursor("grab");
                }
            }
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("pointermove", mv.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(mv);

        let up_inner = inner.clone();
        let up_canvas = target_canvas.clone();
        let up = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<PointerEvent>() else {
                return;
            };
            e.prevent_default();
            let _ = up_canvas.release_pointer_capture(e.pointer_id());
            let outbound = {
                let mut s = up_inner.borrow_mut();
                let (x, y) = s.event_xy(e.client_x() as f64, e.client_y() as f64);
                s.active_pointers.remove(&e.pointer_id());
                if s.active_pointers.len() < 2 {
                    s.pinch_zoom = None;
                }
                if s.drag_slider.take().is_some() {
                    None
                } else if let Some(drag) = s.pointer_down.take() {
                    if let Some(action) = drag.pending_action {
                        s.dispatch_hit(action, x, y)
                    } else if !drag.moved {
                        if s.info_panel_hit(x, y) {
                            None
                        } else {
                            s.selected_id = s.pick_node(x, y);
                            s.panel_scroll = 0.0;
                            s.last_render_ms = 0.0;
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            up_inner.borrow_mut().set_cursor("grab");
            if let Some(action) = outbound {
                let callback = up_inner.borrow().action_callback.clone();
                StationInner::emit_action(callback, action);
            }
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("pointerup", up.as_ref().unchecked_ref())?;
        target.add_event_listener_with_callback("pointercancel", up.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(up);

        let wheel_inner = inner.clone();
        let wheel = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<WheelEvent>() else {
                return;
            };
            e.prevent_default();
            let mut s = wheel_inner.borrow_mut();
            s.mark_input();
            let (x, y) = s.event_xy(e.client_x() as f64, e.client_y() as f64);
            if s.info_panel_hit(x, y) {
                s.panel_scroll = (s.panel_scroll + e.delta_y() as f32).max(0.0);
            } else {
                s.distance = (s.distance + e.delta_y() as f32 * 0.014).clamp(4.2, 25.0);
            }
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("wheel", wheel.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(wheel);

        let key_inner = inner.clone();
        let key = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<KeyboardEvent>() else {
                return;
            };
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
            let Some(e) = event.dyn_ref::<DeviceOrientationEvent>() else {
                return;
            };
            let mut s = orientation_inner.borrow_mut();
            let gamma = e.gamma().unwrap_or(0.0) as f32;
            let beta = e.beta().unwrap_or(0.0) as f32;
            s.ar_x = (gamma / 45.0).clamp(-1.0, 1.0);
            s.ar_y = (beta / 60.0).clamp(-1.0, 1.0);
        }) as Box<dyn FnMut(_)>);
        window.add_event_listener_with_callback(
            "deviceorientation",
            orientation.as_ref().unchecked_ref(),
        )?;
        inner.borrow_mut()._events.push(orientation);

        let resize_inner = inner.clone();
        let resize = Closure::wrap(Box::new(move |_event: Event| {
            {
                let mut s = resize_inner.borrow_mut();
                s.resize();
                s.last_render_ms = 0.0;
            }
            StationInner::schedule_next_loop(&resize_inner);
        }) as Box<dyn FnMut(_)>);
        window.add_event_listener_with_callback("resize", resize.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(resize);

        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    fn start_gpu(inner: Rc<RefCell<Self>>) {
        let canvas = inner.borrow().scene_canvas.clone();
        spawn_local(async move {
            match GpuState::new(canvas).await {
                Ok(gpu) => {
                    {
                        let mut s = inner.borrow_mut();
                        s.gpu = Some(gpu);
                        s.last_render_ms = 0.0;
                        s.resize();
                    }
                    StationInner::schedule_next_loop(&inner);
                }
                Err(err) => {
                    web_sys::console::warn_1(&JsValue::from_str(&format!(
                        "Station WebGPU unavailable, using HUD canvas fallback: {err:?}"
                    )));
                }
            }
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_gpu(_inner: Rc<RefCell<Self>>) {}

    fn start_loop(inner: Rc<RefCell<Self>>) {
        let loop_inner = inner.clone();
        let cb = Closure::wrap(Box::new(move |time_ms: f64| {
            {
                let mut s = loop_inner.borrow_mut();
                s.loop_pending = false;
                s.render(time_ms);
            }
            StationInner::schedule_next_loop(&loop_inner);
        }) as Box<dyn FnMut(f64)>);

        inner.borrow_mut()._raf = Some(cb);
        StationInner::schedule_next_loop(&inner);
    }

    fn schedule_next_loop(inner: &Rc<RefCell<Self>>) {
        let (next, has_gpu) = {
            let mut s = inner.borrow_mut();
            let Some(next) = s
                ._raf
                .as_ref()
                .map(|cb| cb.as_ref().unchecked_ref::<js_sys::Function>().clone())
            else {
                return;
            };
            let has_gpu = s.gpu.is_some();
            let should_schedule = if has_gpu {
                s.active
            } else {
                s.active
                    && (s.last_render_ms == 0.0
                        || s.last_input_ms > s.last_render_ms
                        || now_ms() - s.boot_started_ms < 1300.0)
            };
            if !should_schedule || s.loop_pending {
                return;
            }
            s.loop_pending = true;
            (next, has_gpu)
        };
        let Some(window) = web_sys::window() else {
            inner.borrow_mut().loop_pending = false;
            return;
        };
        let callback = Closure::once_into_js(move || {
            let _ = next.call1(&JsValue::NULL, &JsValue::from_f64(now_ms()));
        });
        let delay_ms = if has_gpu { 250 } else { 180 };
        if window
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                callback.as_ref().unchecked_ref(),
                delay_ms,
            )
            .is_err()
        {
            inner.borrow_mut().loop_pending = false;
        }
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
        if self
            .selected_id
            .as_ref()
            .is_some_and(|id| !self.node_exists(id))
        {
            self.selected_id = None;
        }
        self.last_render_ms = 0.0;
    }

    fn node_exists(&self, id: &str) -> bool {
        id == "op"
            || matches!(
                id,
                "system:activity"
                    | "system:context"
                    | "system:managed"
                    | "system:changes"
                    | "system:sessions"
                    | "system:worktrees"
                    | "system:peers"
                    | "system:controls"
                    | "system:view"
            )
            || self
                .snapshot
                .hosts
                .iter()
                .any(|h| format!("host:{}", h.id) == id)
            || id
                .strip_prefix("activity:")
                .is_some_and(|event_id| self.activity_event(event_id).is_some())
            || self.snapshot.agents.iter().any(|a| a.id == id)
    }

    fn resize(&mut self) {
        let max_dpr = if self.gpu.is_some() { 2.0 } else { 1.0 };
        let dpr = web_sys::window()
            .and_then(|w| Some(w.device_pixel_ratio()))
            .unwrap_or(1.0)
            .clamp(1.0, max_dpr);
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
        if !self.active {
            return;
        }
        if self.gpu.is_none()
            && self.last_render_ms > 0.0
            && self.last_input_ms <= self.last_render_ms
        {
            return;
        }
        self.resize();
        let min_frame_ms = if self.gpu.is_some() { 120.0 } else { 250.0 };
        if self.last_render_ms > 0.0 && time_ms - self.last_render_ms < min_frame_ms {
            return;
        }
        self.last_render_ms = time_ms;
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
                web_sys::console::warn_1(&JsValue::from_str(&format!(
                    "Station GPU render failed: {err:?}"
                )));
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

        let mut live_particles = Vec::with_capacity(self.particles.len());
        for particle in self.particles.drain(..) {
            let t = ((time_ms - particle.born_ms) as f32 / particle.ttl_ms as f32).clamp(0.0, 1.0);
            if t < 1.0 {
                let lifted = particle.start.lerp(particle.end, t)
                    + Vec3::new(0.0, (t * PI).sin() * 0.6, 0.0);
                if let Some(p) = project(lifted) {
                    let size = (0.026 * (1.0 - t) + 0.006) * self.density;
                    frame.add_quad_ndc(
                        p.x,
                        p.y,
                        size,
                        particle.color.with_alpha(0.88 * (1.0 - t)).into(),
                    );
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
            0.82 + (time_ms as f32 * 0.003).sin() * 0.035,
            C_PEACH.with_alpha(0.28),
            Plane::XZ,
        );
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
                0.72 + (time_ms as f32 * 0.004).sin() * 0.05,
                C_TEAL.with_alpha(0.22),
                Plane::XY,
            );
        }
        if agent.needs_approval {
            frame.add_ring(
                project,
                pos,
                0.84 + (time_ms as f32 * 0.006).sin() * 0.07,
                C_YELLOW.with_alpha(0.58),
                Plane::XY,
            );
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
        self.ctx
            .set_transform(self.dpr, 0.0, 0.0, self.dpr, 0.0, 0.0)
            .ok();
        let w = self.css_width();
        let h = self.css_height();
        self.ctx.clear_rect(0.0, 0.0, w as f64, h as f64);
        self.hit_zones.clear();

        self.draw_vignette(w, h);
        self.draw_display_thumbnails(frame);
        self.draw_toolbar(w);
        self.draw_tweaks_panel();
        self.draw_systems_panel();
        self.draw_execution_deck(w, h);
        self.draw_signal_runway(w, h);
        self.draw_operations_runway(w, h);
        self.draw_display_switchboard(w, h);
        self.draw_activity_detail_rail(w, h);
        self.draw_continuity_detail_bar(w, h);
        self.draw_command_lane(w, h);
        self.draw_attention_strip(w, h);
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
            let Some(node) = by_host.get(source.host_id.as_str()) else {
                continue;
            };
            let center = ndc_to_screen([node.ndc.x, node.ndc.y], self.width, self.height);
            let css = Vec2::new(center.x / self.dpr as f32, center.y / self.dpr as f32);
            let tw = 164.0_f32.min(self.css_width() * 0.28).max(98.0);
            let th = tw * 0.5625;
            let x = css.x - tw / 2.0;
            let y = css.y - 118.0 - th * 0.2;
            self.round_rect(
                x,
                y,
                tw,
                th,
                5.0,
                "rgba(17,17,27,0.86)",
                "rgba(250,179,135,0.82)",
            );
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
                self.ctx
                    .set_fill_style(&JsValue::from_str("rgba(49,50,68,0.55)"));
                self.ctx.fill_rect(
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

    fn draw_toolbar(&mut self, w: f32) {
        self.ctx
            .set_fill_style(&JsValue::from_str("rgba(24,24,37,0.88)"));
        self.ctx.fill_rect(0.0, 0.0, w as f64, 39.0);
        self.ctx
            .set_stroke_style(&JsValue::from_str("rgba(49,50,68,0.92)"));
        self.line(0.0, 39.0, w, 39.0);
        self.text("STATION", 13.0, 24.0, 10.0, C_OVERLAY1_CSS, "bold");
        let mut x = 86.0;
        self.pill_button(
            x,
            9.0,
            66.0,
            22.0,
            "orbital",
            self.layout == LayoutName::Orbital,
            HitAction::Layout(LayoutName::Orbital),
        );
        x += 72.0;
        self.pill_button(
            x,
            9.0,
            102.0,
            22.0,
            "constellation",
            self.layout == LayoutName::Constellation,
            HitAction::Layout(LayoutName::Constellation),
        );
        x += 124.0;
        let active_agents = self
            .snapshot
            .agents
            .iter()
            .filter(|a| a.status == "in_progress")
            .count();
        self.text(
            &format!(
                "{} hosts · {} active agents",
                self.snapshot.hosts.len(),
                active_agents
            ),
            x,
            24.0,
            11.0,
            C_SUBTEXT0_CSS,
            "normal",
        );
        let pending = self
            .snapshot
            .agents
            .iter()
            .filter(|a| a.needs_approval)
            .count();
        if pending > 0 {
            self.pill(
                x + 178.0,
                9.0,
                118.0,
                22.0,
                &format!("{pending} approval{}", if pending == 1 { "" } else { "s" }),
                C_YELLOW_CSS,
            );
        }
        let role_counts = [
            ("orch", "orchestrator", C_BLUE_CSS),
            ("direct", "direct", C_TEAL_CSS),
            ("sub", "sub-agent", C_MAUVE_CSS),
        ];
        let mut rx = w - 225.0;
        for (label, role, color) in role_counts {
            let count = self
                .snapshot
                .agents
                .iter()
                .filter(|a| a.role == role)
                .count();
            self.pill(rx, 9.0, 66.0, 22.0, &format!("{label} {count}"), color);
            rx += 72.0;
        }
    }

    fn draw_tweaks_panel(&mut self) {
        let x = 12.0;
        let y = 52.0;
        let w = 222.0;
        self.round_rect(
            x,
            y,
            w,
            178.0,
            6.0,
            "rgba(24,24,37,0.78)",
            "rgba(69,71,90,0.74)",
        );
        self.text("TWEAKS", x + 11.0, y + 19.0, 10.0, C_OVERLAY1_CSS, "bold");
        self.pill_button(
            x + 82.0,
            y + 7.0,
            58.0,
            22.0,
            "cockpit",
            self.mood == Mood::Cockpit,
            HitAction::Mood(Mood::Cockpit),
        );
        self.pill_button(
            x + 145.0,
            y + 7.0,
            48.0,
            22.0,
            "calm",
            self.mood == Mood::Calm,
            HitAction::Mood(Mood::Calm),
        );
        self.slider(
            x + 12.0,
            y + 47.0,
            190.0,
            "fov",
            self.fov_deg,
            35.0,
            85.0,
            SliderKind::Fov,
        );
        self.slider(
            x + 12.0,
            y + 78.0,
            190.0,
            "motion",
            self.motion,
            0.0,
            2.0,
            SliderKind::Motion,
        );
        self.slider(
            x + 12.0,
            y + 109.0,
            190.0,
            "ar",
            self.ar_strength,
            0.0,
            1.0,
            SliderKind::Ar,
        );
        self.slider(
            x + 12.0,
            y + 140.0,
            190.0,
            "density",
            self.density,
            0.5,
            1.8,
            SliderKind::Density,
        );
    }

    fn draw_systems_panel(&mut self) {
        let compact_grid = self.css_height() < 620.0 && self.css_width() > 760.0;
        let x = if compact_grid { 246.0 } else { 12.0 };
        let y = if compact_grid { 52.0 } else { 242.0 };
        let w = if compact_grid { 390.0 } else { 222.0 };
        let card_count = 9.0;
        let rows = if compact_grid {
            (card_count / 2.0_f32).ceil()
        } else {
            card_count
        };
        let card_gap = if compact_grid { 6.0 } else { 4.0 };
        let card_col_gap = if compact_grid { 6.0 } else { 0.0 };
        let inner_w = w - 20.0;
        let card_w = if compact_grid {
            (inner_w - card_col_gap) / 2.0
        } else {
            inner_w
        };
        let card_h = if compact_grid {
            44.0
        } else {
            let available_h = (self.css_height() - y - 14.0).max(0.0);
            ((available_h - 45.0 - (card_gap * (card_count - 1.0))) / card_count).clamp(38.0, 47.0)
        };
        let panel_h = 45.0 + (card_h * rows) + (card_gap * (rows - 1.0));
        self.round_rect(
            x,
            y,
            w,
            panel_h,
            6.0,
            "rgba(24,24,37,0.78)",
            "rgba(69,71,90,0.74)",
        );
        self.text(
            "CONTROL CENTER",
            x + 11.0,
            y + 19.0,
            10.0,
            C_OVERLAY1_CSS,
            "bold",
        );

        let mut card_idx = 0usize;
        let card_slot = |idx: usize| -> (f32, f32) {
            if compact_grid {
                let col = (idx % 2) as f32;
                let row = (idx / 2) as f32;
                (
                    x + 10.0 + col * (card_w + card_col_gap),
                    y + 34.0 + row * (card_h + card_gap),
                )
            } else {
                (x + 10.0, y + 34.0 + idx as f32 * (card_h + card_gap))
            }
        };
        let latest_event = self.snapshot.events.last();
        let activity_value = format!("{} events", self.snapshot.events.len());
        let activity_detail = latest_event
            .map(|ev| truncate(&format!("{} {}", ev.level, ev.msg), 34))
            .unwrap_or_else(|| "waiting for activity".to_string());
        let (card_x, card_y) = card_slot(card_idx);
        card_idx += 1;
        self.summary_card(
            card_x,
            card_y,
            card_w,
            card_h,
            "Activity",
            &activity_value,
            &activity_detail,
            latest_event
                .map(|ev| level_color_css(&ev.level))
                .unwrap_or(C_OVERLAY1_CSS),
            "system:activity",
        );

        let ctx_pct = percent(
            self.snapshot.context.tokens,
            self.snapshot.context.effective_window,
        );
        let context_value = if self.snapshot.context.available {
            format!(
                "{} · {} items",
                pct_label(ctx_pct),
                self.snapshot.context.item_count
            )
        } else {
            "waiting".to_string()
        };
        let context_detail = if self.snapshot.context.available {
            truncate(
                &format!(
                    "{} {}",
                    self.snapshot.context.source, self.snapshot.context.turn
                ),
                34,
            )
        } else {
            "no context snapshot".to_string()
        };
        let (card_x, card_y) = card_slot(card_idx);
        card_idx += 1;
        self.summary_card(
            card_x,
            card_y,
            card_w,
            card_h,
            "Context",
            &context_value,
            &context_detail,
            pressure_color(ctx_pct),
            "system:context",
        );

        let managed_pct = percent(
            self.snapshot.managed.used_tokens,
            self.snapshot.managed.effective_window,
        );
        let managed_value = format!(
            "{} · {}",
            self.snapshot.managed.mode, self.snapshot.managed.status
        );
        let managed_detail = format!(
            "{} records · {} anchors",
            self.snapshot.managed.records, self.snapshot.managed.anchors
        );
        let (card_x, card_y) = card_slot(card_idx);
        card_idx += 1;
        self.summary_card(
            card_x,
            card_y,
            card_w,
            card_h,
            "Managed",
            &truncate(&managed_value, 28),
            &managed_detail,
            pressure_color(managed_pct),
            "system:managed",
        );

        let changes = &self.snapshot.changes;
        let changes_value = if changes.count > 0 {
            format!(
                "{} files · +{} -{}",
                changes.count,
                compact_number(changes.total_added as f64),
                compact_number(changes.total_removed as f64)
            )
        } else {
            nonempty(&changes.status, "clean")
        };
        let changes_detail = if changes.latest_path.is_empty() {
            if changes.count > 0 {
                "tracked changes".to_string()
            } else {
                "working tree clean".to_string()
            }
        } else {
            truncate(&changes.latest_path, 34)
        };
        let (card_x, card_y) = card_slot(card_idx);
        card_idx += 1;
        self.summary_card(
            card_x,
            card_y,
            card_w,
            card_h,
            "Changes",
            &changes_value,
            &changes_detail,
            if changes.count > 0 || changes.status == "mismatch" {
                C_YELLOW_CSS
            } else {
                C_GREEN_CSS
            },
            "system:changes",
        );

        let session_value = format!(
            "{} total · {} active",
            self.snapshot.sessions.total, self.snapshot.sessions.active
        );
        let session_detail = if self.snapshot.sessions.latest_task.is_empty() {
            format!("{} external", self.snapshot.sessions.external)
        } else {
            truncate(&self.snapshot.sessions.latest_task, 34)
        };
        let (card_x, card_y) = card_slot(card_idx);
        card_idx += 1;
        self.summary_card(
            card_x,
            card_y,
            card_w,
            card_h,
            "Sessions",
            &session_value,
            &session_detail,
            if self.snapshot.sessions.active > 0 {
                C_TEAL_CSS
            } else {
                C_BLUE_CSS
            },
            "system:sessions",
        );

        let worktree_value = format!(
            "{} scanned · {} cleanup",
            self.snapshot.sessions.worktrees, self.snapshot.sessions.worktree_cleanup
        );
        let worktree_detail = format!(
            "{} dirty · {} unmerged · {} active",
            self.snapshot.sessions.worktree_dirty,
            self.snapshot.sessions.worktree_unmerged,
            self.snapshot.sessions.worktree_active
        );
        let (card_x, card_y) = card_slot(card_idx);
        card_idx += 1;
        self.summary_card(
            card_x,
            card_y,
            card_w,
            card_h,
            "Worktrees",
            &worktree_value,
            &worktree_detail,
            if self.snapshot.sessions.worktree_dirty > 0
                || self.snapshot.sessions.worktree_unmerged > 0
                || self.snapshot.sessions.worktree_active > 0
            {
                C_YELLOW_CSS
            } else {
                C_BLUE_CSS
            },
            "system:worktrees",
        );

        let peer_count = self.snapshot.hosts.len().saturating_sub(1);
        let display_count = self.display_sources.len();
        let peer_value = format!("{peer_count} peers · {display_count} displays");
        let peer_detail = self
            .display_sources
            .values()
            .next()
            .map(|source| truncate(&source.label, 34))
            .unwrap_or_else(|| "local and federated displays".to_string());
        let (card_x, card_y) = card_slot(card_idx);
        card_idx += 1;
        self.summary_card(
            card_x,
            card_y,
            card_w,
            card_h,
            "Peers",
            &peer_value,
            &peer_detail,
            if display_count > 0 {
                C_PEACH_CSS
            } else {
                C_BLUE_CSS
            },
            "system:peers",
        );

        let controls = &self.snapshot.controls;
        let control_value = truncate(
            &format!(
                "{} · {}",
                nonempty(&controls.backend, "agent"),
                nonempty(&controls.sandbox, "sandbox")
            ),
            30,
        );
        let control_detail = truncate(
            &format!(
                "{} · managed {}",
                nonempty(&controls.approval_policy, "approval"),
                nonempty(&controls.managed_context, "unknown")
            ),
            34,
        );
        let (card_x, card_y) = card_slot(card_idx);
        self.summary_card(
            card_x,
            card_y,
            card_w,
            card_h,
            "Control",
            &control_value,
            &control_detail,
            C_MAUVE_CSS,
            "system:controls",
        );
        card_idx += 1;

        let view_value = format!("{} · {}", self.layout.label(), self.mood.label());
        let view_detail = format!(
            "fov {} · density {:.1}",
            self.fov_deg.round() as i32,
            self.density
        );
        let (card_x, card_y) = card_slot(card_idx);
        self.summary_card(
            card_x,
            card_y,
            card_w,
            card_h,
            "View",
            &view_value,
            &view_detail,
            C_MAUVE_CSS,
            "system:view",
        );
    }

    fn draw_command_lane(&mut self, w: f32, h: f32) {
        if w < 360.0 || h < 340.0 {
            return;
        }
        let compact = w < 760.0;
        let left_clear = if compact { 14.0 } else { 246.0 };
        let max_w = if compact { w - 28.0 } else { 980.0 };
        let lane_w = (w - left_clear - 14.0).min(max_w).max(300.0);
        let dense = compact || lane_w < 760.0;
        let lane_h = if dense { 104.0 } else { 78.0 };
        let x = if compact {
            14.0
        } else {
            ((w - lane_w) * 0.5).max(left_clear)
        };
        let y = (h - lane_h - 14.0).max(52.0);

        self.round_rect(
            x,
            y,
            lane_w,
            lane_h,
            6.0,
            "rgba(17,17,27,0.86)",
            "rgba(137,180,250,0.62)",
        );
        self.text("COMMAND LANE", x + 12.0, y + 18.0, 10.0, C_BLUE_CSS, "bold");

        let controls = self.snapshot.controls.clone();
        let status_a = format!(
            "{} / {} / {}",
            nonempty(&controls.backend, "agent"),
            nonempty(&controls.sandbox, "sandbox"),
            nonempty(&controls.approval_policy, "approval")
        );
        let status_b = format!(
            "managed {} / context {} / changes {}",
            nonempty(&controls.managed_context, "unknown"),
            pct_label(percent(
                self.snapshot.context.tokens,
                self.snapshot.context.effective_window,
            )),
            nonempty(&self.snapshot.changes.status, "clean")
        );
        if dense {
            self.text(
                &truncate(&status_a, 48),
                x + 12.0,
                y + 36.0,
                10.0,
                C_TEXT_CSS,
                "normal",
            );
            self.text(
                &truncate(&status_b, 48),
                x + 12.0,
                y + 50.0,
                9.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
        } else {
            self.status_chip(x + 112.0, y + 8.0, 230.0, &status_a, C_TEAL_CSS);
            self.status_chip(x + 352.0, y + 8.0, 250.0, &status_b, C_MAUVE_CSS);
            let session = if controls.session_active {
                "active"
            } else if controls.session_detached {
                "detached"
            } else {
                "idle"
            };
            self.status_chip(
                x + 612.0,
                y + 8.0,
                (lane_w - 624.0).max(130.0),
                &format!(
                    "target {} / {}",
                    nonempty(&controls.session_selection, "none"),
                    session
                ),
                C_PEACH_CSS,
            );
        }

        let mut actions = vec![
            LaneAction::activity(
                if controls.prompt_mode == "steer" {
                    "steer"
                } else {
                    "send"
                },
                "send",
                62.0,
                C_BLUE_CSS,
            ),
            LaneAction::activity("new session", "new-session", 92.0, C_TEAL_CSS),
        ];
        if controls.session_can_focus {
            actions.push(LaneAction::activity("focus", "target", 58.0, C_PEACH_CSS));
        }
        if controls.session_can_interrupt {
            actions.push(LaneAction::activity("stop", "stop", 50.0, C_RED_CSS));
        }
        if controls.shared_view_can_take_input {
            actions.push(LaneAction::controls(
                "take input",
                "shared-view-take-input",
                84.0,
                C_YELLOW_CSS,
            ));
        }
        actions.extend([
            LaneAction::select("context", "system:context", 72.0, C_BLUE_CSS),
            LaneAction::select("managed", "system:managed", 76.0, C_MAUVE_CSS),
            LaneAction::select("sessions", "system:sessions", 76.0, C_TEAL_CSS),
            LaneAction::select("peers", "system:peers", 58.0, C_PEACH_CSS),
            LaneAction::select("changes", "system:changes", 72.0, C_YELLOW_CSS),
            LaneAction::select("controls", "system:controls", 74.0, C_MAUVE_CSS),
        ]);

        let mut ax = x + 12.0;
        let mut ay = y + if dense { 64.0 } else { 45.0 };
        let max_x = x + lane_w - 12.0;
        let max_rows = if dense { 2 } else { 1 };
        let mut row = 0;
        for action in actions {
            if ax + action.width > max_x {
                row += 1;
                if row >= max_rows {
                    break;
                }
                ax = x + 12.0;
                ay += 25.0;
            }
            self.pill_at(ax, ay, action.width, 21.0, action.label, action.color);
            self.hit_zones
                .push(HitZone::new(ax, ay, action.width, 21.0, action.hit));
            ax += action.width + 8.0;
        }
    }

    fn draw_execution_deck(&mut self, w: f32, h: f32) {
        if w < 940.0 || h < 660.0 || self.selected_id.is_some() {
            return;
        }
        let attention_clear = if !self.attention_items().is_empty() && w >= 900.0 && h >= 520.0 {
            270.0
        } else {
            14.0
        };
        let x = 246.0;
        let y = 52.0;
        let deck_w = (w - x - attention_clear).min(760.0);
        if deck_w < 380.0 {
            return;
        }
        let deck_h = 146.0;
        self.round_rect(
            x,
            y,
            deck_w,
            deck_h,
            6.0,
            "rgba(17,17,27,0.78)",
            "rgba(148,226,213,0.58)",
        );

        let controls = self.snapshot.controls.clone();
        let target_label = nonempty(
            &controls.session_label,
            &nonempty(&controls.session_selection, "no target"),
        );
        let session_state = if controls.session_detached {
            "detached"
        } else if controls.session_active {
            "active"
        } else if controls.session_id.is_empty() {
            "none"
        } else {
            "idle"
        };
        let prompt_mode = if controls.prompt_mode == "steer" {
            "steer"
        } else {
            "send"
        };
        let execution_mode = if controls.direct_mode {
            "direct"
        } else {
            "presence"
        };

        self.text(
            "EXECUTION DECK",
            x + 12.0,
            y + 20.0,
            10.0,
            C_TEAL_CSS,
            "bold",
        );
        self.status_chip(
            x + deck_w - 162.0,
            y + 8.0,
            148.0,
            &format!("{prompt_mode} / {execution_mode}"),
            C_TEAL_CSS,
        );
        self.text(
            &truncate(&target_label, 74),
            x + 12.0,
            y + 40.0,
            11.0,
            C_TEXT_CSS,
            "normal",
        );
        self.text(
            &truncate(
                &format!(
                    "{} · {} · {}",
                    nonempty(&controls.session_source, "agent"),
                    session_state,
                    nonempty(&controls.session_live_phase, "ready")
                ),
                82,
            ),
            x + 12.0,
            y + 56.0,
            9.0,
            C_SUBTEXT0_CSS,
            "normal",
        );

        let gap = 8.0;
        let metric_w = ((deck_w - 24.0 - gap * 3.0) / 4.0).max(78.0);
        let metric_y = y + 68.0;
        let context_pct = percent(
            self.snapshot.context.tokens,
            self.snapshot.context.effective_window,
        );
        let managed_pct = percent(
            self.snapshot.managed.used_tokens,
            self.snapshot.managed.effective_window,
        );
        let external_turn =
            controls.external_turn_state != "internal" && !controls.external_turn_state.is_empty();
        let external_color = external_turn_color_css(&controls.external_turn_state);
        let metrics = [
            (
                "Draft",
                format!("{} chars", controls.draft_chars),
                format!(
                    "{} / {}",
                    prompt_mode,
                    if controls.pending_attachments > 0 {
                        "attachments"
                    } else {
                        "clean"
                    }
                ),
                C_BLUE_CSS,
            ),
            (
                if external_turn { "External" } else { "Session" },
                if external_turn {
                    nonempty(&controls.external_turn_state, session_state)
                } else {
                    session_state.to_string()
                },
                if external_turn {
                    truncate(
                        &format!(
                            "{} / {}",
                            nonempty(&controls.external_turn_label, "external"),
                            nonempty(&controls.external_turn_detail, "controller")
                        ),
                        42,
                    )
                } else {
                    nonempty(&controls.session_goal_status, &controls.session_live_phase)
                },
                if external_turn {
                    external_color
                } else if controls.session_active {
                    C_TEAL_CSS
                } else {
                    C_OVERLAY1_CSS
                },
            ),
            (
                "Context",
                pct_label(context_pct),
                format!("{} items", self.snapshot.context.item_count),
                pressure_color(context_pct),
            ),
            (
                "Managed",
                nonempty(&self.snapshot.managed.status, "unknown"),
                format!("{} anchors", self.snapshot.managed.anchors),
                pressure_color(managed_pct),
            ),
        ];
        for (idx, (title, value, detail, color)) in metrics.iter().enumerate() {
            self.execution_metric_card(
                x + 12.0 + idx as f32 * (metric_w + gap),
                metric_y,
                metric_w,
                *title,
                value,
                detail,
                color,
            );
        }

        let mut actions = vec![
            LaneAction::activity(
                if controls.prompt_mode == "steer" {
                    "steer"
                } else {
                    "send"
                },
                "send",
                58.0,
                C_BLUE_CSS,
            ),
            LaneAction::activity("new session", "new-session", 92.0, C_TEAL_CSS),
        ];
        if controls.session_can_attach && !controls.session_id.is_empty() {
            actions.push(LaneAction::session(
                "attach",
                "attach",
                &controls.session_id,
                64.0,
                C_PEACH_CSS,
            ));
        }
        if controls.session_can_focus {
            actions.push(LaneAction::activity("focus", "target", 58.0, C_PEACH_CSS));
        }
        if controls.session_can_interrupt {
            actions.push(LaneAction::activity("stop", "stop", 50.0, C_RED_CSS));
        }
        if controls.shared_view_can_take_input {
            actions.push(LaneAction::controls(
                "take input",
                "shared-view-take-input",
                84.0,
                C_GREEN_CSS,
            ));
        }
        actions.push(LaneAction::select(
            "sessions",
            "system:sessions",
            76.0,
            C_TEAL_CSS,
        ));
        actions.push(LaneAction::select(
            "controls",
            "system:controls",
            74.0,
            C_MAUVE_CSS,
        ));

        let mut ax = x + 12.0;
        let ay = y + deck_h - 29.0;
        let max_x = x + deck_w - 12.0;
        for action in actions {
            if ax + action.width > max_x {
                break;
            }
            self.pill_at(ax, ay, action.width, 21.0, action.label, action.color);
            self.hit_zones
                .push(HitZone::new(ax, ay, action.width, 21.0, action.hit));
            ax += action.width + 8.0;
        }
    }

    fn execution_metric_card(
        &self,
        x: f32,
        y: f32,
        w: f32,
        title: &str,
        value: &str,
        detail: &str,
        color: &str,
    ) {
        self.round_rect(
            x,
            y,
            w,
            38.0,
            4.0,
            "rgba(24,24,37,0.72)",
            "rgba(49,50,68,0.76)",
        );
        self.ctx.set_fill_style(&JsValue::from_str(color));
        self.ctx.fill_rect(x as f64, y as f64, 3.0, 38.0);
        let text_chars = ((w - 16.0) / 6.0).floor().max(8.0) as usize;
        self.text(title, x + 10.0, y + 12.0, 8.5, C_OVERLAY1_CSS, "bold");
        self.text(
            &truncate(value, text_chars),
            x + 10.0,
            y + 25.0,
            10.0,
            color,
            "bold",
        );
        self.text(
            &truncate(detail, text_chars),
            x + 10.0,
            y + 35.0,
            8.0,
            C_SUBTEXT0_CSS,
            "normal",
        );
    }

    fn draw_signal_runway(&mut self, w: f32, h: f32) {
        if w < 1040.0 || h < 700.0 || self.selected_id.is_some() {
            return;
        }
        let attention_clear = if !self.attention_items().is_empty() && w >= 900.0 && h >= 520.0 {
            270.0
        } else {
            14.0
        };
        let x = 246.0;
        let y = 206.0;
        let runway_w = (w - x - attention_clear).min(760.0);
        if runway_w < 420.0 {
            return;
        }
        let runway_h = 154.0;
        self.round_rect(
            x,
            y,
            runway_w,
            runway_h,
            6.0,
            "rgba(17,17,27,0.72)",
            "rgba(137,180,250,0.48)",
        );
        self.text(
            "SIGNAL RUNWAY",
            x + 12.0,
            y + 20.0,
            10.0,
            C_OVERLAY1_CSS,
            "bold",
        );
        self.text(
            "activity / context / managed",
            x + 116.0,
            y + 20.0,
            9.0,
            C_SUBTEXT0_CSS,
            "normal",
        );

        let latest_event = self.snapshot.events.last();
        let activity_value = latest_event
            .map(|ev| format!("{} {}", ev.level, nonempty(&ev.ts, "--")))
            .unwrap_or_else(|| format!("{} events", self.snapshot.events.len()));
        let activity_detail = latest_event
            .map(|ev| truncate(&ev.msg, 68))
            .unwrap_or_else(|| "Waiting for retained activity".to_string());
        self.signal_runway_row(
            x + 10.0,
            y + 34.0,
            runway_w - 20.0,
            "Activity",
            &activity_value,
            &activity_detail,
            latest_event
                .map(|ev| level_color_css(&ev.level))
                .unwrap_or(C_TEAL_CSS),
            vec![
                RunwayAction::activity("latest", "bottom", 62.0, C_TEAL_CSS),
                RunwayAction::activity("copy", "copy-visible", 50.0, C_BLUE_CSS),
                RunwayAction::select("panel", "system:activity", 54.0, C_OVERLAY1_CSS),
            ],
        );

        let ctx_pct = percent(
            self.snapshot.context.tokens,
            self.snapshot.context.effective_window,
        );
        let context_value = if self.snapshot.context.available {
            format!(
                "{} / {} items",
                pct_label(ctx_pct),
                self.snapshot.context.item_count
            )
        } else {
            "waiting".to_string()
        };
        let context_detail = if self.snapshot.context.available {
            truncate(
                &format!(
                    "{} {}",
                    nonempty(&self.snapshot.context.source, "snapshot"),
                    nonempty(&self.snapshot.context.turn, "--")
                ),
                68,
            )
        } else {
            "No context snapshot available".to_string()
        };
        self.signal_runway_row(
            x + 10.0,
            y + 74.0,
            runway_w - 20.0,
            "Context",
            &context_value,
            &context_detail,
            pressure_color(ctx_pct),
            vec![
                RunwayAction::context("live", "live", 46.0, C_BLUE_CSS),
                RunwayAction::context("replay", "replay", 58.0, C_MAUVE_CSS),
                RunwayAction::context("copy", "copy-snapshot", 50.0, C_TEAL_CSS),
                RunwayAction::select("panel", "system:context", 54.0, C_OVERLAY1_CSS),
            ],
        );

        let managed_pct = percent(
            self.snapshot.managed.used_tokens,
            self.snapshot.managed.effective_window,
        );
        let managed_value = format!(
            "{} / {}",
            nonempty(&self.snapshot.managed.mode, "managed"),
            nonempty(&self.snapshot.managed.status, "unknown")
        );
        let managed_detail = format!(
            "{} records / {} anchors / {} branches",
            self.snapshot.managed.records,
            self.snapshot.managed.anchors,
            self.snapshot.managed.branches
        );
        self.signal_runway_row(
            x + 10.0,
            y + 114.0,
            runway_w - 20.0,
            "Managed",
            &managed_value,
            &managed_detail,
            pressure_color(managed_pct),
            vec![
                RunwayAction::managed(
                    "target",
                    "use-target",
                    "",
                    &self.snapshot.managed.session_id,
                    58.0,
                    C_TEAL_CSS,
                ),
                RunwayAction::managed(
                    "rewind",
                    "rewind",
                    "",
                    &self.snapshot.managed.session_id,
                    64.0,
                    C_MAUVE_CSS,
                ),
                RunwayAction::managed(
                    "copy",
                    "copy-status",
                    "",
                    &self.snapshot.managed.session_id,
                    50.0,
                    C_BLUE_CSS,
                ),
                RunwayAction::select("panel", "system:managed", 54.0, C_OVERLAY1_CSS),
            ],
        );
    }

    fn signal_runway_row(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        title: &str,
        value: &str,
        detail: &str,
        color: &str,
        actions: Vec<RunwayAction>,
    ) {
        self.round_rect(
            x,
            y,
            w,
            34.0,
            4.0,
            "rgba(24,24,37,0.70)",
            "rgba(49,50,68,0.72)",
        );
        self.ctx.set_fill_style(&JsValue::from_str(color));
        self.ctx
            .fill_rect((x + 7.0) as f64, (y + 7.0) as f64, 3.0, 20.0);
        self.text(title, x + 16.0, y + 14.0, 8.5, C_OVERLAY1_CSS, "bold");
        self.text(&truncate(value, 28), x + 88.0, y + 14.0, 9.0, color, "bold");
        self.text(
            &truncate(detail, 64),
            x + 16.0,
            y + 28.0,
            8.5,
            C_SUBTEXT0_CSS,
            "normal",
        );

        let mut ax = x + w - 8.0;
        for action in actions.into_iter().rev() {
            ax -= action.width;
            if ax < x + 245.0 {
                break;
            }
            self.pill_at(ax, y + 7.0, action.width, 20.0, action.label, action.color);
            self.hit_zones
                .push(HitZone::new(ax, y + 7.0, action.width, 20.0, action.hit));
            ax -= 6.0;
        }
    }

    fn draw_operations_runway(&mut self, w: f32, h: f32) {
        if w < 1040.0 || h < 720.0 || self.selected_id.is_some() {
            return;
        }
        let attention_clear = if !self.attention_items().is_empty() && w >= 900.0 && h >= 520.0 {
            270.0
        } else {
            14.0
        };
        let x = 246.0;
        let y = 368.0;
        let runway_w = (w - x - attention_clear).min(760.0);
        if runway_w < 420.0 {
            return;
        }
        let runway_h = 154.0;
        self.round_rect(
            x,
            y,
            runway_w,
            runway_h,
            6.0,
            "rgba(17,17,27,0.72)",
            "rgba(148,226,213,0.48)",
        );
        self.text(
            "OPERATIONS RUNWAY",
            x + 12.0,
            y + 20.0,
            10.0,
            C_OVERLAY1_CSS,
            "bold",
        );
        self.text(
            "sessions / displays / changes",
            x + 146.0,
            y + 20.0,
            9.0,
            C_SUBTEXT0_CSS,
            "normal",
        );

        let controls = self.snapshot.controls.clone();
        let session_state = if controls.session_detached {
            "detached"
        } else if controls.session_active {
            "active"
        } else if controls.session_id.is_empty() {
            "idle"
        } else {
            "selected"
        };
        let session_value = format!(
            "{} total / {} active / {} external",
            self.snapshot.sessions.total,
            self.snapshot.sessions.active,
            self.snapshot.sessions.external
        );
        let session_detail = truncate(
            &format!(
                "{} / {} / {}",
                nonempty(&controls.session_selection, "no target"),
                session_state,
                nonempty(&self.snapshot.sessions.latest_source, "sessions")
            ),
            68,
        );
        let mut session_actions = vec![
            RunwayAction::activity("new", "new-session", 44.0, C_TEAL_CSS),
            RunwayAction::select("panel", "system:sessions", 54.0, C_OVERLAY1_CSS),
        ];
        if controls.session_can_attach && !controls.session_id.is_empty() {
            session_actions.insert(
                1,
                RunwayAction::session("attach", "attach", &controls.session_id, 58.0, C_PEACH_CSS),
            );
        }
        if controls.session_can_focus {
            session_actions.insert(
                1,
                RunwayAction::activity("focus", "target", 54.0, C_BLUE_CSS),
            );
        }
        if controls.session_can_interrupt {
            session_actions.insert(1, RunwayAction::activity("stop", "stop", 46.0, C_RED_CSS));
        }
        self.signal_runway_row(
            x + 10.0,
            y + 34.0,
            runway_w - 20.0,
            "Sessions",
            &session_value,
            &session_detail,
            if self.snapshot.sessions.active > 0 {
                C_TEAL_CSS
            } else {
                C_BLUE_CSS
            },
            session_actions,
        );

        let peer_count = self.snapshot.hosts.len().saturating_sub(1);
        let display_count = self.display_sources.len();
        let display_value = format!("{peer_count} peers / {display_count} displays");
        let display_source = self.display_sources.values().next().map(|source| {
            (
                source.host_id.clone(),
                source.display_id.clone(),
                truncate(&source.label, 60),
            )
        });
        let display_detail = display_source
            .as_ref()
            .map(|(_, _, label)| label.clone())
            .unwrap_or_else(|| {
                format!(
                    "{} / {} / {}",
                    nonempty(&controls.display_access, "display"),
                    if controls.shared_view_visible {
                        "shared view"
                    } else {
                        "no shared view"
                    },
                    nonempty(&controls.cu_backend, "cu")
                )
            });
        let mut display_actions = vec![
            RunwayAction::controls("share", "display-toggle", 54.0, C_BLUE_CSS),
            RunwayAction::select("panel", "system:peers", 54.0, C_OVERLAY1_CSS),
        ];
        if let Some((host_id, display_id, _)) = display_source {
            display_actions.insert(
                0,
                RunwayAction::open_display("open", &host_id, &display_id, 48.0, C_PEACH_CSS),
            );
        }
        if controls.shared_view_can_take_input {
            display_actions.insert(
                0,
                RunwayAction::controls("input", "shared-view-take-input", 52.0, C_GREEN_CSS),
            );
        }
        self.signal_runway_row(
            x + 10.0,
            y + 74.0,
            runway_w - 20.0,
            "Displays",
            &display_value,
            &display_detail,
            if display_count > 0 {
                C_PEACH_CSS
            } else {
                C_BLUE_CSS
            },
            display_actions,
        );

        let changes = self.snapshot.changes.clone();
        let changes_value = if changes.count > 0 {
            format!(
                "{} files / +{} -{}",
                changes.count,
                compact_number(changes.total_added as f64),
                compact_number(changes.total_removed as f64)
            )
        } else {
            nonempty(&changes.status, "clean")
        };
        let changes_detail = if changes.latest_path.is_empty() {
            "No tracked working tree changes".to_string()
        } else {
            truncate(
                &format!("{} {}", changes.latest_kind, changes.latest_path),
                68,
            )
        };
        self.signal_runway_row(
            x + 10.0,
            y + 114.0,
            runway_w - 20.0,
            "Changes",
            &changes_value,
            &changes_detail,
            if changes.count > 0 || changes.status == "mismatch" {
                C_YELLOW_CSS
            } else {
                C_GREEN_CSS
            },
            vec![
                RunwayAction::changes("refresh", "refresh", "", 66.0, C_BLUE_CSS),
                RunwayAction::changes("copy", "copy-paths", "", 50.0, C_TEAL_CSS),
                RunwayAction::select("panel", "system:changes", 54.0, C_OVERLAY1_CSS),
            ],
        );
    }

    fn draw_display_switchboard(&mut self, w: f32, h: f32) {
        if w < 1180.0
            || h < 700.0
            || self.selected_id.is_some()
            || !self.attention_items().is_empty()
        {
            return;
        }
        let panel_w = 264.0;
        let panel_h = 220.0;
        let x = w - panel_w - 14.0;
        let y = 206.0;
        self.round_rect(
            x,
            y,
            panel_w,
            panel_h,
            6.0,
            "rgba(17,17,27,0.76)",
            "rgba(250,179,135,0.52)",
        );
        self.text(
            "DISPLAY SWITCHBOARD",
            x + 12.0,
            y + 20.0,
            10.0,
            C_PEACH_CSS,
            "bold",
        );

        let controls = self.snapshot.controls.clone();
        let tiles = self.display_tiles();
        let tile_y = y + 34.0;
        if tiles.is_empty() {
            self.round_rect(
                x + 10.0,
                tile_y,
                panel_w - 20.0,
                82.0,
                4.0,
                "rgba(24,24,37,0.70)",
                "rgba(49,50,68,0.74)",
            );
            self.text(
                "No live streams",
                x + 22.0,
                tile_y + 24.0,
                11.0,
                C_TEXT_CSS,
                "bold",
            );
            self.text(
                &truncate(
                    &format!(
                        "{} / {}",
                        nonempty(&controls.display_access, "display access"),
                        nonempty(&controls.cu_backend, "computer use")
                    ),
                    34,
                ),
                x + 22.0,
                tile_y + 43.0,
                9.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
        } else {
            for (idx, tile) in tiles.iter().take(2).enumerate() {
                let yy = tile_y + idx as f32 * 74.0;
                self.display_switchboard_tile(x + 10.0, yy, panel_w - 20.0, tile);
            }
        }

        let shared_label = if controls.shared_view_visible {
            truncate(&nonempty(&controls.shared_view_target, "shared view"), 28)
        } else {
            "shared view off".to_string()
        };
        self.text(
            "shared",
            x + 12.0,
            y + panel_h - 49.0,
            9.0,
            C_OVERLAY1_CSS,
            "bold",
        );
        self.text(
            &shared_label,
            x + 68.0,
            y + panel_h - 49.0,
            9.0,
            if controls.shared_view_visible {
                C_GREEN_CSS
            } else {
                C_SUBTEXT0_CSS
            },
            "normal",
        );

        let mut actions = vec![
            RunwayAction::controls("share", "display-toggle", 54.0, C_PEACH_CSS),
            RunwayAction::select("peers", "system:peers", 52.0, C_OVERLAY1_CSS),
        ];
        if controls.shared_view_can_take_input {
            actions.insert(
                0,
                RunwayAction::controls("input", "shared-view-take-input", 52.0, C_GREEN_CSS),
            );
        }
        if controls.shared_view_visible {
            actions.push(RunwayAction::controls(
                "hide",
                "shared-view-hide",
                44.0,
                C_YELLOW_CSS,
            ));
        }
        let mut ax = x + 12.0;
        let ay = y + panel_h - 31.0;
        for action in actions {
            if ax + action.width > x + panel_w - 12.0 {
                break;
            }
            self.pill_at(ax, ay, action.width, 20.0, action.label, action.color);
            self.hit_zones
                .push(HitZone::new(ax, ay, action.width, 20.0, action.hit));
            ax += action.width + 7.0;
        }
    }

    fn display_tiles(&self) -> Vec<DisplayTile> {
        self.display_sources
            .values()
            .map(|source| DisplayTile {
                host_id: source.host_id.clone(),
                display_id: source.display_id.clone(),
                label: source.label.clone(),
                kind: source.kind.clone(),
                ready: source.video.video_width() > 0 && source.video.video_height() > 0,
                video: source.video.clone(),
            })
            .collect()
    }

    fn display_switchboard_tile(&mut self, x: f32, y: f32, w: f32, tile: &DisplayTile) {
        self.round_rect(
            x,
            y,
            w,
            64.0,
            4.0,
            "rgba(24,24,37,0.72)",
            "rgba(49,50,68,0.76)",
        );
        let preview_w = 86.0;
        let preview_h = 48.0;
        let px = x + 8.0;
        let py = y + 8.0;
        self.round_rect(
            px,
            py,
            preview_w,
            preview_h,
            3.0,
            "rgba(17,17,27,0.86)",
            if tile.ready {
                C_GREEN_CSS
            } else {
                C_YELLOW_CSS
            },
        );
        if tile.ready {
            let _ = self.ctx.draw_image_with_html_video_element_and_dw_and_dh(
                &tile.video,
                (px + 2.0) as f64,
                (py + 2.0) as f64,
                (preview_w - 4.0) as f64,
                (preview_h - 4.0) as f64,
            );
        } else {
            self.text(
                "linking",
                px + 14.0,
                py + 28.0,
                9.0,
                C_OVERLAY1_CSS,
                "normal",
            );
        }
        self.text(
            &truncate(&tile.label, 26),
            x + 104.0,
            y + 19.0,
            9.0,
            C_TEXT_CSS,
            "bold",
        );
        self.text(
            &format!(
                "{} :{} / {}",
                truncate(&self.host_name(&tile.host_id), 13),
                tile.display_id,
                nonempty(&tile.kind, "stream")
            ),
            x + 104.0,
            y + 35.0,
            8.5,
            C_SUBTEXT0_CSS,
            "normal",
        );
        self.pill_at(x + 104.0, y + 42.0, 48.0, 18.0, "open", C_PEACH_CSS);
        self.hit_zones.push(HitZone::new(
            x,
            y,
            w,
            64.0,
            HitAction::OpenDisplay {
                host_id: tile.host_id.clone(),
                display_id: tile.display_id.clone(),
            },
        ));
    }

    fn draw_activity_detail_rail(&mut self, w: f32, h: f32) {
        if w < 1180.0
            || h < 720.0
            || self.selected_id.is_some()
            || !self.attention_items().is_empty()
        {
            return;
        }
        let panel_w = 264.0;
        let x = w - panel_w - 14.0;
        let y = 434.0;
        let panel_h = (h - y - 102.0).clamp(132.0, 190.0);
        self.round_rect(
            x,
            y,
            panel_w,
            panel_h,
            6.0,
            "rgba(17,17,27,0.76)",
            "rgba(148,226,213,0.50)",
        );
        self.text(
            "ACTIVITY DETAIL",
            x + 12.0,
            y + 20.0,
            10.0,
            C_TEAL_CSS,
            "bold",
        );
        self.text(
            &format!("{} retained", self.snapshot.events.len()),
            x + panel_w - 91.0,
            y + 20.0,
            9.0,
            C_SUBTEXT0_CSS,
            "normal",
        );

        let actions = [
            RunwayAction::activity("latest", "bottom", 58.0, C_TEAL_CSS),
            RunwayAction::activity("copy", "copy-visible", 48.0, C_BLUE_CSS),
            RunwayAction::select("panel", "system:activity", 52.0, C_OVERLAY1_CSS),
        ];
        let mut ax = x + 12.0;
        for action in actions {
            self.pill_at(ax, y + 31.0, action.width, 20.0, action.label, action.color);
            self.hit_zones
                .push(HitZone::new(ax, y + 31.0, action.width, 20.0, action.hit));
            ax += action.width + 7.0;
        }

        let events = self
            .snapshot
            .events
            .iter()
            .rev()
            .take(5)
            .cloned()
            .collect::<Vec<_>>();
        if events.is_empty() {
            self.round_rect(
                x + 10.0,
                y + 63.0,
                panel_w - 20.0,
                54.0,
                4.0,
                "rgba(24,24,37,0.70)",
                "rgba(49,50,68,0.72)",
            );
            self.text(
                "Waiting for dashboard events",
                x + 20.0,
                y + 91.0,
                10.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
            return;
        }

        let row_h = 24.0;
        let max_rows = ((panel_h - 63.0) / row_h).floor().max(1.0) as usize;
        for (idx, event) in events.into_iter().rev().take(max_rows).enumerate() {
            let row_y = y + 63.0 + idx as f32 * row_h;
            self.activity_detail_row(x + 10.0, row_y, panel_w - 20.0, event);
        }
    }

    fn activity_detail_row(&mut self, x: f32, y: f32, w: f32, event: StationEvent) {
        self.round_rect(
            x,
            y,
            w,
            20.0,
            4.0,
            "rgba(24,24,37,0.64)",
            "rgba(49,50,68,0.64)",
        );
        let color = level_color_css(&event.level);
        self.ctx.set_fill_style(&JsValue::from_str(color));
        self.ctx
            .fill_rect((x + 7.0) as f64, (y + 5.0) as f64, 3.0, 10.0);
        self.text(
            &truncate(&nonempty(&event.ts, "--"), 8),
            x + 16.0,
            y + 13.0,
            8.0,
            C_OVERLAY1_CSS,
            "normal",
        );
        self.text(
            &truncate(&event.level, 6),
            x + 62.0,
            y + 13.0,
            8.0,
            color,
            "bold",
        );
        let detail = if event.action.is_empty() {
            truncate(&event.msg, 34)
        } else {
            truncate(&format!("{} / {}", event.action, event.msg), 34)
        };
        self.text(
            &truncate(&nonempty(&event.host_id, "local"), 10),
            x + 100.0,
            y + 13.0,
            8.0,
            C_PEACH_CSS,
            "normal",
        );
        self.text(&detail, x + 154.0, y + 13.0, 8.0, C_SUBTEXT0_CSS, "normal");
        let hit = if event.action == "log" && !event.id.is_empty() {
            HitAction::ActivityAction {
                action: event.action,
                id: event.id,
            }
        } else {
            HitAction::Select("system:activity".to_string())
        };
        self.hit_zones.push(HitZone::new(x, y, w, 20.0, hit));
    }

    fn draw_continuity_detail_bar(&mut self, w: f32, h: f32) {
        if w < 1040.0
            || h < 520.0
            || self.selected_id.is_some()
            || !self.attention_items().is_empty()
        {
            return;
        }
        let x = 246.0;
        let right_rail_x = w - 264.0 - 14.0;
        let bar_w = (right_rail_x - x - 10.0).min(760.0);
        if bar_w < 560.0 {
            return;
        }
        let y = if h >= 700.0 { 530.0 } else { 206.0 };
        let command_top = (h - 78.0 - 14.0).max(52.0);
        let bar_h = (command_top - y - 8.0).min(124.0);
        if bar_h < 88.0 {
            return;
        }

        self.round_rect(
            x,
            y,
            bar_w,
            bar_h,
            6.0,
            "rgba(17,17,27,0.76)",
            "rgba(203,166,247,0.50)",
        );
        self.text(
            "CONTINUITY DETAIL",
            x + 12.0,
            y + 19.0,
            10.0,
            C_MAUVE_CSS,
            "bold",
        );
        self.text(
            "context / managed",
            x + 133.0,
            y + 19.0,
            9.0,
            C_SUBTEXT0_CSS,
            "normal",
        );

        let card_gap = 8.0;
        let card_y = y + 30.0;
        let card_h = bar_h - 40.0;
        let context_w = ((bar_w - 28.0 - card_gap) * 0.54).clamp(294.0, 406.0);
        let managed_w = bar_w - 28.0 - card_gap - context_w;
        let context = self.snapshot.context.clone();
        let managed = self.snapshot.managed.clone();
        self.continuity_context_card(x + 10.0, card_y, context_w, card_h, &context);
        self.continuity_managed_card(x + 18.0 + context_w, card_y, managed_w, card_h, &managed);
    }

    fn continuity_context_card(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        ctx: &StationContextSummary,
    ) {
        let pct = percent(ctx.tokens, ctx.effective_window);
        let color = if ctx.available {
            pressure_color(pct)
        } else {
            C_OVERLAY1_CSS
        };
        self.round_rect(
            x,
            y,
            w,
            h,
            4.0,
            "rgba(24,24,37,0.70)",
            "rgba(137,180,250,0.48)",
        );
        self.ctx.set_fill_style(&JsValue::from_str(color));
        self.ctx
            .fill_rect((x + 7.0) as f64, (y + 8.0) as f64, 3.0, (h - 16.0) as f64);
        self.text("Context", x + 16.0, y + 14.0, 8.5, C_BLUE_CSS, "bold");
        let value = if ctx.available {
            format!(
                "{} / {} items",
                pct_label(pct),
                compact_number(ctx.item_count as f64)
            )
        } else {
            "waiting".to_string()
        };
        self.text(
            &truncate(&value, 24),
            x + 72.0,
            y + 14.0,
            8.5,
            color,
            "bold",
        );

        let detail = ctx
            .top_items
            .first()
            .map(|row| {
                truncate(
                    &format!("{} {}", nonempty(&row.label, "item"), row.value),
                    42,
                )
            })
            .or_else(|| {
                ctx.top_categories.first().map(|row| {
                    truncate(
                        &format!(
                            "{} {} tokens",
                            nonempty(&row.label, "lane"),
                            compact_number(row.value as f64)
                        ),
                        42,
                    )
                })
            })
            .unwrap_or_else(|| {
                truncate(
                    &format!(
                        "{} / {}",
                        nonempty(&ctx.source, "snapshot"),
                        nonempty(&ctx.turn, "--")
                    ),
                    42,
                )
            });
        self.text(&detail, x + 16.0, y + 32.0, 8.5, C_SUBTEXT0_CSS, "normal");
        self.meter(x + 16.0, y + h - 4.0, w - 32.0, pct, color);

        let actions = [
            RunwayAction::context("live", "live", 42.0, C_BLUE_CSS),
            RunwayAction::context("copy", "copy-snapshot", 44.0, C_TEAL_CSS),
            RunwayAction::select("panel", "system:context", 48.0, C_OVERLAY1_CSS),
        ];
        let mut ax = x + w - 8.0;
        for action in actions.into_iter().rev() {
            ax -= action.width;
            if ax < x + 142.0 {
                break;
            }
            self.pill_at(ax, y + 5.0, action.width, 18.0, action.label, action.color);
            self.hit_zones
                .push(HitZone::new(ax, y + 5.0, action.width, 18.0, action.hit));
            ax -= 6.0;
        }
    }

    fn continuity_managed_card(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        managed: &StationManagedSummary,
    ) {
        let pct = percent(managed.used_tokens, managed.effective_window);
        let color = pressure_color(pct);
        self.round_rect(
            x,
            y,
            w,
            h,
            4.0,
            "rgba(24,24,37,0.70)",
            "rgba(203,166,247,0.48)",
        );
        self.ctx.set_fill_style(&JsValue::from_str(color));
        self.ctx
            .fill_rect((x + 7.0) as f64, (y + 8.0) as f64, 3.0, (h - 16.0) as f64);
        self.text("Managed", x + 16.0, y + 14.0, 8.5, C_MAUVE_CSS, "bold");
        self.text(
            &truncate(&nonempty(&managed.status, "unknown"), 18),
            x + 76.0,
            y + 14.0,
            8.5,
            color,
            "bold",
        );

        let detail = if !managed.error.is_empty() {
            truncate(&managed.error, 42)
        } else if managed.rewind_only {
            "rewind-only pressure".to_string()
        } else {
            truncate(
                &format!(
                    "{} records / {} anchors / {} branches",
                    managed.records, managed.anchors, managed.branches
                ),
                42,
            )
        };
        self.text(&detail, x + 16.0, y + 32.0, 8.5, C_SUBTEXT0_CSS, "normal");
        self.meter(x + 16.0, y + h - 4.0, w - 32.0, pct, color);

        let actions = [
            RunwayAction::managed(
                "target",
                "use-target",
                "",
                &managed.session_id,
                52.0,
                C_TEAL_CSS,
            ),
            RunwayAction::managed(
                "rewind",
                "rewind",
                "",
                &managed.session_id,
                56.0,
                C_MAUVE_CSS,
            ),
            RunwayAction::select("panel", "system:managed", 48.0, C_OVERLAY1_CSS),
        ];
        let mut ax = x + w - 8.0;
        for action in actions.into_iter().rev() {
            ax -= action.width;
            if ax < x + 142.0 {
                break;
            }
            self.pill_at(ax, y + 5.0, action.width, 18.0, action.label, action.color);
            self.hit_zones
                .push(HitZone::new(ax, y + 5.0, action.width, 18.0, action.hit));
            ax -= 6.0;
        }
    }

    fn status_chip(&self, x: f32, y: f32, w: f32, label: &str, color: &str) {
        self.round_rect(x, y, w, 22.0, 4.0, "rgba(30,30,46,0.58)", color);
        self.text(&truncate(label, 42), x + 8.0, y + 14.0, 9.0, color, "bold");
    }

    fn draw_attention_strip(&mut self, w: f32, h: f32) {
        if w < 900.0 || h < 520.0 {
            return;
        }
        let mut items = self.attention_items();
        if items.is_empty() {
            return;
        }
        items.truncate(6);

        let strip_w = 242.0;
        let item_h = 48.0;
        let gap = 6.0;
        let strip_h =
            33.0 + items.len() as f32 * item_h + (items.len().saturating_sub(1)) as f32 * gap;
        let x = w - strip_w - 14.0;
        let y = 52.0;
        self.round_rect(
            x,
            y,
            strip_w,
            strip_h,
            6.0,
            "rgba(17,17,27,0.80)",
            "rgba(249,226,175,0.56)",
        );
        self.text("ATTENTION", x + 12.0, y + 20.0, 10.0, C_YELLOW_CSS, "bold");
        let queue_label = if self.snapshot.attention_queue.count > 0 {
            format!(
                "{} queued / {} blocked",
                self.snapshot.attention_queue.count, self.snapshot.attention_queue.blocked
            )
        } else {
            "rendered queue".to_string()
        };
        self.text(
            &truncate(&queue_label, 24),
            x + strip_w - 93.0,
            y + 20.0,
            9.0,
            C_SUBTEXT0_CSS,
            "normal",
        );

        let mut yy = y + 33.0;
        for item in items {
            self.attention_item(x + 10.0, yy, strip_w - 20.0, item_h, item);
            yy += item_h + gap;
        }
    }

    fn attention_items(&self) -> Vec<AttentionItem> {
        const ATTENTION_VISIBLE_CAP: usize = 6;
        let mut primary_items = Vec::new();
        let mut critical_items = Vec::new();
        let rendered_keys = self.rendered_attention_keys();
        primary_items.extend(self.rendered_attention_items());
        let controls = &self.snapshot.controls;
        if let Some(agent) = self
            .snapshot
            .agents
            .iter()
            .find(|agent| agent.needs_approval)
        {
            push_synth_attention(
                &mut primary_items,
                &rendered_keys,
                "approval",
                AttentionItem {
                    title: "approval".to_string(),
                    detail: truncate(
                        &nonempty(
                            &agent.approval_command,
                            &nonempty(&agent.task, "agent waiting"),
                        ),
                        46,
                    ),
                    color: C_YELLOW_CSS,
                    hit: HitAction::Select(agent.id.clone()),
                },
            );
        }
        if controls.shared_view_can_take_input {
            push_synth_attention(
                &mut primary_items,
                &rendered_keys,
                "shared",
                AttentionItem {
                    title: "shared input".to_string(),
                    detail: truncate(&nonempty(&controls.shared_view_target, "shared view"), 46),
                    color: C_GREEN_CSS,
                    hit: HitAction::ControlsAction {
                        action: "shared-view-take-input".to_string(),
                    },
                },
            );
        } else if controls.shared_view_visible {
            push_synth_attention(
                &mut primary_items,
                &rendered_keys,
                "shared",
                AttentionItem {
                    title: "shared view".to_string(),
                    detail: truncate(&nonempty(&controls.shared_view_target, "visible"), 46),
                    color: C_PEACH_CSS,
                    hit: HitAction::Select("system:peers".to_string()),
                },
            );
        }
        if controls.session_can_interrupt {
            push_synth_attention(
                &mut primary_items,
                &rendered_keys,
                "active",
                AttentionItem {
                    title: "active run".to_string(),
                    detail: truncate(
                        &nonempty(&controls.session_selection, "foreground session"),
                        46,
                    ),
                    color: C_TEAL_CSS,
                    hit: HitAction::Select("system:controls".to_string()),
                },
            );
        } else if controls.session_active {
            push_synth_attention(
                &mut primary_items,
                &rendered_keys,
                "active",
                AttentionItem {
                    title: "session live".to_string(),
                    detail: truncate(
                        &nonempty(&controls.session_selection, "foreground session"),
                        46,
                    ),
                    color: C_BLUE_CSS,
                    hit: HitAction::Select("system:sessions".to_string()),
                },
            );
        }
        if controls.pending_attachments > 0 {
            push_synth_attention(
                &mut primary_items,
                &rendered_keys,
                "attachments",
                AttentionItem {
                    title: "attachments".to_string(),
                    detail: format!("{} pending", controls.pending_attachments),
                    color: C_MAUVE_CSS,
                    hit: HitAction::Select("system:controls".to_string()),
                },
            );
        }
        if self.snapshot.managed.rewind_only {
            critical_items.push(AttentionItem {
                title: "rewind only".to_string(),
                detail: format_token_ratio(
                    self.snapshot.managed.used_tokens,
                    self.snapshot.managed.effective_window,
                ),
                color: C_RED_CSS,
                hit: HitAction::Select("system:managed".to_string()),
            });
        } else if percent(
            self.snapshot.managed.used_tokens,
            self.snapshot.managed.effective_window,
        ) >= 0.78
        {
            critical_items.push(AttentionItem {
                title: "context pressure".to_string(),
                detail: format_token_ratio(
                    self.snapshot.managed.used_tokens,
                    self.snapshot.managed.effective_window,
                ),
                color: C_YELLOW_CSS,
                hit: HitAction::Select("system:managed".to_string()),
            });
        }
        if self.snapshot.changes.count > 0 {
            critical_items.push(AttentionItem {
                title: "working tree".to_string(),
                detail: format!(
                    "{} files +{} -{}",
                    self.snapshot.changes.count,
                    compact_number(self.snapshot.changes.total_added as f64),
                    compact_number(self.snapshot.changes.total_removed as f64)
                ),
                color: C_YELLOW_CSS,
                hit: HitAction::Select("system:changes".to_string()),
            });
        }
        let context_pct = percent(
            self.snapshot.context.tokens,
            self.snapshot.context.effective_window,
        );
        if self.snapshot.context.available && context_pct >= 0.78 {
            critical_items.push(AttentionItem {
                title: "context window".to_string(),
                detail: format!(
                    "{} / {}",
                    compact_number(self.snapshot.context.tokens as f64),
                    compact_number(self.snapshot.context.effective_window as f64)
                ),
                color: pressure_color(context_pct),
                hit: HitAction::Select("system:context".to_string()),
            });
        }
        merge_attention_with_reserved_critical(
            primary_items,
            critical_items,
            ATTENTION_VISIBLE_CAP,
        )
    }

    fn rendered_attention_items(&self) -> Vec<AttentionItem> {
        self.snapshot
            .attention_queue
            .items
            .iter()
            .take(6)
            .map(|item| {
                let mut detail = [item.meta.as_str(), item.detail.as_str()]
                    .into_iter()
                    .filter(|part| !part.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join(" / ");
                if item.can_cancel && !item.id.is_empty() {
                    detail = if detail.is_empty() {
                        "tap to cancel".to_string()
                    } else {
                        format!("{detail} / tap to cancel")
                    };
                }
                AttentionItem {
                    title: truncate(&nonempty(&item.title, &item.kind.replace('_', " ")), 28),
                    detail: truncate(&detail, 58),
                    color: attention_level_color_css(&item.level, &item.kind),
                    hit: self.rendered_attention_hit(item),
                }
            })
            .collect()
    }

    fn rendered_attention_keys(&self) -> HashSet<&'static str> {
        self.snapshot
            .attention_queue
            .items
            .iter()
            .filter_map(|item| match item.kind.as_str() {
                "approval" => Some("approval"),
                "shared_view_input" => Some("shared"),
                "attachments" => Some("attachments"),
                "active_turn" | "steer" | "follow_up" => Some("active"),
                _ => None,
            })
            .collect()
    }

    fn rendered_attention_hit(&self, item: &StationAttentionItem) -> HitAction {
        if item.can_cancel && !item.id.is_empty() {
            return HitAction::ControlsAction {
                action: format!("queue-cancel:{}", item.id),
            };
        }
        match item.kind.as_str() {
            "shared_view_input" => HitAction::ControlsAction {
                action: "shared-view-take-input".to_string(),
            },
            "attachments" => HitAction::ControlsAction {
                action: "attachments-clear".to_string(),
            },
            "active_turn" | "steer" | "follow_up" if !item.session_id.is_empty() => {
                HitAction::SessionAction {
                    action: "focus".to_string(),
                    session_id: item.session_id.clone(),
                }
            }
            _ => HitAction::Select("system:controls".to_string()),
        }
    }

    fn attention_item(&mut self, x: f32, y: f32, w: f32, h: f32, item: AttentionItem) {
        self.round_rect(
            x,
            y,
            w,
            h,
            5.0,
            "rgba(24,24,37,0.78)",
            "rgba(69,71,90,0.78)",
        );
        self.ctx.set_fill_style(&JsValue::from_str(item.color));
        self.ctx
            .fill_rect((x + 8.0) as f64, (y + 9.0) as f64, 3.0, (h - 18.0) as f64);
        self.text(&item.title, x + 18.0, y + 18.0, 9.0, item.color, "bold");
        self.text(
            &truncate(&item.detail, 38),
            x + 18.0,
            y + 36.0,
            9.0,
            C_SUBTEXT0_CSS,
            "normal",
        );
        self.hit_zones.push(HitZone::new(x, y, w, h, item.hit));
    }

    fn summary_card(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        title: &str,
        value: &str,
        detail: &str,
        color: &str,
        select_id: &str,
    ) {
        self.round_rect(
            x,
            y,
            w,
            h,
            4.0,
            "rgba(17,17,27,0.72)",
            "rgba(49,50,68,0.78)",
        );
        self.ctx.set_fill_style(&JsValue::from_str(color));
        self.ctx
            .fill_rect((x + 7.0) as f64, (y + 7.0) as f64, 3.0, (h - 14.0) as f64);
        self.text(title, x + 15.0, y + 14.0, 9.0, C_OVERLAY1_CSS, "bold");
        self.text(
            &truncate(value, 30),
            x + 15.0,
            y + 28.0,
            10.0,
            C_TEXT_CSS,
            "normal",
        );
        self.text(
            &truncate(detail, 38),
            x + 15.0,
            y + h - 8.0,
            8.5,
            C_SUBTEXT0_CSS,
            "normal",
        );
        self.hit_zones.push(HitZone::new(
            x,
            y,
            w,
            h,
            HitAction::Select(select_id.to_string()),
        ));
    }

    fn draw_corners(&self, w: f32, h: f32) {
        let c = "rgba(69,71,90,0.8)";
        self.ctx.set_stroke_style(&JsValue::from_str(c));
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

    fn draw_readout(&self, h: f32) {
        let tokens: f64 = self.snapshot.agents.iter().map(|a| a.tokens as f64).sum();
        let cost: f64 = self.snapshot.agents.iter().map(|a| a.cost).sum();
        let mut y = h - 58.0;
        for (k, v, color) in [
            (
                "cam",
                if self.auto_orbit {
                    "orbit · auto".to_string()
                } else {
                    "orbit".to_string()
                },
                C_SUBTEXT0_CSS,
            ),
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
        self.ctx
            .set_stroke_style(&JsValue::from_str("rgba(69,71,90,0.9)"));
        self.ctx.begin_path();
        let _ = self
            .ctx
            .arc(cx as f64, cy as f64, 18.0, 0.0, std::f64::consts::TAU);
        self.ctx.stroke();
        let angle = -self.yaw as f64;
        self.ctx.set_stroke_style(&JsValue::from_str(C_BLUE_CSS));
        self.ctx.begin_path();
        self.ctx.move_to(cx as f64, cy as f64);
        self.ctx.line_to(
            cx as f64 + angle.sin() * 14.0,
            cy as f64 - angle.cos() * 14.0,
        );
        self.ctx.stroke();
        self.text("N", cx + 27.0, cy + 4.0, 10.0, C_OVERLAY1_CSS, "bold");
    }

    fn draw_ticker(&self, w: f32, h: f32) {
        let events = self
            .snapshot
            .events
            .iter()
            .rev()
            .take(5)
            .collect::<Vec<_>>();
        let row_h = 16.0;
        let x = 250.0;
        let mut y = h - row_h * events.len() as f32 - 13.0;
        for ev in events.into_iter().rev() {
            self.ctx
                .set_fill_style(&JsValue::from_str("rgba(17,17,27,0.55)"));
            self.ctx.fill_rect(
                x as f64,
                (y - 11.0) as f64,
                (w - x - 245.0).max(280.0) as f64,
                15.0,
            );
            self.text(&ev.ts, x + 6.0, y, 10.0, C_OVERLAY1_CSS, "normal");
            self.text(
                &ev.level,
                x + 72.0,
                y,
                10.0,
                level_color_css(&ev.level),
                "bold",
            );
            self.text(
                &truncate(&ev.msg, 96),
                x + 130.0,
                y,
                10.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
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
        let alpha =
            (1.0 - ((time_ms - self.boot_started_ms - 700.0) / 450.0).clamp(0.0, 1.0)) as f32;
        if alpha <= 0.0 {
            return;
        }
        self.ctx.set_global_alpha(alpha as f64);
        self.round_rect(
            w * 0.5 - 155.0,
            h * 0.5 - 34.0,
            310.0,
            68.0,
            6.0,
            "rgba(24,24,37,0.92)",
            "rgba(69,71,90,0.88)",
        );
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let idx = ((time_ms / 90.0) as usize) % frames.len();
        self.text(
            frames[idx],
            w * 0.5 - 95.0,
            h * 0.5 + 5.0,
            21.0,
            C_BLUE_CSS,
            "normal",
        );
        self.text(
            "Initializing station · linking hosts",
            w * 0.5 - 58.0,
            h * 0.5 + 2.0,
            12.0,
            C_SUBTEXT0_CSS,
            "normal",
        );
        self.ctx.set_global_alpha(1.0);
    }

    fn draw_info_panel(&mut self, id: &str, w: f32, h: f32, time_ms: f64) {
        let panel_w = 350.0_f32.min(w - 28.0).max(280.0);
        let x = w - panel_w - 14.0;
        let y = 52.0;
        let panel_h = (h - 76.0).min(560.0);
        self.round_rect(
            x,
            y,
            panel_w,
            panel_h,
            6.0,
            "rgba(24,24,37,0.94)",
            "rgba(69,71,90,0.92)",
        );
        self.hit_zones
            .push(HitZone::new(x, y, panel_w, panel_h, HitAction::Noop));
        self.hit_zones.push(HitZone::new(
            x + panel_w - 31.0,
            y + 8.0,
            22.0,
            22.0,
            HitAction::ClosePanel,
        ));
        self.text(
            "×",
            x + panel_w - 25.0,
            y + 24.0,
            18.0,
            C_OVERLAY1_CSS,
            "normal",
        );
        self.draw_selected_action_menu(id, x, y, panel_h);

        if id == "op" {
            self.text("operator", x + 12.0, y + 25.0, 10.0, C_BLUE_CSS, "bold");
            self.text("you", x + 86.0, y + 25.0, 13.0, C_TEXT_CSS, "bold");
            self.panel_row(x, y + 54.0, "mode", "station origin");
            self.panel_row(x, y + 76.0, "camera", "orbit / parallax");
            return;
        }

        if id == "system:activity" {
            self.draw_activity_info(x, y, panel_w);
            return;
        }
        if let Some(event_id) = id.strip_prefix("activity:") {
            self.draw_activity_event_info(event_id, x, y, panel_w);
            return;
        }
        if id == "system:context" {
            self.draw_context_info(x, y, panel_w);
            return;
        }
        if id == "system:managed" {
            self.draw_managed_info(x, y, panel_w);
            return;
        }
        if id == "system:changes" {
            self.draw_changes_info(x, y, panel_w);
            return;
        }
        if id == "system:sessions" {
            self.draw_sessions_info(x, y, panel_w);
            return;
        }
        if id == "system:worktrees" {
            self.draw_worktrees_info(x, y, panel_w);
            return;
        }
        if id == "system:peers" {
            self.draw_peers_info(x, y, panel_w);
            return;
        }
        if id == "system:controls" {
            self.draw_controls_info(x, y, panel_w);
            return;
        }
        if id == "system:view" {
            self.draw_view_info(x, y, panel_w);
            return;
        }

        if let Some(host) = self
            .snapshot
            .hosts
            .iter()
            .find(|h| format!("host:{}", h.id) == id)
            .cloned()
        {
            self.text("host", x + 12.0, y + 25.0, 10.0, C_PEACH_CSS, "bold");
            self.text(&host.name, x + 72.0, y + 25.0, 13.0, C_TEXT_CSS, "bold");
            let mut yy = y + 54.0 - self.panel_scroll;
            self.panel_row(x, yy, "platform", &host.platform);
            yy += 22.0;
            self.panel_row(x, yy, "region", &host.region);
            yy += 22.0;
            self.panel_row_color(
                x,
                yy,
                "cpu",
                &format!("{:.0}%", host.cpu),
                if host.cpu > 70.0 {
                    C_YELLOW_CSS
                } else {
                    C_TEXT_CSS
                },
            );
            yy += 22.0;
            self.panel_row_color(
                x,
                yy,
                "mem",
                &format!("{:.0}%", host.mem),
                if host.mem > 70.0 {
                    C_YELLOW_CSS
                } else {
                    C_TEXT_CSS
                },
            );
            yy += 30.0;
            self.section_title(x, yy, "Display · WebRTC");
            yy += 18.0;
            self.round_rect(
                x + 12.0,
                yy,
                panel_w - 24.0,
                120.0,
                4.0,
                "rgba(17,17,27,0.86)",
                "rgba(49,50,68,0.86)",
            );
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
                self.text(
                    &format!("{} · live", source.display_id),
                    x + 20.0,
                    yy + 112.0,
                    10.0,
                    C_GREEN_CSS,
                    "normal",
                );
            } else {
                self.text(
                    "no active stream",
                    x + 25.0,
                    yy + 62.0,
                    12.0,
                    C_OVERLAY1_CSS,
                    "normal",
                );
            }
            self.hit_zones.push(HitZone::new(
                x + 188.0,
                yy + 92.0,
                122.0,
                22.0,
                HitAction::OpenDisplay {
                    host_id: host.id.clone(),
                    display_id: source
                        .map(|source| source.display_id.clone())
                        .unwrap_or_else(|| "0".to_string()),
                },
            ));
            self.pill_at(
                x + 188.0,
                yy + 92.0,
                122.0,
                22.0,
                "open display",
                C_BLUE_CSS,
            );
            yy += 148.0;
            self.section_title(
                x,
                yy,
                &format!(
                    "Agents on host ({})",
                    self.snapshot
                        .agents
                        .iter()
                        .filter(|a| a.host_id == host.id)
                        .count()
                ),
            );
            yy += 20.0;
            for agent in self
                .snapshot
                .agents
                .iter()
                .filter(|a| a.host_id == host.id)
                .take(8)
            {
                self.text(
                    &agent.role,
                    x + 16.0,
                    yy,
                    9.0,
                    role_color_css(&agent.role),
                    "bold",
                );
                self.text(&agent.id, x + 88.0, yy, 10.0, C_OVERLAY1_CSS, "normal");
                self.text(
                    &truncate(&agent.task, 32),
                    x + 150.0,
                    yy,
                    10.0,
                    C_SUBTEXT0_CSS,
                    "normal",
                );
                self.hit_zones.push(HitZone::new(
                    x + 12.0,
                    yy - 13.0,
                    panel_w - 24.0,
                    18.0,
                    HitAction::Select(agent.id.clone()),
                ));
                yy += 20.0;
            }
            return;
        }

        if let Some(agent) = self.snapshot.agents.iter().find(|a| a.id == id).cloned() {
            self.text(
                &agent.role,
                x + 12.0,
                y + 25.0,
                10.0,
                role_color_css(&agent.role),
                "bold",
            );
            self.text(&agent.id, x + 102.0, y + 25.0, 13.0, C_TEXT_CSS, "bold");
            let spin =
                ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][(time_ms as usize / 100) % 10];
            let mut yy = y + 54.0 - self.panel_scroll;
            self.panel_row(x, yy, "task", &agent.task);
            yy += 34.0;
            self.panel_row_color(x, yy, "host", &self.host_name(&agent.host_id), C_BLUE_CSS);
            yy += 22.0;
            self.panel_row_color(x, yy, "provider", &agent.provider, C_BLUE_CSS);
            yy += 22.0;
            self.panel_row_color(x, yy, "model", &agent.model, C_GREEN_CSS);
            yy += 22.0;
            self.panel_row_color(
                x,
                yy,
                "phase",
                &format!("{} {spin}", agent.phase),
                phase_color_css(&agent.phase),
            );
            yy += 22.0;
            self.panel_row(x, yy, "status", &agent.status.replace('_', " "));
            yy += 22.0;
            self.panel_row(
                x,
                yy,
                "turns",
                &format!("{}/{}", agent.turns, agent.turn_cap),
            );
            yy += 22.0;
            self.panel_row(x, yy, "autonomy", &agent.autonomy);
            yy += 28.0;
            self.section_title(x, yy, "Token budget");
            yy += 18.0;
            let pct = if agent.token_cap > 0.0 {
                (agent.tokens / agent.token_cap).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let budget = if pct < 0.5 {
                C_GREEN_CSS
            } else if pct < 0.85 {
                C_YELLOW_CSS
            } else {
                C_RED_CSS
            };
            self.ctx
                .set_fill_style(&JsValue::from_str("rgba(49,50,68,0.85)"));
            self.ctx
                .fill_rect((x + 12.0) as f64, yy as f64, (panel_w - 24.0) as f64, 7.0);
            self.ctx.set_fill_style(&JsValue::from_str(budget));
            self.ctx.fill_rect(
                (x + 12.0) as f64,
                yy as f64,
                ((panel_w - 24.0) * pct) as f64,
                7.0,
            );
            yy += 24.0;
            self.panel_row(x, yy, "prompt", &format!("{:.0}", agent.prompt));
            yy += 20.0;
            self.panel_row(x, yy, "complete", &format!("{:.0}", agent.completion));
            yy += 20.0;
            self.panel_row(x, yy, "cached", &format!("{:.0}", agent.cached));
            yy += 20.0;
            self.panel_row_color(x, yy, "cost", &format!("${:.2}", agent.cost), C_GREEN_CSS);
            yy += 30.0;
            self.section_title(x, yy, "Recent events");
            yy += 18.0;
            self.round_rect(
                x + 12.0,
                yy - 8.0,
                panel_w - 24.0,
                92.0,
                4.0,
                "rgba(17,17,27,0.88)",
                "rgba(49,50,68,0.88)",
            );
            let mut ey = yy + 9.0;
            for ev in self
                .snapshot
                .events
                .iter()
                .filter(|e| e.agent_id.as_deref() == Some(&agent.id) || e.host_id == agent.host_id)
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
            {
                self.text(&ev.ts, x + 20.0, ey, 9.0, C_OVERLAY1_CSS, "normal");
                self.text(
                    &truncate(&ev.msg, 42),
                    x + 78.0,
                    ey,
                    9.0,
                    level_color_css(&ev.level),
                    "normal",
                );
                ey += 15.0;
            }
            yy += 108.0;
            if agent.needs_approval {
                self.section_title_color(x, yy, "Action needs approval", C_YELLOW_CSS);
                yy += 18.0;
                self.round_rect(
                    x + 12.0,
                    yy - 6.0,
                    panel_w - 24.0,
                    42.0,
                    4.0,
                    "rgba(17,17,27,0.9)",
                    "rgba(49,50,68,0.88)",
                );
                self.text(
                    &truncate(&agent.approval_command, 56),
                    x + 20.0,
                    yy + 10.0,
                    10.0,
                    C_SUBTEXT0_CSS,
                    "normal",
                );
                yy += 50.0;
                let approval_id = agent
                    .approval_id
                    .clone()
                    .unwrap_or_else(|| agent.id.clone());
                self.approval_button(
                    x + 12.0,
                    yy,
                    76.0,
                    "Approve",
                    &agent.host_id,
                    &approval_id,
                    "approve",
                    C_GREEN_CSS,
                );
                self.approval_button(
                    x + 96.0,
                    yy,
                    52.0,
                    "Skip",
                    &agent.host_id,
                    &approval_id,
                    "skip",
                    C_OVERLAY1_CSS,
                );
                let local_host_id = self
                    .snapshot
                    .hosts
                    .first()
                    .map(|host| host.id.as_str())
                    .unwrap_or("local");
                let deny_x = if agent.host_id == local_host_id || agent.host_id == "local" {
                    self.approval_button(
                        x + 156.0,
                        yy,
                        48.0,
                        "All",
                        &agent.host_id,
                        &approval_id,
                        "approve_all",
                        C_YELLOW_CSS,
                    );
                    x + 212.0
                } else {
                    x + 156.0
                };
                self.approval_button(
                    deny_x,
                    yy,
                    54.0,
                    "Deny",
                    &agent.host_id,
                    &approval_id,
                    "deny",
                    C_RED_CSS,
                );
            }
        }
    }

    fn draw_selected_action_menu(&mut self, id: &str, panel_x: f32, panel_y: f32, panel_h: f32) {
        let Some((title, color, actions)) = self.selected_action_menu_actions(id) else {
            return;
        };
        let menu_w = 188.0;
        let x = panel_x - menu_w - 10.0;
        if x < 246.0 || panel_h < 220.0 {
            return;
        }
        let menu_h = (panel_h - 8.0).min(360.0);
        self.round_rect(
            x,
            panel_y,
            menu_w,
            menu_h,
            6.0,
            "rgba(17,17,27,0.84)",
            color,
        );
        self.hit_zones
            .push(HitZone::new(x, panel_y, menu_w, menu_h, HitAction::Noop));
        self.text("ACTION MENU", x + 12.0, panel_y + 19.0, 10.0, color, "bold");
        self.text(
            &truncate(&title, 24),
            x + 12.0,
            panel_y + 36.0,
            9.0,
            C_SUBTEXT0_CSS,
            "normal",
        );

        let mut yy = panel_y + 52.0;
        for action in actions {
            if yy + 25.0 > panel_y + menu_h - 34.0 {
                break;
            }
            self.action_menu_button(x + 10.0, yy, menu_w - 20.0, action);
            yy += 29.0;
        }

        self.text(
            "rendered primary",
            x + 12.0,
            panel_y + menu_h - 15.0,
            8.0,
            C_OVERLAY1_CSS,
            "normal",
        );
        self.text(
            "DOM dock fallback-only",
            x + 92.0,
            panel_y + menu_h - 15.0,
            8.0,
            C_MAUVE_CSS,
            "normal",
        );
    }

    fn selected_action_menu_actions(
        &self,
        id: &str,
    ) -> Option<(String, &'static str, Vec<MenuAction>)> {
        let controls = self.snapshot.controls.clone();
        let managed = self.snapshot.managed.clone();
        let display_source = self.display_sources.values().next();
        match id {
            "system:activity" => Some((
                "Activity log".to_string(),
                C_TEAL_CSS,
                vec![
                    MenuAction::activity("Latest event", "bottom", C_TEAL_CSS),
                    MenuAction::activity("Copy visible log", "copy-visible", C_BLUE_CSS),
                    MenuAction::activity("Clear triage", "clear-triage", C_YELLOW_CSS),
                    MenuAction::activity("Clear log", "clear-log", C_RED_CSS),
                    MenuAction::select("Open activity detail", "system:activity", C_OVERLAY1_CSS),
                ],
            )),
            "system:context" => Some((
                "Context window".to_string(),
                C_BLUE_CSS,
                vec![
                    MenuAction::context("Live snapshot", "live", C_BLUE_CSS),
                    MenuAction::context("Replay window", "replay", C_MAUVE_CSS),
                    MenuAction::context("Focus item", "focus", C_TEAL_CSS),
                    MenuAction::context("Raw view", "raw", C_PEACH_CSS),
                    MenuAction::context("Copy snapshot", "copy-snapshot", C_TEAL_CSS),
                    MenuAction::context("Load exact", "load-exact", C_YELLOW_CSS),
                    MenuAction::context("Reset view", "reset", C_OVERLAY1_CSS),
                ],
            )),
            "system:managed" => Some((
                "Managed context".to_string(),
                C_MAUVE_CSS,
                vec![
                    MenuAction::managed(
                        "Use target",
                        "use-target",
                        "",
                        &managed.session_id,
                        C_TEAL_CSS,
                    ),
                    MenuAction::managed(
                        "Prepare rewind",
                        "rewind",
                        "",
                        &managed.session_id,
                        C_MAUVE_CSS,
                    ),
                    MenuAction::managed(
                        "Backout record",
                        "backout",
                        "",
                        &managed.session_id,
                        C_PEACH_CSS,
                    ),
                    MenuAction::managed(
                        "Refresh anchors",
                        "refresh",
                        "",
                        &managed.session_id,
                        C_BLUE_CSS,
                    ),
                    MenuAction::managed(
                        "Copy status",
                        "copy-status",
                        "",
                        &managed.session_id,
                        C_TEAL_CSS,
                    ),
                    MenuAction::select("Open managed detail", "system:managed", C_OVERLAY1_CSS),
                ],
            )),
            "system:peers" => {
                let mut actions = vec![
                    MenuAction::controls(
                        if controls.display_access.starts_with("on") {
                            "Revoke local display"
                        } else {
                            "Share local display"
                        },
                        "display-toggle",
                        C_PEACH_CSS,
                    ),
                    MenuAction::controls("List local displays", "display-list", C_BLUE_CSS),
                    MenuAction::controls("Copy peer status", "peer-status-copy", C_TEAL_CSS),
                ];
                if let Some(source) = display_source {
                    actions.insert(
                        0,
                        MenuAction::open_display(
                            "Open first display",
                            &source.host_id,
                            &source.display_id,
                            C_PEACH_CSS,
                        ),
                    );
                }
                if controls.shared_view_can_take_input {
                    actions.push(MenuAction::controls(
                        "Take shared input",
                        "shared-view-take-input",
                        C_GREEN_CSS,
                    ));
                }
                if controls.shared_view_visible {
                    actions.push(MenuAction::controls(
                        "Focus shared view",
                        "shared-view-focus",
                        C_GREEN_CSS,
                    ));
                    actions.push(MenuAction::controls(
                        "Hide shared view",
                        "shared-view-hide",
                        C_YELLOW_CSS,
                    ));
                }
                Some(("Peers and displays".to_string(), C_PEACH_CSS, actions))
            }
            "system:sessions" => {
                let mut actions = vec![
                    MenuAction::activity("New session", "new-session", C_TEAL_CSS),
                    MenuAction::select("Session detail", "system:sessions", C_OVERLAY1_CSS),
                ];
                if controls.session_can_focus {
                    actions.insert(
                        0,
                        MenuAction::activity("Focus target", "target", C_BLUE_CSS),
                    );
                }
                if controls.session_can_attach && !controls.session_id.is_empty() {
                    actions.insert(
                        0,
                        MenuAction::session(
                            "Attach target",
                            "attach",
                            &controls.session_id,
                            C_PEACH_CSS,
                        ),
                    );
                }
                if controls.session_can_interrupt {
                    actions.push(MenuAction::activity("Stop target", "stop", C_RED_CSS));
                }
                Some(("Sessions".to_string(), C_TEAL_CSS, actions))
            }
            "system:controls" => {
                let mut actions = vec![
                    MenuAction::activity(
                        if controls.prompt_mode == "steer" {
                            "Steer"
                        } else {
                            "Send"
                        },
                        "send",
                        C_BLUE_CSS,
                    ),
                    MenuAction::activity("New session", "new-session", C_TEAL_CSS),
                    MenuAction::controls("Share display", "display-toggle", C_PEACH_CSS),
                    MenuAction::controls("Toggle mic", "voice-toggle", C_TEAL_CSS),
                    MenuAction::controls("Toggle video", "video-toggle", C_TEAL_CSS),
                ];
                if controls.session_can_focus {
                    actions.push(MenuAction::activity("Focus target", "target", C_PEACH_CSS));
                }
                if controls.session_can_interrupt {
                    actions.push(MenuAction::activity("Stop target", "stop", C_RED_CSS));
                }
                if controls.shared_view_can_take_input {
                    actions.push(MenuAction::controls(
                        "Take shared input",
                        "shared-view-take-input",
                        C_GREEN_CSS,
                    ));
                }
                Some(("Operator controls".to_string(), C_MAUVE_CSS, actions))
            }
            "system:changes" => Some((
                "Working tree".to_string(),
                C_YELLOW_CSS,
                vec![
                    MenuAction::changes("Refresh changes", "refresh", "", C_BLUE_CSS),
                    MenuAction::changes("Copy paths", "copy-paths", "", C_TEAL_CSS),
                    MenuAction::select("Changes detail", "system:changes", C_OVERLAY1_CSS),
                ],
            )),
            "system:worktrees" => Some((
                "Worktrees".to_string(),
                C_BLUE_CSS,
                vec![
                    MenuAction::session("Refresh index", "worktree-refresh", "", C_BLUE_CSS),
                    MenuAction::session("Search worktrees", "worktree-search", "", C_TEAL_CSS),
                    MenuAction::select("Worktree detail", "system:worktrees", C_OVERLAY1_CSS),
                ],
            )),
            "system:view" => Some((
                "Station view".to_string(),
                C_MAUVE_CSS,
                vec![
                    MenuAction::new(
                        "Orbital layout",
                        C_BLUE_CSS,
                        HitAction::Layout(LayoutName::Orbital),
                    ),
                    MenuAction::new(
                        "Constellation layout",
                        C_MAUVE_CSS,
                        HitAction::Layout(LayoutName::Constellation),
                    ),
                    MenuAction::new("Cockpit mood", C_TEAL_CSS, HitAction::Mood(Mood::Cockpit)),
                    MenuAction::new("Calm mood", C_PEACH_CSS, HitAction::Mood(Mood::Calm)),
                ],
            )),
            _ => None,
        }
    }

    fn action_menu_button(&mut self, x: f32, y: f32, w: f32, action: MenuAction) {
        self.round_rect(x, y, w, 24.0, 4.0, "rgba(49,50,68,0.42)", action.color);
        self.text(
            &truncate(&action.label, 24),
            x + 9.0,
            y + 15.5,
            9.0,
            action.color,
            "bold",
        );
        self.hit_zones.push(HitZone::new(x, y, w, 24.0, action.hit));
    }

    fn activity_event(&self, event_id: &str) -> Option<StationEvent> {
        self.snapshot
            .events
            .iter()
            .find(|event| event.id == event_id)
            .cloned()
    }

    fn draw_activity_event_info(&mut self, event_id: &str, x: f32, y: f32, panel_w: f32) {
        let Some(event) = self.activity_event(event_id) else {
            self.text("activity", x + 12.0, y + 25.0, 10.0, C_TEAL_CSS, "bold");
            self.text(
                "event expired",
                x + 92.0,
                y + 25.0,
                13.0,
                C_TEXT_CSS,
                "bold",
            );
            self.panel_row(x, y + 58.0, "id", &truncate(event_id, 42));
            return;
        };
        let color = level_color_css(&event.level);
        self.text(
            "activity event",
            x + 12.0,
            y + 25.0,
            10.0,
            C_TEAL_CSS,
            "bold",
        );
        self.text(
            &truncate(&event.level, 22),
            x + 126.0,
            y + 25.0,
            13.0,
            color,
            "bold",
        );
        let mut yy = y + 58.0 - self.panel_scroll;
        self.panel_row(x, yy, "id", &truncate(&event.id, 42));
        yy += 22.0;
        self.panel_row(x, yy, "time", &nonempty(&event.ts, "--"));
        yy += 22.0;
        self.panel_row_color(x, yy, "level", &event.level, color);
        yy += 22.0;
        self.panel_row(x, yy, "source", &nonempty(&event.source, "--"));
        yy += 22.0;
        self.panel_row(x, yy, "host", &nonempty(&event.host_id, "local"));
        yy += 22.0;
        self.panel_row(x, yy, "session", &truncate(&event.session_id, 42));
        yy += 30.0;
        self.section_title_color(x, yy, "Rendered event detail", C_TEAL_CSS);
        yy += 18.0;
        self.round_rect(
            x + 12.0,
            yy - 8.0,
            panel_w - 24.0,
            92.0,
            4.0,
            "rgba(17,17,27,0.82)",
            "rgba(148,226,213,0.42)",
        );
        self.text(
            &truncate(&event.msg, 66),
            x + 20.0,
            yy + 12.0,
            10.0,
            C_TEXT_CSS,
            "normal",
        );
        let detail = format!(
            "{} / {}{}{}",
            nonempty(&event.action, "log"),
            nonempty(&event.source, "activity"),
            if event.editable { " / editable" } else { "" },
            if event.historical {
                " / historical"
            } else {
                ""
            }
        );
        self.text(
            &truncate(&detail, 66),
            x + 20.0,
            yy + 32.0,
            9.0,
            C_SUBTEXT0_CSS,
            "normal",
        );
        yy += 112.0;
        self.section_title_color(x, yy, "Actions", C_TEAL_CSS);
        yy += 22.0;
        let mut actions = vec![
            ("show-log", "show log", 78.0, C_TEAL_CSS.to_string()),
            ("copy-event", "copy text", 82.0, C_BLUE_CSS.to_string()),
            (
                "copy-event-json",
                "copy JSON",
                86.0,
                C_MAUVE_CSS.to_string(),
            ),
        ];
        if !event.session_id.is_empty() {
            actions.push(("activity-session", "session", 70.0, C_PEACH_CSS.to_string()));
        }
        if activity_event_is_managed(&event) {
            actions.push(("activity-managed", "managed", 78.0, C_MAUVE_CSS.to_string()));
        }
        if event.editable {
            actions.push((
                if event.historical { "branch" } else { "edit" },
                if event.historical { "branch" } else { "edit" },
                58.0,
                C_YELLOW_CSS.to_string(),
            ));
        }
        self.draw_activity_event_action_pills(x, panel_w, yy - 14.0, &actions, &event.id);
    }

    fn draw_activity_event_action_pills(
        &mut self,
        x: f32,
        panel_w: f32,
        y: f32,
        actions: &[(&str, &str, f32, String)],
        event_id: &str,
    ) -> f32 {
        let mut ax = x + 14.0;
        let mut ay = y;
        for (action, label, width, color) in actions {
            if ax + *width > x + panel_w - 14.0 {
                ax = x + 14.0;
                ay += 25.0;
            }
            self.pill_at(ax, ay, *width, 21.0, label, color);
            self.hit_zones.push(HitZone::new(
                ax,
                ay,
                *width,
                21.0,
                HitAction::ActivityAction {
                    action: (*action).to_string(),
                    id: event_id.to_string(),
                },
            ));
            ax += *width + 8.0;
        }
        ay + 35.0
    }

    fn draw_activity_info(&mut self, x: f32, y: f32, panel_w: f32) {
        let events = self.snapshot.events.clone();
        self.text("activity", x + 12.0, y + 25.0, 10.0, C_TEAL_CSS, "bold");
        self.text(
            &format!("{} events", events.len()),
            x + 92.0,
            y + 25.0,
            13.0,
            C_TEXT_CSS,
            "bold",
        );
        let mut yy = y + 58.0 - self.panel_scroll;
        self.panel_row(x, yy, "retained", &events.len().to_string());
        yy += 22.0;
        let latest = events.last();
        self.panel_row_color(
            x,
            yy,
            "latest",
            latest.map(|ev| ev.level.as_str()).unwrap_or("--"),
            latest
                .map(|ev| level_color_css(&ev.level))
                .unwrap_or(C_OVERLAY1_CSS),
        );
        yy += 30.0;
        self.section_title_color(x, yy, "Log controls", C_TEAL_CSS);
        yy += 22.0;
        let log_actions = [
            ("verbosity:normal", "normal", 66.0),
            ("verbosity:verbose", "verbose", 74.0),
            ("verbosity:debug", "debug", 58.0),
            ("host:all", "all hosts", 76.0),
            ("copy-visible", "copy visible", 96.0),
            ("clear-triage", "clear triage", 96.0),
            ("bottom", "bottom", 62.0),
            ("clear-log", "clear log", 76.0),
        ];
        let mut ax = x + 14.0;
        let mut ay = yy - 14.0;
        for (action, label, width) in log_actions {
            if ax + width > x + panel_w - 14.0 {
                ax = x + 14.0;
                ay += 25.0;
            }
            self.pill_at(ax, ay, width, 21.0, label, C_TEAL_CSS);
            self.hit_zones.push(HitZone::new(
                ax,
                ay,
                width,
                21.0,
                HitAction::ActivityAction {
                    action: action.to_string(),
                    id: String::new(),
                },
            ));
            ax += width + 8.0;
        }
        yy = ay + 35.0;
        self.section_title(x, yy, "Recent activity");
        yy += 18.0;
        self.round_rect(
            x + 12.0,
            yy - 8.0,
            panel_w - 24.0,
            150.0,
            4.0,
            "rgba(17,17,27,0.88)",
            "rgba(49,50,68,0.88)",
        );
        let mut ey = yy + 10.0;
        if events.is_empty() {
            self.text(
                "Waiting for dashboard events",
                x + 20.0,
                ey,
                10.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
        } else {
            for ev in events
                .iter()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
            {
                if ev.action == "log" && !ev.id.is_empty() {
                    self.hit_zones.push(HitZone::new(
                        x + 12.0,
                        ey - 11.0,
                        panel_w - 24.0,
                        17.0,
                        HitAction::ActivityAction {
                            action: ev.action.clone(),
                            id: ev.id.clone(),
                        },
                    ));
                }
                self.text(&ev.ts, x + 20.0, ey, 9.0, C_OVERLAY1_CSS, "normal");
                self.text(
                    &truncate(&ev.level, 8),
                    x + 76.0,
                    ey,
                    9.0,
                    level_color_css(&ev.level),
                    "bold",
                );
                self.text(
                    &truncate(&ev.msg, 44),
                    x + 132.0,
                    ey,
                    9.0,
                    C_SUBTEXT0_CSS,
                    "normal",
                );
                ey += 16.0;
            }
        }
    }

    fn draw_context_info(&mut self, x: f32, y: f32, panel_w: f32) {
        let ctx = self.snapshot.context.clone();
        self.text("context", x + 12.0, y + 25.0, 10.0, C_BLUE_CSS, "bold");
        self.text(
            if ctx.available {
                "model window"
            } else {
                "waiting for snapshot"
            },
            x + 92.0,
            y + 25.0,
            13.0,
            C_TEXT_CSS,
            "bold",
        );
        let mut yy = y + 58.0 - self.panel_scroll;
        self.panel_row(x, yy, "source", &nonempty(&ctx.source, "--"));
        yy += 22.0;
        self.panel_row(x, yy, "session", &truncate(&ctx.session_id, 42));
        yy += 22.0;
        self.panel_row(x, yy, "turn", &nonempty(&ctx.turn, "--"));
        yy += 22.0;
        self.panel_row(x, yy, "format", &nonempty(&ctx.format, "--"));
        yy += 30.0;
        self.section_title_color(x, yy, "View controls", C_BLUE_CSS);
        yy += 22.0;
        let context_actions = [
            ("live", "live", 50.0),
            ("replay", "replay", 64.0),
            ("focus", "focus", 58.0),
            ("raw", "raw", 46.0),
            ("reset", "reset", 54.0),
            ("copy-snapshot", "copy", 54.0),
            ("load-exact", "exact", 54.0),
        ];
        let mut ax = x + 14.0;
        let mut ay = yy - 14.0;
        for (action, label, width) in context_actions {
            if ax + width > x + panel_w - 14.0 {
                ax = x + 14.0;
                ay += 25.0;
            }
            self.pill_at(ax, ay, width, 21.0, label, C_BLUE_CSS);
            self.hit_zones.push(HitZone::new(
                ax,
                ay,
                width,
                21.0,
                HitAction::ContextAction {
                    action: action.to_string(),
                    id: String::new(),
                },
            ));
            ax += width + 8.0;
        }
        yy = ay + 35.0;
        yy = self.draw_context_replay_controls(x, yy, panel_w, &ctx);
        self.section_title(x, yy, "Token pressure");
        yy += 18.0;
        self.meter(
            x + 12.0,
            yy,
            panel_w - 24.0,
            percent(ctx.tokens, ctx.effective_window),
            pressure_color(percent(ctx.tokens, ctx.effective_window)),
        );
        yy += 24.0;
        self.panel_row(
            x,
            yy,
            "effective",
            &format!(
                "{} / {}",
                compact_number(ctx.tokens as f64),
                compact_number(ctx.effective_window as f64)
            ),
        );
        yy += 20.0;
        self.panel_row(
            x,
            yy,
            "hard",
            &format!(
                "{} / {}",
                compact_number(ctx.tokens as f64),
                compact_number(ctx.hard_window as f64)
            ),
        );
        yy += 24.0;
        self.panel_row(
            x,
            yy,
            "shape",
            &format!(
                "{} items · {} categories",
                ctx.item_count, ctx.category_count
            ),
        );
        yy += 30.0;
        self.section_title(x, yy, "Top context lanes");
        yy += 18.0;
        if ctx.top_categories.is_empty() {
            self.panel_row(x, yy, "lanes", "--");
        } else {
            for item in ctx.top_categories.iter().take(5) {
                self.panel_row(
                    x,
                    yy,
                    &truncate(&item.label, 13),
                    &format!("{} tokens", compact_number(item.value as f64)),
                );
                yy += 19.0;
            }
        }
        yy += 12.0;
        self.section_title_color(x, yy, "Largest context items", C_MAUVE_CSS);
        yy += 18.0;
        self.context_detail_rows(
            x,
            yy,
            panel_w,
            &ctx.top_items,
            "No item details in this snapshot",
            5,
        );
    }

    fn draw_context_replay_controls(
        &mut self,
        x: f32,
        y: f32,
        panel_w: f32,
        ctx: &StationContextSummary,
    ) -> f32 {
        let mut yy = y;
        self.section_title_color(x, yy, "Replay timeline", C_BLUE_CSS);
        yy += 18.0;
        self.round_rect(
            x + 12.0,
            yy - 10.0,
            panel_w - 24.0,
            66.0,
            4.0,
            "rgba(17,17,27,0.76)",
            "rgba(137,180,250,0.46)",
        );
        let selected = if ctx.replay_count > 0 {
            format!("{} / {}", ctx.replay_index, ctx.replay_count)
        } else {
            "--".to_string()
        };
        self.text(
            &format!("{} snapshots", ctx.replay_count),
            x + 22.0,
            yy + 4.0,
            9.0,
            C_TEXT_CSS,
            "bold",
        );
        self.text(
            &format!(
                "{} / {} / {}",
                nonempty(&ctx.replay_mode, "live"),
                selected,
                nonempty(&ctx.replay_time, "--")
            ),
            x + 22.0,
            yy + 21.0,
            8.5,
            C_SUBTEXT0_CSS,
            "normal",
        );
        self.text(
            &format!("raw {}", nonempty(&ctx.exact_status, "compact")),
            x + 22.0,
            yy + 38.0,
            8.5,
            if ctx.exact_status.contains("failed") {
                C_YELLOW_CSS
            } else {
                C_OVERLAY1_CSS
            },
            "normal",
        );

        let actions = [
            ("replay-prev", "prev", 48.0, C_BLUE_CSS),
            ("replay-next", "next", 50.0, C_BLUE_CSS),
            ("replay-latest", "latest", 58.0, C_TEAL_CSS),
            ("live", "live", 46.0, C_GREEN_CSS),
            ("copy-snapshot", "copy", 48.0, C_MAUVE_CSS),
            ("load-exact", "exact", 52.0, C_PEACH_CSS),
        ];
        let mut bx = x + panel_w - 22.0;
        for (action, label, width, color) in actions.into_iter().rev() {
            bx -= width;
            if bx < x + 106.0 {
                break;
            }
            self.pill_at(bx, yy + 39.0, width, 18.0, label, color);
            self.hit_zones.push(HitZone::new(
                bx,
                yy + 39.0,
                width,
                18.0,
                HitAction::ContextAction {
                    action: action.to_string(),
                    id: String::new(),
                },
            ));
            bx -= 6.0;
        }
        yy + 82.0
    }

    fn draw_managed_info(&mut self, x: f32, y: f32, panel_w: f32) {
        let managed = self.snapshot.managed.clone();
        self.text("managed", x + 12.0, y + 25.0, 10.0, C_MAUVE_CSS, "bold");
        self.text(
            &nonempty(&managed.mode, "unknown"),
            x + 92.0,
            y + 25.0,
            13.0,
            C_TEXT_CSS,
            "bold",
        );
        let mut yy = y + 58.0 - self.panel_scroll;
        self.panel_row(x, yy, "session", &truncate(&managed.session_id, 42));
        yy += 22.0;
        self.panel_row_color(
            x,
            yy,
            "pressure",
            &nonempty(&managed.status, "unknown"),
            pressure_color(percent(managed.used_tokens, managed.effective_window)),
        );
        yy += 28.0;
        self.meter(
            x + 12.0,
            yy,
            panel_w - 24.0,
            percent(managed.used_tokens, managed.effective_window),
            pressure_color(percent(managed.used_tokens, managed.effective_window)),
        );
        yy += 24.0;
        self.panel_row(
            x,
            yy,
            "effective",
            &format_token_ratio(managed.used_tokens, managed.effective_window),
        );
        yy += 20.0;
        self.panel_row(
            x,
            yy,
            "hard",
            &format_token_ratio(managed.used_tokens, managed.hard_window),
        );
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "rewind-only",
            if managed.rewind_only { "on" } else { "off" },
        );
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "history",
            &format!("{} records · {} anchors", managed.records, managed.anchors),
        );
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "branches",
            &format!(
                "{} lineage · {} fission · {} branches",
                managed.lineage_groups, managed.fission_groups, managed.branches
            ),
        );
        yy += 30.0;
        self.section_title_color(x, yy, "Actions", C_MAUVE_CSS);
        yy += 22.0;
        let managed_actions = [
            ("rewind", "prepare rewind", 116.0),
            ("backout", "backout", 72.0),
            ("refresh", "refresh", 68.0),
            ("use-target", "use target", 86.0),
            ("copy-status", "copy status", 94.0),
        ];
        let mut ax = x + 14.0;
        let mut ay = yy - 14.0;
        for (action, label, width) in managed_actions {
            if ax + width > x + panel_w - 14.0 {
                ax = x + 14.0;
                ay += 25.0;
            }
            self.pill_at(ax, ay, width, 21.0, label, C_MAUVE_CSS);
            self.hit_zones.push(HitZone::new(
                ax,
                ay,
                width,
                21.0,
                HitAction::ManagedAction {
                    action: action.to_string(),
                    id: String::new(),
                    session_id: managed.session_id.clone(),
                },
            ));
            ax += width + 8.0;
        }
        yy = ay + 35.0;
        self.section_title_color(
            x,
            yy,
            if managed.error.is_empty() {
                "Status"
            } else {
                "Warning"
            },
            if managed.error.is_empty() {
                C_GREEN_CSS
            } else {
                C_YELLOW_CSS
            },
        );
        yy += 18.0;
        self.text(
            &truncate(
                if managed.error.is_empty() {
                    "Managed context data is flowing from the existing dashboard/MCP state."
                } else {
                    &managed.error
                },
                68,
            ),
            x + 12.0,
            yy,
            10.0,
            C_SUBTEXT0_CSS,
            "normal",
        );
        yy += 38.0;
        yy = self.draw_managed_action_cockpit(x, yy, panel_w, &managed);
        yy = self.draw_managed_activity_signal(x, yy, panel_w, &managed);
        self.section_title_color(x, yy, "Recent rewinds", C_MAUVE_CSS);
        yy += 18.0;
        yy = self.managed_detail_rows(
            x,
            yy,
            panel_w,
            &managed.recent_records,
            "No rewind records for this session",
            4,
        );
        yy += 10.0;
        self.section_title_color(x, yy, "Anchor catalog", C_PEACH_CSS);
        yy += 18.0;
        yy = self.managed_detail_rows(
            x,
            yy,
            panel_w,
            &managed.recent_anchors,
            "No anchors discovered for this session",
            5,
        );
        yy += 10.0;
        self.section_title_color(x, yy, "Edit branches", C_BLUE_CSS);
        yy += 18.0;
        self.managed_detail_rows(
            x,
            yy,
            panel_w,
            &managed.recent_branches,
            "No claimable fission branches",
            3,
        );
    }

    fn draw_managed_action_cockpit(
        &mut self,
        x: f32,
        y: f32,
        panel_w: f32,
        managed: &StationManagedSummary,
    ) -> f32 {
        let action = &managed.action_state;
        let card_h = 130.0;
        self.round_rect(
            x + 12.0,
            y - 10.0,
            panel_w - 24.0,
            card_h,
            4.0,
            "rgba(17,17,27,0.78)",
            "rgba(203,166,247,0.56)",
        );
        self.text(
            "Managed action cockpit",
            x + 20.0,
            y + 2.0,
            9.0,
            C_MAUVE_CSS,
            "bold",
        );
        self.text(
            &truncate(&nonempty(&action.readiness, "ready"), 38),
            x + 20.0,
            y + 18.0,
            8.5,
            if action.can_rewind || action.can_backout || action.can_inspect {
                C_GREEN_CSS
            } else {
                C_YELLOW_CSS
            },
            "normal",
        );
        self.text(
            &format!("anchor {}", truncate(&nonempty(&action.anchor, "--"), 22)),
            x + 20.0,
            y + 34.0,
            8.0,
            C_SUBTEXT0_CSS,
            "normal",
        );
        self.text(
            &format!("record {}", truncate(&nonempty(&action.record, "--"), 22)),
            x + 20.0,
            y + 49.0,
            8.0,
            C_SUBTEXT0_CSS,
            "normal",
        );
        let draft_state = format!(
            "{} / reason {} / primer {}",
            nonempty(&action.position, "after"),
            if action.has_reason { "ready" } else { "missing" },
            if action.has_primer { "ready" } else { "missing" }
        );
        self.text(
            &truncate(&draft_state, 37),
            x + 20.0,
            y + 65.0,
            8.0,
            if action.has_reason && action.has_primer {
                C_GREEN_CSS
            } else {
                C_YELLOW_CSS
            },
            "normal",
        );
        self.text(
            &truncate(&nonempty(&action.result, "No action result yet"), 37),
            x + 20.0,
            y + 80.0,
            8.0,
            C_OVERLAY1_CSS,
            "normal",
        );

        let mut buttons: Vec<(&str, f32, &str, HitAction, bool)> = Vec::new();
        buttons.push((
            "seed",
            46.0,
            C_GREEN_CSS,
            HitAction::ManagedAction {
                action: "seed-context".to_string(),
                id: String::new(),
                session_id: managed.session_id.clone(),
            },
            !managed.session_id.is_empty(),
        ));
        buttons.push((
            "inspect",
            58.0,
            C_PEACH_CSS,
            HitAction::ManagedAction {
                action: "anchor-inspect".to_string(),
                id: action.anchor.clone(),
                session_id: managed.session_id.clone(),
            },
            action.can_inspect,
        ));
        buttons.push((
            "rewind",
            58.0,
            C_MAUVE_CSS,
            HitAction::ManagedAction {
                action: "dispatch-rewind".to_string(),
                id: action.anchor.clone(),
                session_id: managed.session_id.clone(),
            },
            action.can_rewind,
        ));
        buttons.push((
            if action.backout_mode.is_empty() {
                "backout"
            } else {
                "run"
            },
            58.0,
            C_TEAL_CSS,
            HitAction::ManagedAction {
                action: "run-backout".to_string(),
                id: action.record.clone(),
                session_id: managed.session_id.clone(),
            },
            action.can_backout,
        ));
        buttons.push((
            "refresh",
            58.0,
            C_BLUE_CSS,
            HitAction::ManagedAction {
                action: "refresh".to_string(),
                id: String::new(),
                session_id: managed.session_id.clone(),
            },
            true,
        ));
        let button_gap = 6.0;
        let total_w = buttons.iter().map(|(_, w, _, _, _)| *w).sum::<f32>()
            + buttons.len().saturating_sub(1) as f32 * button_gap;
        let mut bx = (x + panel_w - total_w - 20.0).max(x + 20.0);
        for (label, button_w, color, hit, enabled) in buttons {
            let draw_color = if enabled { color } else { C_OVERLAY1_CSS };
            if bx + button_w > x + panel_w - 18.0 {
                break;
            }
            self.pill_at(bx, y + 98.0, button_w, 19.0, label, draw_color);
            if enabled {
                self.hit_zones
                    .push(HitZone::new(bx, y + 98.0, button_w, 19.0, hit));
            }
            bx += button_w + button_gap;
        }
        y + card_h + 4.0
    }

    fn draw_managed_activity_signal(
        &mut self,
        x: f32,
        y: f32,
        panel_w: f32,
        managed: &StationManagedSummary,
    ) -> f32 {
        let signal = &managed.activity_signal;
        if signal.id.is_empty() {
            return y;
        }
        self.section_title_color(x, y, "Activity signal", C_TEAL_CSS);
        let card_y = y + 18.0;
        self.round_rect(
            x + 12.0,
            card_y - 10.0,
            panel_w - 24.0,
            67.0,
            4.0,
            "rgba(17,17,27,0.76)",
            "rgba(148,226,213,0.46)",
        );
        self.ctx.set_fill_style(&JsValue::from_str(C_TEAL_CSS));
        self.ctx
            .fill_rect((x + 20.0) as f64, (card_y - 2.0) as f64, 3.0, 31.0);
        self.text(
            &truncate(&nonempty(&signal.label, "managed activity"), 27),
            x + 30.0,
            card_y + 3.0,
            9.5,
            C_TEAL_CSS,
            "bold",
        );
        self.text(
            &truncate(&signal.value, 27),
            x + panel_w - 122.0,
            card_y + 3.0,
            8.5,
            C_TEXT_CSS,
            "normal",
        );
        self.text(
            &truncate(&signal.detail, 45),
            x + 30.0,
            card_y + 20.0,
            8.5,
            C_SUBTEXT0_CSS,
            "normal",
        );

        let mut buttons = vec![
            ("log", "show-log", 42.0, C_TEAL_CSS),
            ("copy", "copy-event-json", 48.0, C_BLUE_CSS),
        ];
        if !signal.session_id.is_empty() {
            buttons.push(("session", "activity-session", 64.0, C_PEACH_CSS));
        }
        buttons.push(("clear", "clear-activity-signal", 48.0, C_OVERLAY1_CSS));
        let total_w = buttons.iter().map(|(_, _, w, _)| *w).sum::<f32>()
            + buttons.len().saturating_sub(1) as f32 * 6.0;
        let mut bx = x + panel_w - total_w - 28.0;
        for (label, action, width, color) in buttons {
            self.pill_at(bx, card_y + 34.0, width, 19.0, label, color);
            let hit = if action == "clear-activity-signal" {
                HitAction::ManagedAction {
                    action: action.to_string(),
                    id: String::new(),
                    session_id: managed.session_id.clone(),
                }
            } else {
                HitAction::ActivityAction {
                    action: action.to_string(),
                    id: signal.id.clone(),
                }
            };
            self.hit_zones
                .push(HitZone::new(bx, card_y + 34.0, width, 19.0, hit));
            bx += width + 6.0;
        }
        card_y + 69.0
    }

    fn draw_changes_info(&mut self, x: f32, y: f32, panel_w: f32) {
        let changes = self.snapshot.changes.clone();
        self.text("changes", x + 12.0, y + 25.0, 10.0, C_YELLOW_CSS, "bold");
        self.text(
            &format!("{} files", changes.count),
            x + 92.0,
            y + 25.0,
            13.0,
            C_TEXT_CSS,
            "bold",
        );
        let mut yy = y + 58.0 - self.panel_scroll;
        self.panel_row_color(
            x,
            yy,
            "state",
            &nonempty(&changes.status, "clean"),
            if changes.count > 0 || changes.status == "mismatch" {
                C_YELLOW_CSS
            } else {
                C_GREEN_CSS
            },
        );
        yy += 22.0;
        self.panel_row(x, yy, "files", &changes.count.to_string());
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "kinds",
            &format!(
                "{} add · {} mod · {} del",
                changes.added, changes.modified, changes.deleted
            ),
        );
        yy += 22.0;
        self.panel_row(x, yy, "external", &changes.external.to_string());
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "lines",
            &format!(
                "+{} / -{}",
                compact_number(changes.total_added as f64),
                compact_number(changes.total_removed as f64)
            ),
        );
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "latest",
            &truncate(
                &if changes.latest_path.is_empty() {
                    "--".to_string()
                } else {
                    format!(
                        "{} · {}",
                        nonempty(&changes.latest_kind, "file"),
                        changes.latest_path
                    )
                },
                42,
            ),
        );
        yy += 30.0;
        self.section_title_color(x, yy, "Change controls", C_YELLOW_CSS);
        yy += 22.0;
        let change_actions = [
            ("refresh", "refresh", 68.0),
            ("copy-paths", "copy paths", 88.0),
            ("copy-diff", "copy diff", 80.0),
            ("history", "history", 68.0),
            ("redo", "redo", 54.0),
            ("prune", "prune", 62.0),
        ];
        let mut ax = x + 14.0;
        for (action, label, width) in change_actions {
            self.pill_at(ax, yy - 14.0, width, 21.0, label, C_YELLOW_CSS);
            self.hit_zones.push(HitZone::new(
                ax,
                yy - 14.0,
                width,
                21.0,
                HitAction::ChangesAction {
                    action: action.to_string(),
                    path: if action == "copy-diff" {
                        changes.latest_path.clone()
                    } else {
                        String::new()
                    },
                },
            ));
            ax += width + 8.0;
        }
        yy += 30.0;
        self.section_title_color(x, yy, "Changed files", C_YELLOW_CSS);
        yy += 18.0;
        yy = self.changes_detail_rows(
            x,
            yy,
            panel_w,
            &changes.recent,
            if changes.status == "mismatch" {
                "Change tracking is pointed at another root"
            } else {
                "No file changes yet"
            },
            7,
        );
        yy += 12.0;
        if !changes.latest_path.is_empty() {
            self.section_title_color(x, yy, "Selected file", C_PEACH_CSS);
            yy += 18.0;
            self.panel_row(x, yy, "path", &truncate(&changes.latest_path, 42));
            yy += 24.0;
            let selected_actions = [("file", "view", 52.0), ("copy-diff", "copy diff", 80.0)];
            let mut ax = x + 14.0;
            for (action, label, width) in selected_actions {
                self.pill_at(ax, yy - 14.0, width, 21.0, label, C_PEACH_CSS);
                self.hit_zones.push(HitZone::new(
                    ax,
                    yy - 14.0,
                    width,
                    21.0,
                    HitAction::ChangesAction {
                        action: action.to_string(),
                        path: changes.latest_path.clone(),
                    },
                ));
                ax += width + 8.0;
            }
        }
    }

    fn draw_sessions_info(&mut self, x: f32, y: f32, panel_w: f32) {
        let sessions = self.snapshot.sessions.clone();
        self.text("sessions", x + 12.0, y + 25.0, 10.0, C_TEAL_CSS, "bold");
        self.text(
            &format!("{} known", sessions.total),
            x + 92.0,
            y + 25.0,
            13.0,
            C_TEXT_CSS,
            "bold",
        );
        let mut yy = y + 58.0 - self.panel_scroll;
        self.panel_row(x, yy, "active", &sessions.active.to_string());
        yy += 22.0;
        self.panel_row(x, yy, "external", &sessions.external.to_string());
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "tokens",
            &compact_number(sessions.total_tokens as f64),
        );
        yy += 22.0;
        self.panel_row(x, yy, "disk", &format_bytes(sessions.disk_bytes));
        yy += 22.0;
        self.panel_row(x, yy, "index", &nonempty(&sessions.index_status, "cold"));
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "worktrees",
            &format!(
                "{} · {} cleanup",
                sessions.worktrees, sessions.worktree_cleanup
            ),
        );
        yy += 22.0;
        self.panel_row_color(
            x,
            yy,
            "wt risk",
            &format!(
                "{} dirty · {} unmerged · {} active",
                sessions.worktree_dirty, sessions.worktree_unmerged, sessions.worktree_active
            ),
            if sessions.worktree_dirty > 0
                || sessions.worktree_unmerged > 0
                || sessions.worktree_active > 0
            {
                C_YELLOW_CSS
            } else {
                C_TEXT_CSS
            },
        );
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "wt disk",
            &format!(
                "{} · {}",
                format_bytes(sessions.worktree_bytes),
                nonempty(&sessions.worktree_scan_status, "cold")
            ),
        );
        yy += 30.0;
        self.section_title_color(x, yy, "Shortcuts", C_TEAL_CSS);
        yy += 22.0;
        self.pill_at(x + 14.0, yy - 14.0, 98.0, 21.0, "new session", C_TEAL_CSS);
        self.hit_zones.push(HitZone::new(
            x + 14.0,
            yy - 14.0,
            98.0,
            21.0,
            HitAction::SessionAction {
                action: "new-session".to_string(),
                session_id: String::new(),
            },
        ));
        self.pill_at(x + 122.0, yy - 14.0, 68.0, 21.0, "search", C_TEAL_CSS);
        self.hit_zones.push(HitZone::new(
            x + 122.0,
            yy - 14.0,
            68.0,
            21.0,
            HitAction::SessionAction {
                action: "search".to_string(),
                session_id: String::new(),
            },
        ));
        self.pill_at(x + 198.0, yy - 14.0, 98.0, 21.0, "deep search", C_MAUVE_CSS);
        self.hit_zones.push(HitZone::new(
            x + 198.0,
            yy - 14.0,
            98.0,
            21.0,
            HitAction::SessionAction {
                action: "deep-search".to_string(),
                session_id: String::new(),
            },
        ));
        yy += 25.0;
        self.pill_at(x + 14.0, yy - 14.0, 86.0, 21.0, "worktrees", C_BLUE_CSS);
        self.hit_zones.push(HitZone::new(
            x + 14.0,
            yy - 14.0,
            86.0,
            21.0,
            HitAction::SessionAction {
                action: "worktrees".to_string(),
                session_id: String::new(),
            },
        ));
        self.pill_at(x + 108.0, yy - 14.0, 78.0, 21.0, "wt search", C_TEAL_CSS);
        self.hit_zones.push(HitZone::new(
            x + 108.0,
            yy - 14.0,
            78.0,
            21.0,
            HitAction::SessionAction {
                action: "worktree-search".to_string(),
                session_id: String::new(),
            },
        ));
        self.pill_at(x + 196.0, yy - 14.0, 52.0, 21.0, "scan", C_PEACH_CSS);
        self.hit_zones.push(HitZone::new(
            x + 196.0,
            yy - 14.0,
            52.0,
            21.0,
            HitAction::SessionAction {
                action: "worktrees-scan".to_string(),
                session_id: String::new(),
            },
        ));
        self.pill_at(x + 258.0, yy - 14.0, 62.0, 21.0, "cached", C_BLUE_CSS);
        self.hit_zones.push(HitZone::new(
            x + 258.0,
            yy - 14.0,
            62.0,
            21.0,
            HitAction::SessionAction {
                action: "worktrees-cache".to_string(),
                session_id: String::new(),
            },
        ));
        yy += 28.0;
        self.section_title(x, yy, "Latest session");
        yy += 18.0;
        self.round_rect(
            x + 12.0,
            yy - 7.0,
            panel_w - 24.0,
            86.0,
            4.0,
            "rgba(17,17,27,0.88)",
            "rgba(49,50,68,0.86)",
        );
        self.text(
            &truncate(
                &nonempty(&sessions.latest_task, "No cached sessions yet"),
                56,
            ),
            x + 20.0,
            yy + 11.0,
            10.0,
            C_TEXT_CSS,
            "normal",
        );
        self.text(
            &truncate(
                &format!(
                    "{} · {}",
                    nonempty(&sessions.latest_source, "source"),
                    nonempty(&sessions.latest_updated, "updated --")
                ),
                58,
            ),
            x + 20.0,
            yy + 34.0,
            10.0,
            C_SUBTEXT0_CSS,
            "normal",
        );
        yy += 78.0;
        self.section_title_color(x, yy, "Recent sessions", C_MAUVE_CSS);
        yy += 18.0;
        yy = self.session_detail_rows(
            x,
            yy,
            panel_w,
            &sessions.recent,
            "No cached sessions yet",
            5,
        );
        yy += 20.0;
        self.section_title_color(x, yy, "Worktree watchlist", C_BLUE_CSS);
        yy += 18.0;
        self.session_detail_rows(
            x,
            yy,
            panel_w,
            &sessions.recent_worktrees,
            "No worktree scan yet",
            4,
        );
    }

    fn draw_worktrees_info(&mut self, x: f32, y: f32, panel_w: f32) {
        let sessions = self.snapshot.sessions.clone();
        self.text("worktrees", x + 12.0, y + 25.0, 10.0, C_BLUE_CSS, "bold");
        self.text(
            &format!("{} scanned", sessions.worktrees),
            x + 92.0,
            y + 25.0,
            13.0,
            C_TEXT_CSS,
            "bold",
        );
        let mut yy = y + 58.0 - self.panel_scroll;
        self.panel_row(
            x,
            yy,
            "cleanup",
            &format!("{} candidates", sessions.worktree_cleanup),
        );
        yy += 22.0;
        self.panel_row_color(
            x,
            yy,
            "dirty",
            &format!("{} worktrees", sessions.worktree_dirty),
            if sessions.worktree_dirty > 0 {
                C_YELLOW_CSS
            } else {
                C_TEXT_CSS
            },
        );
        yy += 22.0;
        self.panel_row_color(
            x,
            yy,
            "unmerged",
            &format!("{} worktrees", sessions.worktree_unmerged),
            if sessions.worktree_unmerged > 0 {
                C_RED_CSS
            } else {
                C_TEXT_CSS
            },
        );
        yy += 22.0;
        self.panel_row(x, yy, "active", &sessions.worktree_active.to_string());
        yy += 22.0;
        self.panel_row(x, yy, "disk", &format_bytes(sessions.worktree_bytes));
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "scan",
            &nonempty(&sessions.worktree_scan_status, "cold"),
        );
        yy += 30.0;
        self.section_title_color(x, yy, "Actions", C_BLUE_CSS);
        yy += 22.0;
        self.pill_at(x + 14.0, yy - 14.0, 78.0, 21.0, "search", C_TEAL_CSS);
        self.hit_zones.push(HitZone::new(
            x + 14.0,
            yy - 14.0,
            78.0,
            21.0,
            HitAction::SessionAction {
                action: "worktree-search".to_string(),
                session_id: String::new(),
            },
        ));
        self.pill_at(x + 104.0, yy - 14.0, 56.0, 21.0, "scan", C_PEACH_CSS);
        self.hit_zones.push(HitZone::new(
            x + 104.0,
            yy - 14.0,
            56.0,
            21.0,
            HitAction::SessionAction {
                action: "worktrees-scan".to_string(),
                session_id: String::new(),
            },
        ));
        self.pill_at(x + 172.0, yy - 14.0, 66.0, 21.0, "cached", C_BLUE_CSS);
        self.hit_zones.push(HitZone::new(
            x + 172.0,
            yy - 14.0,
            66.0,
            21.0,
            HitAction::SessionAction {
                action: "worktrees-cache".to_string(),
                session_id: String::new(),
            },
        ));
        yy += 30.0;
        self.section_title_color(x, yy, "Watchlist", C_BLUE_CSS);
        yy += 18.0;
        self.session_detail_rows(
            x,
            yy,
            panel_w,
            &sessions.recent_worktrees,
            "No worktree scan yet",
            7,
        );
    }

    fn draw_peers_info(&mut self, x: f32, y: f32, panel_w: f32) {
        let hosts = self.snapshot.hosts.clone();
        let controls = self.snapshot.controls.clone();
        let displays = self
            .display_sources
            .values()
            .map(|source| {
                (
                    source.host_id.clone(),
                    source.display_id.clone(),
                    source.label.clone(),
                    source.video.video_width() > 0,
                )
            })
            .collect::<Vec<_>>();
        self.text("peers", x + 12.0, y + 25.0, 10.0, C_PEACH_CSS, "bold");
        self.text(
            &format!("{} hosts · {} displays", hosts.len(), displays.len()),
            x + 92.0,
            y + 25.0,
            13.0,
            C_TEXT_CSS,
            "bold",
        );
        let mut yy = y + 58.0 - self.panel_scroll;
        self.panel_row(x, yy, "peers", &hosts.len().saturating_sub(1).to_string());
        yy += 22.0;
        self.panel_row(x, yy, "streams", &displays.len().to_string());
        yy += 22.0;
        let shared_view_label = if controls.shared_view_visible {
            nonempty(&controls.shared_view_target, "active")
        } else {
            "none".to_string()
        };
        self.panel_row(x, yy, "shared", &shared_view_label);
        yy += 30.0;
        self.section_title_color(x, yy, "Display controls", C_PEACH_CSS);
        yy += 22.0;
        let mut display_actions = vec![
            (
                "display-toggle".to_string(),
                if controls.display_access.starts_with("on") {
                    "revoke local".to_string()
                } else {
                    "share local".to_string()
                },
                100.0,
                C_PEACH_CSS.to_string(),
            ),
            (
                "display-list".to_string(),
                "list displays".to_string(),
                98.0,
                C_BLUE_CSS.to_string(),
            ),
            (
                "peer-status-copy".to_string(),
                "copy status".to_string(),
                94.0,
                C_TEAL_CSS.to_string(),
            ),
        ];
        if controls.shared_view_visible {
            display_actions.push((
                "shared-view-focus".to_string(),
                "focus shared".to_string(),
                104.0,
                C_GREEN_CSS.to_string(),
            ));
        }
        if controls.shared_view_can_take_input {
            display_actions.push((
                "shared-view-take-input".to_string(),
                "take input".to_string(),
                88.0,
                C_GREEN_CSS.to_string(),
            ));
        }
        if controls.shared_view_visible {
            display_actions.push((
                "shared-view-hide".to_string(),
                "hide shared".to_string(),
                94.0,
                C_YELLOW_CSS.to_string(),
            ));
        }
        yy = self.draw_controls_action_pills(x, panel_w, yy - 14.0, &display_actions);
        yy = self.draw_display_runway_lanes(x, panel_w, yy);
        self.section_title(x, yy, "Hosts");
        yy += 18.0;
        for host in hosts.iter().take(7) {
            self.panel_row_color(
                x,
                yy,
                &truncate(&host.name, 13),
                if host.connected {
                    "connected"
                } else {
                    "offline"
                },
                if host.connected {
                    C_GREEN_CSS
                } else {
                    C_RED_CSS
                },
            );
            self.hit_zones.push(HitZone::new(
                x + 12.0,
                yy - 13.0,
                panel_w - 24.0,
                18.0,
                HitAction::Select(format!("host:{}", host.id)),
            ));
            yy += 20.0;
        }
        yy += 10.0;
        self.section_title_color(x, yy, "Displays", C_PEACH_CSS);
        yy += 18.0;
        if displays.is_empty() {
            self.panel_row(x, yy, "streams", "--");
        } else {
            for (host_id, display_id, label, ready) in displays.iter().take(5) {
                self.panel_row_color(
                    x,
                    yy,
                    &truncate(&self.host_name(host_id), 13),
                    &format!(
                        "{} · {}",
                        truncate(&nonempty(label, display_id), 24),
                        if *ready { "live" } else { "linking" }
                    ),
                    if *ready { C_GREEN_CSS } else { C_YELLOW_CSS },
                );
                self.hit_zones.push(HitZone::new(
                    x + 12.0,
                    yy - 13.0,
                    panel_w - 24.0,
                    18.0,
                    HitAction::OpenDisplay {
                        host_id: host_id.clone(),
                        display_id: display_id.clone(),
                    },
                ));
                yy += 20.0;
            }
        }
    }

    fn draw_display_runway_lanes(&mut self, x: f32, panel_w: f32, y: f32) -> f32 {
        let runway = self.snapshot.display_runway.clone();
        let mut yy = y;
        self.section_title_color(x, yy, "Display runway", C_PEACH_CSS);
        yy += 18.0;
        self.panel_row(
            x,
            yy,
            "streams",
            &format!(
                "{} local · {} remote",
                runway.local_streams, runway.remote_streams
            ),
        );
        yy += 20.0;
        self.panel_row(
            x,
            yy,
            "selected",
            &format!(
                "{} :{}",
                nonempty(&runway.selected_peer_id, "none"),
                runway.selected_display_id
            ),
        );
        yy += 24.0;
        if runway.lanes.is_empty() {
            self.panel_row(x, yy, "lanes", "no display/session lanes");
            return yy + 28.0;
        }
        for lane in runway.lanes.iter().take(4) {
            yy = self.display_runway_lane_card(x, yy, panel_w, lane);
        }
        yy + 6.0
    }

    fn display_runway_lane_card(
        &mut self,
        x: f32,
        y: f32,
        panel_w: f32,
        lane: &StationDisplayRunwayLane,
    ) -> f32 {
        let color = display_lane_color_css(&lane.kind);
        let card_h = 54.0;
        self.round_rect(
            x + 12.0,
            y - 9.0,
            panel_w - 24.0,
            card_h,
            4.0,
            if lane.selected {
                "rgba(49,50,68,0.78)"
            } else {
                "rgba(17,17,27,0.76)"
            },
            color,
        );
        self.ctx.set_fill_style(&JsValue::from_str(color));
        self.ctx
            .fill_rect((x + 20.0) as f64, (y - 2.0) as f64, 3.0, 32.0);
        self.text(
            &truncate(&nonempty(&lane.title, &lane.kind), 28),
            x + 30.0,
            y + 3.0,
            9.5,
            color,
            "bold",
        );
        self.text(
            &truncate(&nonempty(&lane.meta, &lane.kind.replace('_', " ")), 31),
            x + 30.0,
            y + 19.0,
            8.5,
            C_TEXT_CSS,
            "normal",
        );
        self.text(
            &truncate(&lane.detail, 38),
            x + 30.0,
            y + 35.0,
            8.0,
            C_SUBTEXT0_CSS,
            "normal",
        );
        if !lane.id.is_empty() {
            self.hit_zones.push(HitZone::new(
                x + 12.0,
                y - 9.0,
                panel_w - 24.0,
                card_h,
                HitAction::DisplayRunwayAction {
                    action: "open".to_string(),
                    lane_id: lane.id.clone(),
                },
            ));
        }

        let actions = self.display_runway_lane_actions(lane);
        let visible = actions.into_iter().take(3).collect::<Vec<_>>();
        let total_w = visible
            .iter()
            .map(|action| (action.label.len() as f32 * 6.0).clamp(44.0, 70.0))
            .sum::<f32>()
            + visible.len().saturating_sub(1) as f32 * 6.0;
        let mut bx = x + panel_w - 20.0 - total_w;
        for action in visible {
            let button_w = (action.label.len() as f32 * 6.0).clamp(44.0, 70.0);
            self.pill_at(bx, y + 25.0, button_w, 18.0, &action.label, action.color);
            self.hit_zones
                .push(HitZone::new(bx, y + 25.0, button_w, 18.0, action.hit));
            bx += button_w + 6.0;
        }

        y + card_h + 2.0
    }

    fn display_runway_lane_actions(&self, lane: &StationDisplayRunwayLane) -> Vec<MenuAction> {
        let action = |label: &str, op: &str, color: &'static str| {
            MenuAction::new(
                label,
                color,
                HitAction::DisplayRunwayAction {
                    action: op.to_string(),
                    lane_id: lane.id.clone(),
                },
            )
        };
        match lane.kind.as_str() {
            "operator_target" => {
                let mut actions = Vec::new();
                if !lane.session_id.is_empty() {
                    actions.push(action("session", "session", C_TEAL_CSS));
                }
                if lane.can_focus {
                    actions.push(action("focus", "focus", C_BLUE_CSS));
                }
                if lane.can_interrupt {
                    actions.push(action("stop", "stop", C_RED_CSS));
                }
                actions
            }
            "shared_view" => {
                let mut actions = vec![action("focus", "focus", C_GREEN_CSS)];
                if lane.can_take_input {
                    actions.push(action("input", "input", C_TEAL_CSS));
                }
                actions.push(action("hide", "hide", C_YELLOW_CSS));
                actions
            }
            "local_stream" => vec![
                action("video", "open", C_BLUE_CSS),
                action("input", "input", C_TEAL_CSS),
                action("attach", "attach", C_PEACH_CSS),
                action("record", "record", C_RED_CSS),
                action("full", "fullscreen", C_MAUVE_CSS),
            ],
            "remote_stream" => {
                let mut actions = vec![
                    action("focus", "focus", C_PEACH_CSS),
                    action("input", "input", C_TEAL_CSS),
                ];
                if !lane.session_id.is_empty() {
                    actions.push(action("session", "session", C_BLUE_CSS));
                }
                actions.push(action("close", "close", C_RED_CSS));
                actions
            }
            "peer_target" => vec![
                action("open", "open", C_PEACH_CSS),
                action("select", "select", C_BLUE_CSS),
            ],
            _ => vec![action("open", "open", C_OVERLAY1_CSS)],
        }
    }

    fn draw_controls_info(&mut self, x: f32, y: f32, panel_w: f32) {
        let controls = self.snapshot.controls.clone();
        self.text("control", x + 12.0, y + 25.0, 10.0, C_MAUVE_CSS, "bold");
        self.text(
            &nonempty(&controls.backend, "agent"),
            x + 92.0,
            y + 25.0,
            13.0,
            C_TEXT_CSS,
            "bold",
        );
        let mut yy = y + 58.0 - self.panel_scroll;
        self.section_title_color(x, yy, "Operator controls", C_TEAL_CSS);
        yy += 22.0;
        let prompt_label = if controls.prompt_mode == "steer" {
            "steer"
        } else {
            "send"
        };
        let mut operator_actions = vec![
            (
                "send".to_string(),
                prompt_label.to_string(),
                58.0,
                C_TEAL_CSS.to_string(),
            ),
            (
                "new-session".to_string(),
                "new session".to_string(),
                96.0,
                C_BLUE_CSS.to_string(),
            ),
        ];
        if controls.session_can_focus {
            operator_actions.push((
                "target".to_string(),
                "focus target".to_string(),
                98.0,
                C_PEACH_CSS.to_string(),
            ));
        }
        if controls.session_can_interrupt {
            operator_actions.push((
                "stop".to_string(),
                "stop".to_string(),
                54.0,
                C_RED_CSS.to_string(),
            ));
        }
        yy = self.draw_activity_action_pills(x, panel_w, yy - 14.0, &operator_actions);
        self.panel_row(
            x,
            yy,
            "draft",
            &format!(
                "{} chars · {}",
                controls.draft_chars,
                if controls.direct_mode {
                    "direct"
                } else {
                    "presence"
                }
            ),
        );
        yy += 30.0;

        self.section_title_color(x, yy, "Launch defaults", C_MAUVE_CSS);
        yy += 22.0;
        self.panel_row(x, yy, "binary", &truncate(&controls.command, 42));
        yy += 22.0;
        self.panel_row(x, yy, "model", &truncate(&controls.model, 42));
        yy += 22.0;
        self.panel_row(x, yy, "sandbox", &nonempty(&controls.sandbox, "--"));
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "approval",
            &nonempty(&controls.approval_policy, "--"),
        );
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "reasoning",
            &nonempty(&controls.reasoning_effort, "--"),
        );
        yy += 22.0;
        self.panel_row(x, yy, "service", &nonempty(&controls.service_tier, "--"));
        yy += 22.0;
        self.panel_row(x, yy, "managed", &nonempty(&controls.managed_context, "--"));
        yy += 22.0;
        self.panel_row(x, yy, "archive", &nonempty(&controls.context_archive, "--"));
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "web/net",
            &format!(
                "{} / {}",
                if controls.web_search {
                    "web on"
                } else {
                    "web off"
                },
                if controls.network_access {
                    "net on"
                } else {
                    "net off"
                }
            ),
        );
        yy += 22.0;
        self.panel_row(x, yy, "roots", &controls.writable_roots.to_string());
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "new task",
            &nonempty(&controls.new_session_agent, "--"),
        );
        yy += 30.0;
        let launch_actions = vec![(
            "start-session".to_string(),
            "start session".to_string(),
            104.0,
            C_GREEN_CSS.to_string(),
        )];
        yy = self.draw_controls_action_pills(x, panel_w, yy - 14.0, &launch_actions);

        self.section_title_color(x, yy, "Live surfaces", C_BLUE_CSS);
        yy += 22.0;
        self.panel_row(x, yy, "display", &nonempty(&controls.display_access, "--"));
        yy += 22.0;
        let shared_view_label = if controls.shared_view_visible {
            nonempty(&controls.shared_view_target, "active")
        } else {
            "none".to_string()
        };
        self.panel_row(x, yy, "shared", &shared_view_label);
        yy += 22.0;
        if controls.shared_view_visible {
            self.panel_row(
                x,
                yy,
                "shared op",
                &nonempty(&controls.shared_view_action, "visible"),
            );
            yy += 22.0;
            if !controls.shared_view_note.is_empty() {
                self.panel_row(
                    x,
                    yy,
                    "shared note",
                    &truncate(&controls.shared_view_note, 38),
                );
                yy += 22.0;
            }
        }
        self.panel_row(
            x,
            yy,
            "voice",
            &format!(
                "{}{}{}",
                nonempty(&controls.voice_state, "idle"),
                if controls.mic_active { " / mic" } else { "" },
                if controls.video_active {
                    " / video"
                } else {
                    ""
                }
            ),
        );
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "browser",
            &format!("{} workspaces", controls.browser_workspaces),
        );
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "recording",
            &truncate(
                &format!(
                    "{} streams{}{}{}",
                    controls.recordings,
                    if controls.active_recording.is_empty() {
                        ""
                    } else {
                        " · "
                    },
                    truncate(&controls.active_recording, 18),
                    if controls.debug_recording {
                        " / debug rec"
                    } else if controls.debug_screen {
                        " / debug"
                    } else {
                        ""
                    }
                ),
                46,
            ),
        );
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "attach",
            &format!("{} pending", controls.pending_attachments),
        );
        yy += 30.0;
        let mut surface_actions = vec![
            (
                "display-toggle".to_string(),
                if controls.display_access.starts_with("on") {
                    "revoke display".to_string()
                } else {
                    "share display".to_string()
                },
                104.0,
                C_BLUE_CSS.to_string(),
            ),
            (
                "debug-screen".to_string(),
                if controls.debug_screen {
                    "hide debug".to_string()
                } else {
                    "debug screen".to_string()
                },
                98.0,
                C_PEACH_CSS.to_string(),
            ),
        ];
        if !controls.active_browser {
            surface_actions.push((
                "voice-active".to_string(),
                "make active".to_string(),
                94.0,
                C_YELLOW_CSS.to_string(),
            ));
        }
        surface_actions.push((
            "voice-toggle".to_string(),
            if controls.mic_active {
                "mic off".to_string()
            } else {
                "mic".to_string()
            },
            58.0,
            C_TEAL_CSS.to_string(),
        ));
        surface_actions.push((
            "video-toggle".to_string(),
            if controls.video_active {
                "video off".to_string()
            } else {
                "video".to_string()
            },
            76.0,
            C_TEAL_CSS.to_string(),
        ));
        if controls.debug_screen {
            surface_actions.push((
                "debug-record".to_string(),
                if controls.debug_recording {
                    "stop rec".to_string()
                } else {
                    "debug rec".to_string()
                },
                82.0,
                C_RED_CSS.to_string(),
            ));
        }
        if controls.pending_attachments > 0 {
            surface_actions.push((
                "attachments-clear".to_string(),
                "clear attach".to_string(),
                98.0,
                C_RED_CSS.to_string(),
            ));
        }
        if controls.shared_view_can_take_input {
            surface_actions.push((
                "shared-view-take-input".to_string(),
                "take input".to_string(),
                88.0,
                C_GREEN_CSS.to_string(),
            ));
        }
        if controls.shared_view_visible {
            surface_actions.push((
                "shared-view-hide".to_string(),
                "hide shared".to_string(),
                94.0,
                C_YELLOW_CSS.to_string(),
            ));
        }
        yy = self.draw_controls_action_pills(x, panel_w, yy - 14.0, &surface_actions);

        self.section_title_color(x, yy, "Computer use", C_MAUVE_CSS);
        yy += 22.0;
        self.panel_row(x, yy, "provider", &nonempty(&controls.cu_provider, "auto"));
        yy += 22.0;
        self.panel_row(x, yy, "model", &nonempty(&controls.cu_model, "default"));
        yy += 22.0;
        self.panel_row(x, yy, "backend", &nonempty(&controls.cu_backend, "auto"));
        yy += 30.0;

        self.section_title_color(x, yy, "Active target", C_PEACH_CSS);
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "target",
            &truncate(
                &nonempty(
                    &controls.session_label,
                    &nonempty(&controls.session_selection, "--"),
                ),
                42,
            ),
        );
        yy += 22.0;
        let target_state = if controls.session_detached {
            "detached"
        } else if controls.session_active {
            "active"
        } else if controls.session_id.is_empty() {
            "none"
        } else {
            "idle"
        };
        self.panel_row_color(
            x,
            yy,
            "state",
            target_state,
            if controls.session_detached {
                C_YELLOW_CSS
            } else if controls.session_active {
                C_GREEN_CSS
            } else {
                C_TEXT_CSS
            },
        );
        yy += 22.0;
        self.panel_row(x, yy, "source", &nonempty(&controls.session_source, "--"));
        yy += 22.0;
        if !controls.session_live_id.is_empty() || !controls.session_action_id.is_empty() {
            self.panel_row(
                x,
                yy,
                "window",
                &truncate(&nonempty(&controls.session_live_id, "--"), 42),
            );
            yy += 22.0;
            self.panel_row(
                x,
                yy,
                "actions",
                &truncate(&nonempty(&controls.session_action_id, "--"), 42),
            );
            yy += 22.0;
            if !controls.session_attach_id.is_empty() || !controls.session_stop_id.is_empty() {
                self.panel_row(
                    x,
                    yy,
                    "op ids",
                    &truncate(
                        &format!(
                            "attach {} · stop {}",
                            nonempty(&controls.session_attach_id, "--"),
                            nonempty(&controls.session_stop_id, "--")
                        ),
                        42,
                    ),
                );
                yy += 22.0;
            }
        }
        if !controls.session_live_phase.is_empty() {
            self.panel_row(
                x,
                yy,
                "phase",
                &nonempty(&controls.session_live_phase, "--"),
            );
            yy += 22.0;
        }
        yy = self.draw_external_turn_monitor(x, panel_w, yy, &controls);
        self.panel_row(
            x,
            yy,
            "binary",
            &truncate(&nonempty(&controls.session_command, "--"), 42),
        );
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "managed",
            &nonempty(&controls.session_managed_context, "--"),
        );
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "archive",
            &nonempty(&controls.session_context_archive, "--"),
        );
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "service",
            &nonempty(&controls.session_service_tier, "--"),
        );
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "caps",
            &format!(
                "{} / {}{}",
                if controls.session_can_steer {
                    "steer"
                } else {
                    "no steer"
                },
                if controls.session_can_interrupt {
                    "interrupt"
                } else {
                    "no interrupt"
                },
                if controls.session_is_codex {
                    " / codex"
                } else {
                    ""
                }
            ),
        );
        yy += 22.0;
        if !controls.session_goal_objective.is_empty() {
            self.panel_row_color(
                x,
                yy,
                "goal",
                &truncate(
                    &format!(
                        "{} · {}{}",
                        nonempty(&controls.session_goal_status, "active"),
                        controls.session_goal_objective,
                        if controls.session_goal_tokens.is_empty() {
                            String::new()
                        } else {
                            format!(" · {} tok", controls.session_goal_tokens)
                        }
                    ),
                    46,
                ),
                C_MAUVE_CSS,
            );
        } else {
            self.panel_row(x, yy, "goal", "--");
        }
        yy += 30.0;
        if !controls.session_id.is_empty() {
            self.section_title_color(x, yy, "Target actions", C_PEACH_CSS);
            yy += 22.0;
            let mut session_actions = Vec::new();
            if controls.session_can_focus {
                session_actions.push((
                    "focus".to_string(),
                    "focus".to_string(),
                    56.0,
                    C_TEAL_CSS.to_string(),
                ));
            }
            session_actions.push((
                "copy".to_string(),
                "copy id".to_string(),
                66.0,
                C_BLUE_CSS.to_string(),
            ));
            if controls.session_can_attach {
                session_actions.push((
                    "attach".to_string(),
                    "attach".to_string(),
                    68.0,
                    C_TEAL_CSS.to_string(),
                ));
            }
            if controls.session_can_config {
                session_actions.push((
                    "config".to_string(),
                    "launch config".to_string(),
                    112.0,
                    C_MAUVE_CSS.to_string(),
                ));
                session_actions.push((
                    "restart".to_string(),
                    "restart saved".to_string(),
                    112.0,
                    C_PEACH_CSS.to_string(),
                ));
            }
            if controls.session_can_rename {
                session_actions.push((
                    "rename".to_string(),
                    "rename".to_string(),
                    72.0,
                    C_BLUE_CSS.to_string(),
                ));
            }
            if controls.session_can_stop {
                session_actions.push((
                    "stop".to_string(),
                    "stop".to_string(),
                    54.0,
                    C_RED_CSS.to_string(),
                ));
            }
            yy = self.draw_session_action_pills(
                x,
                panel_w,
                yy - 14.0,
                &session_actions,
                &controls.session_id,
            );
        }
        self.section_title_color(x, yy, "Thread actions", C_MAUVE_CSS);
        yy += 22.0;
        let codex_target = if controls.session_source == "codex" {
            nonempty(&controls.session_action_id, &controls.session_id)
        } else {
            String::new()
        };
        let thread_actions = [
            ("new", "new", 48.0),
            ("fast", "fast", 52.0),
            ("compact", "compact", 72.0),
            ("undo", "undo", 54.0),
            ("fork", "fork", 52.0),
            ("side", "side", 52.0),
            ("review", "review", 66.0),
            ("rename", "rename", 70.0),
        ];
        yy = self.draw_thread_action_pills(x, panel_w, yy - 14.0, &thread_actions, &codex_target);
        self.section_title_color(x, yy, "Goal actions", C_MAUVE_CSS);
        yy += 22.0;
        let goal_actions = [
            ("goal-get", "status", 62.0),
            ("goal", "set goal", 72.0),
            ("goal-pause", "pause", 58.0),
            ("goal-resume", "resume", 72.0),
            ("goal-clear", "clear", 58.0),
        ];
        yy = self.draw_thread_action_pills(x, panel_w, yy - 14.0, &goal_actions, &codex_target);
        self.section_title_color(x, yy, "Setup and memory", C_MAUVE_CSS);
        yy += 22.0;
        let maintenance_actions = [
            ("init", "init AGENTS", 92.0),
            ("memory-reset", "reset memory", 104.0),
        ];
        let _ = self.draw_thread_action_pills(
            x,
            panel_w,
            yy - 14.0,
            &maintenance_actions,
            &codex_target,
        );
    }

    fn draw_external_turn_monitor(
        &mut self,
        x: f32,
        panel_w: f32,
        y: f32,
        controls: &StationControlsSummary,
    ) -> f32 {
        if controls.external_turn_state.is_empty() || controls.external_turn_state == "internal" {
            return y;
        }
        let mut yy = y + 8.0;
        let color = external_turn_color_css(&controls.external_turn_state);
        self.section_title_color(x, yy, "External turn", color);
        yy += 22.0;
        self.round_rect(
            x + 12.0,
            yy - 14.0,
            panel_w - 24.0,
            74.0,
            4.0,
            "rgba(17,17,27,0.78)",
            color,
        );
        self.ctx.set_fill_style(&JsValue::from_str(color));
        self.ctx
            .fill_rect((x + 20.0) as f64, (yy - 6.0) as f64, 3.0, 48.0);
        self.text(
            &truncate(&nonempty(&controls.external_turn_label, "external"), 24),
            x + 30.0,
            yy,
            10.0,
            color,
            "bold",
        );
        self.text(
            &truncate(&controls.external_turn_state, 18),
            x + panel_w - 116.0,
            yy,
            10.0,
            color,
            "bold",
        );
        yy += 18.0;
        self.text(
            &truncate(&controls.external_turn_detail, 48),
            x + 30.0,
            yy,
            9.0,
            C_SUBTEXT0_CSS,
            "normal",
        );
        yy += 18.0;
        self.text(
            &truncate(
                &format!(
                    "{} / {}",
                    nonempty(&controls.external_turn_session_id, "no session"),
                    nonempty(&controls.session_live_phase, "controller")
                ),
                48,
            ),
            x + 30.0,
            yy,
            8.5,
            C_OVERLAY1_CSS,
            "normal",
        );
        yy += 31.0;

        let session_id = nonempty(&controls.external_turn_session_id, &controls.session_id);
        let mut actions = Vec::new();
        if controls.session_can_focus {
            actions.push((
                "focus".to_string(),
                "focus".to_string(),
                56.0,
                C_TEAL_CSS.to_string(),
            ));
        }
        if controls.session_can_attach {
            actions.push((
                "attach".to_string(),
                "attach".to_string(),
                68.0,
                C_PEACH_CSS.to_string(),
            ));
        }
        if controls.session_can_interrupt || controls.session_can_stop {
            actions.push((
                "stop".to_string(),
                "stop".to_string(),
                54.0,
                C_RED_CSS.to_string(),
            ));
        }
        if controls.session_can_config {
            actions.push((
                "config".to_string(),
                "config".to_string(),
                64.0,
                C_MAUVE_CSS.to_string(),
            ));
        }
        if !actions.is_empty() && !session_id.is_empty() {
            yy = self.draw_session_action_pills(x, panel_w, yy - 14.0, &actions, &session_id);
        }
        yy + 8.0
    }

    fn draw_view_info(&mut self, x: f32, y: f32, _panel_w: f32) {
        self.text("view", x + 12.0, y + 25.0, 10.0, C_MAUVE_CSS, "bold");
        self.text(
            self.layout.label(),
            x + 92.0,
            y + 25.0,
            13.0,
            C_TEXT_CSS,
            "bold",
        );
        let mut yy = y + 82.0 - self.panel_scroll;
        self.panel_row(x, yy, "layout", self.layout.label());
        yy += 22.0;
        self.panel_row(x, yy, "mood", self.mood.label());
        yy += 22.0;
        self.panel_row(
            x,
            yy,
            "fov",
            &format!("{} deg", self.fov_deg.round() as i32),
        );
        yy += 22.0;
        self.panel_row(x, yy, "motion", &format!("{:.1}", self.motion));
        yy += 22.0;
        self.panel_row(x, yy, "ar", &format!("{:.1}", self.ar_strength));
        yy += 22.0;
        self.panel_row(x, yy, "density", &format!("{:.1}", self.density));
        yy += 30.0;
        self.section_title_color(x, yy, "Canvas controls", C_MAUVE_CSS);
        yy += 22.0;
        self.panel_row(x, yy, "toolbar", "layout buttons");
        yy += 22.0;
        self.panel_row(x, yy, "tweaks", "mood and sliders");
        yy += 22.0;
        self.panel_row(x, yy, "dock", "Station View");
    }

    fn draw_thread_action_pills(
        &mut self,
        x: f32,
        panel_w: f32,
        y: f32,
        actions: &[(&str, &str, f32)],
        session_id: &str,
    ) -> f32 {
        let mut ax = x + 14.0;
        let mut ay = y;
        for (op, label, width) in actions.iter().copied() {
            if ax + width > x + panel_w - 14.0 {
                ax = x + 14.0;
                ay += 25.0;
            }
            self.pill_at(ax, ay, width, 21.0, label, C_MAUVE_CSS);
            self.hit_zones.push(HitZone::new(
                ax,
                ay,
                width,
                21.0,
                HitAction::ThreadAction {
                    op: op.to_string(),
                    session_id: session_id.to_string(),
                },
            ));
            ax += width + 8.0;
        }
        ay + 35.0
    }

    fn draw_activity_action_pills(
        &mut self,
        x: f32,
        panel_w: f32,
        y: f32,
        actions: &[(String, String, f32, String)],
    ) -> f32 {
        let mut ax = x + 14.0;
        let mut ay = y;
        for (action, label, width, color) in actions {
            if ax + *width > x + panel_w - 14.0 {
                ax = x + 14.0;
                ay += 25.0;
            }
            self.pill_at(ax, ay, *width, 21.0, label, color);
            self.hit_zones.push(HitZone::new(
                ax,
                ay,
                *width,
                21.0,
                HitAction::ActivityAction {
                    action: action.clone(),
                    id: String::new(),
                },
            ));
            ax += *width + 8.0;
        }
        ay + 35.0
    }

    fn draw_controls_action_pills(
        &mut self,
        x: f32,
        panel_w: f32,
        y: f32,
        actions: &[(String, String, f32, String)],
    ) -> f32 {
        let mut ax = x + 14.0;
        let mut ay = y;
        for (action, label, width, color) in actions {
            if ax + *width > x + panel_w - 14.0 {
                ax = x + 14.0;
                ay += 25.0;
            }
            self.pill_at(ax, ay, *width, 21.0, label, color);
            self.hit_zones.push(HitZone::new(
                ax,
                ay,
                *width,
                21.0,
                HitAction::ControlsAction {
                    action: action.clone(),
                },
            ));
            ax += *width + 8.0;
        }
        ay + 35.0
    }

    fn draw_session_action_pills(
        &mut self,
        x: f32,
        panel_w: f32,
        y: f32,
        actions: &[(String, String, f32, String)],
        session_id: &str,
    ) -> f32 {
        let mut ax = x + 14.0;
        let mut ay = y;
        for (action, label, width, color) in actions {
            if ax + *width > x + panel_w - 14.0 {
                ax = x + 14.0;
                ay += 25.0;
            }
            self.pill_at(ax, ay, *width, 21.0, label, color);
            self.hit_zones.push(HitZone::new(
                ax,
                ay,
                *width,
                21.0,
                HitAction::SessionAction {
                    action: action.clone(),
                    session_id: session_id.to_string(),
                },
            ));
            ax += *width + 8.0;
        }
        ay + 35.0
    }

    fn panel_row(&self, x: f32, y: f32, k: &str, v: &str) {
        self.panel_row_color(x, y, k, v, C_TEXT_CSS);
    }

    fn panel_row_color(&self, x: f32, y: f32, k: &str, v: &str, color: &str) {
        self.text(k, x + 14.0, y, 10.0, C_OVERLAY1_CSS, "bold");
        self.text(&truncate(v, 46), x + 94.0, y, 11.0, color, "normal");
    }

    fn session_detail_rows(
        &mut self,
        x: f32,
        y: f32,
        panel_w: f32,
        rows: &[StationDetailRow],
        empty: &str,
        max_rows: usize,
    ) -> f32 {
        let mut yy = y;
        if rows.is_empty() {
            self.panel_row(x, yy, "items", empty);
            return yy + 20.0;
        }
        for row in rows.iter().take(max_rows) {
            let has_goal = !row.goal_status.is_empty() || !row.goal_objective.is_empty();
            let show_thread_controls = row.is_codex || has_goal;
            let card_h = if show_thread_controls { 68.0 } else { 43.0 };
            self.round_rect(
                x + 12.0,
                yy - 11.0,
                panel_w - 24.0,
                card_h,
                4.0,
                "rgba(17,17,27,0.76)",
                "rgba(49,50,68,0.72)",
            );
            if !row.id.is_empty() {
                let action = if row.action.is_empty() {
                    HitAction::Noop
                } else {
                    HitAction::SessionAction {
                        action: row.action.clone(),
                        session_id: row.id.clone(),
                    }
                };
                self.hit_zones.push(HitZone::new(
                    x + 12.0,
                    yy - 11.0,
                    panel_w - 24.0,
                    card_h,
                    action,
                ));
            }
            self.text(
                &truncate(&row.label, 28),
                x + 20.0,
                yy + 1.0,
                9.5,
                tone_color_css(&row.tone),
                "bold",
            );
            self.text(
                &truncate(&row.value, 20),
                x + panel_w - 126.0,
                yy + 1.0,
                9.5,
                C_TEXT_CSS,
                "normal",
            );
            self.text(
                &truncate(&row.detail, 36),
                x + 20.0,
                yy + 18.0,
                9.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
            if show_thread_controls {
                let goal_status = nonempty(&row.goal_status, "goal");
                let goal_detail = if row.goal_objective.is_empty() {
                    if row.is_codex {
                        "goal controls available".to_string()
                    } else {
                        "goal --".to_string()
                    }
                } else {
                    format!("{} · {}", goal_status, row.goal_objective)
                };
                let goal_tokens = if row.goal_tokens.is_empty() {
                    String::new()
                } else if row.goal_token_budget.is_empty() {
                    format!(" · {} tok", row.goal_tokens)
                } else {
                    format!(" · {}/{} tok", row.goal_tokens, row.goal_token_budget)
                };
                self.text(
                    &truncate(&format!("{goal_detail}{goal_tokens}"), 28),
                    x + 20.0,
                    yy + 35.0,
                    8.5,
                    if row.goal_status == "active" {
                        C_GREEN_CSS
                    } else if row.goal_status == "paused" {
                        C_YELLOW_CSS
                    } else {
                        C_MAUVE_CSS
                    },
                    "normal",
                );
                let session_id = nonempty(&row.thread_action_session_id, &row.id);
                if !session_id.is_empty() {
                    let middle = if row.goal_status == "paused" {
                        ("goal-resume", "resume", 58.0, C_GREEN_CSS)
                    } else {
                        ("goal-pause", "pause", 50.0, C_YELLOW_CSS)
                    };
                    let goal_buttons = [
                        ("goal-get", "status", 54.0, C_MAUVE_CSS),
                        middle,
                        ("goal-clear", "clear", 48.0, C_RED_CSS),
                    ];
                    let total_w = goal_buttons
                        .iter()
                        .map(|(_, _, width, _)| *width)
                        .sum::<f32>()
                        + (goal_buttons.len().saturating_sub(1) as f32 * 6.0);
                    let mut bx = x + panel_w - total_w - 28.0;
                    for (op, label, width, color) in goal_buttons {
                        self.pill_at(bx, yy + 39.0, width, 18.0, label, color);
                        self.hit_zones.push(HitZone::new(
                            bx,
                            yy + 39.0,
                            width,
                            18.0,
                            HitAction::ThreadAction {
                                op: op.to_string(),
                                session_id: session_id.clone(),
                            },
                        ));
                        bx += width + 6.0;
                    }
                }
            }
            let mut buttons = Vec::new();
            if row.can_attach && !row.id.is_empty() {
                buttons.push(("attach", "attach", 58.0, C_TEAL_CSS));
            }
            if row.can_resume && !row.id.is_empty() {
                buttons.push(("resume", "resume", 58.0, C_TEAL_CSS));
            }
            if row.can_config && !row.id.is_empty() {
                buttons.push(("config", "config", 58.0, C_MAUVE_CSS));
            }
            if row.can_stop && !row.id.is_empty() {
                buttons.push(("stop", "stop", 46.0, C_RED_CSS));
            }
            if row.can_rename && !row.id.is_empty() && buttons.len() < 3 {
                buttons.push(("rename", "rename", 58.0, C_BLUE_CSS));
            }
            let visible_buttons = buttons.into_iter().take(3).collect::<Vec<_>>();
            if !visible_buttons.is_empty() {
                let total_w: f32 = visible_buttons
                    .iter()
                    .map(|(_, _, width, _)| *width)
                    .sum::<f32>()
                    + (visible_buttons.len().saturating_sub(1) as f32 * 6.0);
                let mut bx = x + panel_w - total_w - 28.0;
                for (action, label, width, color) in visible_buttons {
                    self.pill_at(bx, yy + 15.0, width, 19.0, label, color);
                    self.hit_zones.push(HitZone::new(
                        bx,
                        yy + 15.0,
                        width,
                        19.0,
                        HitAction::SessionAction {
                            action: action.to_string(),
                            session_id: row.id.clone(),
                        },
                    ));
                    bx += width + 6.0;
                }
            }
            yy += card_h + 4.0;
        }
        yy
    }

    fn managed_detail_rows(
        &mut self,
        x: f32,
        y: f32,
        panel_w: f32,
        rows: &[StationDetailRow],
        empty: &str,
        max_rows: usize,
    ) -> f32 {
        let mut yy = y;
        if rows.is_empty() {
            self.panel_row(x, yy, "items", empty);
            return yy + 20.0;
        }
        for row in rows.iter().take(max_rows) {
            self.round_rect(
                x + 12.0,
                yy - 11.0,
                panel_w - 24.0,
                43.0,
                4.0,
                "rgba(17,17,27,0.76)",
                "rgba(49,50,68,0.72)",
            );
            if !row.id.is_empty() && !row.action.is_empty() {
                self.hit_zones.push(HitZone::new(
                    x + 12.0,
                    yy - 11.0,
                    panel_w - 24.0,
                    43.0,
                    HitAction::ManagedAction {
                        action: row.action.clone(),
                        id: row.id.clone(),
                        session_id: row.session_id.clone(),
                    },
                ));
            }
            self.text(
                &truncate(&row.label, 28),
                x + 20.0,
                yy + 1.0,
                9.5,
                tone_color_css(&row.tone),
                "bold",
            );
            self.text(
                &truncate(&row.value, 20),
                x + panel_w - 126.0,
                yy + 1.0,
                9.5,
                C_TEXT_CSS,
                "normal",
            );
            self.text(
                &truncate(&row.detail, 36),
                x + 20.0,
                yy + 18.0,
                9.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
            let buttons = managed_row_buttons(row);
            if !buttons.is_empty() && !row.id.is_empty() {
                let total_w = buttons.iter().map(|(_, _, w)| *w).sum::<f32>()
                    + (buttons.len().saturating_sub(1) as f32 * 5.0);
                let mut bx = x + panel_w - total_w - 28.0;
                for (label, action, button_w) in buttons {
                    self.pill_at(bx, yy + 15.0, button_w, 19.0, label, C_MAUVE_CSS);
                    self.hit_zones.push(HitZone::new(
                        bx,
                        yy + 15.0,
                        button_w,
                        19.0,
                        HitAction::ManagedAction {
                            action: action.to_string(),
                            id: row.id.clone(),
                            session_id: row.session_id.clone(),
                        },
                    ));
                    bx += button_w + 5.0;
                }
            }
            yy += 47.0;
        }
        yy
    }

    fn context_detail_rows(
        &mut self,
        x: f32,
        y: f32,
        panel_w: f32,
        rows: &[StationDetailRow],
        empty: &str,
        max_rows: usize,
    ) -> f32 {
        let mut yy = y;
        if rows.is_empty() {
            self.panel_row(x, yy, "items", empty);
            return yy + 20.0;
        }
        for row in rows.iter().take(max_rows) {
            self.round_rect(
                x + 12.0,
                yy - 11.0,
                panel_w - 24.0,
                43.0,
                4.0,
                "rgba(17,17,27,0.76)",
                "rgba(49,50,68,0.72)",
            );
            if !row.id.is_empty() && !row.action.is_empty() {
                self.hit_zones.push(HitZone::new(
                    x + 12.0,
                    yy - 11.0,
                    panel_w - 24.0,
                    43.0,
                    HitAction::ContextAction {
                        action: row.action.clone(),
                        id: row.id.clone(),
                    },
                ));
            }
            self.text(
                &truncate(&row.label, 28),
                x + 20.0,
                yy + 1.0,
                9.5,
                tone_color_css(&row.tone),
                "bold",
            );
            self.text(
                &truncate(&row.value, 20),
                x + panel_w - 126.0,
                yy + 1.0,
                9.5,
                C_TEXT_CSS,
                "normal",
            );
            self.text(
                &truncate(&row.detail, 36),
                x + 20.0,
                yy + 18.0,
                9.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
            let buttons = context_row_buttons(row);
            if !buttons.is_empty() {
                let total_w = buttons.iter().map(|(_, _, w, _)| *w).sum::<f32>()
                    + buttons.len().saturating_sub(1) as f32 * 6.0;
                let mut bx = x + panel_w - total_w - 28.0;
                for (label, action, width, color) in buttons {
                    self.pill_at(bx, yy + 15.0, width, 19.0, label, color);
                    self.hit_zones.push(HitZone::new(
                        bx,
                        yy + 15.0,
                        width,
                        19.0,
                        HitAction::ContextAction {
                            action: action.to_string(),
                            id: row.id.clone(),
                        },
                    ));
                    bx += width + 6.0;
                }
            }
            yy += 47.0;
        }
        yy
    }

    fn changes_detail_rows(
        &mut self,
        x: f32,
        y: f32,
        panel_w: f32,
        rows: &[StationDetailRow],
        empty: &str,
        max_rows: usize,
    ) -> f32 {
        let mut yy = y;
        if rows.is_empty() {
            self.panel_row(x, yy, "items", empty);
            return yy + 20.0;
        }
        for row in rows.iter().take(max_rows) {
            self.round_rect(
                x + 12.0,
                yy - 11.0,
                panel_w - 24.0,
                43.0,
                4.0,
                "rgba(17,17,27,0.76)",
                "rgba(49,50,68,0.72)",
            );
            if !row.id.is_empty() && !row.action.is_empty() {
                self.hit_zones.push(HitZone::new(
                    x + 12.0,
                    yy - 11.0,
                    panel_w - 24.0,
                    43.0,
                    HitAction::ChangesAction {
                        action: row.action.clone(),
                        path: row.id.clone(),
                    },
                ));
            }
            self.text(
                &truncate(&row.label, 28),
                x + 20.0,
                yy + 1.0,
                9.5,
                tone_color_css(&row.tone),
                "bold",
            );
            self.text(
                &truncate(&row.value, 20),
                x + panel_w - 126.0,
                yy + 1.0,
                9.5,
                C_TEXT_CSS,
                "normal",
            );
            self.text(
                &truncate(&row.detail, 36),
                x + 20.0,
                yy + 18.0,
                9.0,
                C_SUBTEXT0_CSS,
                "normal",
            );
            if !row.id.is_empty() && row.action == "file" {
                self.pill_at(
                    x + panel_w - 78.0,
                    yy + 15.0,
                    50.0,
                    19.0,
                    "open",
                    C_YELLOW_CSS,
                );
                self.hit_zones.push(HitZone::new(
                    x + panel_w - 78.0,
                    yy + 15.0,
                    50.0,
                    19.0,
                    HitAction::ChangesAction {
                        action: row.action.clone(),
                        path: row.id.clone(),
                    },
                ));
            }
            yy += 47.0;
        }
        yy
    }

    fn section_title(&self, x: f32, y: f32, title: &str) {
        self.section_title_color(x, y, title, C_BLUE_CSS);
    }

    fn section_title_color(&self, x: f32, y: f32, title: &str, color: &str) {
        self.text(title, x + 14.0, y, 10.0, color, "bold");
    }

    fn meter(&self, x: f32, y: f32, w: f32, pct: f32, color: &str) {
        let pct = pct.clamp(0.0, 1.0);
        self.ctx
            .set_fill_style(&JsValue::from_str("rgba(49,50,68,0.92)"));
        self.ctx
            .fill_rect(x as f64, (y - 6.0) as f64, w as f64, 5.0);
        self.ctx.set_fill_style(&JsValue::from_str(color));
        self.ctx
            .fill_rect(x as f64, (y - 6.0) as f64, (w * pct) as f64, 5.0);
        self.ctx
            .set_stroke_style(&JsValue::from_str("rgba(127,132,156,0.5)"));
        self.ctx
            .stroke_rect(x as f64, (y - 6.0) as f64, w as f64, 5.0);
    }

    fn approval_button(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        label: &str,
        host_id: &str,
        approval_id: &str,
        decision: &str,
        color: &str,
    ) {
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

    fn slider(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        label: &str,
        value: f32,
        min: f32,
        max: f32,
        kind: SliderKind,
    ) {
        self.text(label, x, y, 10.0, C_OVERLAY1_CSS, "bold");
        let track_x = x + 66.0;
        let pct = ((value - min) / (max - min)).clamp(0.0, 1.0);
        self.ctx
            .set_fill_style(&JsValue::from_str("rgba(49,50,68,0.92)"));
        self.ctx
            .fill_rect(track_x as f64, (y - 6.0) as f64, (w - 82.0) as f64, 5.0);
        self.ctx.set_fill_style(&JsValue::from_str(C_BLUE_CSS));
        self.ctx.fill_rect(
            track_x as f64,
            (y - 6.0) as f64,
            ((w - 82.0) * pct) as f64,
            5.0,
        );
        self.ctx.begin_path();
        let _ = self.ctx.arc(
            (track_x + (w - 82.0) * pct) as f64,
            (y - 3.5) as f64,
            5.0,
            0.0,
            std::f64::consts::TAU,
        );
        self.ctx.fill();
        self.text(
            &format!("{value:.1}"),
            x + w - 26.0,
            y,
            9.0,
            C_SUBTEXT0_CSS,
            "normal",
        );
        self.hit_zones.push(HitZone::new(
            track_x - 6.0,
            y - 16.0,
            w - 70.0,
            24.0,
            HitAction::Slider(kind),
        ));
    }

    fn pill_button(
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
        );
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
        let _ = ctx.quadratic_curve_to(
            (x + w) as f64,
            (y + h) as f64,
            (x + w - r) as f64,
            (y + h) as f64,
        );
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
        self.ctx.set_font(&format!(
            "{weight} {px}px 'SF Mono', Menlo, Consolas, monospace"
        ));
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
        for agent in &self.snapshot.agents {
            by_host
                .entry(agent.host_id.as_str())
                .or_default()
                .push(agent);
        }
        for host in &self.snapshot.hosts {
            let host_pos = map
                .get(&format!("host:{}", host.id))
                .copied()
                .unwrap_or(Vec3::ZERO);
            let agents = by_host.get(host.id.as_str()).cloned().unwrap_or_default();
            let count = agents.len().max(1);
            for (idx, agent) in agents.into_iter().enumerate() {
                let pos = match self.layout {
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

    fn camera(&self) -> Camera {
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

    fn dispatch_hit(&mut self, action: HitAction, x: f32, _y: f32) -> Option<serde_json::Value> {
        match action {
            HitAction::Layout(layout) => {
                self.layout = layout;
                self.last_render_ms = 0.0;
                None
            }
            HitAction::Mood(mood) => {
                self.mood = mood;
                self.last_render_ms = 0.0;
                None
            }
            HitAction::ClosePanel => {
                self.selected_id = None;
                self.last_render_ms = 0.0;
                None
            }
            HitAction::Select(id) => {
                self.selected_id = Some(id);
                self.panel_scroll = 0.0;
                self.last_render_ms = 0.0;
                None
            }
            HitAction::Slider(kind) => {
                self.apply_slider_at(kind, x);
                None
            }
            HitAction::Noop => None,
            HitAction::Approval {
                host_id,
                approval_id,
                decision,
            } => Some(serde_json::json!({
                    "type": "approval",
                    "host_id": host_id,
                    "approval_id": approval_id,
                    "decision": decision,
            })),
            HitAction::OpenDisplay {
                host_id,
                display_id,
            } => Some(serde_json::json!({
                    "type": "open_display",
                    "host_id": host_id,
                    "display_id": display_id,
            })),
            HitAction::DisplayRunwayAction { action, lane_id } => Some(serde_json::json!({
                    "type": "display_runway_action",
                    "action": action,
                    "lane_id": lane_id,
            })),
            HitAction::ContextAction { action, id } => Some(serde_json::json!({
                    "type": "context_action",
                    "action": action,
                    "id": id,
            })),
            HitAction::ActivityAction { action, id } if action == "log" && !id.is_empty() => {
                self.selected_id = Some(format!("activity:{id}"));
                self.panel_scroll = 0.0;
                self.last_render_ms = 0.0;
                None
            }
            HitAction::ActivityAction { action, id } => Some(serde_json::json!({
                    "type": "activity_action",
                    "action": action,
                    "id": id,
            })),
            HitAction::ControlsAction { action } => Some(serde_json::json!({
                    "type": "controls_action",
                    "action": action,
            })),
            HitAction::ManagedAction {
                action,
                id,
                session_id,
            } => Some(serde_json::json!({
                    "type": "managed_action",
                    "action": action,
                    "id": id,
                    "session_id": session_id,
            })),
            HitAction::ChangesAction { action, path } => Some(serde_json::json!({
                    "type": "changes_action",
                    "action": action,
                    "path": path,
            })),
            HitAction::SessionAction { action, session_id } => Some(serde_json::json!({
                    "type": "session_action",
                    "action": action,
                    "session_id": session_id,
            })),
            HitAction::ThreadAction { op, session_id } => Some(serde_json::json!({
                    "type": "thread_action",
                    "op": op,
                    "session_id": session_id,
            })),
        }
    }

    fn emit_action(callback: Option<js_sys::Function>, action: serde_json::Value) {
        if let Some(cb) = callback {
            if let Ok(value) = action
                .serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
            {
                let callback = Closure::once_into_js(move || {
                    let _ = cb.call1(&JsValue::NULL, &value);
                });
                if let Some(window) = web_sys::window() {
                    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                        callback.as_ref().unchecked_ref(),
                        0,
                    );
                }
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
        self.last_render_ms = 0.0;
    }

    fn first_two_pointer_positions(&self) -> Option<(Vec2, Vec2)> {
        let mut iter = self.active_pointers.values().copied();
        Some((iter.next()?, iter.next()?))
    }

    fn begin_pinch(&mut self) {
        let Some((a, b)) = self.first_two_pointer_positions() else {
            return;
        };
        let dist = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt().max(1.0);
        self.pinch_zoom = Some(PinchZoom {
            start_distance: dist,
            start_camera_distance: self.distance,
        });
    }

    fn apply_pinch(&mut self) {
        let Some((a, b)) = self.first_two_pointer_positions() else {
            return;
        };
        if self.pinch_zoom.is_none() {
            self.begin_pinch();
        }
        let Some(pinch) = self.pinch_zoom else {
            return;
        };
        let dist = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt().max(1.0);
        let scale = (pinch.start_distance / dist).clamp(0.25, 4.0);
        self.distance = (pinch.start_camera_distance * scale).clamp(4.2, 25.0);
        self.last_render_ms = 0.0;
    }

    fn set_cursor(&self, cursor: &str) {
        if cursor == "grab" {
            let _ = self.hud_canvas.remove_attribute("data-station-cursor");
        } else {
            let _ = self.hud_canvas.set_attribute("data-station-cursor", cursor);
        }
    }

    fn hit_action_at(&self, x: f32, y: f32) -> Option<HitAction> {
        self.hit_zones
            .iter()
            .rev()
            .find(|z| x >= z.x && x <= z.x + z.w && y >= z.y && y <= z.y + z.h)
            .map(|z| z.action.clone())
    }

    fn info_panel_hit(&self, x: f32, y: f32) -> bool {
        if self.selected_id.is_none() {
            return false;
        }
        let w = self.css_width();
        let h = self.css_height();
        let panel_w = 350.0_f32.min(w - 28.0).max(280.0);
        let panel_x = w - panel_w - 14.0;
        let panel_y = 52.0;
        let panel_h = (h - 76.0).min(560.0);
        x >= panel_x && x <= panel_x + panel_w && y >= panel_y && y <= panel_y + panel_h
    }

    fn event_xy(&self, client_x: f64, client_y: f64) -> (f32, f32) {
        let rect = self.hud_canvas.get_bounding_client_rect();
        (
            (client_x - rect.left()) as f32,
            (client_y - rect.top()) as f32,
        )
    }

    fn mark_input(&mut self) {
        self.last_input_ms = now_ms();
        self.last_render_ms = 0.0;
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

#[cfg(target_arch = "wasm32")]
struct GpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    line_pipeline: wgpu::RenderPipeline,
    tri_pipeline: wgpu::RenderPipeline,
}

#[cfg(target_arch = "wasm32")]
impl GpuState {
    async fn new(canvas: HtmlCanvasElement) -> Result<Self, JsValue> {
        let width = canvas.width().max(1);
        let height = canvas.height().max(1);
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        instance_desc.backends = wgpu::Backends::BROWSER_WEBGPU;
        let instance = wgpu::Instance::new(instance_desc);
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
            .map_err(|e| JsValue::from_str(&format!("no WebGPU adapter available: {e:?}")))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("Intendant Station Device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_webgl2_defaults(),
                ..Default::default()
            })
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
            alpha_mode: caps
                .alpha_modes
                .first()
                .copied()
                .unwrap_or(wgpu::CompositeAlphaMode::Auto),
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
            immediate_size: 0,
        });
        let make_pipeline = |topology| {
            let vertex_layout = GpuVertex::layout();
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Station Render Pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[vertex_layout],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
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
                multiview_mask: None,
                cache: None,
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

    fn render(&mut self, frame: &GpuFrame) -> Result<(), JsValue> {
        let output = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output)
            | wgpu::CurrentSurfaceTexture::Suboptimal(output) => output,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(output)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(output) => output,
                    state => {
                        return Err(JsValue::from_str(&format!(
                            "surface unavailable after reconfigure: {state:?}"
                        )))
                    }
                }
            }
            state => {
                return Err(JsValue::from_str(&format!(
                    "surface unavailable: {state:?}"
                )))
            }
        };
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Station Encoder"),
            });

        let line_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Station Lines"),
                contents: bytemuck::cast_slice(&frame.line_vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
        let tri_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("Station Triangles"),
                contents: bytemuck::cast_slice(&frame.tri_vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Station Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
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
                multiview_mask: None,
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

#[cfg(not(target_arch = "wasm32"))]
struct GpuState;

#[cfg(not(target_arch = "wasm32"))]
impl GpuState {
    fn resize(&mut self, _width: u32, _height: u32) {}

    fn render(&mut self, _frame: &GpuFrame) -> Result<(), JsValue> {
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
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
    #[cfg(target_arch = "wasm32")]
    const ATTRS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4];

    #[cfg(target_arch = "wasm32")]
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
        self.line_vertices.push(GpuVertex {
            pos: [a.x, a.y],
            color: color.into(),
        });
        self.line_vertices.push(GpuVertex {
            pos: [b.x, b.y],
            color: color.into(),
        });
    }

    fn add_line_projected(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        a: Vec3,
        b: Vec3,
        color: Color,
    ) {
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

    fn add_ring(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        radius: f32,
        color: Color,
        plane: Plane,
    ) {
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

    fn add_wire_octa(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        scale: f32,
        spin: f32,
        color: Color,
    ) {
        let verts = [
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
            Vec3::new(-1.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, -1.0),
            Vec3::new(0.0, -1.0, 0.0),
        ];
        let edges = [
            (0, 1),
            (0, 2),
            (0, 3),
            (0, 4),
            (5, 1),
            (5, 2),
            (5, 3),
            (5, 4),
            (1, 2),
            (2, 3),
            (3, 4),
            (4, 1),
        ];
        self.add_edges(project, center, scale, spin, &verts, &edges, color);
    }

    fn add_wire_tetra(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        scale: f32,
        spin: f32,
        color: Color,
    ) {
        let verts = [
            Vec3::new(1.0, 1.0, 1.0),
            Vec3::new(-1.0, -1.0, 1.0),
            Vec3::new(-1.0, 1.0, -1.0),
            Vec3::new(1.0, -1.0, -1.0),
        ];
        let edges = [(0, 1), (0, 2), (0, 3), (1, 2), (2, 3), (3, 1)];
        self.add_edges(project, center, scale, spin, &verts, &edges, color);
    }

    fn add_wire_icosa(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        scale: f32,
        spin: f32,
        color: Color,
    ) {
        let phi = 1.618;
        let verts = [
            Vec3::new(-1.0, phi, 0.0),
            Vec3::new(1.0, phi, 0.0),
            Vec3::new(-1.0, -phi, 0.0),
            Vec3::new(1.0, -phi, 0.0),
            Vec3::new(0.0, -1.0, phi),
            Vec3::new(0.0, 1.0, phi),
            Vec3::new(0.0, -1.0, -phi),
            Vec3::new(0.0, 1.0, -phi),
            Vec3::new(phi, 0.0, -1.0),
            Vec3::new(phi, 0.0, 1.0),
            Vec3::new(-phi, 0.0, -1.0),
            Vec3::new(-phi, 0.0, 1.0),
        ];
        let edges = [
            (0, 1),
            (0, 5),
            (0, 7),
            (0, 10),
            (0, 11),
            (1, 5),
            (1, 7),
            (1, 8),
            (1, 9),
            (2, 3),
            (2, 4),
            (2, 6),
            (2, 10),
            (2, 11),
            (3, 4),
            (3, 6),
            (3, 8),
            (3, 9),
            (4, 5),
            (4, 9),
            (4, 11),
            (5, 9),
            (5, 11),
            (6, 7),
            (6, 8),
            (6, 10),
            (7, 8),
            (7, 10),
            (8, 9),
            (10, 11),
        ];
        self.add_edges(project, center, scale * 0.55, spin, &verts, &edges, color);
    }

    fn add_wire_hex(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        radius: f32,
        height: f32,
        spin: f32,
        color: Color,
    ) {
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
            self.add_line_projected(
                project,
                bottom[i],
                bottom[n],
                color.with_alpha(color.a * 0.7),
            );
            self.add_line_projected(project, top[i], bottom[i], color.with_alpha(color.a * 0.6));
        }
    }

    fn add_edges(
        &mut self,
        project: &mut impl FnMut(Vec3) -> Option<Vec2>,
        center: Vec3,
        scale: f32,
        spin: f32,
        verts: &[Vec3],
        edges: &[(usize, usize)],
        color: Color,
    ) {
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
    context: StationContextSummary,
    managed: StationManagedSummary,
    changes: StationChangesSummary,
    sessions: StationSessionsSummary,
    controls: StationControlsSummary,
    attention_queue: StationAttentionQueueSummary,
    display_runway: StationDisplayRunwaySummary,
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
    action: String,
    host_id: String,
    session_id: String,
    agent_id: Option<String>,
    ts: String,
    level: String,
    source: String,
    msg: String,
    editable: bool,
    historical: bool,
}

impl Default for StationEvent {
    fn default() -> Self {
        Self {
            id: "event".into(),
            action: String::new(),
            host_id: "local".into(),
            session_id: String::new(),
            agent_id: None,
            ts: String::new(),
            level: "info".into(),
            source: String::new(),
            msg: String::new(),
            editable: false,
            historical: false,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationContextSummary {
    available: bool,
    label: String,
    source: String,
    session_id: String,
    format: String,
    turn: String,
    tokens: f32,
    effective_window: f32,
    hard_window: f32,
    item_count: u32,
    category_count: u32,
    replay_mode: String,
    replay_count: u32,
    replay_index: u32,
    replay_time: String,
    exact_status: String,
    top_categories: Vec<StationBreakdown>,
    top_items: Vec<StationDetailRow>,
}

impl Default for StationContextSummary {
    fn default() -> Self {
        Self {
            available: false,
            label: String::new(),
            source: String::new(),
            session_id: String::new(),
            format: String::new(),
            turn: String::new(),
            tokens: 0.0,
            effective_window: 0.0,
            hard_window: 0.0,
            item_count: 0,
            category_count: 0,
            replay_mode: "live".into(),
            replay_count: 0,
            replay_index: 0,
            replay_time: String::new(),
            exact_status: "none".into(),
            top_categories: Vec::new(),
            top_items: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationManagedSummary {
    session_id: String,
    mode: String,
    status: String,
    used_tokens: f32,
    effective_window: f32,
    hard_window: f32,
    rewind_only: bool,
    records: u32,
    anchors: u32,
    lineage_groups: u32,
    fission_groups: u32,
    branches: u32,
    error: String,
    action_state: StationManagedActionState,
    activity_signal: StationDetailRow,
    recent_records: Vec<StationDetailRow>,
    recent_anchors: Vec<StationDetailRow>,
    recent_branches: Vec<StationDetailRow>,
}

impl Default for StationManagedSummary {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            mode: "unknown".into(),
            status: "unknown".into(),
            used_tokens: 0.0,
            effective_window: 0.0,
            hard_window: 0.0,
            rewind_only: false,
            records: 0,
            anchors: 0,
            lineage_groups: 0,
            fission_groups: 0,
            branches: 0,
            error: String::new(),
            action_state: StationManagedActionState::default(),
            activity_signal: StationDetailRow::default(),
            recent_records: Vec::new(),
            recent_anchors: Vec::new(),
            recent_branches: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct StationManagedActionState {
    anchor: String,
    record: String,
    position: String,
    backout_mode: String,
    readiness: String,
    result: String,
    has_reason: bool,
    has_primer: bool,
    can_inspect: bool,
    can_rewind: bool,
    can_backout: bool,
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationChangesSummary {
    status: String,
    count: u32,
    added: u32,
    modified: u32,
    deleted: u32,
    external: u32,
    total_added: u32,
    total_removed: u32,
    latest_path: String,
    latest_kind: String,
    recent: Vec<StationDetailRow>,
}

impl Default for StationChangesSummary {
    fn default() -> Self {
        Self {
            status: "clean".into(),
            count: 0,
            added: 0,
            modified: 0,
            deleted: 0,
            external: 0,
            total_added: 0,
            total_removed: 0,
            latest_path: String::new(),
            latest_kind: String::new(),
            recent: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationSessionsSummary {
    total: u32,
    active: u32,
    external: u32,
    total_tokens: f32,
    disk_bytes: f64,
    worktrees: u32,
    worktree_dirty: u32,
    worktree_unmerged: u32,
    worktree_active: u32,
    worktree_cleanup: u32,
    worktree_bytes: f64,
    worktree_scan_status: String,
    latest_task: String,
    latest_source: String,
    latest_updated: String,
    index_status: String,
    recent: Vec<StationDetailRow>,
    recent_worktrees: Vec<StationDetailRow>,
}

impl Default for StationSessionsSummary {
    fn default() -> Self {
        Self {
            total: 0,
            active: 0,
            external: 0,
            total_tokens: 0.0,
            disk_bytes: 0.0,
            worktrees: 0,
            worktree_dirty: 0,
            worktree_unmerged: 0,
            worktree_active: 0,
            worktree_cleanup: 0,
            worktree_bytes: 0.0,
            worktree_scan_status: String::new(),
            latest_task: String::new(),
            latest_source: String::new(),
            latest_updated: String::new(),
            index_status: String::new(),
            recent: Vec::new(),
            recent_worktrees: Vec::new(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationControlsSummary {
    backend: String,
    command: String,
    sandbox: String,
    approval_policy: String,
    model: String,
    reasoning_effort: String,
    service_tier: String,
    managed_context: String,
    context_archive: String,
    web_search: bool,
    network_access: bool,
    writable_roots: u32,
    new_session_agent: String,
    session_id: String,
    session_label: String,
    session_selection: String,
    session_source: String,
    session_command: String,
    session_live_id: String,
    session_live_phase: String,
    session_action_id: String,
    session_attach_id: String,
    session_stop_id: String,
    session_managed_context: String,
    session_context_archive: String,
    session_can_config: bool,
    session_can_focus: bool,
    session_can_attach: bool,
    session_can_stop: bool,
    session_can_rename: bool,
    session_can_interrupt: bool,
    session_can_steer: bool,
    session_detached: bool,
    session_active: bool,
    session_is_codex: bool,
    session_service_tier: String,
    session_goal_status: String,
    session_goal_objective: String,
    session_goal_tokens: String,
    external_turn_state: String,
    external_turn_backend: String,
    external_turn_label: String,
    external_turn_detail: String,
    external_turn_session_id: String,
    prompt_mode: String,
    direct_mode: bool,
    draft_chars: u32,
    display_access: String,
    voice_state: String,
    mic_active: bool,
    video_active: bool,
    active_browser: bool,
    browser_workspaces: u32,
    recordings: u32,
    active_recording: String,
    cu_provider: String,
    cu_model: String,
    cu_backend: String,
    debug_screen: bool,
    debug_recording: bool,
    pending_attachments: u32,
    shared_view_visible: bool,
    shared_view_target: String,
    shared_view_action: String,
    shared_view_note: String,
    shared_view_can_take_input: bool,
}

impl Default for StationControlsSummary {
    fn default() -> Self {
        Self {
            backend: String::new(),
            command: String::new(),
            sandbox: String::new(),
            approval_policy: String::new(),
            model: String::new(),
            reasoning_effort: String::new(),
            service_tier: String::new(),
            managed_context: String::new(),
            context_archive: String::new(),
            web_search: false,
            network_access: false,
            writable_roots: 0,
            new_session_agent: String::new(),
            session_id: String::new(),
            session_label: String::new(),
            session_selection: String::new(),
            session_source: String::new(),
            session_command: String::new(),
            session_live_id: String::new(),
            session_live_phase: String::new(),
            session_action_id: String::new(),
            session_attach_id: String::new(),
            session_stop_id: String::new(),
            session_managed_context: String::new(),
            session_context_archive: String::new(),
            session_can_config: false,
            session_can_focus: false,
            session_can_attach: false,
            session_can_stop: false,
            session_can_rename: false,
            session_can_interrupt: false,
            session_can_steer: false,
            session_detached: false,
            session_active: false,
            session_is_codex: false,
            session_service_tier: String::new(),
            session_goal_status: String::new(),
            session_goal_objective: String::new(),
            session_goal_tokens: String::new(),
            external_turn_state: String::new(),
            external_turn_backend: String::new(),
            external_turn_label: String::new(),
            external_turn_detail: String::new(),
            external_turn_session_id: String::new(),
            prompt_mode: String::new(),
            direct_mode: false,
            draft_chars: 0,
            display_access: String::new(),
            voice_state: String::new(),
            mic_active: false,
            video_active: false,
            active_browser: true,
            browser_workspaces: 0,
            recordings: 0,
            active_recording: String::new(),
            cu_provider: String::new(),
            cu_model: String::new(),
            cu_backend: String::new(),
            debug_screen: false,
            debug_recording: false,
            pending_attachments: 0,
            shared_view_visible: false,
            shared_view_target: String::new(),
            shared_view_action: String::new(),
            shared_view_note: String::new(),
            shared_view_can_take_input: false,
        }
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct StationAttentionQueueSummary {
    count: u32,
    blocked: u32,
    warn: u32,
    ready: u32,
    items: Vec<StationAttentionItem>,
}

#[derive(Clone, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct StationAttentionItem {
    id: String,
    kind: String,
    level: String,
    title: String,
    meta: String,
    detail: String,
    session_id: String,
    can_cancel: bool,
}

#[derive(Clone, Deserialize, Default)]
#[serde(default)]
struct StationDisplayRunwaySummary {
    selected_peer_id: String,
    selected_display_id: i32,
    operator_session_id: String,
    local_streams: u32,
    remote_streams: u32,
    shared_view_visible: bool,
    lanes: Vec<StationDisplayRunwayLane>,
}

#[derive(Clone, Deserialize, Default)]
#[serde(default)]
struct StationDisplayRunwayLane {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    title: String,
    meta: String,
    detail: String,
    host_id: String,
    display_id: i32,
    session_id: String,
    live_id: String,
    selected: bool,
    can_focus: bool,
    can_interrupt: bool,
    can_take_input: bool,
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationBreakdown {
    label: String,
    value: f32,
}

impl Default for StationBreakdown {
    fn default() -> Self {
        Self {
            label: String::new(),
            value: 0.0,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StationDetailRow {
    id: String,
    session_id: String,
    action: String,
    label: String,
    value: String,
    detail: String,
    tone: String,
    is_codex: bool,
    thread_action_session_id: String,
    goal_status: String,
    goal_objective: String,
    goal_tokens: String,
    goal_token_budget: String,
    can_resume: bool,
    can_config: bool,
    can_rename: bool,
    can_attach: bool,
    can_stop: bool,
}

impl Default for StationDetailRow {
    fn default() -> Self {
        Self {
            id: String::new(),
            session_id: String::new(),
            action: String::new(),
            label: String::new(),
            value: String::new(),
            detail: String::new(),
            tone: String::new(),
            is_codex: false,
            thread_action_session_id: String::new(),
            goal_status: String::new(),
            goal_objective: String::new(),
            goal_tokens: String::new(),
            goal_token_budget: String::new(),
            can_resume: false,
            can_config: false,
            can_rename: false,
            can_attach: false,
            can_stop: false,
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

struct DisplayTile {
    host_id: String,
    display_id: String,
    label: String,
    kind: String,
    ready: bool,
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

    fn label(&self) -> &'static str {
        match self {
            Self::Orbital => "orbital",
            Self::Constellation => "constellation",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mood {
    Cockpit,
    Calm,
}

impl Mood {
    fn from_str(s: &str) -> Self {
        match s {
            "calm" => Self::Calm,
            _ => Self::Cockpit,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Cockpit => "cockpit",
            Self::Calm => "calm",
        }
    }
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
    Noop,
    Select(String),
    ClosePanel,
    Approval {
        host_id: String,
        approval_id: String,
        decision: String,
    },
    OpenDisplay {
        host_id: String,
        display_id: String,
    },
    DisplayRunwayAction {
        action: String,
        lane_id: String,
    },
    ContextAction {
        action: String,
        id: String,
    },
    ActivityAction {
        action: String,
        id: String,
    },
    ControlsAction {
        action: String,
    },
    ManagedAction {
        action: String,
        id: String,
        session_id: String,
    },
    ChangesAction {
        action: String,
        path: String,
    },
    SessionAction {
        action: String,
        session_id: String,
    },
    ThreadAction {
        op: String,
        session_id: String,
    },
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

struct LaneAction {
    label: &'static str,
    width: f32,
    color: &'static str,
    hit: HitAction,
}

impl LaneAction {
    fn select(label: &'static str, id: &'static str, width: f32, color: &'static str) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::Select(id.to_string()),
        }
    }

    fn activity(
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

    fn controls(
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

    fn session(
        label: &'static str,
        action: &'static str,
        session_id: &str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::SessionAction {
                action: action.to_string(),
                session_id: session_id.to_string(),
            },
        }
    }
}

struct RunwayAction {
    label: &'static str,
    width: f32,
    color: &'static str,
    hit: HitAction,
}

impl RunwayAction {
    fn select(label: &'static str, id: &'static str, width: f32, color: &'static str) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::Select(id.to_string()),
        }
    }

    fn activity(
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

    fn context(label: &'static str, action: &'static str, width: f32, color: &'static str) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::ContextAction {
                action: action.to_string(),
                id: String::new(),
            },
        }
    }

    fn controls(
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

    fn changes(
        label: &'static str,
        action: &'static str,
        path: &str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::ChangesAction {
                action: action.to_string(),
                path: path.to_string(),
            },
        }
    }

    fn session(
        label: &'static str,
        action: &'static str,
        session_id: &str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::SessionAction {
                action: action.to_string(),
                session_id: session_id.to_string(),
            },
        }
    }

    fn open_display(
        label: &'static str,
        host_id: &str,
        display_id: &str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::OpenDisplay {
                host_id: host_id.to_string(),
                display_id: display_id.to_string(),
            },
        }
    }

    fn managed(
        label: &'static str,
        action: &'static str,
        id: &str,
        session_id: &str,
        width: f32,
        color: &'static str,
    ) -> Self {
        Self {
            label,
            width,
            color,
            hit: HitAction::ManagedAction {
                action: action.to_string(),
                id: id.to_string(),
                session_id: session_id.to_string(),
            },
        }
    }
}

struct MenuAction {
    label: String,
    color: &'static str,
    hit: HitAction,
}

impl MenuAction {
    fn new(label: &str, color: &'static str, hit: HitAction) -> Self {
        Self {
            label: label.to_string(),
            color,
            hit,
        }
    }

    fn select(label: &str, id: &str, color: &'static str) -> Self {
        Self::new(label, color, HitAction::Select(id.to_string()))
    }

    fn activity(label: &str, action: &str, color: &'static str) -> Self {
        Self::new(
            label,
            color,
            HitAction::ActivityAction {
                action: action.to_string(),
                id: String::new(),
            },
        )
    }

    fn context(label: &str, action: &str, color: &'static str) -> Self {
        Self::new(
            label,
            color,
            HitAction::ContextAction {
                action: action.to_string(),
                id: String::new(),
            },
        )
    }

    fn controls(label: &str, action: &str, color: &'static str) -> Self {
        Self::new(
            label,
            color,
            HitAction::ControlsAction {
                action: action.to_string(),
            },
        )
    }

    fn managed(label: &str, action: &str, id: &str, session_id: &str, color: &'static str) -> Self {
        Self::new(
            label,
            color,
            HitAction::ManagedAction {
                action: action.to_string(),
                id: id.to_string(),
                session_id: session_id.to_string(),
            },
        )
    }

    fn changes(label: &str, action: &str, path: &str, color: &'static str) -> Self {
        Self::new(
            label,
            color,
            HitAction::ChangesAction {
                action: action.to_string(),
                path: path.to_string(),
            },
        )
    }

    fn session(label: &str, action: &str, session_id: &str, color: &'static str) -> Self {
        Self::new(
            label,
            color,
            HitAction::SessionAction {
                action: action.to_string(),
                session_id: session_id.to_string(),
            },
        )
    }

    fn open_display(label: &str, host_id: &str, display_id: &str, color: &'static str) -> Self {
        Self::new(
            label,
            color,
            HitAction::OpenDisplay {
                host_id: host_id.to_string(),
                display_id: display_id.to_string(),
            },
        )
    }
}

struct AttentionItem {
    title: String,
    detail: String,
    color: &'static str,
    hit: HitAction,
}

struct PointerDrag {
    x: f32,
    y: f32,
    last_x: f32,
    last_y: f32,
    moved: bool,
    pending_action: Option<HitAction>,
}

#[derive(Clone, Copy)]
struct PinchZoom {
    start_distance: f32,
    start_camera_distance: f32,
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
    const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
    const Y: Self = Self {
        x: 0.0,
        y: 1.0,
        z: 0.0,
    };

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
        Self {
            eye,
            right,
            up,
            forward,
        }
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
const C_LAVENDER_CSS: &str = "#b4befe";
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

fn attention_level_color_css(level: &str, kind: &str) -> &'static str {
    match level {
        "blocked" | "error" => C_RED_CSS,
        "warn" | "warning" => C_YELLOW_CSS,
        "ready" | "ok" => C_GREEN_CSS,
        _ => tone_color_css(kind),
    }
}

fn push_synth_attention(
    items: &mut Vec<AttentionItem>,
    rendered_keys: &HashSet<&'static str>,
    key: &'static str,
    item: AttentionItem,
) {
    if !rendered_keys.contains(key) {
        items.push(item);
    }
}

fn merge_attention_with_reserved_critical(
    mut primary_items: Vec<AttentionItem>,
    mut critical_items: Vec<AttentionItem>,
    cap: usize,
) -> Vec<AttentionItem> {
    if critical_items.is_empty() {
        primary_items.truncate(cap);
        return primary_items;
    }
    let primary_keep = cap.saturating_sub(critical_items.len()).min(primary_items.len());
    primary_items.truncate(primary_keep);
    primary_items.append(&mut critical_items);
    primary_items.truncate(cap);
    primary_items
}

fn activity_event_is_managed(event: &StationEvent) -> bool {
    let text = format!("{} {} {}", event.source, event.msg, event.level).to_lowercase();
    [
        "managed", "context", "rewind", "backout", "anchor", "lineage", "fission",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn tone_color_css(tone: &str) -> &'static str {
    match tone {
        "error" | "red" => C_RED_CSS,
        "warn" | "warning" | "yellow" => C_YELLOW_CSS,
        "managed" | "mauve" => C_MAUVE_CSS,
        "context" | "blue" => C_BLUE_CSS,
        "session" | "teal" => C_TEAL_CSS,
        "ok" | "green" => C_GREEN_CSS,
        "changes" => C_YELLOW_CSS,
        "peer" | "peach" => C_PEACH_CSS,
        _ => C_OVERLAY1_CSS,
    }
}

fn display_lane_color_css(kind: &str) -> &'static str {
    match kind {
        "local_stream" => C_TEAL_CSS,
        "remote_stream" | "peer_target" => C_PEACH_CSS,
        "shared_view" => C_GREEN_CSS,
        "operator_target" => C_BLUE_CSS,
        _ => C_OVERLAY1_CSS,
    }
}

fn external_turn_color_css(state: &str) -> &'static str {
    match state {
        "thinking" => C_LAVENDER_CSS,
        "running tools" => C_TEAL_CSS,
        "waiting" => C_YELLOW_CSS,
        "queued" => C_BLUE_CSS,
        "misconfigured" => C_RED_CSS,
        "stopped" => C_OVERLAY1_CSS,
        _ => C_MAUVE_CSS,
    }
}

fn managed_row_buttons(row: &StationDetailRow) -> Vec<(&'static str, &'static str, f32)> {
    match row.action.as_str() {
        "anchor" => vec![("use", "anchor", 40.0), ("inspect", "anchor-inspect", 58.0)],
        "record" => vec![
            ("inspect", "record-inspect", 58.0),
            ("fork", "record-fork", 42.0),
        ],
        "branch" => vec![("claim", "branch", 50.0)],
        _ => Vec::new(),
    }
}

fn context_row_buttons(
    row: &StationDetailRow,
) -> Vec<(&'static str, &'static str, f32, &'static str)> {
    if row.id.is_empty() || row.action != "part" {
        return Vec::new();
    }
    vec![
        ("focus", "part", 48.0, C_BLUE_CSS),
        ("copy", "copy-part", 42.0, C_TEAL_CSS),
        ("exact", "load-exact", 46.0, C_MAUVE_CSS),
    ]
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
    Vec2::new(
        (pos[0] * 0.5 + 0.5) * width as f32,
        (0.5 - pos[1] * 0.5) * height as f32,
    )
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

fn percent(value: f32, max: f32) -> f32 {
    if max <= 0.0 {
        0.0
    } else {
        (value / max).clamp(0.0, 1.0)
    }
}

fn pct_label(pct: f32) -> String {
    format!("{:.0}%", pct.clamp(0.0, 1.0) * 100.0)
}

fn nonempty(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn compact_number(value: f64) -> String {
    let abs = value.abs();
    if abs >= 1_000_000_000.0 {
        format!("{:.1}b", value / 1_000_000_000.0)
    } else if abs >= 1_000_000.0 {
        format!("{:.1}m", value / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else {
        format!("{value:.0}")
    }
}

fn format_token_ratio(used: f32, window: f32) -> String {
    if window <= 0.0 {
        "-- / --".to_string()
    } else {
        format!(
            "{} / {}",
            compact_number(used as f64),
            compact_number(window as f64)
        )
    }
}

fn format_bytes(bytes: f64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", units[unit])
    } else if value >= 10.0 {
        format!("{value:.0} {}", units[unit])
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

fn pressure_color(pct: f32) -> &'static str {
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

fn station_enable_webgpu() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|document| document.url().ok())
        .is_some_and(|url| url.contains("station_gpu=webgpu"))
}

fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}
