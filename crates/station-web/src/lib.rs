//! Station tab WASM renderer: a WebGPU (or Canvas-fallback) 3D scene of
//! hosts and agents with a 2D HUD overlay, driven by snapshots from
//! `static/app.html`. Rendering is scheduled on demand: see
//! `StationInner::schedule_frame` / `is_animating`.

mod gpu;
mod hud;
mod input;
mod model;
mod scene;
mod util;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::f32::consts::PI;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::spawn_local;
use web_sys::{CanvasRenderingContext2d, Event, HtmlCanvasElement, HtmlVideoElement};

use gpu::{GpuFrame, GpuState};
use hud::{Hud, SystemTarget};
use input::{HitZone, PinchZoom, PointerDrag};
use model::{StationEvent, StationSnapshot};
use scene::{layout_positions, LayoutName, Mood, Particle, Vec2, Vec3};
use util::{lcg, level_color, now_ms, station_enable_webgpu, unit};

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
                "Station WebGPU disabled by station_gpu URL parameter; using Canvas renderer",
            ));
        }
        StationInner::start_loop(inner.clone());
        Ok(Self { inner })
    }

    pub fn set_active(&self, active: bool) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.active = active;
            // The pane may have moved or resized while the tab was hidden.
            inner.canvas_origin = None;
        }
        if active {
            StationInner::schedule_frame(&self.inner);
        }
    }

    pub fn set_action_callback(&self, callback: js_sys::Function) {
        self.inner.borrow_mut().action_callback = Some(callback);
    }

    pub fn resize(&self) {
        self.inner.borrow_mut().resize();
        StationInner::schedule_frame(&self.inner);
    }

    pub fn update_snapshot(&self, snapshot: JsValue) -> Result<(), JsValue> {
        let snapshot: StationSnapshot = serde_wasm_bindgen::from_value(snapshot)?;
        self.inner.borrow_mut().apply_snapshot(snapshot);
        StationInner::schedule_frame(&self.inner);
        Ok(())
    }

    pub fn register_display_source(
        &self,
        source_id: String,
        host_id: String,
        _display_id: String,
        label: String,
        _kind: String,
        video: HtmlVideoElement,
    ) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.display_sources.insert(
                source_id,
                DisplaySource {
                    host_id,
                    label,
                    video,
                },
            );
            inner.targets_dirty = true;
        }
        StationInner::schedule_frame(&self.inner);
    }

    pub fn unregister_display_source(&self, source_id: String) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.display_sources.remove(&source_id);
            inner.targets_dirty = true;
        }
        StationInner::schedule_frame(&self.inner);
    }

    pub fn set_layout(&self, layout: String) {
        {
            let mut inner = self.inner.borrow_mut();
            inner.set_layout(LayoutName::from_str(&layout));
        }
        StationInner::schedule_frame(&self.inner);
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
            inner.targets_dirty = true;
        }
        StationInner::schedule_frame(&self.inner);
    }

    pub fn select_by_id(&self, id: Option<String>) {
        self.inner.borrow_mut().selected_id = id;
        StationInner::schedule_frame(&self.inner);
    }

    pub fn focus_on(&self, id: String) {
        self.inner.borrow_mut().focus_id = Some(id);
        StationInner::schedule_frame(&self.inner);
    }

    pub fn debug_state(&self) -> String {
        let inner = self.inner.borrow();
        format!(
            "station hosts={} agents={} events={} displays={} renderer={} gpu={}",
            inner.snapshot.hosts.len(),
            inner.snapshot.agents.len(),
            inner.snapshot.events.len(),
            inner.display_sources.len(),
            if inner.gpu.is_some() {
                "WebGPU"
            } else {
                "Canvas"
            },
            inner.gpu.is_some(),
        )
    }
}

struct StationInner {
    scene_canvas: HtmlCanvasElement,
    hud_canvas: HtmlCanvasElement,
    hud: Hud,
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
    last_input_ms: f64,
    selected_id: Option<String>,
    focus_id: Option<String>,
    pointer_down: Option<PointerDrag>,
    active_pointers: HashMap<i32, Vec2>,
    pinch_zoom: Option<PinchZoom>,
    ar_x: f32,
    ar_y: f32,
    hit_zones: Vec<HitZone>,
    action_callback: Option<js_sys::Function>,
    /// World positions per node id, rebuilt when the snapshot or layout
    /// changes (never per frame).
    layout_cache: HashMap<String, Vec3>,
    /// Control-center summaries, rebuilt lazily when `targets_dirty`.
    system_targets: Vec<SystemTarget>,
    targets_dirty: bool,
    /// Reused per-frame geometry; cleared and refilled, never reallocated.
    frame: GpuFrame,
    /// Cached canvas origin for pointer math; None forces one
    /// getBoundingClientRect on the next event.
    canvas_origin: Option<(f64, f64)>,
    _events: Vec<Closure<dyn FnMut(Event)>>,
    raf_cb: Option<Closure<dyn FnMut(f64)>>,
    raf_pending: bool,
    /// Timestamp of the previously rendered frame, for frame-rate-independent
    /// accumulation (auto-orbit drift).
    last_tick_ms: f64,
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
            hud: Hud::new(ctx),
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
            last_input_ms: now_ms(),
            selected_id: None,
            focus_id: None,
            pointer_down: None,
            active_pointers: HashMap::new(),
            pinch_zoom: None,
            ar_x: 0.0,
            ar_y: 0.0,
            hit_zones: Vec::new(),
            action_callback: None,
            layout_cache: HashMap::new(),
            system_targets: Vec::new(),
            targets_dirty: true,
            frame: GpuFrame::default(),
            canvas_origin: None,
            _events: Vec::new(),
            raf_cb: None,
            raf_pending: false,
            last_tick_ms: 0.0,
        };
        inner.rebuild_layout_cache();
        inner.resize();
        inner
    }

    fn set_layout(&mut self, layout: LayoutName) {
        if self.layout != layout {
            self.layout = layout;
            self.rebuild_layout_cache();
            self.targets_dirty = true;
        }
    }

    fn rebuild_layout_cache(&mut self) {
        self.layout_cache = layout_positions(&self.snapshot, self.layout);
    }

    #[cfg(target_arch = "wasm32")]
    fn start_gpu(inner: Rc<RefCell<Self>>) {
        let canvas = inner.borrow().scene_canvas.clone();
        spawn_local(async move {
            match GpuState::new(canvas).await {
                Ok(gpu) => {
                    let mut s = inner.borrow_mut();
                    s.gpu = Some(gpu);
                    s.resize();
                }
                Err(err) => {
                    web_sys::console::warn_1(&JsValue::from_str(&format!(
                        "Station WebGPU unavailable, falling back to Canvas renderer: {err:?}"
                    )));
                    inner.borrow_mut().install_canvas_scene_fallback();
                }
            }
            StationInner::schedule_frame(&inner);
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_gpu(_inner: Rc<RefCell<Self>>) {}

    /// Runtime WebGPU failure: give the scene a 2D context so the wireframe
    /// fallback renders, matching the `?station_gpu=canvas` visual. If wgpu
    /// already claimed the canvas with a `webgpu` context (adapter or device
    /// request failed after the surface was created), a 2D context can no
    /// longer be obtained from it; `draw_hud` then paints the scene as an
    /// underlay on the HUD canvas instead.
    #[cfg(target_arch = "wasm32")]
    fn install_canvas_scene_fallback(&mut self) {
        if self.scene_ctx.is_some() {
            return;
        }
        self.scene_ctx = self
            .scene_canvas
            .get_context("2d")
            .ok()
            .flatten()
            .and_then(|ctx| ctx.dyn_into::<CanvasRenderingContext2d>().ok());
        if self.scene_ctx.is_none() {
            web_sys::console::warn_1(&JsValue::from_str(
                "Station scene canvas already consumed by WebGPU; drawing scene on HUD canvas",
            ));
        }
    }

    /// One persistent rAF callback drives rendering. `schedule_frame` arms it
    /// after any state change; the tick re-arms itself only while something
    /// is actually animating, so an idle station costs zero CPU.
    fn start_loop(inner: Rc<RefCell<Self>>) {
        let loop_inner = inner.clone();
        let cb = Closure::wrap(Box::new(move |time_ms: f64| {
            let animating = {
                let mut s = loop_inner.borrow_mut();
                s.raf_pending = false;
                if !s.active {
                    false
                } else {
                    s.render(time_ms);
                    s.is_animating()
                }
            };
            if animating {
                StationInner::schedule_frame(&loop_inner);
            }
        }) as Box<dyn FnMut(f64)>);

        inner.borrow_mut().raf_cb = Some(cb);
        StationInner::schedule_frame(&inner);
    }

    /// Request one animation frame if the tab is active and none is pending.
    fn schedule_frame(inner: &Rc<RefCell<Self>>) {
        let mut s = inner.borrow_mut();
        if !s.active || s.raf_pending {
            return;
        }
        let Some(cb) = s.raf_cb.as_ref() else {
            return;
        };
        let Some(window) = web_sys::window() else {
            return;
        };
        if window
            .request_animation_frame(cb.as_ref().unchecked_ref())
            .is_ok()
        {
            s.raf_pending = true;
        }
    }

    /// Whether the loop must keep ticking without further input. All ambient
    /// time-based animation (spins, pulses, breathing, auto-orbit) is gated
    /// behind `motion > 0`; live video thumbnails, an in-flight camera
    /// drag/pinch, and still-fading event particles also keep it running.
    fn is_animating(&self) -> bool {
        self.active
            && (self.motion > 0.0
                || !self.display_sources.is_empty()
                || self.pointer_down.is_some()
                || self.pinch_zoom.is_some()
                || !self.particles.is_empty())
    }

    fn apply_snapshot(&mut self, snapshot: StationSnapshot) {
        // Spawn a particle per newly seen event, positioned with the layout
        // of the snapshot being replaced (the cache is rebuilt below).
        let positions = &self.layout_cache;
        for event in &snapshot.events {
            if !self.seen_events.contains(&event.id) {
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
        // Event ids are unique and the snapshot carries a rolling window, so
        // retaining only the current window's ids bounds the set while still
        // deduplicating every id that can reappear.
        self.seen_events.clear();
        self.seen_events
            .extend(snapshot.events.iter().map(|event| event.id.clone()));
        self.snapshot = snapshot;
        self.rebuild_layout_cache();
        self.targets_dirty = true;
        if self
            .selected_id
            .as_ref()
            .is_some_and(|id| !self.node_exists(id))
        {
            self.selected_id = None;
        }
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
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0)
            .clamp(1.0, max_dpr);
        let css_w = self.hud_canvas.client_width().max(1) as f64;
        let css_h = self.hud_canvas.client_height().max(1) as f64;
        let width = (css_w * dpr).round().max(1.0) as u32;
        let height = (css_h * dpr).round().max(1.0) as u32;
        self.dpr = dpr;
        self.canvas_origin = None;
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        self.scene_canvas.set_width(width);
        self.scene_canvas.set_height(height);
        self.hud_canvas.set_width(width);
        self.hud_canvas.set_height(height);
        // Setting a canvas size resets its 2D context state.
        self.hud.invalidate();
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.resize(width, height);
        }
    }

    fn render(&mut self, time_ms: f64) {
        if !self.active {
            return;
        }
        // Guard against the backing store being resized behind our back.
        // Plain attribute reads; the JS side is responsible for calling
        // resize() when the pane's CSS size changes.
        if self.hud_canvas.width() != self.width || self.hud_canvas.height() != self.height {
            self.resize();
        }
        // With motion at zero every time-based phase freezes; events still
        // schedule one-shot frames through schedule_frame.
        let anim_ms = if self.motion > 0.0 { time_ms } else { 0.0 };
        let idle_ms = time_ms - self.last_input_ms;
        // dt-scaled so the drift rate is frame-rate independent (tuned
        // against the old ~250ms tick); clamped to absorb parked gaps.
        let dt_ms = (time_ms - self.last_tick_ms).clamp(0.0, 1000.0);
        self.last_tick_ms = time_ms;
        if self.motion > 0.0 && idle_ms > 2400.0 {
            self.yaw -= 0.000055
                * self.motion
                * (idle_ms.min(5000.0) as f32 / 1000.0)
                * (dt_ms as f32 / 250.0);
        }
        if let Some(focus_id) = self.focus_id.take() {
            if let Some(pos) = self.layout_cache.get(&focus_id).copied() {
                let dir = pos.normalized();
                if dir.len() > 0.001 {
                    self.yaw = dir.x.atan2(dir.z);
                    self.pitch = (-dir.y * 0.22).clamp(-0.75, 0.75);
                    self.distance = 8.0;
                }
            }
        }
        if self.targets_dirty {
            self.system_targets = self.compute_system_targets();
            self.targets_dirty = false;
        }

        self.build_frame(anim_ms, time_ms);
        if let Some(gpu) = self.gpu.as_mut() {
            // The canvas backing store can be resized by JS (or by a missed
            // resize event) after the surface was configured; presenting at a
            // stale size makes every frame's swapchain texture invalid. The
            // attribute reads are layout-free, so guard each frame.
            gpu.resize(self.scene_canvas.width(), self.scene_canvas.height());
            if let Err(err) = gpu.render(&self.frame) {
                web_sys::console::warn_1(&JsValue::from_str(&format!(
                    "Station GPU render failed: {err:?}"
                )));
            }
        } else if let Some(scene_ctx) = self.scene_ctx.as_ref() {
            self.draw_scene_lines(scene_ctx);
        }
        self.draw_hud(anim_ms);
    }

    fn activity_event(&self, event_id: &str) -> Option<StationEvent> {
        self.snapshot
            .events
            .iter()
            .find(|event| event.id == event_id)
            .cloned()
    }
}

struct DisplaySource {
    host_id: String,
    label: String,
    video: HtmlVideoElement,
}
