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
                    });
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
                if s.active_pointers.len() >= 2 {
                    s.apply_pinch();
                    s.mark_input();
                    s.set_cursor("drag");
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
                    if let Some(action) = drag.pending_action {
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

        // Leaving the canvas must drop the hover-lit state on pills/tiles.
        let leave_inner = inner.clone();
        let leave = Closure::wrap(Box::new(move |_event: Event| {
            {
                let mut s = leave_inner.borrow_mut();
                if s.hover_xy.take().is_none() {
                    return;
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
                s.distance = (s.distance + e.delta_y() as f32 * 0.014).clamp(4.2, 25.0);
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
            let used = {
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
                    "1" => s.set_layout(LayoutName::Orbital),
                    "2" => s.set_layout(LayoutName::Constellation),
                    _ => used = false,
                }
                if used {
                    e.prevent_default();
                    s.mark_input();
                }
                used
            };
            if used {
                StationInner::schedule_frame(&key_inner);
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
                let mut s = orientation_inner.borrow_mut();
                let gamma = e.gamma().unwrap_or(0.0) as f32;
                let beta = e.beta().unwrap_or(0.0) as f32;
                s.ar_x = (gamma / 45.0).clamp(-1.0, 1.0);
                s.ar_y = (beta / 60.0).clamp(-1.0, 1.0);
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
            HitAction::Select(id) => {
                self.selected_id = Some(id);
                self.hud_dirty = true;
                None
            }
            HitAction::Noop => None,
            HitAction::ActivityAction { action, id } => Some(serde_json::json!({
                    "type": "activity_action",
                    "action": action,
                    "id": id,
            })),
            HitAction::ControlsAction { action } => Some(serde_json::json!({
                    "type": "controls_action",
                    "action": action,
            })),
        }
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
    ActivityAction { action: String, id: String },
    ControlsAction { action: String },
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
}

#[derive(Clone, Copy)]
pub(crate) struct PinchZoom {
    pub(crate) start_distance: f32,
    pub(crate) start_camera_distance: f32,
}
