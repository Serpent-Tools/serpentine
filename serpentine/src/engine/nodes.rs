//! Contains the implementation of all the nodes

use std::borrow::Cow;
use std::marker::PhantomData;
use std::path::Path;
use std::pin::Pin;
use std::rc::Rc;

use crate::engine::cache::CacheKey;
use crate::engine::data_model::{Data, DataType, NodeInstanceId, NodeKindId};
use crate::engine::filesystem::{self, FileSystem};
use crate::engine::scheduler::Scheduler;
use crate::engine::{RuntimeContext, RuntimeError, containerd};
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

            if self.should_be_cached() {
                let key = CacheKey {
                    node: kind,
                    inputs: &inputs,
                };
                let key = key.content_hash().await?;
                log::debug!("Checking cache with {key:?}");
                if let Some(cached_value) = scheduler.context().cache.lock().await.get(key).cloned()
                {
                    log::debug!("Cache hit on {}", self.describe());
                    if cached_value.healthcheck(scheduler.context()).await {
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
            } else {
                log::debug!("Cache not enabled for node, executing directly.");
                self.execute(scheduler.context(), inputs).await
            }
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

impl RawData for containerd::ContainerState {
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
) -> Result<containerd::ContainerState, RuntimeError> {
    context.containerd.pull_image(&image).await
}

/// Run a command in a container
async fn exec(
    context: Rc<RuntimeContext>,
    container: containerd::ContainerState,
    command: Rc<str>,
) -> Result<containerd::ContainerState, RuntimeError> {
    context
        .containerd
        .exec(&container, command.to_string())
        .await
}

/// Run a command in a container, getting its output
async fn exec_output(
    context: Rc<RuntimeContext>,
    container: containerd::ContainerState,
    command: Rc<str>,
) -> Result<Rc<str>, RuntimeError> {
    context
        .containerd
        .exec_get_output(&container, command.to_string())
        .await
        .map(Into::into)
}

/// Read a file/folder into a tar from the host system.
// PERF: Rewrite to use `tar_async`?
async fn from_host(_context: Rc<RuntimeContext>, src: Rc<str>) -> Result<FileSystem, RuntimeError> {
    Ok(filesystem::LocalFiles(src.as_ref().into()).into())
}

/// Extract a `FileSystem` from a container at the given path
async fn export(
    context: Rc<RuntimeContext>,

    container: containerd::ContainerState,
    path: Rc<str>,
) -> Result<FileSystem, RuntimeError> {
    context.containerd.export_path(&container, &path).await
}

/// Write the given file to the host
async fn to_host(
    _context: Rc<RuntimeContext>,
    fs: FileSystem,
    path: Rc<str>,
) -> Result<i128, RuntimeError> {
    let mut reader = fs.get_reader().await?;

    serpentine_internal::read_filesystem_stream_to_disk(Path::new(&*path), &mut reader, false)
        .await?;

    Ok(0)
}

/// Copy a `FileSystem` into a container at the given path
async fn with(
    context: Rc<RuntimeContext>,

    container: containerd::ContainerState,
    fs: FileSystem,
    path: Rc<str>,
) -> Result<containerd::ContainerState, RuntimeError> {
    context
        .containerd
        .copy_fs_into_container(&container, fs, &path)
        .await
}

/// Modify the working directory of the container
async fn with_working_dir(
    context: Rc<RuntimeContext>,
    container: containerd::ContainerState,
    dir: Rc<str>,
) -> Result<containerd::ContainerState, RuntimeError> {
    Ok(context.containerd.set_working_dir(&container, &dir))
}

/// Set a environment variable.
async fn env(
    context: Rc<RuntimeContext>,
    container: containerd::ContainerState,
    env: Rc<str>,
    value: Rc<str>,
) -> Result<containerd::ContainerState, RuntimeError> {
    Ok(context.containerd.set_env_var(&container, env, value))
}

/// get a environment variable.
async fn get_env(
    context: Rc<RuntimeContext>,
    container: containerd::ContainerState,
    env: Rc<str>,
) -> Result<Rc<str>, RuntimeError> {
    Ok(context
        .containerd
        .get_env_var(&container, &env)
        .map(Rc::clone)
        .unwrap_or_default())
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
        ("Image", Box::new(Wrap::<_, Rc<str>>::new(image, true))),
        (
            "Exec",
            Box::new(Wrap::<_, (containerd::ContainerState, Rc<str>)>::new(
                exec, true,
            )),
        ),
        (
            "ExecOutput",
            Box::new(Wrap::<_, (containerd::ContainerState, Rc<str>)>::new(
                exec_output,
                true,
            )),
        ),
        (
            "FromHost",
            Box::new(Wrap::<_, Rc<str>>::new(from_host, false)),
        ),
        (
            "Export",
            Box::new(Wrap::<_, (containerd::ContainerState, Rc<str>)>::new(
                export, false,
            )),
        ),
        (
            "ToHost",
            Box::new(Wrap::<_, (FileSystem, Rc<str>)>::new(to_host, false)),
        ),
        (
            "With",
            Box::new(Wrap::<_, (containerd::ContainerState, FileSystem, Rc<str>)>::new(with, true)),
        ),
        (
            "WorkingDir",
            Box::new(Wrap::<_, (containerd::ContainerState, Rc<str>)>::new(
                with_working_dir,
                false,
            )),
        ),
        (
            "Env",
            Box::new(Wrap::<_, (containerd::ContainerState, Rc<str>, Rc<str>)>::new(env, false)),
        ),
        (
            "GetEnv",
            Box::new(Wrap::<_, (containerd::ContainerState, Rc<str>)>::new(
                get_env, false,
            )),
        ),
        ("Join", Box::new(Join)),
    ]
}
