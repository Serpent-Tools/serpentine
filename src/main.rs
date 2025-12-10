#![doc = include_str!(concat!("../", std::env!("CARGO_PKG_README")))]

use std::borrow::Cow;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::Parser;
use miette::Diagnostic;
use thiserror::Error;

mod engine;
mod snek;
mod tui;

/// Serpentine is a build system and programming language.
///
/// CACHE CACHE CACHE CACHE -- HELLO CACHE
#[derive(clap::Parser)]
struct Cli {
    /// Action to take
    #[command(subcommand)]
    command: Command,
}

/// Return the path to the cache file to use
fn get_default_cache_file() -> PathBuf {
    if let Some(project_dirs) = directories::ProjectDirs::from("org", "serpent-tools", "serpentine")
    {
        project_dirs.cache_dir().join("cache.bincode")
    } else {
        log::warn!("Failed to determine default cache location.");
        PathBuf::from("./cache.bincode")
    }
}

/// Subcommands for serpentine
#[derive(clap::Subcommand)]
enum Command {
    /// Run a serpentine pipeline
    Run(Run),
    /// Clear out serpentine's cache.
    Clean {
        /// The cache file to clean
        cache: Option<PathBuf>,
    },
    /// Render a graph of the given pipeline.
    Graph {
        /// The pipeline to use
        #[arg(short, long, default_value = "./main.snek")]
        pipeline: PathBuf,
        /// The output file
        #[arg(short, long, default_value = "./serpentine.svg")]
        output: PathBuf,
    },
}

/// Arguments for the run command
#[derive(clap::Args)]
struct Run {
    /// The pipeline to use
    #[arg(short, long, default_value = "./main.snek")]
    pipeline: PathBuf,
    /// The entry point to use for the pipeline
    #[arg(short, long, default_value = "DEFAULT")]
    entry_point: String,
    /// CI mode, disables TUI and logs directly to stdout.
    #[arg(long)]
    ci: bool,
    /// Location of the cache file
    #[arg(short, long)]
    cache: Option<PathBuf>,
    /// Also export docker images, and any other external data referenced by the cache to the cache
    /// file.
    ///
    /// This is intended for use with CI, or generally when the cache needs to be transferred
    /// between systems.
    #[arg(long)]
    standalone_cache: bool,
    /// Delete old cache entries (also cleans out stale docker images).
    #[arg(long)]
    clean_old: bool,
    /// Limit of the number of parallel exec jobs allowed to run
    ///
    /// Due to most build systems already using all available cores it usually smart to set this to
    /// a smaller value, at least when first priming caches.
    #[arg(short, long, default_value_t = 2)]
    jobs: usize,
}

impl Run {
    /// Should we use the tui?
    ///
    /// This checks the `--ci` flag and whether we are in an interactive terminal
    fn use_tui(&self) -> bool {
        if self.ci {
            false
        } else {
            std::io::stdout().is_terminal()
        }
    }

    /// Get the cache to use
    fn get_cache(&self) -> Cow<'_, Path> {
        if let Some(cache) = &self.cache {
            Cow::Borrowed(cache.as_ref())
        } else {
            Cow::Owned(get_default_cache_file())
        }
    }
}
/// An error produced by serpentine
#[derive(Debug, Error, Diagnostic)]
enum SerpentineError {
    /// We failed to compile the file.
    #[error("Compile Error")]
    Compile {
        /// The source code that produced the compile error
        #[source_code]
        source_code: snek::span::OwnedVirtualFile,
        /// The compile Error
        #[related]
        error: Vec<snek::CompileError>,
    },

    /// Something failed at runtime.
    #[error(transparent)]
    #[diagnostic(transparent)]
    Runtime(engine::RuntimeError),
}

/// Setup logging using `fern`.
///
/// Only logs `Info` or higher levels from non-serpentine sources.
/// Logs to file in `~/.local/share/serpentine/logs` (or equivalent on other platforms), at TRACE level.
/// If `non_tui` is false sends logs at DEBUG level to `tui`.
/// If `non_tui` is true sends logs at TRACE level to stdout.
fn setup_logging(tui: tui::TuiSender, non_tui: bool) -> miette::Result<()> {
    let project_dirs = directories::ProjectDirs::from("org", "serpent-tools", "serpentine")
        .ok_or_else(|| miette::miette!("Failed to determine log directory"))?;

    let log_dir = project_dirs.data_local_dir().join("logs");

    let current_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|timestamp| timestamp.as_secs())
        .unwrap_or(0);

    let log_file = log_dir.join(format!("{current_timestamp}.log"));
    println!("Saving logs in {}", log_file.display());

    std::fs::create_dir_all(&log_dir).map_err(|error| {
        miette::miette!(
            "Failed to create log directory {}: {}",
            log_dir.display(),
            error
        )
    })?;

    fern::Dispatch::new()
        .filter(|metadata| {
            // Filter out noisy docker logs
            if metadata.target().starts_with("serpentine") {
                true
            } else {
                metadata.level() <= log::Level::Info
            }
        })
        .chain(
            fern::Dispatch::new()
                .format(|out, message, record| {
                    out.finish(format_args!(
                        "[{}][{}] {}",
                        record.level(),
                        record.target(),
                        message
                    ));
                })
                .level(log::LevelFilter::Trace)
                .chain(
                    fern::log_file(log_file)
                        .map_err(|error| miette::miette!("Failed to open log file: {}", error))?,
                ),
        )
        .chain(fern::Dispatch::new().chain(if non_tui {
            fern::Dispatch::new()
                .format(|out, message, record| {
                    out.finish(format_args!(
                        "[{}][{}] {}",
                        record.level(),
                        record.target(),
                        message
                    ));
                })
                .level(log::LevelFilter::Trace)
                .chain(std::io::stdout())
        } else {
            fern::Dispatch::new()
                .level(log::LevelFilter::Debug)
                .chain(fern::Output::call(move |record| {
                    let message = record.args().to_string();
                    tui.send(tui::TuiMessage::Log(message.into_boxed_str()));
                }))
        }))
        .apply()
        .map_err(|error| miette::miette!("Failed to initialize logging: {}", error))?;
    Ok(())
}

fn main() -> miette::Result<()> {
    let command = Cli::parse();

    match command.command {
        Command::Run(run) => handle_run(&run),
        Command::Clean { cache } => {
            setup_logging(tui::TuiSender(None), true)?;
            engine::clear_cache(&cache.unwrap_or_else(get_default_cache_file))?;
            println!("Cleaned out the cache.");
            Ok(())
        }
        Command::Graph { pipeline, output } => {
            setup_logging(tui::TuiSender(None), true)?;
            let graph = snek::compile_graph(&pipeline, "DEFAULT")?;
            render_graph(graph, &output)?;
            Ok(())
        }
    }
}

/// Handle the `run` subcommand
fn handle_run(command: &Run) -> Result<(), miette::Error> {
    println!("Storing cache in {}", command.get_cache().display());

    if command.use_tui() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let res = setup_logging(tui::TuiSender(Some(sender.clone())), false);
        if let Err(error) = res {
            eprintln!("Failed to initialize logging: {error}");
        }

        log::info!("Compiling pipeline: {}", command.pipeline.display());
        let result = snek::compile_graph(&command.pipeline, &command.entry_point)?;

        log::info!("Executing pipeline");
        let total_nodes = result.graph.len();
        let tui = std::thread::spawn(move || tui::start_tui(receiver, total_nodes));
        let result = engine::run(result, tui::TuiSender(Some(sender.clone())), command);

        log::info!("Executor returned, waiting for TUI to exit");
        let _ = sender.send(tui::TuiMessage::Shutdown);
        let _ = tui.join();
        ratatui::restore();

        result.map_err(Into::into)
    } else {
        let res = setup_logging(tui::TuiSender(None), true);
        if let Err(error) = res {
            eprintln!("Failed to initialize logging: {error}");
        }

        log::info!("Compiling pipeline: {}", command.pipeline.display());
        let result = snek::compile_graph(&command.pipeline, &command.entry_point)?;

        log::info!("Executing pipeline");
        let result = engine::run(result, tui::TuiSender(None), command);

        result.map_err(Into::into)
    }
}

/// Render the given compile result to a svg graph.
fn render_graph(graph: snek::CompileResult, output: &Path) -> Result<(), engine::RuntimeError> {
    use layout::backends::svg::SVGWriter;
    use layout::core::base::Orientation;
    use layout::core::geometry::get_size_for_str;
    use layout::core::style::{LineStyleKind, StyleAttr};
    use layout::std_shapes::shapes::{Arrow, Element, ShapeKind};
    use layout::topo::layout::VisualGraph;

    let mut nodes = Vec::new();
    let mut vg = VisualGraph::new(Orientation::TopToBottom);
    let arrow = Arrow::simple("");
    let mut phantom_arrow = Arrow::simple("");
    phantom_arrow.line_style = LineStyleKind::Dotted;
    let mut look = StyleAttr::simple();
    look.font_size = 16;

    for node in graph.graph {
        let node_impl = graph.nodes.get(node.kind);

        let label = node_impl.describe();

        let mut size = get_size_for_str(&label, look.font_size);
        size.x /= 2.0;
        size.y += 10.0;
        size.x += 10.0;

        let shape = ShapeKind::new_box(&label);
        let handle = vg.add_node(Element::create(
            shape,
            look.clone(),
            Orientation::TopToBottom,
            size,
        ));

        nodes.push(handle);

        for input in node.inputs {
            if let Some(input) = nodes.get(input.index()) {
                vg.add_edge(arrow.clone(), *input, handle);
            } else {
                debug_assert!(false, "Missing node for connection {input:?}");
            }
        }

        for input in node.phantom_inputs {
            if let Some(input) = nodes.get(input.index()) {
                vg.add_edge(phantom_arrow.clone(), *input, handle);
            } else {
                debug_assert!(false, "Missing node for connection {input:?}");
            }
        }
    }

    log::info!("Writing graph to {}", output.display());
    let mut writer = SVGWriter::new();
    vg.do_it(false, false, false, &mut writer);
    std::fs::write(output, writer.finalize())?;

    Ok(())
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "Tests")]
#[cfg(feature = "_test_docker")]
mod tests {
    use std::path::PathBuf;

    use rstest::rstest;

    #[rstest]
    #[test_log::test]
    fn live_examples(#[files("test_cases/live/**/*.snek")] path: PathBuf) {
        let graph =
            crate::snek::compile_graph(&path, "DEFAULT").expect("Failed to compile pipeline");
        let cli = crate::Run {
            pipeline: path,
            ci: true,
            cache: None,
            standalone_cache: false,
            clean_old: false,
            entry_point: "DEFAULT".into(),
            jobs: 1,
        };
        crate::engine::run(graph, crate::tui::TuiSender(None), &cli).expect("Failed to execute");
    }
}
