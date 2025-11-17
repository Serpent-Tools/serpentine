//! Handles the display of progress and container state to the terminal.

use std::time::Duration;

use ratatui::{
    crossterm,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    symbols,
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Padding, Paragraph},
};

/// The messages that can be passed to the tui;
///
/// These messages carry no information about which node is being updated,
/// as the tui is only concerned with overall progress.
/// and trusts the executor to send sensible messages.
#[derive(Debug)]
pub enum TuiMessage {
    /// The executor is complete, shutdown the tui
    Shutdown,
    /// We are in the process of shutting down
    ShuttingDown,
    /// A node is pending execution
    PendingNode,
    /// A node is now running
    RunningNode,
    /// A node has finished execution
    NodeFinished,
    /// Update the progress of a task (or create it if it doesn't exist)
    UpdateTask(Task),
    /// A task is done
    FinishTask(String),
    /// Add a line to the logs
    Log(String),
    /// A new container was created
    Container(String),
}

pub type TuiSender = std::sync::mpsc::Sender<TuiMessage>;

/// Represents the progress of a task
#[derive(Debug)]
pub enum TaskProgress {
    /// A task with a measurable progress
    #[expect(dead_code, reason = "Will be used in the future")]
    Measurable {
        /// How many units have been completed
        completed: u64,
        /// How much work there is total
        total: u64,
    },
    /// A task with no measurable progress
    Log(String),
}

/// Represents a task being executed in the pipeline
#[derive(Debug)]
pub struct Task {
    /// The identiifer for the task
    pub identifier: String,
    /// The name to show in the UI
    pub title: String,
    /// The current progress of the task
    pub progress: TaskProgress,
}

/// The current state of the ui
#[derive(Debug)]
struct UiState {
    /// The total amount of nodes
    total_nodes: usize,
    /// Nodes that are pending execution.
    pending_nodes: usize,
    /// Nodes that are currently running.
    running_nodes: usize,
    /// Nodes that are finished executing.
    finished_nodes: usize,
    /// Are we shutting down?
    shutting_down: bool,
    /// Tasks being tracked by the UI
    tasks: Vec<Task>,
    /// Log lines
    logs: Vec<String>,
    /// Containers spawned by serpentine
    containers: Vec<String>,
    /// Logs from tasks
    task_logs: Vec<String>,
}

impl UiState {
    /// Create a new ui state
    fn new(total_nodes: usize) -> Self {
        Self {
            total_nodes,
            pending_nodes: 0,
            running_nodes: 0,
            finished_nodes: 0,
            shutting_down: false,
            tasks: Vec::new(),
            logs: Vec::new(),
            containers: Vec::new(),
            task_logs: Vec::new(),
        }
    }

    /// Update the ui state based on a message
    fn update(&mut self, message: TuiMessage) {
        match message {
            TuiMessage::Shutdown | TuiMessage::ShuttingDown => {
                self.shutting_down = true;
            }
            TuiMessage::PendingNode => {
                self.pending_nodes = self.pending_nodes.saturating_add(1);
            }
            TuiMessage::RunningNode => {
                self.pending_nodes = self.pending_nodes.saturating_sub(1);
                self.running_nodes = self.running_nodes.saturating_add(1);
            }
            TuiMessage::NodeFinished => {
                self.running_nodes = self.running_nodes.saturating_sub(1);
                self.finished_nodes = self.finished_nodes.saturating_add(1);
            }
            TuiMessage::UpdateTask(task) => {
                if let TaskProgress::Log(msg) = &task.progress {
                    self.task_logs.push(msg.clone());
                    if self.task_logs.len() > 20 {
                        self.task_logs.remove(0);
                    }
                }

                for existing_task in &mut self.tasks {
                    if existing_task.identifier == task.identifier {
                        *existing_task = task;
                        return;
                    }
                }
                self.tasks.push(task);
            }
            TuiMessage::FinishTask(identifier) => {
                self.tasks.retain(|task| task.identifier != identifier);
            }
            TuiMessage::Log(msg) => {
                self.logs.push(msg);
                if self.logs.len() > 20 {
                    self.logs.remove(0);
                }
            }
            TuiMessage::Container(id) => {
                self.containers.push(id);
            }
        }
    }

    /// Draw the current state to the terminal
    fn draw(&self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        let [progress_area, task_area, status_area] = Layout::new(
            Direction::Vertical,
            [
                Constraint::Length(3),
                Constraint::Min(6),
                Constraint::Max(22),
            ],
        )
        .areas(area);

        let progress = Block::default()
            .borders(Borders::ALL)
            .title(" Pipeline Progress ");
        let progress_inner = progress.inner(progress_area);

        frame.render_widget(progress, progress_area);
        self.draw_progress_bar(progress_inner, frame);

        let tasks = Block::default()
            .borders(Borders::ALL)
            .title(" Tasks ")
            .padding(Padding {
                left: 1,
                right: 1,
                top: 0,
                bottom: 0,
            });
        let tasks_inner = tasks.inner(task_area);
        frame.render_widget(tasks, task_area);
        self.draw_tasks(tasks_inner, frame);

        self.draw_status(status_area, frame);
    }

    /// Draw the tasks
    fn draw_tasks(&self, area: Rect, frame: &mut ratatui::Frame) {
        let areas = Layout::new(
            Direction::Vertical,
            self.tasks.iter().map(|_| Constraint::Length(1)),
        )
        .split(area);

        for (task, task_area) in self.tasks.iter().zip(areas.iter()) {
            match &task.progress {
                TaskProgress::Measurable { completed, total } => {
                    #[expect(
                        clippy::cast_precision_loss,
                        reason = "we want to do floating point division here"
                    )]
                    let widget = Gauge::default()
                        .ratio((*completed as f64) / (*total as f64).max(1.0))
                        .label(task.title.clone());
                    frame.render_widget(widget, *task_area);
                }
                TaskProgress::Log(message) => {
                    let widget = Line::from(vec![
                        Span {
                            content: format!("{:<30}  ", task.title).into(),
                            style: Style::default().fg(Color::Yellow),
                        },
                        Span {
                            content: message.into(),
                            style: Style::default().fg(Color::Gray),
                        },
                    ]);
                    frame.render_widget(widget, *task_area);
                }
            }
        }
    }

    /// Draw the progress bar widget
    fn draw_progress_bar(&self, area: Rect, frame: &mut ratatui::Frame) {
        let prefix = Line::from(vec![
            Span {
                content: self.pending_nodes.to_string().into(),
                style: Style::default().fg(Color::White),
            },
            Span::from(">"),
            Span {
                content: self.running_nodes.to_string().into(),
                style: Style::default().fg(Color::Yellow),
            },
            Span::from(">"),
            Span {
                content: self.finished_nodes.to_string().into(),
                style: Style::default().fg(Color::Green),
            },
            Span {
                content: " ".into(),
                style: Style::default(),
            },
        ]);

        let suffix = Line::from(vec![
            Span {
                content: "···".into(),
                style: Style::default().fg(Color::Gray),
            },
            Span {
                content: " ".into(),
                style: Style::default(),
            },
            Span::from("/"),
            Span {
                content: self.total_nodes.to_string().into(),
                style: Style::default().fg(Color::Gray),
            },
        ]);

        let [prefix_area, bar_area, suffix_area] = Layout::new(
            Direction::Horizontal,
            [
                Constraint::Length(
                    prefix
                        .width()
                        .try_into()
                        .unwrap_or(u16::MAX)
                        .min(area.width),
                ),
                Constraint::Fill(1),
                Constraint::Length(
                    suffix
                        .width()
                        .try_into()
                        .unwrap_or(u16::MAX)
                        .min(area.width),
                ),
            ],
        )
        .areas(area);

        frame.render_widget(prefix, prefix_area);
        frame.render_widget(suffix, suffix_area);
        self.create_bar_widget(bar_area, frame);
    }

    /// Create the actual bar widget of the progress bar
    fn create_bar_widget(&self, area: Rect, frame: &mut ratatui::Frame) {
        let total_width = area.width as usize;

        let finished_width = fraction_of(self.finished_nodes, self.total_nodes, total_width);
        let pending_width = fraction_of(self.pending_nodes, self.total_nodes, total_width);
        let running_width = total_width
            .saturating_sub(finished_width)
            .saturating_sub(pending_width);

        let bar = Line::from(vec![
            Span {
                content: symbols::line::DOUBLE_HORIZONTAL
                    .repeat(finished_width)
                    .into(),
                style: Style::default().fg(Color::Green),
            },
            Span {
                content: symbols::line::HORIZONTAL.repeat(running_width).into(),
                style: Style::default().fg(if self.shutting_down {
                    Color::Red
                } else {
                    Color::Yellow
                }),
            },
            Span {
                content: ":".repeat(pending_width).into(),
                style: Style::default().fg(if self.shutting_down {
                    Color::Red
                } else {
                    Color::White
                }),
            },
        ]);

        frame.render_widget(bar, area);
    }

    /// Draw the various status readouts
    fn draw_status(&self, area: Rect, frame: &mut ratatui::Frame) {
        let [log_area, task_area, container_area] =
            Layout::new(Direction::Horizontal, [Constraint::Fill(1); 3]).areas(area);

        let logs = Block::default()
            .borders(Borders::ALL)
            .title(" Serpentine Logs ");
        let log_inner = logs.inner(log_area);
        frame.render_widget(logs, log_area);
        frame.render_widget(Paragraph::new(self.logs.join("\n")), log_inner);

        let tasks = Block::default().borders(Borders::ALL).title(" Task Logs ");
        let task_inner = tasks.inner(task_area);
        frame.render_widget(tasks, task_area);
        frame.render_widget(Paragraph::new(self.task_logs.join("\n")), task_inner);

        let containers = Block::default().borders(Borders::ALL).title(" Containers ");
        let container_inner = containers.inner(container_area);
        frame.render_widget(containers, container_area);
        frame.render_widget(Paragraph::new(self.containers.join("\n")), container_inner);
    }
}

/// Calculate the usize width that represents the given fraction of `total_width`
#[expect(
    clippy::cast_precision_loss,
    reason = "if you terminal size exceeds f32 precision you have bigger problems"
)]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "we want to round to usize"
)]
fn fraction_of(denominator: usize, numerator: usize, total_width: usize) -> usize {
    let denominator = denominator as f32;
    let numerator = numerator as f32;
    let total_width = total_width as f32;

    let ratio = denominator / numerator;
    let perfect_width = ratio * total_width;

    perfect_width.round().abs() as usize
}

/// Start the TUI to display progress of the running pipeline
#[expect(clippy::needless_pass_by_value, reason = "TUI runs in its own thread")]
pub fn start_tui(events: std::sync::mpsc::Receiver<TuiMessage>, total_nodes: usize) {
    log::info!("Starting TUI");

    std::panic::set_hook(Box::new(|info| {
        ratatui::restore();
        log::error!("Serpentine panicked: {info}");
        eprintln!("Tui panicked: {info}");
    }));
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen);

    let Ok(mut terminal) = ratatui::Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(std::io::stdout()),
        ratatui::TerminalOptions::default(),
    ) else {
        log::error!("Failed to initialize terminal for TUI, terminating TUI");
        return;
    };

    let mut ui_state = UiState::new(total_nodes);

    'draw_loop: loop {
        let draw_result = terminal.draw(|frame| {
            ui_state.draw(frame);
        });
        if let Err(err) = draw_result {
            log::error!("Error drawing TUI: {err}, terminating TUI");
            break;
        }

        while let Ok(message) = events.recv_timeout(Duration::from_millis(10)) {
            match message {
                TuiMessage::Shutdown => {
                    log::info!("Received shutdown message, terminating TUI");
                    break 'draw_loop;
                }
                message => {
                    ui_state.update(message);
                }
            }
        }
    }

    println!("Executed {} nodes", ui_state.finished_nodes);

    ratatui::restore();
    log::info!("TUI terminated");
}
