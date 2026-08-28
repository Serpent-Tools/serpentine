//! Writes log lines straight to stdout, for CI and terminals that cannot host the TUI.

use std::sync::mpsc::Receiver;

use crate::events::{Lifecycle, SerpentineEvent, TaskUpdate};

/// Drain events to stdout until the run stops.
#[expect(
    clippy::needless_pass_by_value,
    reason = "Receiver is deliberately owned by the consumer thread"
)]
pub fn start(events: Receiver<SerpentineEvent>) {
    while let Ok(event) = events.recv() {
        match event {
            SerpentineEvent::Lifecycle(Lifecycle::Stop) => break,
            SerpentineEvent::Log {
                level,
                target,
                message,
            } => println!("[{level}][{target}] {message}"),
            // Captured command output, which the log chain drops at trace level.
            SerpentineEvent::Task {
                update: TaskUpdate::Line(line),
                ..
            } => println!("{line}"),
            _ => {}
        }
    }
}
