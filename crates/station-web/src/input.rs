//! Pointer, wheel, keyboard, orientation, and window event handling plus
//! hit-zone dispatch.

use std::cell::RefCell;
use std::rc::Rc;

use serde::Serialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{DeviceOrientationEvent, Event, KeyboardEvent, PointerEvent, WheelEvent};

use crate::scene::{ndc_to_screen, LayoutName, Vec2};
use crate::util::now_ms;
use crate::StationInner;

impl StationInner {
    pub(crate) fn install_events(inner: Rc<RefCell<Self>>) -> Result<(), JsValue> {
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
            {
                let mut s = down_inner.borrow_mut();
                s.mark_input();
                let (x, y) = s.event_xy(e.client_x() as f64, e.client_y() as f64);
                s.active_pointers.insert(e.pointer_id(), Vec2::new(x, y));
                if s.active_pointers.len() >= 2 {
                    s.begin_pinch();
                    s.pointer_down = None;
                    s.set_cursor("drag");
                } else if let Some(scroll) = s.scrollbar_drag_at(x, y) {
                    // A press on a panel scrollbar starts a thumb drag;
                    // pressing the track outside the thumb jumps there
                    // first, then keeps dragging.
                    s.apply_scroll_drag(&scroll, y);
                    s.set_cursor("pointer");
                    s.pointer_down = Some(PointerDrag {
                        x,
                        y,
                        last_x: x,
                        last_y: y,
                        moved: false,
                        pending_action: None,
                        slider: None,
                        scrollbar: Some(scroll),
                    });
                } else {
                    // A press on a slider track starts a scrub: jump the
                    // value to the press point and keep tracking the
                    // pointer; the final value is emitted on pointer-up.
                    let slider = s.slider_at(x, y);
                    if let Some(slider) = slider {
                        s.apply_slider(slider, x);
                        s.set_cursor("pointer");
                        s.pointer_down = Some(PointerDrag {
                            x,
                            y,
                            last_x: x,
                            last_y: y,
                            moved: false,
                            pending_action: None,
                            slider: Some(slider),
                            scrollbar: None,
                        });
                    } else {
                        let pending_action = s.hit_action_at(x, y);
                        s.set_cursor(if pending_action.is_some() {
                            "pointer"
                        } else {
                            "drag"
                        });
                        s.pointer_down = Some(PointerDrag {
                            x,
                            y,
                            last_x: x,
                            last_y: y,
                            moved: false,
                            pending_action,
                            slider: None,
                            scrollbar: None,
                        });
                    }
                }
            }
            StationInner::schedule_frame(&down_inner);
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("pointerdown", down.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(down);

        let move_inner = inner.clone();
        let mv = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<PointerEvent>() else {
                return;
            };
            {
                let mut s = move_inner.borrow_mut();
                let (x, y) = s.event_xy(e.client_x() as f64, e.client_y() as f64);
                s.hover_xy = Some((x, y));
                // Hover visuals only change when the pointer crosses a hit
                // zone boundary; mark the HUD dirty exactly then.
                let zone = s.hover_zone_rect_at(x, y);
                if zone != s.hover_zone_rect {
                    s.hover_zone_rect = zone;
                    s.hud_dirty = true;
                }
                if s.active_pointers.contains_key(&e.pointer_id()) {
                    s.active_pointers.insert(e.pointer_id(), Vec2::new(x, y));
                }
                // Pointer-tilt parallax: without a real device-orientation
                // source, the pointer position substitutes for tilt so the
                // AR-strength setting does something on desktops. Gated on
                // ar_strength so 0 keeps the camera (and repaints) still.
                if !s.has_device_orientation && s.ar_strength > 0.0 {
                    let cw = s.css_width().max(1.0);
                    let ch = s.css_height().max(1.0);
                    s.ar_x = ((x / cw) * 2.0 - 1.0).clamp(-1.0, 1.0) * 0.6;
                    s.ar_y = ((y / ch) * 2.0 - 1.0).clamp(-1.0, 1.0) * 0.6;
                }
                let active_slider = s.pointer_down.as_ref().and_then(|drag| drag.slider);
                let active_scrollbar = s
                    .pointer_down
                    .as_ref()
                    .and_then(|drag| drag.scrollbar.clone());
                if s.active_pointers.len() >= 2 {
                    s.apply_pinch();
                    s.mark_input();
                    s.set_cursor("drag");
                } else if let Some(scroll) = active_scrollbar {
                    s.apply_scroll_drag(&scroll, y);
                    s.mark_input();
                    s.set_cursor("pointer");
                } else if let Some(slider) = active_slider {
                    s.apply_slider(slider, x);
                    s.mark_input();
                    s.set_cursor("pointer");
                } else if let Some(drag) = s.pointer_down.as_mut() {
                    let dx = x - drag.last_x;
                    let dy = y - drag.last_y;
                    drag.last_x = x;
                    drag.last_y = y;
                    let travel = (x - drag.x).abs() + (y - drag.y).abs();
                    if drag.pending_action.is_some() && travel <= 12.0 {
                        s.mark_input();
                    } else {
                        if travel > 4.0 {
                            drag.moved = true;
                            drag.pending_action = None;
                        }
                        s.yaw -= dx * 0.006;
                        s.pitch = (s.pitch + dy * 0.005).clamp(-1.05, 1.05);
                        s.mark_input();
                        s.set_cursor("drag");
                    }
                } else if s.hit_action_at(x, y).is_some() || s.pick_node(x, y).is_some() {
                    s.set_cursor("pointer");
                } else {
                    s.set_cursor("grab");
                }
            }
            StationInner::schedule_frame(&move_inner);
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
                if let Some(drag) = s.pointer_down.take() {
                    if let Some(scroll) = drag.scrollbar {
                        s.apply_scroll_drag(&scroll, y);
                        None
                    } else if let Some(slider) = drag.slider {
                        // Final scrub position, then hand the value to the
                        // dashboard for persistence + set_visuals re-apply.
                        s.apply_slider(slider, x);
                        Some(serde_json::json!({
                            "type": "view_set",
                            "key": slider.key.name(),
                            "value": s.view_slider_value(slider.key),
                        }))
                    } else if let Some(action) = drag.pending_action {
                        s.dispatch_hit(action)
                    } else if !drag.moved {
                        s.selected_id = s.pick_node(x, y);
                        s.hud_dirty = true;
                        None
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            up_inner.borrow_mut().set_cursor("grab");
            StationInner::schedule_frame(&up_inner);
            if let Some(action) = outbound {
                let callback = up_inner.borrow().action_callback.clone();
                StationInner::emit_action(callback, action);
            }
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("pointerup", up.as_ref().unchecked_ref())?;
        target.add_event_listener_with_callback("pointercancel", up.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(up);

        // Leaving the canvas must drop the hover-lit state on pills/tiles
        // (and recentre the pointer-tilt parallax).
        let leave_inner = inner.clone();
        let leave = Closure::wrap(Box::new(move |_event: Event| {
            {
                let mut s = leave_inner.borrow_mut();
                if s.hover_xy.take().is_none() {
                    return;
                }
                if !s.has_device_orientation {
                    s.ar_x = 0.0;
                    s.ar_y = 0.0;
                }
                s.hover_zone_rect = None;
                s.hud_dirty = true;
                s.mark_input();
            }
            StationInner::schedule_frame(&leave_inner);
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("pointerleave", leave.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(leave);

        let wheel_inner = inner.clone();
        let wheel = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<WheelEvent>() else {
                return;
            };
            e.prevent_default();
            {
                let mut s = wheel_inner.borrow_mut();
                s.mark_input();
                // Wheel over a scrollable panel scrolls that panel; only
                // open scene space zooms the camera.
                let (x, y) = s.event_xy(e.client_x() as f64, e.client_y() as f64);
                if !s.scroll_panel_by(x, y, e.delta_y() as f32) {
                    s.distance = (s.distance + e.delta_y() as f32 * 0.014).clamp(4.2, 25.0);
                }
            }
            StationInner::schedule_frame(&wheel_inner);
        }) as Box<dyn FnMut(_)>);
        target.add_event_listener_with_callback("wheel", wheel.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(wheel);

        let key_inner = inner.clone();
        let key = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<KeyboardEvent>() else {
                return;
            };
            // This is a window-level listener on a page full of real form
            // controls: never react to shortcuts (meta/ctrl/alt chords stay
            // the browser's / dashboard's) or to typing in an editable
            // element — "a" in the task composer must not orbit the camera.
            if e.meta_key() || e.ctrl_key() || e.alt_key() {
                return;
            }
            if event
                .target()
                .and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok())
                .is_some_and(|el| {
                    matches!(el.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT")
                        || el.is_content_editable()
                })
            {
                return;
            }
            let (used, outbound) = {
                let mut s = key_inner.borrow_mut();
                if !s.active {
                    return;
                }
                let mut used = true;
                let mut outbound: Option<serde_json::Value> = None;
                match e.key().as_str() {
                    "ArrowLeft" | "a" | "A" => s.yaw += 0.08,
                    "ArrowRight" | "d" | "D" => s.yaw -= 0.08,
                    "ArrowUp" | "w" | "W" => s.pitch = (s.pitch - 0.06).clamp(-1.05, 1.05),
                    "ArrowDown" | "s" | "S" => s.pitch = (s.pitch + 0.06).clamp(-1.05, 1.05),
                    "+" | "=" => s.distance = (s.distance - 0.6).clamp(4.2, 25.0),
                    "-" | "_" => s.distance = (s.distance + 0.6).clamp(4.2, 25.0),
                    // Slack-style composer summon: `/` opens the composer
                    // and hands keyboard focus to its input overlay.
                    "/" => outbound = Some(s.open_composer("send")),
                    "PageDown" => used = s.scroll_focused_panel(0.85),
                    "PageUp" => used = s.scroll_focused_panel(-0.85),
                    // Escape unwinds Station surfaces innermost-first:
                    // composer, then transcript viewer, then selection.
                    // Only consumed when it actually closed something;
                    // otherwise left to the dashboard (modal dismissal).
                    "Escape" => {
                        if s.composer_open {
                            outbound = Some(s.close_composer());
                        } else if s.transcript.is_some() {
                            s.close_transcript();
                        } else if s.selected_id.is_some() {
                            s.selected_id = None;
                            s.hud_dirty = true;
                        } else {
                            used = false;
                        }
                    }
                    "1" => s.set_layout(LayoutName::Orbital),
                    "2" => s.set_layout(LayoutName::Constellation),
                    _ => used = false,
                }
                if used {
                    e.prevent_default();
                    s.mark_input();
                }
                (used, outbound.map(|action| (action, s.action_callback.clone())))
            };
            if used {
                StationInner::schedule_frame(&key_inner);
            }
            if let Some((action, callback)) = outbound {
                StationInner::emit_action(callback, action);
            }
        }) as Box<dyn FnMut(_)>);
        window.add_event_listener_with_callback("keydown", key.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(key);

        let orientation_inner = inner.clone();
        let orientation = Closure::wrap(Box::new(move |event: Event| {
            let Some(e) = event.dyn_ref::<DeviceOrientationEvent>() else {
                return;
            };
            {
                // Desktop browsers can fire one all-null event; only a real
                // reading claims the AR channel (and silences the
                // pointer-tilt fallback).
                let (Some(gamma), Some(beta)) = (e.gamma(), e.beta()) else {
                    return;
                };
                let mut s = orientation_inner.borrow_mut();
                s.has_device_orientation = true;
                s.ar_x = (gamma as f32 / 45.0).clamp(-1.0, 1.0);
                s.ar_y = (beta as f32 / 60.0).clamp(-1.0, 1.0);
            }
            StationInner::schedule_frame(&orientation_inner);
        }) as Box<dyn FnMut(_)>);
        window.add_event_listener_with_callback(
            "deviceorientation",
            orientation.as_ref().unchecked_ref(),
        )?;
        inner.borrow_mut()._events.push(orientation);

        let resize_inner = inner.clone();
        let resize = Closure::wrap(Box::new(move |_event: Event| {
            resize_inner.borrow_mut().resize();
            StationInner::schedule_frame(&resize_inner);
        }) as Box<dyn FnMut(_)>);
        window.add_event_listener_with_callback("resize", resize.as_ref().unchecked_ref())?;
        inner.borrow_mut()._events.push(resize);

        // Scrolling moves the canvas within the viewport without resizing it;
        // only the cached pointer-math origin needs to be invalidated.
        // Capture phase so scrolls inside nested containers are seen too.
        let scroll_inner = inner.clone();
        let scroll = Closure::wrap(Box::new(move |_event: Event| {
            scroll_inner.borrow_mut().canvas_origin = None;
        }) as Box<dyn FnMut(_)>);
        window.add_event_listener_with_callback_and_bool(
            "scroll",
            scroll.as_ref().unchecked_ref(),
            true,
        )?;
        inner.borrow_mut()._events.push(scroll);

        Ok(())
    }

    pub(crate) fn pick_node(&self, x: f32, y: f32) -> Option<String> {
        let px = x * self.dpr as f32;
        let py = y * self.dpr as f32;
        self.frame
            .projected_nodes
            .iter()
            .filter_map(|n| {
                let p = ndc_to_screen([n.ndc.x, n.ndc.y], self.width, self.height);
                let d = ((p.x - px).powi(2) + (p.y - py).powi(2)).sqrt();
                (d <= n.radius * self.dpr as f32 + 10.0).then(|| (d, n.id.clone()))
            })
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, id)| id)
    }

    pub(crate) fn dispatch_hit(&mut self, action: HitAction) -> Option<serde_json::Value> {
        match action {
            HitAction::Layout(layout) => {
                self.set_layout(layout);
                None
            }
            HitAction::ClosePanel => {
                self.selected_id = None;
                self.hud_dirty = true;
                None
            }
            HitAction::CloseTranscript => {
                self.close_transcript();
                None
            }
            HitAction::Select(id) => {
                self.selected_id = Some(id);
                self.hud_dirty = true;
                None
            }
            HitAction::Noop => None,
            HitAction::Composer { op } => Some(self.composer_op(op)),
            HitAction::SessionAction { action, id } => Some(serde_json::json!({
                    "type": "session_action",
                    "action": action,
                    "session_id": id,
            })),
            HitAction::ManagedAction { action, id } => Some(serde_json::json!({
                    "type": "managed_action",
                    "action": action,
                    "id": id,
            })),
            HitAction::ChangesAction { action, path } => Some(serde_json::json!({
                    "type": "changes_action",
                    "action": action,
                    "path": path,
            })),
            HitAction::ContextAction { action, id } => Some(serde_json::json!({
                    "type": "context_action",
                    "action": action,
                    "id": id,
            })),
            HitAction::RunwayAction { action, lane_id } => Some(serde_json::json!({
                    "type": "display_runway_action",
                    "action": action,
                    "lane_id": lane_id,
            })),
            HitAction::ActivityAction { action, id } => Some(serde_json::json!({
                    "type": "activity_action",
                    "action": action,
                    "id": id,
            })),
            HitAction::ControlsAction { action } => Some(serde_json::json!({
                    "type": "controls_action",
                    "action": action,
            })),
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
            HitAction::ViewSet { key, value } => {
                // Instant local feedback; the emitted action persists the
                // draft and re-applies the same value through set_visuals.
                if key == "mood" {
                    self.mood = crate::scene::Mood::from_str(value);
                    self.targets_dirty = true;
                    self.hud_dirty = true;
                }
                Some(serde_json::json!({
                        "type": "view_set",
                        "key": key,
                        "value": value,
                }))
            }
            // Activating a slider by name (no drag geometry) re-emits its
            // current value — an idempotent persist, and the way the E2E
            // driver confirms the wiring without synthesizing a drag.
            HitAction::ViewSlider { key } => Some(serde_json::json!({
                    "type": "view_set",
                    "key": key.name(),
                    "value": self.view_slider_value(key),
            })),
        }
    }

    /// Current value backing a view slider.
    pub(crate) fn view_slider_value(&self, key: ViewSliderKey) -> f32 {
        match key {
            ViewSliderKey::Fov => self.fov_deg,
            ViewSliderKey::Motion => self.motion,
            ViewSliderKey::Ar => self.ar_strength,
            ViewSliderKey::Density => self.density,
        }
    }

    /// Apply a scrubbed slider value locally (clamped to the key's range).
    /// fov/density/mood feed the View target's summary text, so those mark
    /// the cached system targets dirty too.
    pub(crate) fn set_view_slider_value(&mut self, key: ViewSliderKey, value: f32) {
        let (min, max) = key.range();
        let value = value.clamp(min, max);
        match key {
            ViewSliderKey::Fov => self.fov_deg = value,
            ViewSliderKey::Motion => self.motion = value,
            ViewSliderKey::Ar => self.ar_strength = value,
            ViewSliderKey::Density => self.density = value,
        }
        self.targets_dirty = true;
        self.hud_dirty = true;
    }

    /// Begin or continue a slider scrub at pointer x.
    pub(crate) fn apply_slider(&mut self, slider: ActiveSlider, x: f32) {
        let value = slider.key.value_at(x, slider.track_x, slider.track_w);
        self.set_view_slider_value(slider.key, value);
    }

    pub(crate) fn emit_action(callback: Option<js_sys::Function>, action: serde_json::Value) {
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

    pub(crate) fn first_two_pointer_positions(&self) -> Option<(Vec2, Vec2)> {
        let mut iter = self.active_pointers.values().copied();
        Some((iter.next()?, iter.next()?))
    }

    pub(crate) fn begin_pinch(&mut self) {
        let Some((a, b)) = self.first_two_pointer_positions() else {
            return;
        };
        let dist = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt().max(1.0);
        self.pinch_zoom = Some(PinchZoom {
            start_distance: dist,
            start_camera_distance: self.distance,
        });
    }

    pub(crate) fn apply_pinch(&mut self) {
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
    }

    pub(crate) fn set_cursor(&self, cursor: &str) {
        if cursor == "grab" {
            let _ = self.hud_canvas.remove_attribute("data-station-cursor");
        } else {
            let _ = self.hud_canvas.set_attribute("data-station-cursor", cursor);
        }
    }

    pub(crate) fn hit_action_at(&self, x: f32, y: f32) -> Option<HitAction> {
        self.zone_at(x, y).map(|z| z.action.clone())
    }

    /// Rect of the top-most hit zone under the pointer (the one a click
    /// would dispatch to); used to detect hover transitions cheaply.
    pub(crate) fn hover_zone_rect_at(&self, x: f32, y: f32) -> Option<(f32, f32, f32, f32)> {
        self.zone_at(x, y).map(|z| (z.x, z.y, z.w, z.h))
    }

    /// Top-most (= last drawn, matching draw order) hit zone at a point.
    fn zone_at(&self, x: f32, y: f32) -> Option<&HitZone> {
        self.hit_zones
            .iter()
            .rev()
            .find(|z| x >= z.x && x <= z.x + z.w && y >= z.y && y <= z.y + z.h)
    }

    /// Composer-related local state transitions, shared by pills and keys.
    /// Every transition is also emitted to the dashboard (`{type:
    /// 'composer', op, mode}`) so it can place/focus/hide the DOM input
    /// overlay that does the actual text editing.
    pub(crate) fn open_composer(&mut self, mode: &str) -> serde_json::Value {
        self.composer_open = true;
        self.composer_mode = if mode == "launch" { "launch" } else { "send" }.to_string();
        self.hud_dirty = true;
        serde_json::json!({ "type": "composer", "op": "focus", "mode": self.composer_mode })
    }

    pub(crate) fn close_composer(&mut self) -> serde_json::Value {
        self.composer_open = false;
        self.hud_dirty = true;
        serde_json::json!({ "type": "composer", "op": "closed", "mode": self.composer_mode })
    }

    pub(crate) fn close_transcript(&mut self) {
        if self.transcript.take().is_some() {
            self.transcript_layout = None;
            self.panel_scroll.remove("transcript");
            self.transcript_follow = true;
            self.hud_dirty = true;
        }
    }

    fn composer_op(&mut self, op: &'static str) -> serde_json::Value {
        match op {
            "open-send" => self.open_composer("send"),
            "open-launch" => self.open_composer("launch"),
            "close" => self.close_composer(),
            // send / launch / target-pick: the dashboard owns the input
            // text and the submit path; pass the op through with the mode.
            other => serde_json::json!({
                "type": "composer",
                "op": other,
                "mode": self.composer_mode,
            }),
        }
    }

    /// Scroll the panel under the pointer by a wheel delta. Returns false
    /// when no scrollable region sits there (the camera should zoom).
    pub(crate) fn scroll_panel_by(&mut self, x: f32, y: f32, delta: f32) -> bool {
        let Some(zone) = self
            .scroll_zones
            .iter()
            .rev()
            .find(|z| x >= z.x && x <= z.x + z.w && y >= z.y && y <= z.y + z.h)
        else {
            return false;
        };
        let panel = zone.panel.clone();
        let max = (zone.content_h - zone.h).max(0.0);
        if max <= 0.0 {
            // Nothing to scroll, but the pointer is over panel content —
            // swallowing the wheel keeps the camera from zooming "through"
            // a panel the user is reading.
            return true;
        }
        let cur = self.panel_scroll.get(&panel).copied().unwrap_or(0.0);
        let next = (cur + delta).clamp(0.0, max);
        if (next - cur).abs() > 0.01 {
            // Scrolling the transcript away from the bottom pauses
            // follow-latest; scrolling back to the bottom resumes it.
            if panel == "transcript" {
                self.transcript_follow = next >= max - 4.0;
            }
            self.panel_scroll.insert(panel, next);
            self.hud_dirty = true;
        }
        true
    }

    /// Keyboard paging for the most relevant open panel: the transcript
    /// viewer when open, else the selected system panel. `pages` is in
    /// viewport fractions (negative = up). Returns whether a scrollable
    /// surface existed.
    pub(crate) fn scroll_focused_panel(&mut self, pages: f32) -> bool {
        let key = if self.transcript.is_some() {
            Some("transcript".to_string())
        } else {
            self.selected_id.clone()
        };
        let Some(key) = key else {
            return false;
        };
        let Some(zone) = self.scroll_zones.iter().find(|z| z.panel == key) else {
            return false;
        };
        let max = (zone.content_h - zone.h).max(0.0);
        if max <= 0.0 {
            return false;
        }
        let cur = self.panel_scroll.get(&key).copied().unwrap_or(0.0);
        let next = (cur + zone.h * pages).clamp(0.0, max);
        if (next - cur).abs() > 0.01 {
            self.panel_scroll.insert(key, next);
            self.hud_dirty = true;
        }
        true
    }

    /// Current clamped scroll offset for a panel key.
    pub(crate) fn scroll_offset(&self, panel: &str, content_h: f32, viewport_h: f32) -> f32 {
        let max = (content_h - viewport_h).max(0.0);
        self.panel_scroll
            .get(panel)
            .copied()
            .unwrap_or(0.0)
            .clamp(0.0, max)
    }

    /// Scrollbar drag descriptor when the press sits on a panel's
    /// scrollbar gutter (the right edge strip of a scrollable zone).
    pub(crate) fn scrollbar_drag_at(&self, x: f32, y: f32) -> Option<ScrollDrag> {
        let zone = self
            .scroll_zones
            .iter()
            .rev()
            .find(|z| y >= z.y && y <= z.y + z.h && x >= z.x + z.w - SCROLLBAR_GUTTER && x <= z.x + z.w + 2.0)?;
        let max = (zone.content_h - zone.h).max(0.0);
        if max <= 0.0 {
            return None;
        }
        Some(ScrollDrag {
            panel: zone.panel.clone(),
            track_y: zone.y,
            track_h: zone.h,
            content_h: zone.content_h,
            viewport_h: zone.h,
        })
    }

    /// Move a dragged scrollbar thumb so it tracks pointer y (the thumb
    /// center follows the pointer — a press on the track jumps there).
    pub(crate) fn apply_scroll_drag(&mut self, drag: &ScrollDrag, y: f32) {
        let max = (drag.content_h - drag.viewport_h).max(0.0);
        if max <= 0.0 {
            return;
        }
        let (thumb_h, _) = scrollbar_thumb(drag.track_h, drag.content_h, drag.viewport_h, 0.0);
        let travel = (drag.track_h - thumb_h).max(1.0);
        let t = ((y - drag.track_y - thumb_h * 0.5) / travel).clamp(0.0, 1.0);
        let next = max * t;
        let cur = self.panel_scroll.get(&drag.panel).copied().unwrap_or(0.0);
        if (next - cur).abs() > 0.01 {
            if drag.panel == "transcript" {
                self.transcript_follow = next >= max - 4.0;
            }
            self.panel_scroll.insert(drag.panel.clone(), next);
            self.hud_dirty = true;
        }
    }

    /// Slider scrub descriptor when the point sits on a slider track zone.
    pub(crate) fn slider_at(&self, x: f32, y: f32) -> Option<ActiveSlider> {
        let zone = self.zone_at(x, y)?;
        match zone.action {
            HitAction::ViewSlider { key } => Some(ActiveSlider {
                key,
                track_x: zone.x,
                track_w: zone.w,
            }),
            _ => None,
        }
    }

    /// Map client coordinates into canvas CSS coordinates, reusing a cached
    /// canvas origin so pointermove storms do not force layout. The cache is
    /// dropped on resize, scroll, and tab activation.
    pub(crate) fn event_xy(&mut self, client_x: f64, client_y: f64) -> (f32, f32) {
        let (left, top) = match self.canvas_origin {
            Some(origin) => origin,
            None => {
                let rect = self.hud_canvas.get_bounding_client_rect();
                let origin = (rect.left(), rect.top());
                self.canvas_origin = Some(origin);
                origin
            }
        };
        ((client_x - left) as f32, (client_y - top) as f32)
    }

    pub(crate) fn mark_input(&mut self) {
        self.last_input_ms = now_ms();
    }
}

#[derive(Clone)]
pub(crate) enum HitAction {
    Layout(LayoutName),
    Noop,
    Select(String),
    ClosePanel,
    /// Close the transcript/diff viewer (it floats independently of the
    /// focus-panel selection).
    CloseTranscript,
    ActivityAction { action: String, id: String },
    ControlsAction { action: String },
    /// Session row / pill ops, emitted as the dashboard's existing
    /// `{type:'session_action', action, session_id}` shape
    /// (stationHandleSessionAction: focus/resume/stop/copy/fork/...).
    SessionAction { action: String, id: String },
    /// Managed-context record ops (`{type:'managed_action', action, id}`).
    ManagedAction { action: String, id: String },
    /// Changed-file ops (`{type:'changes_action', action, path}`).
    ChangesAction { action: String, path: String },
    /// Context part/replay ops (`{type:'context_action', action, id}`).
    ContextAction { action: String, id: String },
    /// Display-runway lane ops (`{type:'display_runway_action', action,
    /// lane_id}`), routed by the dashboard per lane type.
    RunwayAction { action: String, lane_id: String },
    /// Composer surface ops. open-send/open-launch/close mutate local
    /// state and notify the dashboard (which owns the DOM input overlay);
    /// send/launch/target pass through for the dashboard to submit.
    Composer { op: &'static str },
    /// Approve/deny pill in the agent focus panel. Emits the dashboard's
    /// existing `{type:'approval', host_id, approval_id, decision}` action
    /// (handleStationAction routes local approvals to `app.send_approval`
    /// and peer approvals to `resolvePeerApproval`).
    Approval {
        host_id: String,
        approval_id: String,
        decision: &'static str,
    },
    /// Discrete view-settings toggle (mood pills). Applied locally for
    /// instant feedback, then emitted as `{type:'view_set', key, value}` so
    /// the dashboard persists the draft and re-applies via `set_visuals`.
    ViewSet {
        key: &'static str,
        value: &'static str,
    },
    /// Drag-aware view-settings slider track. The owning `HitZone`'s rect
    /// is the track geometry; pointer x within it maps onto the key's
    /// range. Scrubbing applies locally per move; the final value is
    /// emitted as `{type:'view_set', ...}` on pointer-up.
    ViewSlider { key: ViewSliderKey },
}

/// The continuously adjustable view settings exposed as slider tracks in
/// the View focus panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ViewSliderKey {
    Fov,
    Motion,
    Ar,
    Density,
}

impl ViewSliderKey {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Fov => "fov",
            Self::Motion => "motion",
            Self::Ar => "ar",
            Self::Density => "density",
        }
    }

    /// Inclusive value range, matching the clamps in `set_visuals` and the
    /// dashboard's `stationViewSet`.
    pub(crate) fn range(self) -> (f32, f32) {
        match self {
            Self::Fov => (35.0, 85.0),
            Self::Motion => (0.0, 2.0),
            Self::Ar => (0.0, 1.0),
            Self::Density => (0.5, 1.8),
        }
    }

    /// Map a pointer x within a track rect onto the key's value range.
    pub(crate) fn value_at(self, x: f32, track_x: f32, track_w: f32) -> f32 {
        let (min, max) = self.range();
        let t = ((x - track_x) / track_w.max(1.0)).clamp(0.0, 1.0);
        min + (max - min) * t
    }

    /// Normalized [0,1] position of a value within the range.
    pub(crate) fn t_of(self, value: f32) -> f32 {
        let (min, max) = self.range();
        ((value - min) / (max - min).max(0.0001)).clamp(0.0, 1.0)
    }
}

/// In-flight slider scrub: the track geometry captured at pointer-down so
/// every subsequent move maps x consistently even when the pointer leaves
/// the track rect.
#[derive(Clone, Copy)]
pub(crate) struct ActiveSlider {
    pub(crate) key: ViewSliderKey,
    pub(crate) track_x: f32,
    pub(crate) track_w: f32,
}

/// Stable name for a hit zone, used by the agentic introspection API
/// (`debug_json` / `hotspot_rects` / `activate`). `None` for inert zones
/// (panel bodies that only swallow clicks). Select zones use the node id
/// itself (`system:peers`, `host:alpha`, agent ids); layout buttons use
/// the `layout:<name>` convention the dashboard's accessibility overlay
/// already speaks.
pub(crate) fn zone_name(action: &HitAction) -> Option<String> {
    fn scoped(scope: &str, action: &str, id: &str) -> String {
        if id.is_empty() {
            format!("{scope}:{action}")
        } else {
            format!("{scope}:{action}:{id}")
        }
    }
    match action {
        HitAction::Layout(layout) => Some(format!("layout:{}", layout.label())),
        HitAction::Select(id) => Some(id.clone()),
        HitAction::ClosePanel => Some("close-panel".to_string()),
        HitAction::CloseTranscript => Some("close-transcript".to_string()),
        HitAction::ActivityAction { action, id } => Some(scoped("activity", action, id)),
        HitAction::ControlsAction { action } => Some(format!("controls:{action}")),
        HitAction::SessionAction { action, id } => Some(scoped("session", action, id)),
        HitAction::ManagedAction { action, id } => Some(scoped("managed", action, id)),
        HitAction::ChangesAction { action, path } => Some(scoped("changes", action, path)),
        HitAction::ContextAction { action, id } => Some(scoped("ctx", action, id)),
        HitAction::RunwayAction { action, lane_id } => Some(scoped("runway", action, lane_id)),
        HitAction::Composer { op } => Some(format!("composer:{op}")),
        HitAction::Approval {
            host_id,
            approval_id,
            decision,
        } => Some(format!(
            "approval:{decision}:{}",
            if approval_id.is_empty() {
                host_id
            } else {
                approval_id
            }
        )),
        HitAction::ViewSet { key, value } => Some(format!("view:{key}:{value}")),
        HitAction::ViewSlider { key } => Some(format!("view:{}", key.name())),
        HitAction::Noop => None,
    }
}

/// Coarse kind tag reported alongside each named zone in `debug_json`.
pub(crate) fn zone_kind(action: &HitAction) -> &'static str {
    match action {
        HitAction::Layout(_) => "layout",
        HitAction::Select(_) => "select",
        HitAction::ClosePanel => "close-panel",
        HitAction::CloseTranscript => "close-transcript",
        HitAction::ActivityAction { .. } => "activity",
        HitAction::ControlsAction { .. } => "controls",
        HitAction::SessionAction { .. } => "session",
        HitAction::ManagedAction { .. } => "managed",
        HitAction::ChangesAction { .. } => "changes",
        HitAction::ContextAction { .. } => "ctx",
        HitAction::RunwayAction { .. } => "runway",
        HitAction::Composer { .. } => "composer",
        HitAction::Approval { .. } => "approval",
        HitAction::ViewSet { .. } => "view-set",
        HitAction::ViewSlider { .. } => "slider",
        HitAction::Noop => "panel",
    }
}

/// The system/layout hotspot rects currently drawn (CSS px), one entry
/// per name with the same precedence a click has (later-drawn zones
/// win). This is the truthful geometry source for the dashboard's
/// accessibility overlay, replacing its hand-mirrored constants.
pub(crate) fn hotspot_rects_from_zones(zones: &[HitZone]) -> Vec<(String, f32, f32, f32, f32)> {
    let mut out: Vec<(String, f32, f32, f32, f32)> = Vec::new();
    for zone in zones {
        let is_hotspot = match &zone.action {
            HitAction::Layout(_) => true,
            HitAction::Select(id) => id.starts_with("system:"),
            _ => false,
        };
        if !is_hotspot {
            continue;
        }
        let Some(name) = zone_name(&zone.action) else {
            continue;
        };
        let entry = (name, zone.x, zone.y, zone.w, zone.h);
        match out.iter_mut().find(|existing| existing.0 == entry.0) {
            Some(existing) => *existing = entry,
            None => out.push(entry),
        }
    }
    out
}

/// Resolve an `activate` name to the action a click on that zone would
/// dispatch, honoring hit-test precedence (last-drawn zone wins).
pub(crate) fn zone_action_by_name(zones: &[HitZone], name: &str) -> Option<HitAction> {
    zones
        .iter()
        .rev()
        .find(|zone| zone_name(&zone.action).as_deref() == Some(name))
        .map(|zone| zone.action.clone())
}

pub(crate) struct HitZone {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) w: f32,
    pub(crate) h: f32,
    pub(crate) action: HitAction,
}

impl HitZone {
    pub(crate) fn new(x: f32, y: f32, w: f32, h: f32, action: HitAction) -> Self {
        Self { x, y, w, h, action }
    }
}

pub(crate) struct PointerDrag {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) last_x: f32,
    pub(crate) last_y: f32,
    pub(crate) moved: bool,
    pub(crate) pending_action: Option<HitAction>,
    /// Set when the press landed on a slider track: the drag scrubs the
    /// slider instead of orbiting the camera or arming a click.
    pub(crate) slider: Option<ActiveSlider>,
    /// Set when the press landed on a panel scrollbar: the drag moves
    /// that panel's scroll thumb.
    pub(crate) scrollbar: Option<ScrollDrag>,
}

/// Width of the grabbable scrollbar gutter on a scrollable panel's right
/// edge (the drawn track is thinner; the hit area is forgiving).
pub(crate) const SCROLLBAR_GUTTER: f32 = 16.0;

/// One scrollable viewport registered during the HUD paint (cleared and
/// refilled like `hit_zones`). `panel` keys the persistent scroll offset.
pub(crate) struct ScrollZone {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) w: f32,
    pub(crate) h: f32,
    pub(crate) panel: String,
    pub(crate) content_h: f32,
}

/// In-flight scrollbar drag: the track geometry captured at pointer-down.
#[derive(Clone)]
pub(crate) struct ScrollDrag {
    pub(crate) panel: String,
    pub(crate) track_y: f32,
    pub(crate) track_h: f32,
    pub(crate) content_h: f32,
    pub(crate) viewport_h: f32,
}

/// Thumb geometry for a scrollbar: `(thumb_h, thumb_y_offset)` within a
/// track of `track_h` showing `viewport_h` of `content_h` at `offset`.
pub(crate) fn scrollbar_thumb(
    track_h: f32,
    content_h: f32,
    viewport_h: f32,
    offset: f32,
) -> (f32, f32) {
    let visible = (viewport_h / content_h.max(1.0)).clamp(0.05, 1.0);
    let thumb_h = (track_h * visible).max(22.0).min(track_h);
    let max = (content_h - viewport_h).max(0.0);
    let t = if max > 0.0 { (offset / max).clamp(0.0, 1.0) } else { 0.0 };
    (thumb_h, (track_h - thumb_h) * t)
}

#[derive(Clone, Copy)]
pub(crate) struct PinchZoom {
    pub(crate) start_distance: f32,
    pub(crate) start_camera_distance: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_names_cover_every_action_kind() {
        assert_eq!(
            zone_name(&HitAction::Layout(LayoutName::Orbital)).as_deref(),
            Some("layout:orbital")
        );
        assert_eq!(
            zone_name(&HitAction::Layout(LayoutName::Constellation)).as_deref(),
            Some("layout:constellation")
        );
        assert_eq!(
            zone_name(&HitAction::Select("system:peers".into())).as_deref(),
            Some("system:peers")
        );
        assert_eq!(zone_name(&HitAction::ClosePanel).as_deref(), Some("close-panel"));
        assert_eq!(
            zone_name(&HitAction::ActivityAction {
                action: "copy-visible".into(),
                id: String::new(),
            })
            .as_deref(),
            Some("activity:copy-visible")
        );
        assert_eq!(
            zone_name(&HitAction::ActivityAction {
                action: "open".into(),
                id: "evt-1".into(),
            })
            .as_deref(),
            Some("activity:open:evt-1")
        );
        assert_eq!(
            zone_name(&HitAction::ControlsAction {
                action: "shared-view-take-input".into(),
            })
            .as_deref(),
            Some("controls:shared-view-take-input")
        );
        assert_eq!(zone_name(&HitAction::Noop), None);
        assert_eq!(zone_kind(&HitAction::Noop), "panel");
        assert_eq!(zone_kind(&HitAction::Layout(LayoutName::Orbital)), "layout");
        assert_eq!(zone_kind(&HitAction::Select(String::new())), "select");
    }

    #[test]
    fn row_action_zone_names_scope_action_and_id() {
        assert_eq!(
            zone_name(&HitAction::SessionAction {
                action: "station-log".into(),
                id: "sid-1".into(),
            })
            .as_deref(),
            Some("session:station-log:sid-1")
        );
        assert_eq!(
            zone_name(&HitAction::ManagedAction {
                action: "record-inspect".into(),
                id: "rec-9".into(),
            })
            .as_deref(),
            Some("managed:record-inspect:rec-9")
        );
        assert_eq!(
            zone_name(&HitAction::ChangesAction {
                action: "station-diff".into(),
                path: "src/main.rs".into(),
            })
            .as_deref(),
            Some("changes:station-diff:src/main.rs")
        );
        assert_eq!(
            zone_name(&HitAction::RunwayAction {
                action: "open".into(),
                lane_id: "lane-2".into(),
            })
            .as_deref(),
            Some("runway:open:lane-2")
        );
        assert_eq!(
            zone_name(&HitAction::Composer { op: "open-send" }).as_deref(),
            Some("composer:open-send")
        );
        assert_eq!(
            zone_name(&HitAction::CloseTranscript).as_deref(),
            Some("close-transcript")
        );
        assert_eq!(zone_kind(&HitAction::Composer { op: "send" }), "composer");
        assert_eq!(
            zone_kind(&HitAction::SessionAction {
                action: "focus".into(),
                id: String::new(),
            }),
            "session"
        );
        // Row/composer zones stay out of the system hotspot overlay.
        let zones = vec![HitZone::new(
            0.0,
            0.0,
            10.0,
            10.0,
            HitAction::SessionAction {
                action: "focus".into(),
                id: "s".into(),
            },
        )];
        assert!(hotspot_rects_from_zones(&zones).is_empty());
    }

    #[test]
    fn scrollbar_thumb_scales_with_content_and_clamps() {
        // Content fits: thumb fills the track, no offset.
        let (h, off) = scrollbar_thumb(200.0, 100.0, 200.0, 0.0);
        assert_eq!((h, off), (200.0, 0.0));
        // 4x content: quarter thumb, offset tracks position.
        let (h, off) = scrollbar_thumb(200.0, 800.0, 200.0, 0.0);
        assert_eq!(h, 50.0);
        assert_eq!(off, 0.0);
        let (_, off) = scrollbar_thumb(200.0, 800.0, 200.0, 600.0);
        assert_eq!(off, 150.0);
        let (_, off) = scrollbar_thumb(200.0, 800.0, 200.0, 9000.0);
        assert_eq!(off, 150.0);
        // Tiny viewport keeps a grabbable minimum thumb.
        let (h, _) = scrollbar_thumb(100.0, 100_000.0, 100.0, 0.0);
        assert!(h >= 22.0);
    }

    #[test]
    fn approval_zones_name_by_approval_id_with_host_fallback() {
        let with_id = HitAction::Approval {
            host_id: "peer-1".into(),
            approval_id: "ap-42".into(),
            decision: "approve",
        };
        assert_eq!(zone_name(&with_id).as_deref(), Some("approval:approve:ap-42"));
        assert_eq!(zone_kind(&with_id), "approval");
        // Local primary approvals carry no id; the host disambiguates.
        let local = HitAction::Approval {
            host_id: "local".into(),
            approval_id: String::new(),
            decision: "deny",
        };
        assert_eq!(zone_name(&local).as_deref(), Some("approval:deny:local"));
        // Approval pills are panel controls, not overlay hotspots.
        let zones = vec![HitZone::new(0.0, 0.0, 10.0, 10.0, with_id)];
        assert!(hotspot_rects_from_zones(&zones).is_empty());
        assert!(matches!(
            zone_action_by_name(&zones, "approval:approve:ap-42"),
            Some(HitAction::Approval { decision: "approve", .. })
        ));
    }

    #[test]
    fn view_zones_name_sliders_and_toggles() {
        let slider = HitAction::ViewSlider {
            key: ViewSliderKey::Fov,
        };
        assert_eq!(zone_name(&slider).as_deref(), Some("view:fov"));
        assert_eq!(zone_kind(&slider), "slider");
        let toggle = HitAction::ViewSet {
            key: "mood",
            value: "calm",
        };
        assert_eq!(zone_name(&toggle).as_deref(), Some("view:mood:calm"));
        assert_eq!(zone_kind(&toggle), "view-set");
        // View controls live in the focus panel, not the hotspot overlay.
        let zones = vec![
            HitZone::new(0.0, 0.0, 100.0, 10.0, slider),
            HitZone::new(0.0, 20.0, 60.0, 10.0, toggle),
        ];
        assert!(hotspot_rects_from_zones(&zones).is_empty());
        assert!(matches!(
            zone_action_by_name(&zones, "view:fov"),
            Some(HitAction::ViewSlider { key: ViewSliderKey::Fov })
        ));
    }

    #[test]
    fn view_slider_keys_map_pointer_x_onto_their_ranges() {
        for key in [
            ViewSliderKey::Fov,
            ViewSliderKey::Motion,
            ViewSliderKey::Ar,
            ViewSliderKey::Density,
        ] {
            let (min, max) = key.range();
            assert_eq!(key.value_at(100.0, 100.0, 200.0), min, "{key:?} left edge");
            assert_eq!(key.value_at(300.0, 100.0, 200.0), max, "{key:?} right edge");
            let mid = key.value_at(200.0, 100.0, 200.0);
            assert!(
                (mid - (min + max) / 2.0).abs() < 1e-4,
                "{key:?} midpoint: {mid}"
            );
            // Out-of-track presses clamp to the range.
            assert_eq!(key.value_at(0.0, 100.0, 200.0), min);
            assert_eq!(key.value_at(900.0, 100.0, 200.0), max);
            // t_of inverts value_at across the range.
            assert!((key.t_of(mid) - 0.5).abs() < 1e-4);
        }
    }

    #[test]
    fn hotspot_rects_filter_and_dedupe_last_wins() {
        let zones = vec![
            HitZone::new(96.0, 10.0, 78.0, 23.0, HitAction::Layout(LayoutName::Orbital)),
            // Inert and non-system zones are not overlay hotspots.
            HitZone::new(0.0, 0.0, 10.0, 10.0, HitAction::Noop),
            HitZone::new(0.0, 0.0, 10.0, 10.0, HitAction::ClosePanel),
            HitZone::new(
                1.0,
                1.0,
                10.0,
                10.0,
                HitAction::Select("host:alpha".into()),
            ),
            // The orbital node zone is superseded by the matrix zone for
            // the same target, mirroring click precedence.
            HitZone::new(
                100.0,
                100.0,
                200.0,
                60.0,
                HitAction::Select("system:peers".into()),
            ),
            HitZone::new(
                30.0,
                400.0,
                120.0,
                25.0,
                HitAction::Select("system:peers".into()),
            ),
        ];
        let rects = hotspot_rects_from_zones(&zones);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].0, "layout:orbital");
        let peers = &rects[1];
        assert_eq!(
            (peers.0.as_str(), peers.1, peers.2, peers.3, peers.4),
            ("system:peers", 30.0, 400.0, 120.0, 25.0)
        );
    }

    #[test]
    fn zone_action_lookup_resolves_names_and_rejects_unknown() {
        let zones = vec![
            HitZone::new(0.0, 0.0, 10.0, 10.0, HitAction::Select("system:view".into())),
            HitZone::new(
                5.0,
                5.0,
                10.0,
                10.0,
                HitAction::ActivityAction {
                    action: "send".into(),
                    id: String::new(),
                },
            ),
        ];
        // Unknown names resolve to nothing.
        assert!(zone_action_by_name(&zones, "system:bogus").is_none());
        assert!(zone_action_by_name(&zones, "").is_none());
        assert!(matches!(
            zone_action_by_name(&zones, "system:view"),
            Some(HitAction::Select(id)) if id == "system:view"
        ));
        assert!(matches!(
            zone_action_by_name(&zones, "activity:send"),
            Some(HitAction::ActivityAction { action, .. }) if action == "send"
        ));
    }
}
