//! Contains the implementation of all the nodes

use std::borrow::Cow;
use std::marker::PhantomData;
use std::os::unix::fs::MetadataExt;
use std::pin::Pin;
use std::rc::Rc;

use crate::engine::cache::CacheKey;
use crate::engine::data_model::{
    Data,
    DataType,
    FILE_SYSTEM_FILE_TAR_NAME,
    FileSystem,
    NodeInstanceId,
    NodeKindId,
};
use crate::engine::scheduler::Scheduler;
use crate::engine::{RuntimeContext, RuntimeError, docker};
use crate::snek::CompileError;
use crate::snek::span::{Span, Spanned};

/// A node implementation
pub trait NodeImpl {
    /// Should this node be cached?
    /// In general quick to execute pure nodes should not be cached.
    /// As well as nodes that read external resources should not be cached.
    fn should_be_cached(&self) -> bool;

    /// Describe this node, used by the graph builder.
    fn describe(&self) -> Cow<'static, str>;

    /// Given the input types return the return type of the node.
    /// Error on invalid types
    fn return_type(
        &self,
        arguments: &[Spanned<DataType>],
        node_span: Span,
    ) -> Result<DataType, CompileError>;

    /// Execute this node
    ///
    /// The default implementation calls `get_output` in parallel on the scheduler,
    /// And talks with the cache to cache the result of `execute`.
    ///
    /// If you want to have lazy inputs (i.e the node might not always need all its inputs),
    /// You should overwrite this and leave `execute` empty.
    fn execute_raw<'scheduler>(
        &'scheduler self,
        kind: NodeKindId,
        scheduler: &'scheduler Scheduler,
        inputs: &'scheduler [NodeInstanceId],
    ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + 'scheduler>> {
        Box::pin(async move {
            let inputs = futures_util::future::try_join_all(
                inputs
                    .iter()
                    .map(async |input| scheduler.get_output(*input).await),
            )
            .await?;

            scheduler
                .context()
                .tui
                .send(crate::tui::TuiMessage::RunningNode);

            let key = CacheKey {
                node: kind,
                inputs: &inputs,
            };
            let key = key.sha256()?;
            log::debug!("Checking cache with {key:?}");
            if self.should_be_cached()
                && let Some(cached_value) =
                    scheduler.context().cache.lock().await.get(&key).cloned()
            {
                log::debug!("Cache hit on {}", self.describe());
                if cached_value.health_check(&scheduler.context().docker).await {
                    return Ok(cached_value);
                }
                log::warn!("value {cached_value:?} failed health-check, not using cache.");
            }

            log::debug!("Executing {}", self.describe());
            let result = self.execute(scheduler.context(), inputs).await?;

            if self.should_be_cached() {
                scheduler
                    .context()
                    .cache
                    .lock()
                    .await
                    .insert(key, result.clone());
            }

            Ok(result)
        })
    }

    /// Execute the node with pre-given inputs.
    /// This is called by `execute_raw` and can be set to empty if overwriting it.
    fn execute<'scheduler>(
        &'scheduler self,
        context: &'scheduler Rc<RuntimeContext>,
        inputs: Vec<&'scheduler Data>,
    ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + 'scheduler>>;
}

/// Trait implemented on the raw types in `Data`
/// Used for unwrapping inputs in the automatic function implementation for `NodeImpl`,
/// And for converting back.
trait RawData: Sized {
    /// The `DataType` this would respond to
    const KIND: DataType;

    /// Unwrap the `Data` into this type, returning a internal error if mismatched
    /// (compiler should have ensured types match up)
    fn from_data(data: &Data) -> Option<Self>;

    /// Convert from this into `Data`
    fn into_data(self) -> Data;
}

impl RawData for i128 {
    const KIND: DataType = DataType::Int;

    fn from_data(data: &Data) -> Option<Self> {
        if let Data::Int(value) = data {
            Some(*value)
        } else {
            None
        }
    }

    fn into_data(self) -> Data {
        Data::Int(self)
    }
}

impl RawData for Rc<str> {
    const KIND: DataType = DataType::String;

    fn from_data(data: &Data) -> Option<Self> {
        if let Data::String(value) = data {
            Some(Rc::clone(value))
        } else {
            None
        }
    }
    fn into_data(self) -> Data {
        Data::String(self)
    }
}

impl RawData for docker::ContainerState {
    const KIND: DataType = DataType::Container;
    fn from_data(data: &Data) -> Option<Self> {
        if let Data::Container(value) = data {
            Some(value.clone())
        } else {
            None
        }
    }
    fn into_data(self) -> Data {
        Data::Container(self)
    }
}

impl RawData for FileSystem {
    const KIND: DataType = DataType::FileSystem;

    fn from_data(data: &Data) -> Option<Self> {
        if let Data::FileSystem(value) = data {
            Some(value.clone())
        } else {
            None
        }
    }
    fn into_data(self) -> Data {
        Data::FileSystem(self)
    }
}

/// Wrap a function with phantomdata to allow trait impls to work.
struct Wrap<F, P> {
    /// The function thats wrapped
    function: F,
    /// Should this node be cached
    should_be_cached: bool,
    /// The argument types needs to exist as a generic on this type for rust trait resolution to be
    /// happy.
    phantom: PhantomData<P>,
}

impl<F, P> Wrap<F, P> {
    /// Create a new wrapped
    fn new(func: F, should_be_cached: bool) -> Self {
        Self {
            function: func,
            should_be_cached,
            phantom: PhantomData,
        }
    }
}

/// Implement `NodeImpl` for a closure of the given size
macro_rules! impl_node_impl {
    ($($arg: ident),*) => {
        #[expect(clippy::allow_attributes, reason="auto generated")]
        #[allow(warnings, reason="auto generated")]
        impl< F, R, Fut, $($arg),*> NodeImpl for Wrap<F, ($($arg),*)>
        where F: Fn(Rc<RuntimeContext>, $($arg),*) -> Fut,
              Fut: Future<Output=Result<R, RuntimeError>>,
              R: RawData,
              $($arg: RawData),*
        {
            fn should_be_cached(&self) -> bool {
                self.should_be_cached
            }

            fn describe(&self) -> Cow<'static, str> {
                std::any::type_name::<F>().into()
            }

            fn return_type(&self, arguments: &[Spanned<DataType>], node_span: Span) -> Result<DataType, CompileError> {
                let count = $({
                    #[cfg(false)]
                    {$arg;}
                    1
                }+)* 0;
                if arguments.len() != count {
                    return Err(CompileError::ArgumentCountMismatch {
                        expected: count,
                        got: arguments.len(),
                        location: node_span
                    })
                }

                let mut arguments = arguments.iter();
                $(
                    if let Some(argument) = arguments.next() {
                        if **argument != $arg::KIND {
                            return Err(CompileError::TypeMismatch {
                                expected: $arg::KIND.describe().to_owned(),
                                got: argument.describe().to_owned(),
                                location: argument.span(),
                                node: node_span,
                            })
                        }
                    }
                )*

                Ok(R::KIND)
            }

            fn execute<'scheduler>(
                &'scheduler self,
                context: &'scheduler Rc<RuntimeContext>,
                inputs: Vec<&'scheduler Data>,
            ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + 'scheduler>> {
                Box::pin(async move {
                    let mut inputs = inputs.into_iter();
                    let ($($arg),*,) = (
                        $({
                            #[cfg(false)]
                            {$arg;}

                            inputs.next().ok_or_else(|| RuntimeError::internal("Missing arguments at runtime"))?
                        }),*,
                    );

                    $(
                        let $arg = $arg::from_data($arg).ok_or_else(|| RuntimeError::internal("Type mismatch at runtime"))?;
                    )*

                    log::debug!("Executing {}", std::any::type_name::<F>());
                    Ok((self.function)(Rc::clone(context), $($arg),*).await?.into_data())
                })
            }
        }
    };
}

impl_node_impl!(A);
impl_node_impl!(A, B);
impl_node_impl!(A, B, C);

/// A node that just returns the first input
struct Noop;

impl NodeImpl for Noop {
    fn should_be_cached(&self) -> bool {
        false
    }

    fn describe(&self) -> Cow<'static, str> {
        "Noop".into()
    }

    fn return_type(
        &self,
        arguments: &[Spanned<DataType>],
        node_span: Span,
    ) -> Result<DataType, CompileError> {
        if let Some(arg) = arguments.first()
            && arguments.len() == 1
        {
            Ok(**arg)
        } else {
            Err(CompileError::ArgumentCountMismatch {
                expected: 1,
                got: arguments.len(),
                location: node_span,
            })
        }
    }

    fn execute<'scheduler>(
        &'scheduler self,
        _context: &'scheduler Rc<RuntimeContext>,
        inputs: Vec<&'scheduler Data>,
    ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + 'scheduler>> {
        Box::pin(async move {
            inputs
                .into_iter()
                .next()
                .ok_or_else(|| RuntimeError::internal("Argument count mismatch at runtime"))
                .cloned()
        })
    }
}

/// A node the just returns the given value
pub struct LiteralNode(pub Data);

impl NodeImpl for LiteralNode {
    fn should_be_cached(&self) -> bool {
        false
    }

    fn describe(&self) -> Cow<'static, str> {
        format!("{:?}", self.0).into()
    }

    fn return_type(
        &self,
        // Should only be constructed by `Compiler`, hence we don't check this.
        _arguments: &[Spanned<DataType>],
        _node_span: Span,
    ) -> Result<DataType, CompileError> {
        Ok(self.0.type_())
    }

    fn execute<'scheduler>(
        &'scheduler self,
        _context: &'scheduler Rc<RuntimeContext>,
        _inputs: Vec<&'scheduler Data>,
    ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + 'scheduler>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}

/// The name of the noop node.
/// This is used by the compiler to insert noop nodes when a inlined node has phantom inputs.
pub const NOOP_NAME: &str = "Noop";

/// Create a docker image
async fn image(
    context: Rc<RuntimeContext>,
    image: Rc<str>,
) -> Result<docker::ContainerState, RuntimeError> {
    context.docker.pull_image(&image).await
}

/// Run a shell command in a container
async fn exec_sh(
    context: Rc<RuntimeContext>,
    container: docker::ContainerState,
    command: Rc<str>,
) -> Result<docker::ContainerState, RuntimeError> {
    context
        .docker
        .exec(&container, &["/bin/sh", "-c", &command])
        .await
}

/// Run a command in a container
async fn exec(
    context: Rc<RuntimeContext>,
    container: docker::ContainerState,
    command: Rc<str>,
) -> Result<docker::ContainerState, RuntimeError> {
    let command = shell_words::split(&command)?;

    context
        .docker
        .exec(
            &container,
            &command.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
        )
        .await
}

/// Run a command in a container, getting its output
async fn exec_output(
    context: Rc<RuntimeContext>,
    container: docker::ContainerState,
    command: Rc<str>,
) -> Result<Rc<str>, RuntimeError> {
    let command = shell_words::split(&command)?;

    context
        .docker
        .exec_get_output(
            &container,
            &command.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
        )
        .await
        .map(Into::into)
}

/// Read a file/folder into a tar from the host system.
// PERF: Rewrite to use `tar_async`?
async fn from_host(_context: Rc<RuntimeContext>, src: Rc<str>) -> Result<FileSystem, RuntimeError> {
    let src: std::path::PathBuf = src.to_string().into();

    if src.is_dir() {
        let tar_data = tokio::task::spawn_blocking(move || read_folder_to_tar(src))
            .await
            .map_err(|_| RuntimeError::internal("Tokio join error"))??;

        Ok(FileSystem::Folder(tar_data.into()))
    } else {
        let tar_data = tokio::task::spawn_blocking(move || -> Result<_, RuntimeError> {
            let mut tar_data = Vec::new();
            {
                let mut tar_builder = tar::Builder::new(&mut tar_data);
                let file = std::fs::File::open(src)?;
                let mut header = tar::Header::new_gnu();
                let metadata = file.metadata()?;
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(metadata.mode());
                header.set_size(metadata.len());
                header.set_cksum();
                tar_builder.append_data(&mut header, FILE_SYSTEM_FILE_TAR_NAME, file)?;
                tar_builder.finish()?;
            }
            Ok(tar_data)
        })
        .await
        .map_err(|_| RuntimeError::internal("Tokio join error"))??;

        Ok(FileSystem::File(tar_data.into()))
    }
}

/// Read the given path into a tar
#[expect(clippy::needless_pass_by_value, reason = "Spawned in a thread")]
fn read_folder_to_tar(src: std::path::PathBuf) -> Result<Vec<u8>, RuntimeError> {
    let mut tar_data = Vec::new();
    let paths = ignore::WalkBuilder::new(&src)
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .build()
        .filter_map(Result::ok);

    {
        let mut tar_builder = tar::Builder::new(&mut tar_data);

        for entry in paths {
            let path = entry.path();
            let relative_path = path.strip_prefix(&src).unwrap_or(path);

            if relative_path.to_string_lossy().is_empty() {
                continue;
            }

            if path.is_file() {
                let file = std::fs::File::open(path)?;
                let mut header = tar::Header::new_gnu();
                let metadata = file.metadata()?;
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(metadata.mode());
                header.set_size(metadata.len());
                header.set_cksum();
                tar_builder.append_data(&mut header, relative_path, file)?;
            } else if path.is_dir() {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(path.metadata()?.mode());
                header.set_size(0);
                header.set_cksum();
                tar_builder.append(&header, std::io::empty())?;
            }
        }

        tar_builder.finish()?;
    }
    Ok(tar_data)
}

/// Extract a `FileSystem` from a container at the given path
async fn export(
    context: Rc<RuntimeContext>,

    container: docker::ContainerState,
    path: Rc<str>,
) -> Result<FileSystem, RuntimeError> {
    context.docker.export_path(&container, &path).await
}

/// Write the given file to the host
async fn to_host(
    _context: Rc<RuntimeContext>,
    fs: FileSystem,
    path: Rc<str>,
) -> Result<i128, RuntimeError> {
    let data = match fs {
        FileSystem::File(data) | FileSystem::Folder(data) => data,
    };

    let mut archive = tar::Archive::new(&*data);
    log::info!("Writing fs to {path}");
    archive.unpack(&*path)?;

    Ok(0)
}

/// Copy a `FileSystem` into a container at the given path
async fn with(
    context: Rc<RuntimeContext>,

    container: docker::ContainerState,
    fs: FileSystem,
    path: Rc<str>,
) -> Result<docker::ContainerState, RuntimeError> {
    context
        .docker
        .copy_fs_into_container(&container, fs, &path)
        .await
}

/// Modify the working directory of the container
async fn with_working_dir(
    context: Rc<RuntimeContext>,
    container: docker::ContainerState,
    dir: Rc<str>,
) -> Result<docker::ContainerState, RuntimeError> {
    context.docker.set_working_dir(&container, &dir).await
}

/// Set a environment variable.
async fn env(
    context: Rc<RuntimeContext>,
    container: docker::ContainerState,
    env: Rc<str>,
    value: Rc<str>,
) -> Result<docker::ContainerState, RuntimeError> {
    context.docker.set_env_var(&container, &env, &value).await
}

/// A node for joining strings
struct Join;

impl NodeImpl for Join {
    fn should_be_cached(&self) -> bool {
        false
    }

    fn describe(&self) -> Cow<'static, str> {
        "Join".into()
    }

    fn return_type(
        &self,
        arguments: &[Spanned<DataType>],
        node_span: Span,
    ) -> Result<DataType, CompileError> {
        for argument in arguments {
            if argument.0 != DataType::String {
                return Err(CompileError::TypeMismatch {
                    expected: DataType::String.describe().into(),
                    got: argument.0.describe().into(),
                    location: argument.span(),
                    node: node_span,
                });
            }
        }

        Ok(DataType::String)
    }

    fn execute<'scheduler>(
        &'scheduler self,
        _context: &Rc<RuntimeContext>,
        inputs: Vec<&'scheduler Data>,
    ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + 'scheduler>> {
        Box::pin(async move {
            inputs
                .into_iter()
                .map(|data| {
                    if let Data::String(data) = data {
                        Ok(data)
                    } else {
                        Err(RuntimeError::internal("Type mismatch at runtime"))
                    }
                })
                .try_fold(String::new(), |mut result, data| {
                    result.push_str(data?);
                    Ok(result)
                })
                .map(|result| Data::String(result.into()))
        })
    }
}

/// Return the list of prelude nodes
pub fn prelude() -> Vec<(&'static str, Box<dyn NodeImpl>)> {
    vec![
        (NOOP_NAME, Box::new(Noop) as Box<dyn NodeImpl>),
        // cache reasoning: Image is at best a simple check with the docker client, which is super
        // quick, and more importantly the output is consistent.
        ("Image", Box::new(Wrap::<_, Rc<str>>::new(image, false))),
        // cache reasoning: Executing command is the bulk of what serpentine does and takes the most
        // time, this is the primary target for caching
        (
            "ExecSh",
            Box::new(Wrap::<_, (docker::ContainerState, Rc<str>)>::new(
                exec_sh, true,
            )),
        ),
        // cache reasoning: see ExecSh
        (
            "Exec",
            Box::new(Wrap::<_, (docker::ContainerState, Rc<str>)>::new(
                exec, true,
            )),
        ),
        // cache reasoning: see ExecSh
        (
            "ExecOutput",
            Box::new(Wrap::<_, (docker::ContainerState, Rc<str>)>::new(
                exec_output,
                true,
            )),
        ),
        // cache reasoning: We need to check with the host system on each run to know whether the output
        // of this is still valid, if the files didn't change on disk this will still spend some time
        // reading them, but will result in the same result afterwards and further nodes will have cache
        // hits.
        (
            "FromHost",
            Box::new(Wrap::<_, Rc<str>>::new(from_host, false)),
        ),
        // cache reasoning: Copying the files from a container is usually a quick job, and keeping such
        // files in the cache is just duplicating data that is also stored in docker.
        // The one downside here is that we have to spin up a container to export the data, but that
        // should be quick enough.
        (
            "Export",
            Box::new(Wrap::<_, (docker::ContainerState, Rc<str>)>::new(
                export, false,
            )),
        ),
        (
            "ToHost",
            Box::new(Wrap::<_, (FileSystem, Rc<str>)>::new(to_host, false)),
        ),
        // cache reasoning: While this operation is generally quick it is a primary candidate for
        // caching, not because the operation is nice to skip, but because it isn't truly pure as
        // docker will likely give a new image id even when run with the same inputs.
        // Hence we use the CaC to skip this node if the input files and the input image match the
        // cache.
        (
            "With",
            Box::new(Wrap::<_, (docker::ContainerState, FileSystem, Rc<str>)>::new(with, true)),
        ),
        // cache reasoning: See With
        (
            "WorkingDir",
            Box::new(Wrap::<_, (docker::ContainerState, Rc<str>)>::new(
                with_working_dir,
                true,
            )),
        ),
        // cache reasoning: See With
        (
            "Env",
            Box::new(Wrap::<_, (docker::ContainerState, Rc<str>, Rc<str>)>::new(
                env, true,
            )),
        ),
        ("Join", Box::new(Join)),
    ]
}
