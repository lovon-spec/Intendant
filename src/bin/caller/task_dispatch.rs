//! Backend task dispatcher.
//!
//! Listens on the EventBus for `AppEvent::ControlCommand(StartTask | FollowUp)` and
//! routes each task to the correct output channel (`task_tx`, `follow_up_tx`, or
//! `presence_tx`) based on which channels are wired and whether the caller
//! requested direct dispatch (bypass presence).
//!
//! This module replaces the routing logic that previously lived in the TUI's
//! `handle_control_command`. The TUI is now display-only — it observes phase
//! changes and renders updates, but no longer owns dispatch authority.
//!
//! Routing policy for a task {text, direct, metadata}:
//!   1. If `direct != true` AND `presence_tx` is available: send text to
//!      `presence_tx`. The presence LLM decides whether to forward as a real
//!      task (via its own `submit_task` tool -> task_tx) or respond in-line.
//!   2. Else if `task_tx` is available: wrap in `TaskEnvelope` and send.
//!      `force_direct` is derived from the `direct` flag (plus legacy
//!      `orchestrate == Some(false)` for StartTask).
//!   3. Else if `follow_up_tx` is available: send text only. Metadata is
//!      dropped (non-presence mode has no CU-first routing anyway).
//!   4. Else: warn and drop.
//!
//! Presence's own `submit_task` tool keeps direct `task_tx` access for
//! synchronous tool-result semantics — the dispatcher coordinates frontend
//! → backend routing, not presence-internal LLM tool calls.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::event::{AppEvent, ControlMsg, EventBus};

/// Senders the dispatcher owns. Clone to populate these from the channels
/// already created in `main.rs` (e.g. for presence task loop / agent loop).
#[derive(Clone)]
pub struct Dispatcher {
    /// Presence user-input channel. When `Some`, non-direct tasks route here
    /// so the presence LLM can mediate.
    pub presence_tx: Option<mpsc::Sender<String>>,
    /// Task envelope channel consumed by `run_with_presence`. When `Some`,
    /// direct tasks go here (full metadata preserved).
    pub task_tx: Option<mpsc::Sender<presence_core::TaskEnvelope>>,
    /// Text follow-up channel consumed by `run_direct_mode` /
    /// `run_external_agent_mode` in non-presence mode.
    pub follow_up_tx: Option<mpsc::Sender<String>>,
}

impl Dispatcher {
    /// Spawn a background task that subscribes to the bus and routes task
    /// dispatch commands. The handle is aborted on session end.
    pub fn spawn(self, bus: EventBus) -> JoinHandle<()> {
        let mut rx = bus.subscribe();
        let bus_for_log = bus.clone();
        let arc = Arc::new(self);

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if let AppEvent::ControlCommand(msg) = event {
                            arc.route(msg, &bus_for_log).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Bus lagged — continue; the dispatcher is idempotent
                        // per event and cannot recover lost ones.
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    async fn route(&self, msg: ControlMsg, bus: &EventBus) {
        match msg {
            ControlMsg::StartTask {
                task,
                orchestrate,
                direct,
                reference_frame_ids,
                display_target,
                attachments,
            } => {
                let is_direct = direct.unwrap_or(false) || orchestrate == Some(false);
                let has_metadata = !reference_frame_ids.is_empty()
                    || display_target.is_some()
                    || !attachments.is_empty();

                // If the task has metadata (attachments, frame refs, display
                // target), it MUST go via task_tx to preserve that data. Non-
                // direct is overridden in that case — presence can't carry
                // metadata through its text channel.
                let prefer_task_tx = is_direct || has_metadata;

                if !prefer_task_tx {
                    if let Some(ref tx) = self.presence_tx {
                        if tx.try_send(task.clone()).is_ok() {
                            return;
                        }
                    }
                }

                if let Some(ref tx) = self.task_tx {
                    let envelope = presence_core::TaskEnvelope {
                        task: task.clone(),
                        force_direct: is_direct,
                        context_hints: vec![],
                        reference_frame_ids,
                        display_target,
                        attachment_frame_ids: attachments,
                    };
                    if tx.try_send(envelope).is_ok() {
                        return;
                    }
                }

                if let Some(ref tx) = self.follow_up_tx {
                    if tx.try_send(task.clone()).is_ok() {
                        return;
                    }
                }

                self.warn_drop(bus, "StartTask", &task);
            }

            ControlMsg::ResumeSession { .. } => {
                // The daemon loop owns session reattachment because it needs
                // to choose the log dir, project root, and backend-native id.
            }

            ControlMsg::FollowUp { text, direct } => {
                let is_direct = direct.unwrap_or(false);

                if !is_direct {
                    if let Some(ref tx) = self.presence_tx {
                        if tx.try_send(text.clone()).is_ok() {
                            return;
                        }
                    }
                }

                if let Some(ref tx) = self.task_tx {
                    let envelope = presence_core::TaskEnvelope {
                        task: text.clone(),
                        force_direct: is_direct,
                        context_hints: vec![],
                        reference_frame_ids: vec![],
                        display_target: None,
                        attachment_frame_ids: vec![],
                    };
                    if tx.try_send(envelope).is_ok() {
                        return;
                    }
                }

                if let Some(ref tx) = self.follow_up_tx {
                    if tx.try_send(text.clone()).is_ok() {
                        return;
                    }
                }

                self.warn_drop(bus, "FollowUp", &text);
            }

            ControlMsg::Interrupt { expected_turn: _ } => {
                // Re-emit as AppEvent::InterruptRequested so agent loops can subscribe
                // and cancel their own work. The dispatcher itself doesn't hold loop
                // handles — loops register interest via the bus.
                bus.send(AppEvent::InterruptRequested);
            }

            ControlMsg::Steer { text, id } => {
                // Re-emit as AppEvent::SteerRequested so agent loops can
                // subscribe and either inject the text into the active turn
                // (native mid-turn steering) or queue it onto
                // `context_injection` for the next turn. `id` defaults to
                // "" so downstream consumers never have to handle an Option.
                bus.send(AppEvent::SteerRequested {
                    text,
                    id: id.unwrap_or_default(),
                });
            }

            _ => {
                // Not a task-dispatch command — ignore.
            }
        }
    }

    fn warn_drop(&self, bus: &EventBus, kind: &str, preview: &str) {
        let trunc: String = preview.chars().take(80).collect();
        bus.send(AppEvent::LogEntry {
            level: "warn".to_string(),
            source: "system".to_string(),
            content: format!(
                "{} dropped (no dispatch target available): {}",
                kind, trunc
            ),
            turn: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn make_test_bus() -> EventBus {
        EventBus::new()
    }

    #[tokio::test]
    async fn start_task_with_metadata_prefers_task_tx() {
        let (task_tx, mut task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(4);
        let (presence_tx, mut presence_rx) = mpsc::channel::<String>(4);
        let (follow_up_tx, mut follow_up_rx) = mpsc::channel::<String>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: Some(presence_tx),
            task_tx: Some(task_tx),
            follow_up_tx: Some(follow_up_tx),
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::StartTask {
            task: "do thing".into(),
            orchestrate: None,
            direct: None,
            reference_frame_ids: vec!["f1".into()],
            display_target: None,
            attachments: vec![],
        }));

        let envelope =
            tokio::time::timeout(std::time::Duration::from_millis(200), task_rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(envelope.task, "do thing");
        assert_eq!(envelope.reference_frame_ids, vec!["f1".to_string()]);
        assert!(!envelope.force_direct);

        // Presence and follow_up NOT consulted for metadata tasks
        assert!(presence_rx.try_recv().is_err());
        assert!(follow_up_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn start_task_non_direct_with_presence_routes_to_presence() {
        let (task_tx, mut task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(4);
        let (presence_tx, mut presence_rx) = mpsc::channel::<String>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: Some(presence_tx),
            task_tx: Some(task_tx),
            follow_up_tx: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::StartTask {
            task: "chat with me".into(),
            orchestrate: None,
            direct: None,
            reference_frame_ids: vec![],
            display_target: None,
            attachments: vec![],
        }));

        let text =
            tokio::time::timeout(std::time::Duration::from_millis(200), presence_rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(text, "chat with me");
        assert!(task_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn start_task_direct_bypasses_presence() {
        let (task_tx, mut task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(4);
        let (presence_tx, mut presence_rx) = mpsc::channel::<String>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: Some(presence_tx),
            task_tx: Some(task_tx),
            follow_up_tx: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::StartTask {
            task: "code thing".into(),
            orchestrate: None,
            direct: Some(true),
            reference_frame_ids: vec![],
            display_target: None,
            attachments: vec![],
        }));

        let envelope =
            tokio::time::timeout(std::time::Duration::from_millis(200), task_rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(envelope.task, "code thing");
        assert!(envelope.force_direct);
        assert!(presence_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn follow_up_direct_to_task_tx() {
        let (task_tx, mut task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(4);
        let (presence_tx, _presence_rx) = mpsc::channel::<String>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: Some(presence_tx),
            task_tx: Some(task_tx),
            follow_up_tx: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::FollowUp {
            text: "more please".into(),
            direct: Some(true),
        }));

        let envelope =
            tokio::time::timeout(std::time::Duration::from_millis(200), task_rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(envelope.task, "more please");
        assert!(envelope.force_direct);
    }

    #[tokio::test]
    async fn follow_up_non_presence_goes_to_follow_up_tx() {
        let (follow_up_tx, mut follow_up_rx) = mpsc::channel::<String>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: None,
            follow_up_tx: Some(follow_up_tx),
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::FollowUp {
            text: "keep going".into(),
            direct: None,
        }));

        let text =
            tokio::time::timeout(std::time::Duration::from_millis(200), follow_up_rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(text, "keep going");
    }

    #[tokio::test]
    async fn orchestrate_false_implies_direct() {
        let (task_tx, mut task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(4);
        let (presence_tx, mut presence_rx) = mpsc::channel::<String>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: Some(presence_tx),
            task_tx: Some(task_tx),
            follow_up_tx: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::StartTask {
            task: "legacy direct".into(),
            orchestrate: Some(false),
            direct: None,
            reference_frame_ids: vec![],
            display_target: None,
            attachments: vec![],
        }));

        let envelope =
            tokio::time::timeout(std::time::Duration::from_millis(200), task_rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert!(envelope.force_direct);
        assert!(presence_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn non_task_control_messages_ignored() {
        let (task_tx, mut task_rx) = mpsc::channel::<presence_core::TaskEnvelope>(4);
        let bus = make_test_bus();

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: Some(task_tx),
            follow_up_tx: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::Status));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(task_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn interrupt_emits_interrupt_requested() {
        let bus = make_test_bus();
        let mut rx = bus.subscribe();

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: None,
            follow_up_tx: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::Interrupt {
            expected_turn: None,
        }));

        // Drain events until we see an InterruptRequested, or time out.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut saw_interrupt_requested = false;
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::InterruptRequested)) => {
                    saw_interrupt_requested = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        assert!(
            saw_interrupt_requested,
            "expected AppEvent::InterruptRequested to be emitted"
        );
    }

    #[tokio::test]
    async fn steer_emits_steer_requested_with_id() {
        // The dispatcher re-emits `ControlMsg::Steer` as
        // `AppEvent::SteerRequested`, defaulting a missing id to "" so
        // downstream consumers never have to handle an Option.
        let bus = make_test_bus();
        let mut rx = bus.subscribe();

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: None,
            follow_up_tx: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::Steer {
            text: "use SQLite instead".into(),
            id: Some("s1".into()),
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut seen: Option<(String, String)> = None;
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::SteerRequested { text, id })) => {
                    seen = Some((text, id));
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        let (text, id) = seen.expect("expected AppEvent::SteerRequested");
        assert_eq!(text, "use SQLite instead");
        assert_eq!(id, "s1");
    }

    #[tokio::test]
    async fn steer_without_id_defaults_to_empty_string() {
        let bus = make_test_bus();
        let mut rx = bus.subscribe();

        let dispatcher = Dispatcher {
            presence_tx: None,
            task_tx: None,
            follow_up_tx: None,
        };
        let _h = dispatcher.spawn(bus.clone());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        bus.send(AppEvent::ControlCommand(ControlMsg::Steer {
            text: "never mind".into(),
            id: None,
        }));

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut seen_id: Option<String> = None;
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(AppEvent::SteerRequested { id, .. })) => {
                    seen_id = Some(id);
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }
        assert_eq!(seen_id.as_deref(), Some(""));
    }
}
