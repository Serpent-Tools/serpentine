//! Renders pipeline progress to the terminal.

use std::collections::BTreeMap;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::events::{Lifecycle, NodeTransition, SerpentineEvent, TaskId, TaskKind, TaskUpdate};

/// Frames of the activity spinner, advanced by elapsed time.
const SPINNER: [char; 4] = ['│', '╱', '─', '╲'];
/// The shell prompt glyph.
const PROMPT: char = '>';
/// Marker shown beside a scrollback milestone.
const MILESTONE_MARK: char = '·';
/// Marker shown beside an image-pull row.
const PULL_MARK: char = '↓';
/// The filled segment of a progress bar.
const BAR_FILLED: char = '━';
/// The empty segment of a progress bar.
const BAR_EMPTY: char = '─';
/// Width in cells reserved for a task's name column.
const NAME_WIDTH: usize = 30;
/// Width in cells of an in-row progress bar.
const PULL_BAR_WIDTH: usize = 12;
/// Number of recent engine log lines shown at the bottom of the live block.
const LOG_LINES: usize = 8;

/// A line of persistent scrollback summarising something the run reached.
struct Milestone {
    /// The short label, e.g. `engine ready`.
    label: Box<str>,
    /// The detail, e.g. `podman · 0.4.1`.
    value: Box<str>,
}

/// A task currently shown in the live block.
struct LiveTask {
    /// Whether this is an image pull or a command exec.
    kind: TaskKind,
    /// The name shown for the task.
    label: Box<str>,
    /// When the task started, for the elapsed readout.
    started: Instant,
    /// The most recent line of output (execs).
    detail: Box<str>,
    /// Bytes transferred so far (pulls).
    done_bytes: u64,
    /// Total bytes expected (pulls).
    total_bytes: u64,
    /// Smoothed transfer rate in bytes per second (pulls).
    rate: f64,
    /// Byte count at the last rate sample.
    last_bytes: u64,
    /// When the rate was last sampled.
    last_sample: Instant,
    /// Layers fully pulled so far (pulls).
    layer_done: usize,
    /// Total layers discovered so far (pulls).
    layer_total: usize,
}

impl LiveTask {
    /// Apply a progress update, recomputing the transfer rate for byte updates.
    #[expect(
        clippy::cast_precision_loss,
        reason = "the transfer-rate readout tolerates rounding"
    )]
    fn apply(&mut self, update: TaskUpdate) {
        match update {
            TaskUpdate::Bytes { done, total } => {
                self.done_bytes = done;
                self.total_bytes = total;

                let now = Instant::now();
                let elapsed = now.duration_since(self.last_sample).as_secs_f64();
                if elapsed >= 0.25 {
                    let delta = done.saturating_sub(self.last_bytes);
                    let instant_rate = delta as f64 / elapsed;
                    self.rate = if self.rate <= 0.0 {
                        instant_rate
                    } else {
                        self.rate * 0.6 + instant_rate * 0.4
                    };
                    self.last_bytes = done;
                    self.last_sample = now;
                }
            }
            TaskUpdate::Line(line) => self.detail = line,
            TaskUpdate::LayerProgress { done, total } => {
                self.layer_done = done;
                self.layer_total = total;
            }
        }
    }
}

/// The current state of the ui.
struct UiState {
    /// When the run started, driving the timer and spinner.
    start: Instant,
    /// The pipeline file being run.
    pipeline: Box<str>,
    /// Total number of nodes in the pipeline.
    total_nodes: usize,
    /// Nodes scheduled but not yet running.
    queued: usize,
    /// Nodes currently running.
    active: usize,
    /// Nodes finished, whether cached or executed.
    done: usize,
    /// Nodes finished by reusing a cached result.
    cached: usize,
    /// Nodes finished by executing.
    ran: usize,
    /// Whether the engine has begun shutting down.
    shutting_down: bool,
    /// Live tasks keyed by id, ordered by start.
    tasks: BTreeMap<TaskId, LiveTask>,
    /// Scrollback milestones, in the order they were reached.
    milestones: Vec<Milestone>,
    /// Engine log lines.
    logs: heapless::Deque<Box<str>, LOG_LINES>,
}

impl UiState {
    /// Create a new ui state for a run of `total_nodes` nodes.
    fn new() -> Self {
        Self {
            start: Instant::now(),
            pipeline: "".into(),
            total_nodes: 0,
            queued: 0,
            active: 0,
            done: 0,
            cached: 0,
            ran: 0,
            shutting_down: false,
            tasks: BTreeMap::new(),
            milestones: Vec::new(),
            logs: heapless::Deque::new(),
        }
    }

    /// Update the ui state from an event.
    fn update(&mut self, event: SerpentineEvent) {
        match event {
            SerpentineEvent::Node(transition) => self.apply_node(transition),
            SerpentineEvent::TaskStarted { id, kind, label } => {
                let now = Instant::now();
                self.tasks.insert(
                    id,
                    LiveTask {
                        kind,
                        label,
                        started: now,
                        detail: Box::default(),
                        done_bytes: 0,
                        total_bytes: 0,
                        rate: 0.0,
                        last_bytes: 0,
                        last_sample: now,
                        layer_done: 0,
                        layer_total: 0,
                    },
                );
            }
            SerpentineEvent::Task { id, update } => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.apply(update);
                }
            }
            SerpentineEvent::TaskFinished { id } => {
                self.tasks.remove(&id);
            }
            SerpentineEvent::Lifecycle(lifecycle) => self.apply_lifecycle(lifecycle),
            SerpentineEvent::Log(line) => {
                if self.logs.is_full() {
                    self.logs.pop_front();
                }
                let _ = self.logs.push_back(line);
            }
        }
    }

    /// Move a node between the queued/active/done tallies.
    fn apply_node(&mut self, transition: NodeTransition) {
        match transition {
            NodeTransition::Queued => self.queued = self.queued.saturating_add(1),
            NodeTransition::Started => {
                self.queued = self.queued.saturating_sub(1);
                self.active = self.active.saturating_add(1);
            }
            NodeTransition::Cached => {
                self.active = self.active.saturating_sub(1);
                self.done = self.done.saturating_add(1);
                self.cached = self.cached.saturating_add(1);
            }
            NodeTransition::Ran => {
                self.active = self.active.saturating_sub(1);
                self.done = self.done.saturating_add(1);
                self.ran = self.ran.saturating_add(1);
            }
        }
    }

    /// Record a lifecycle stage.
    fn apply_lifecycle(&mut self, lifecycle: Lifecycle) {
        match lifecycle {
            Lifecycle::EngineReady { runtime, image_tag } => self.milestones.push(Milestone {
                label: "engine ready".into(),
                value: format!("{runtime} · {image_tag}").into(),
            }),
            Lifecycle::PipelineParsed {
                total_nodes,
                pipeline,
            } => {
                self.total_nodes = total_nodes;
                self.pipeline = pipeline;
            }
            Lifecycle::ShuttingDown => self.shutting_down = true,
            // Stop is intercepted in the event loop before update() is called; never reaches here.
            Lifecycle::Stop => {}
        }
    }

    /// The current spinner frame.
    fn spinner(&self) -> char {
        let ticks = self
            .start
            .elapsed()
            .as_millis()
            .checked_div(110)
            .unwrap_or(0);
        let index = usize::try_from(ticks)
            .unwrap_or(0)
            .checked_rem(SPINNER.len())
            .unwrap_or(0);
        SPINNER.get(index).copied().unwrap_or(PROMPT)
    }

    /// Draw the current state to the terminal.
    fn draw(&self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        let width = usize::from(area.width);

        let mut lines: Vec<Line> = Vec::new();

        for milestone in &self.milestones {
            lines.push(milestone_line(milestone));
        }

        lines.push(Line::default());
        lines.push(self.live_header_line(width));
        if !self.shutting_down {
            for task in self.tasks.values() {
                lines.push(self.task_line(task, width));
            }
        }

        if !self.logs.is_empty() {
            lines.push(Line::default());
            for entry in &self.logs {
                lines.push(Line::from(Span::styled(
                    truncate(entry, width),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    /// The live-block header: spinner, command, node tallies, and timer.
    fn live_header_line(&self, width: usize) -> Line<'static> {
        let spinner_style = if self.shutting_down {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Green)
        };
        let mut right = vec![
            Span::styled(
                format!("{} cached", self.cached),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw(" · "),
            Span::styled(
                format!("{} active", self.active),
                Style::default().fg(Color::Green),
            ),
            Span::raw(" · "),
            Span::styled(
                format!("{} queued", self.queued),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("  "),
            Span::raw(format!("{}/{}", self.done, self.total_nodes)),
            Span::raw("  "),
        ];
        if self.shutting_down {
            right.push(Span::styled(
                "shutting down  ",
                Style::default().fg(Color::Red),
            ));
        }
        right.push(Span::raw(format_timer(self.start.elapsed())));
        right.push(Span::raw(" "));
        justify(
            width,
            vec![
                Span::styled(format!(" {} ", self.spinner()), spinner_style),
                Span::raw(format!("serpentine {}", self.pipeline)),
            ],
            right,
        )
    }

    /// A single live task row.
    fn task_line(&self, task: &LiveTask, width: usize) -> Line<'static> {
        match task.kind {
            TaskKind::Exec => {
                let elapsed = task.started.elapsed().as_secs();
                let right = format!(
                    "{}:{:02} ",
                    elapsed.checked_div(60).unwrap_or(0),
                    elapsed.checked_rem(60).unwrap_or(0),
                );
                let prefix_width = 3 + NAME_WIDTH + 1;
                let detail_room = width
                    .saturating_sub(prefix_width)
                    .saturating_sub(right.chars().count());
                justify(
                    width,
                    vec![
                        Span::styled(
                            format!(" {} ", self.spinner()),
                            Style::default().fg(Color::Green),
                        ),
                        Span::raw(pad_truncate(&task.label, NAME_WIDTH)),
                        Span::raw(" "),
                        Span::styled(
                            truncate(&task.detail, detail_room),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ],
                    vec![Span::styled(right, Style::default().fg(Color::DarkGray))],
                )
            }
            TaskKind::Pull => {
                let percent = percent(task.done_bytes, task.total_bytes);
                let filled = scaled(percent, 100, PULL_BAR_WIDTH);
                let mut left = vec![
                    Span::styled(format!(" {PULL_MARK} "), Style::default().fg(Color::Yellow)),
                    Span::raw(pad_truncate(&task.label, NAME_WIDTH)),
                    Span::raw(" "),
                ];
                left.extend(bar_spans(filled, PULL_BAR_WIDTH, Color::Yellow));
                left.push(Span::raw(format!(" {percent}%")));
                if task.layer_total > 0 {
                    left.push(Span::styled(
                        format!("  {}/{}", task.layer_done, task.layer_total),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                justify(
                    width,
                    left,
                    vec![Span::styled(
                        format!("{} ", format_rate(task.rate)),
                        Style::default().fg(Color::Yellow),
                    )],
                )
            }
        }
    }

    /// Print a final summary to stdout once the tui has exited.
    fn print_summary(&self) {
        println!(
            "  {}  {} nodes · {} cached · {} ran · {}",
            self.pipeline,
            self.total_nodes,
            self.cached,
            self.ran,
            format_timer(self.start.elapsed()),
        );
    }
}

/// Render a scrollback milestone line.
fn milestone_line(milestone: &Milestone) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {MILESTONE_MARK} "),
            Style::default().fg(Color::Green),
        ),
        Span::raw(pad_truncate(&milestone.label, 14)),
        Span::raw(" "),
        Span::styled(
            milestone.value.to_string(),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

/// The filled and empty spans of a progress bar.
fn bar_spans(filled: usize, total_width: usize, color: Color) -> Vec<Span<'static>> {
    let filled = filled.min(total_width);
    let empty = total_width.saturating_sub(filled);
    vec![
        Span::styled(
            std::iter::repeat_n(BAR_FILLED, filled).collect::<String>(),
            Style::default().fg(color),
        ),
        Span::styled(
            std::iter::repeat_n(BAR_EMPTY, empty).collect::<String>(),
            Style::default().fg(Color::DarkGray),
        ),
    ]
}

/// Combine left- and right-aligned spans into one line, padding the gap to `width`.
fn justify(width: usize, mut left: Vec<Span<'static>>, right: Vec<Span<'static>>) -> Line<'static> {
    let used = span_width(&left).saturating_add(span_width(&right));
    left.push(Span::raw(" ".repeat(width.saturating_sub(used))));
    left.extend(right);
    Line::from(left)
}

/// The total display width of a run of spans.
fn span_width(spans: &[Span]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
}

/// A percentage of `done` out of `total`, clamped to 0..=100.
fn percent(done: u64, total: u64) -> usize {
    if total == 0 {
        return 0;
    }
    let percent = done
        .saturating_mul(100)
        .checked_div(total)
        .unwrap_or(0)
        .min(100);
    usize::try_from(percent).unwrap_or(0)
}

/// Scale `part` of `whole` onto a bar of `width` cells.
fn scaled(part: usize, whole: usize, width: usize) -> usize {
    if whole == 0 {
        return 0;
    }
    part.saturating_mul(width)
        .checked_div(whole)
        .unwrap_or(0)
        .min(width)
}

/// Truncate `text` to at most `width` cells, appending an ellipsis when shortened.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() > width {
        let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        text.to_owned()
    }
}

/// Pad `text` with spaces, or truncate it with an ellipsis, to exactly `width` cells.
fn pad_truncate(text: &str, width: usize) -> String {
    let truncated = truncate(text, width);
    let count = truncated.chars().count();
    if count < width {
        let mut out = truncated;
        out.push_str(&" ".repeat(width.saturating_sub(count)));
        out
    } else {
        truncated
    }
}

/// Format a transfer rate, or `pulling` when not yet known.
fn format_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec <= 0.0 {
        return "pulling".to_owned();
    }
    let megabytes = bytes_per_sec / 1_000_000.0;
    if megabytes >= 1.0 {
        format!("{megabytes:.1} MB/s")
    } else {
        format!("{:.0} KB/s", bytes_per_sec / 1000.0)
    }
}

/// Format a duration as `mm:ss`.
fn format_timer(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let minutes = seconds.checked_div(60).unwrap_or(0);
    let remainder = seconds.checked_rem(60).unwrap_or(0);
    format!("{minutes:02}:{remainder:02}")
}

/// Start the TUI to display progress of the running pipeline.
#[expect(
    clippy::needless_pass_by_value,
    reason = "Receiver is deliberately owned by the consumer thread"
)]
pub fn start_tui(events: Receiver<SerpentineEvent>) {
    log::info!("Starting TUI");

    std::panic::set_hook(Box::new(|info| {
        log::error!("Serpentine panicked: {info}");
        eprintln!("Tui panicked: {info}");
    }));

    let max_tasks = 10_usize;
    let reserved = max_tasks
        .saturating_add(7)
        .saturating_add(LOG_LINES.saturating_add(1));
    let height = u16::try_from(reserved).unwrap_or(25);

    let Ok(mut terminal) = ratatui::Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(std::io::stdout()),
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Inline(height),
        },
    ) else {
        log::error!("Failed to initialize terminal for TUI, terminating TUI");
        return;
    };

    let mut ui_state = UiState::new();

    'draw_loop: loop {
        let draw_result = terminal.draw(|frame| {
            ui_state.draw(frame);
        });
        if let Err(err) = draw_result {
            log::error!("Error drawing TUI: {err}, terminating TUI");
            break;
        }

        while let Ok(event) = events.recv_timeout(Duration::from_millis(10)) {
            match event {
                SerpentineEvent::Lifecycle(Lifecycle::Stop) => {
                    log::info!("Received stop, terminating TUI");
                    break 'draw_loop;
                }
                event => ui_state.update(event),
            }
        }
    }

    drop(terminal);
    println!();
    ui_state.print_summary();
    log::info!("TUI terminated");
}
