//! A reporter backend for github that, assuming `jobs=1`, produces easier to follow logs.

use std::sync::mpsc::Receiver;

use crate::events::{Lifecycle, SerpentineEvent, TaskKind, TaskUpdate};

/// Drain events to stdout, folding each command into a collapsible log group.
///
/// Only [`TaskKind::Exec`] is grouped, since layer moves and pulls run alongside it however
/// `--jobs` is set, and a group would swallow whatever they printed.
#[expect(
    clippy::needless_pass_by_value,
    reason = "Receiver is deliberately owned by the consumer thread"
)]
pub fn start(events: Receiver<SerpentineEvent>) {
    let mut open_group = None;

    while let Ok(event) = events.recv() {
        match event {
            SerpentineEvent::Lifecycle(Lifecycle::Stop) => break,
            SerpentineEvent::TaskStarted {
                id,
                kind: TaskKind::Exec,
                label,
            } => {
                println!("::group::{}", escape(&label));
                open_group = Some(id);
            }
            SerpentineEvent::TaskFinished { id } if open_group == Some(id) => {
                println!("::endgroup::");
                open_group = None;
            }
            SerpentineEvent::Task {
                update: TaskUpdate::Line(line),
                ..
            } => println!("{line}"),
            _ => {}
        }
    }

    if open_group.is_some() {
        println!("::endgroup::");
    }
}

/// Escape the characters that would otherwise terminate a workflow command.
///
/// <https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands>
fn escape(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}
