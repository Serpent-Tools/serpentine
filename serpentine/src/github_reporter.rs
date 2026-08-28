//! A reporter backend for github that, assuming `jobs=1`, produces easier to follow logs.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::mpsc::Receiver;

use crate::events::{Lifecycle, NodeTransition, SerpentineEvent, TaskKind, TaskUpdate};

/// The environment variable holding the file a step's summary is appended to.
const STEP_SUMMARY: &str = "GITHUB_STEP_SUMMARY";

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
    let mut summary = Summary::default();

    while let Ok(event) = events.recv() {
        match event {
            SerpentineEvent::Lifecycle(Lifecycle::Stop) => break,
            SerpentineEvent::Lifecycle(lifecycle) => summary.apply_lifecycle(lifecycle),
            SerpentineEvent::Node(transition) => summary.apply_node(transition),
            SerpentineEvent::TaskStarted {
                id,
                kind: TaskKind::Exec,
                label,
            } => {
                println!("::group::{}", escape(&label));
                open_group = Some(id);
            }
            SerpentineEvent::TaskStarted {
                kind: TaskKind::Pull,
                ..
            } => summary.pulled = summary.pulled.saturating_add(1),
            SerpentineEvent::TaskFinished { id } if open_group == Some(id) => {
                println!("::endgroup::");
                open_group = None;
            }
            SerpentineEvent::Task {
                update: TaskUpdate::Line(line),
                ..
            } => println!("{line}"),
            // Groups are meant to be the command and nothing else, but a run that goes wrong
            // explains itself at warn or above, and that is worth more than a tidy log.
            SerpentineEvent::Log {
                level,
                target,
                message,
            } if level <= log::Level::Warn => println!("[{level}][{target}] {message}"),
            _ => {}
        }
    }

    if open_group.is_some() {
        println!("::endgroup::");
    }

    summary.write();
}

/// What the run summary reports, accumulated from the event stream.
#[derive(Default)]
struct Summary {
    /// The pipeline file that was run.
    pipeline: Box<str>,
    /// Total number of nodes in the pipeline.
    total_nodes: usize,
    /// Nodes finished by reusing a cached result.
    cached: usize,
    /// Nodes finished by executing.
    ran: usize,
    /// Images pulled from a registry.
    pulled: usize,
    /// The rendered diagnostic, when the run failed.
    failure: Option<Box<str>>,
}

impl Summary {
    /// Record a run lifecycle stage.
    fn apply_lifecycle(&mut self, lifecycle: Lifecycle) {
        match lifecycle {
            Lifecycle::PipelineParsed {
                total_nodes,
                pipeline,
            } => {
                self.total_nodes = total_nodes;
                self.pipeline = pipeline;
            }
            Lifecycle::Failed { report } => self.failure = Some(report),
            Lifecycle::EngineReady { .. } | Lifecycle::ShuttingDown | Lifecycle::Stop => {}
        }
    }

    /// Tally a finished node.
    fn apply_node(&mut self, transition: NodeTransition) {
        match transition {
            NodeTransition::Cached => self.cached = self.cached.saturating_add(1),
            NodeTransition::Ran => self.ran = self.ran.saturating_add(1),
            NodeTransition::Queued | NodeTransition::Started => {}
        }
    }

    /// Render the summary as markdown.
    ///
    /// The diagnostic goes in a fence long enough to survive the code blocks miette draws around
    /// source snippets.
    fn render(&self) -> String {
        let status = if self.failure.is_some() {
            "failed"
        } else {
            "passed"
        };
        // The name arrives with the parsed pipeline, so a run that failed to compile has none.
        let name = if self.pipeline.is_empty() {
            "pipeline"
        } else {
            &self.pipeline
        };
        let mut out = format!("## {name} {status}\n");

        if self.total_nodes > 0 {
            out.push_str("\n| | |\n| --- | --- |\n");
            let _ = writeln!(out, "| nodes | {} |", self.total_nodes);
            let _ = writeln!(out, "| cached | {} |", self.cached);
            let _ = writeln!(out, "| ran | {} |", self.ran);
            let _ = writeln!(out, "| images pulled | {} |", self.pulled);
        }

        if let Some(report) = &self.failure {
            let report = strip_ansi_escapes::strip_str(report);
            let _ = write!(out, "\n`````\n{}\n`````\n", report.trim_end());
        }

        out
    }

    /// Append the summary to the step summary file, if there is one.
    fn write(&self) {
        let Some(path) = std::env::var_os(STEP_SUMMARY) else {
            log::debug!("{STEP_SUMMARY} is unset, skipping the run summary");
            return;
        };

        let appended = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .and_then(|mut file| file.write_all(self.render().as_bytes()));

        if let Err(err) = appended {
            log::error!(
                "Failed to write the run summary to {}: {err}",
                std::path::Path::new(&path).display()
            );
        }
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
