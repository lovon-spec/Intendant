//! Wayland display backend using XDG Desktop Portal (ashpd) for screen capture
//! and input injection, and PipeWire for frame acquisition.
//!
//! The PipeWire main loop runs on a dedicated `std::thread` (it is not `Send`).
//! Communication with the tokio runtime is via a bounded `mpsc` channel for
//! frames and an `AtomicBool` for shutdown signaling.

use super::{DisplayBackend, Frame, FrameFormat, InputEvent};
use crate::error::CallerError;
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::{PersistMode, Session};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};

/// Portal session handle + PipeWire capture thread.
///
/// Stores the `RemoteDesktop` proxy and its `Session` handle so that
/// `inject_input()` can call the `notify_*` D-Bus methods on the original
/// portal session.  Both types carry a `'static` lifetime because the
/// underlying `zbus::Connection` is held in a global `OnceLock`.
struct PortalSession {
    /// The PipeWire node ID (used for pointer_motion_absolute stream param).
    node_id: u32,
    pw_thread: Option<std::thread::JoinHandle<()>>,
    /// The RemoteDesktop D-Bus proxy, kept alive for input injection.
    remote_desktop: RemoteDesktop<'static>,
    /// The session handle obtained from `create_session()`.
    session: Session<'static, RemoteDesktop<'static>>,
}

/// Wayland screen capture and input injection backend.
///
/// Uses the XDG Desktop Portal `RemoteDesktop` + `ScreenCast` interfaces for a
/// combined session that provides both keyboard/pointer injection and PipeWire
/// video frames.
pub struct WaylandBackend {
    portal_session: Mutex<Option<PortalSession>>,
    resolution: RwLock<(u32, u32)>,
    shutdown: Arc<AtomicBool>,
}

impl WaylandBackend {
    /// Create a new backend. Resolution is populated once capture starts.
    pub fn new() -> Self {
        Self {
            portal_session: Mutex::new(None),
            resolution: RwLock::new((0, 0)),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl DisplayBackend for WaylandBackend {
    async fn start_capture(
        &self,
        _fps: u32,
    ) -> Result<mpsc::Receiver<Frame>, CallerError> {
        self.shutdown.store(false, Ordering::SeqCst);

        // --- Portal session: RemoteDesktop + ScreenCast combined ---
        let remote_desktop = RemoteDesktop::new()
            .await
            .map_err(|e| CallerError::Display(format!("RemoteDesktop proxy: {e}")))?;
        let screencast = Screencast::new()
            .await
            .map_err(|e| CallerError::Display(format!("ScreenCast proxy: {e}")))?;

        let session = remote_desktop
            .create_session()
            .await
            .map_err(|e| CallerError::Display(format!("create session: {e}")))?;

        remote_desktop
            .select_devices(
                &session,
                DeviceType::Keyboard | DeviceType::Pointer,
                None,
                PersistMode::DoNot,
            )
            .await
            .map_err(|e| CallerError::Display(format!("select devices: {e}")))?;

        screencast
            .select_sources(
                &session,
                CursorMode::Embedded,
                SourceType::Monitor | SourceType::Window,
                true,
                None,
                PersistMode::DoNot,
            )
            .await
            .map_err(|e| CallerError::Display(format!("select sources: {e}")))?;

        let started = remote_desktop
            .start(&session, None)
            .await
            .map_err(|e| CallerError::Display(format!("start request: {e}")))?
            .response()
            .map_err(|e| CallerError::Display(format!("start response: {e}")))?;

        // Extract PipeWire node ID from the screencast streams.
        let streams = started
            .streams()
            .ok_or_else(|| {
                CallerError::Display("no screencast streams returned by portal".to_string())
            })?;
        if streams.is_empty() {
            return Err(CallerError::Display(
                "empty stream list from portal".to_string(),
            ));
        }

        let stream = &streams[0];
        let node_id = stream.pipe_wire_node_id();
        let (width, height) = match stream.size() {
            Some((w, h)) => (w as u32, h as u32),
            None => (1920, 1080),
        };

        *self.resolution.write().await = (width, height);

        // Get PipeWire fd via the screencast portal.
        let pw_fd = screencast
            .open_pipe_wire_remote(&session)
            .await
            .map_err(|e| CallerError::Display(format!("pipewire fd: {e}")))?;

        // --- Bounded frame channel: PipeWire thread -> tokio ---
        let (tx, rx) = mpsc::channel::<Frame>(4);

        // --- Spawn dedicated PipeWire thread ---
        let shutdown_flag = Arc::clone(&self.shutdown);
        let pw_thread = std::thread::spawn(move || {
            run_pipewire_capture(pw_fd, node_id, tx, shutdown_flag, width, height);
        });

        // Store the RemoteDesktop proxy and session handle so inject_input()
        // can call notify_* methods on the original portal session.
        *self.portal_session.lock().await = Some(PortalSession {
            node_id,
            pw_thread: Some(pw_thread),
            remote_desktop,
            session,
        });

        Ok(rx)
    }

    async fn stop_capture(&self) {
        self.shutdown.store(true, Ordering::SeqCst);

        if let Some(mut ps) = self.portal_session.lock().await.take() {
            if let Some(handle) = ps.pw_thread.take() {
                let _ = handle.join();
            }
            // Explicitly close the portal session so the GNOME sharing
            // indicator disappears immediately on revoke.
            let _ = ps.session.close().await;
        }
    }

    async fn inject_input(&self, event: InputEvent) -> Result<(), CallerError> {
        let (width, height) = *self.resolution.read().await;
        let guard = self.portal_session.lock().await;
        let ps = guard.as_ref().ok_or_else(|| {
            CallerError::Display("no active portal session for input injection".to_string())
        })?;

        let rd = &ps.remote_desktop;
        let session = &ps.session;
        let node_id = ps.node_id;

        match event {
            InputEvent::KeyDown { ref code, .. } => {
                if let Some(keycode) = super::keymap::dom_code_to_evdev(code) {
                    rd.notify_keyboard_keycode(session, keycode as i32, KeyState::Pressed)
                        .await
                        .map_err(|e| CallerError::Display(format!("key inject: {e}")))?;
                }
            }
            InputEvent::KeyUp { ref code, .. } => {
                if let Some(keycode) = super::keymap::dom_code_to_evdev(code) {
                    rd.notify_keyboard_keycode(session, keycode as i32, KeyState::Released)
                        .await
                        .map_err(|e| CallerError::Display(format!("key inject: {e}")))?;
                }
            }
            InputEvent::MouseMove { x, y, .. } => {
                rd.notify_pointer_motion_absolute(
                    session,
                    node_id,
                    x * width as f64,
                    y * height as f64,
                )
                .await
                .map_err(|e| CallerError::Display(format!("pointer inject: {e}")))?;
            }
            InputEvent::MouseDown { x, y, b } => {
                // Move to position first (best-effort).
                let _ = rd
                    .notify_pointer_motion_absolute(
                        session,
                        node_id,
                        x * width as f64,
                        y * height as f64,
                    )
                    .await;
                // Linux evdev button codes: BTN_LEFT=0x110, BTN_MIDDLE=0x112, BTN_RIGHT=0x111
                let button_code: i32 = match b {
                    0 => 0x110,
                    1 => 0x112,
                    2 => 0x111,
                    _ => 0x110,
                };
                rd.notify_pointer_button(session, button_code, KeyState::Pressed)
                    .await
                    .map_err(|e| CallerError::Display(format!("button inject: {e}")))?;
            }
            InputEvent::MouseUp { x, y, b } => {
                let _ = rd
                    .notify_pointer_motion_absolute(
                        session,
                        node_id,
                        x * width as f64,
                        y * height as f64,
                    )
                    .await;
                let button_code: i32 = match b {
                    0 => 0x110,
                    1 => 0x112,
                    2 => 0x111,
                    _ => 0x110,
                };
                rd.notify_pointer_button(session, button_code, KeyState::Released)
                    .await
                    .map_err(|e| CallerError::Display(format!("button inject: {e}")))?;
            }
            InputEvent::Scroll { dx, dy, .. } => {
                // Use discrete axis scrolling: convert raw deltas to integer
                // steps. Vertical scroll (dy) maps to Axis::Vertical, horizontal
                // (dx) to Axis::Horizontal.
                if dy.abs() > f64::EPSILON {
                    let steps = dy.round() as i32;
                    if steps != 0 {
                        rd.notify_pointer_axis_discrete(session, Axis::Vertical, steps)
                            .await
                            .map_err(|e| {
                                CallerError::Display(format!("scroll inject: {e}"))
                            })?;
                    }
                }
                if dx.abs() > f64::EPSILON {
                    let steps = dx.round() as i32;
                    if steps != 0 {
                        rd.notify_pointer_axis_discrete(session, Axis::Horizontal, steps)
                            .await
                            .map_err(|e| {
                                CallerError::Display(format!("scroll inject: {e}"))
                            })?;
                    }
                }
            }
        }
        Ok(())
    }

    fn resolution(&self) -> (u32, u32) {
        self.resolution
            .try_read()
            .map(|r| *r)
            .unwrap_or((0, 0))
    }

    fn kind(&self) -> &'static str {
        "wayland"
    }
}

/// Run the PipeWire main loop on a dedicated OS thread.
///
/// This function blocks until the `shutdown` flag is set or the PipeWire
/// connection is lost. Frames are sent to `tx` via `try_send()` -- if the
/// channel is full the frame is dropped (backpressure).
fn run_pipewire_capture(
    pw_fd: std::os::fd::OwnedFd,
    node_id: u32,
    tx: mpsc::Sender<Frame>,
    shutdown: Arc<AtomicBool>,
    width: u32,
    height: u32,
) {
    use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use pipewire::spa::param::video::VideoFormat;
    use pipewire::spa::param::ParamType;
    use pipewire::spa::pod::{Object, Property, PropertyFlags, Value};
    use pipewire::spa::utils::{Id, SpaTypes};

    pipewire::init();

    let mainloop = match pipewire::main_loop::MainLoop::new(None) {
        Ok(ml) => ml,
        Err(e) => {
            eprintln!("[display/wayland] pipewire MainLoop::new failed: {e}");
            return;
        }
    };

    let context = match pipewire::context::Context::new(&mainloop) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[display/wayland] pipewire Context::new failed: {e}");
            return;
        }
    };

    let core = match context.connect_fd(pw_fd, None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[display/wayland] pipewire connect_fd failed: {e}");
            return;
        }
    };

    let stream = match pipewire::stream::Stream::new(
        &core,
        "intendant-capture",
        pipewire::properties::properties! {
            *pipewire::keys::MEDIA_TYPE => "Video",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Screen",
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[display/wayland] pipewire Stream::new failed: {e}");
            return;
        }
    };

    // Stream listener: process frames from the PipeWire buffer.
    let tx_clone = tx.clone();
    let _listener = stream
        .add_local_listener()
        .process(move |stream_ref, _: &mut ()| {
            if let Some(mut buffer) = stream_ref.dequeue_buffer() {
                if let Some(buf) = buffer.datas_mut().first_mut() {
                    // Read chunk metadata before taking the mutable data borrow.
                    let stride = buf.chunk().stride() as u32;

                    if let Some(data) = buf.data() {
                        // Use the stream's actual dimensions, not stride-derived.
                        // stride may include row padding for memory alignment,
                        // so stride / 4 can be wider than the real pixel width.
                        let frame_w = width;
                        let frame_h = height;

                        let frame = Frame {
                            data: data.to_vec(),
                            format: FrameFormat::Bgra,
                            width: frame_w,
                            height: frame_h,
                            stride,
                            timestamp: std::time::Instant::now(),
                        };

                        // Backpressure: drop frame if channel is full.
                        let _ = tx_clone.try_send(frame);
                    }
                }
            }
        })
        .register()
        .expect("pipewire stream listener");

    // Build format parameters for the stream.
    let format_pod_bytes = pipewire::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(vec![0u8; 1024]),
        &Value::Object(Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: ParamType::EnumFormat.as_raw(),
            properties: vec![
                Property {
                    key: FormatProperties::MediaType.as_raw(),
                    flags: PropertyFlags::empty(),
                    value: Value::Id(Id(MediaType::Video.as_raw())),
                },
                Property {
                    key: FormatProperties::MediaSubtype.as_raw(),
                    flags: PropertyFlags::empty(),
                    value: Value::Id(Id(MediaSubtype::Raw.as_raw())),
                },
                Property {
                    key: FormatProperties::VideoFormat.as_raw(),
                    flags: PropertyFlags::empty(),
                    value: Value::Id(Id(VideoFormat::BGRx.as_raw())),
                },
            ],
        }),
    )
    .expect("pipewire format pod serialization")
    .0
    .into_inner();

    let format_pod =
        pipewire::spa::pod::Pod::from_bytes(&format_pod_bytes).expect("pipewire pod from bytes");

    stream
        .connect(
            pipewire::spa::utils::Direction::Input,
            Some(node_id),
            pipewire::stream::StreamFlags::AUTOCONNECT | pipewire::stream::StreamFlags::MAP_BUFFERS,
            &mut [format_pod],
        )
        .expect("pipewire stream connect");

    // Idle callback: check shutdown flag periodically.
    let shutdown_check = shutdown.clone();
    let mainloop_weak = mainloop.downgrade();
    let _idle = mainloop.loop_().add_idle(true, move || {
        if shutdown_check.load(Ordering::SeqCst) {
            if let Some(ml) = mainloop_weak.upgrade() {
                ml.quit();
            }
        }
    });

    // Run until shutdown or error.
    mainloop.run();
}
