//! Run events emitted by the engine as a pipeline executes.
//!
//! Producers emit [`SerpentineEvent`]s through a [`Reporter`]. Exactly one consumer drains them to
//! render progress.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};

/// Identifies a tracked task (an image pull or a container exec) for the duration of a run.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TaskId(u64);

/// The kind of work a task represents.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskKind {
    /// Pulling an image layer from a registry.
    Pull,
    /// Running a pipeline node's command inside a container.
    ///
    /// These are the tasks `--jobs` bounds, and the only ones producing [`TaskUpdate::Line`].
    Exec,
    /// Any other work worth showing progress for, such as moving layers or a healthcheck.
    Status,
}

/// A progress update for an in-flight task.
#[derive(Debug)]
pub enum TaskUpdate {
    /// Byte-transfer progress.
    Bytes {
        /// Bytes transferred so far.
        done: u64,
        /// Total bytes expected, or `0` when unknown.
        total: u64,
    },
    /// A line of captured output.
    Line(Box<str>),
    /// Layer count progress for a multi-layer image pull.
    LayerProgress {
        /// Layers fully pulled so far.
        done: usize,
        /// Total layers discovered so far.
        total: usize,
    },
}

/// A lifecycle transition of a DAG node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeTransition {
    /// Scheduled and waiting on its inputs.
    Queued,
    /// Inputs are ready; the node is running.
    Started,
    /// Finished by reusing a cached result.
    Cached,
    /// Finished by executing.
    Ran,
}

/// A stage in the run's lifecycle.
#[derive(Debug)]
pub enum Lifecycle {
    /// The container engine connected.
    EngineReady {
        /// The container runtime that answered, such as `podman` or `docker`.
        runtime: Box<str>,
        /// The tag of the serpentine engine image in use.
        image_tag: Box<str>,
    },
    /// The pipeline was fully parsed
    PipelineParsed {
        /// The number of nodes
        total_nodes: usize,
        /// The name of the file name
        pipeline: Box<str>,
    },
    /// The engine has started shutting down: saving the cache and tearing down containers.
    ShuttingDown,
    /// The run failed, carrying the diagnostic as it is rendered to stderr.
    Failed {
        /// The rendered report.
        report: Box<str>,
    },
    /// The run is over and the consumer should leave its event loop.
    Stop,
}

/// An event emitted by the engine.
#[derive(Debug)]
pub enum SerpentineEvent {
    /// A node changed state.
    Node(NodeTransition),
    /// A task started.
    TaskStarted {
        /// The new task's id.
        id: TaskId,
        /// The kind of work.
        kind: TaskKind,
        /// An image reference or command to show alongside it.
        label: Box<str>,
    },
    /// A task made progress.
    Task {
        /// The task's id.
        id: TaskId,
        /// The update.
        update: TaskUpdate,
    },
    /// A task finished.
    TaskFinished {
        /// The task's id.
        id: TaskId,
    },
    /// The run reached a lifecycle stage.
    Lifecycle(Lifecycle),
    /// An engine log line.
    Log {
        /// The severity the line was logged at.
        level: log::Level,
        /// The module path the line came from.
        target: Box<str>,
        /// The formatted message.
        message: Box<str>,
    },
}

/// A handle the engine sends [`SerpentineEvent`]s through.
///
/// Cloning is cheap; clones share the [`TaskId`] counter. A reporter built with [`Reporter::none`]
/// discards every event, which is how the engine runs without a consumer.
#[derive(Clone)]
pub struct Reporter {
    /// The channel to the consumer, if there is one.
    sink: Option<Sender<SerpentineEvent>>,
    /// Source of unique [`TaskId`]s, shared across clones.
    next_task: Arc<AtomicU64>,
}

impl Reporter {
    /// Build a reporter from an optional sink channel.
    fn new(sink: Option<Sender<SerpentineEvent>>) -> Self {
        Self {
            sink,
            next_task: Arc::new(AtomicU64::new(0)),
        }
    }

    /// A reporter with no consumer; every send is discarded.
    pub fn none() -> Self {
        Self::new(None)
    }

    /// A reporter paired with the receiver its consumer drains.
    pub fn channel() -> (Self, Receiver<SerpentineEvent>) {
        let (sender, receiver) = std::sync::mpsc::channel();
        (Self::new(Some(sender)), receiver)
    }

    /// Send an event, discarding it when there is no consumer or it has hung up.
    fn emit(&self, event: SerpentineEvent) {
        if let Some(sink) = &self.sink {
            let _ = sink.send(event);
        }
    }

    /// Report a node lifecycle transition.
    pub fn node(&self, transition: NodeTransition) {
        self.emit(SerpentineEvent::Node(transition));
    }

    /// Report a run lifecycle stage.
    pub fn lifecycle(&self, lifecycle: Lifecycle) {
        self.emit(SerpentineEvent::Lifecycle(lifecycle));
    }

    /// Report an engine log line.
    pub fn log(&self, level: log::Level, target: &str, message: Box<str>) {
        self.emit(SerpentineEvent::Log {
            level,
            target: target.into(),
            message,
        });
    }

    /// Start tracking a task, returning a handle that finishes it when dropped.
    pub fn start_task(&self, kind: TaskKind, label: impl Into<Box<str>>) -> TaskHandle {
        let id = TaskId(self.next_task.fetch_add(1, Ordering::Relaxed));
        self.emit(SerpentineEvent::TaskStarted {
            id,
            kind,
            label: label.into(),
        });
        TaskHandle {
            id,
            reporter: self.clone(),
        }
    }

    /// Report byte-transfer progress for a task by id.
    ///
    /// For call sites that update a task from somewhere the [`TaskHandle`] cannot reach, such as a
    /// `move` stream combinator that keeps the handle alive in the outer scope.
    pub fn task_bytes(&self, id: TaskId, done: u64, total: u64) {
        self.emit(SerpentineEvent::Task {
            id,
            update: TaskUpdate::Bytes { done, total },
        });
    }

    /// Report a line of captured output for a task by id.
    pub fn task_line(&self, id: TaskId, line: Box<str>) {
        self.emit(SerpentineEvent::Task {
            id,
            update: TaskUpdate::Line(line),
        });
    }

    /// Report layer-count progress for an image pull task by id.
    pub fn task_layer_progress(&self, id: TaskId, done: usize, total: usize) {
        self.emit(SerpentineEvent::Task {
            id,
            update: TaskUpdate::LayerProgress { done, total },
        });
    }
}

/// Tracks a single task for as long as it is held. Dropping it finishes the task.
pub struct TaskHandle {
    /// The task this handle tracks.
    id: TaskId,
    /// The reporter to emit through.
    reporter: Reporter,
}

impl TaskHandle {
    /// The id of the tracked task, for reporting progress through the [`Reporter`].
    pub fn id(&self) -> TaskId {
        self.id
    }
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        self.reporter
            .emit(SerpentineEvent::TaskFinished { id: self.id });
    }
}
