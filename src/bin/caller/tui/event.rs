//! TUI-specific event helpers (crossterm reader).
//!
//! Shared event types (`EventBus`, `AppEvent`, `ControlMsg`, etc.) now live in
//! `crate::event`. This module only contains the crossterm terminal reader.

use crate::event::{AppEvent, EventBus};
use crossterm::event::{Event as CrosstermEvent, EventStream};

// EventStream implements futures_core::Stream; use tokio_stream for .next()
use tokio_stream::StreamExt as _;

/// Spawns a background task that reads crossterm events and forwards them.
pub fn spawn_crossterm_reader(bus: EventBus) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = EventStream::new();
        loop {
            match reader.next().await {
                Some(Ok(event)) => match event {
                    CrosstermEvent::Key(key) => {
                        bus.send(AppEvent::Key(key));
                    }
                    CrosstermEvent::Resize(w, h) => {
                        bus.send(AppEvent::Resize(w, h));
                    }
                    _ => {}
                },
                Some(Err(_)) => break,
                None => break,
            }
        }
    })
}
