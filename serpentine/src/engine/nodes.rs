//! Contains the implementation of all the nodes

use std::borrow::Cow;
use std::marker::PhantomData;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use miette::{Context, IntoDiagnostic};
use typed_path::{PlatformPathBuf, UnixPath};

use crate::engine::cache::{CacheHash, CacheKey, CacheScope};
use crate::engine::data_model::{Data, DataType, NodeInstanceId, NodeKindId};
use crate::engine::filesystem::{self, FileSystem};
use crate::engine::scheduler::Scheduler;
use crate::engine::{RuntimeContext, containerd, internal};
use crate::snek::CompileError;
use crate::snek::span::{Span, Spanned};

/// A node implementation
///
/// `Send + Sync` because node implementations live in the shared `Arc<Scheduler>` and their futures
/// run on spawned tasks across worker threads.
pub trait NodeImpl: Send + Sync {
    /// Should the result of this node be looked up in, and written to, the data cache?
    ///
    /// The `Wrap` implementations cache whenever the output is one the cache can store (see
    /// [`DataType::is_cacheable`]) and `Wrap::uncached` was not applied. Reach for `uncached` when
    /// a round trip through the cache costs more than recomputing the node, or when the node's
    /// real work is a side effect rather than its return value, as with `ToHost`.
    ///
    /// Hand written implementations answer directly; `Noop`, `Join` and literals return `false`
    /// for the same "cheaper to recompute" reason.
    fn should_be_cached(&self) -> bool;

    /// A human readable name for this node, used in log lines.
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
    /// The default implementation resolves every input through the scheduler, each on its own task
    /// so independent branches of the graph run in parallel, reports the node's transitions to the
    /// reporter, and wraps `execute` in the data cache when `should_be_cached` allows it.
    ///
    /// A cache hit is health-checked before it is handed back, so a value naming containerd state
    /// that has since been dropped falls through to a real execution instead of being trusted.
    ///
    /// If you want to have lazy inputs (i.e the node might not always need all its inputs),
    /// You should overwrite this and resolve the inputs yourself.
    fn execute_raw<'scheduler>(
        &'scheduler self,
        node_id: NodeInstanceId,
        kind: NodeKindId,
        scheduler: Arc<Scheduler>,
        inputs: &'scheduler [NodeInstanceId],
    ) -> BoxFuture<'scheduler, miette::Result<Data>> {
        Box::pin(async move {
            let inputs = scheduler.resolve_all(inputs).await?;

            let context = scheduler.context();
            context
                .reporter
                .node(crate::events::NodeTransition::Started);

            let outcome: miette::Result<(Data, crate::events::NodeTransition)> = async move {
                if self.should_be_cached() {
                    let key = CacheKey {
                        node: kind,
                        inputs: &inputs,
                    };
                    let key = CacheHash::from_data(CacheScope::Data, &key).await?;
                    log::debug!("Checking cache with {key:?}");

                    if let Some(cached_value) =
                        // NOTE: braces such that the mutex lock is dropped.
                        {
                            context
                                .cache
                                .data_cache
                                .lock()
                                .map_err(|_| internal("data cache mutex poisoned"))?
                                .get(key)
                                .cloned()
                        }
                    {
                        log::debug!("Cache hit on {}", self.describe());

                        if cached_value.healthcheck(context).await {
                            let cached_value = Data::from_cacheable(cached_value);
                            return Ok((cached_value, crate::events::NodeTransition::Cached));
                        }
                        log::warn!("value {cached_value:?} failed health-check, not using cache.");
                    }

                    log::debug!("Executing {}", self.describe());
                    let result = self.execute(context, inputs).await?;

                    if let Some(result) = result.cacheable() {
                        log::debug!("Caching result of {} with {key:?}", self.describe());

                        if context.standalone_cache {
                            result.export_external_data(context).await;
                        }

                        context
                            .cache
                            .data_cache
                            .lock()
                            .map_err(|_| internal("data cache mutex poisoned"))?
                            .insert(key, result);
                    }

                    Ok((result, crate::events::NodeTransition::Ran))
                } else {
                    log::debug!("Cache not enabled for node, executing directly.");
                    let result = self.execute(context, inputs).await?;
                    Ok((result, crate::events::NodeTransition::Ran))
                }
            }
            .await;

            let transition = outcome
                .as_ref()
                .map_or(crate::events::NodeTransition::Ran, |(_, picked)| *picked);
            context.reporter.node(transition);
            outcome
                .map(|(data, _)| data)
                .map_err(|err| scheduler.node_error(node_id, err))
        })
    }

    /// Execute the node with its inputs already resolved.
    ///
    /// Called by `execute_raw`, which is also where caching happens, so an implementation that
    /// overrides `execute_raw` never has this called and can stub it out.
    fn execute<'scheduler>(
        &'scheduler self,
        context: &'scheduler Arc<RuntimeContext>,
        inputs: Vec<Data>,
    ) -> BoxFuture<'scheduler, miette::Result<Data>>;
}

/// Trait implemented on the raw types in `Data`
/// Used for unwrapping inputs in the automatic function implementation for `NodeImpl`,
/// And for converting back.
trait RawData: Sized {
    /// The canonical `DataType` for this type.
    ///
    /// For union types (like `ContainerLike`) this is arbitrary and should not be relied upon
    /// for return type inference — use `Wrap::passthrough` for those instead.
    const KIND: DataType;

    /// Check if a `DataType` can be converted to this type.
    ///
    /// Defaults to checking against `KIND`. Override for union types that accept multiple variants.
    fn accepts(dt: DataType) -> bool {
        dt == Self::KIND
    }

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

impl RawData for Arc<str> {
    const KIND: DataType = DataType::String;

    fn from_data(data: &Data) -> Option<Self> {
        if let Data::String(value) = data {
            Some(Arc::clone(value))
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

impl RawData for containerd::ServiceState {
    const KIND: DataType = DataType::Service;
    fn from_data(data: &Data) -> Option<Self> {
        if let Data::Service(value) = data {
            Some(value.clone())
        } else {
            None
        }
    }
    fn into_data(self) -> Data {
        Data::Service(self)
    }
}

impl RawData for containerd::ContainerLike {
    // Arbitrary — not used for return type inference (use Wrap::passthrough)
    const KIND: DataType = DataType::Container;

    fn accepts(dt: DataType) -> bool {
        matches!(dt, DataType::Container | DataType::Service)
    }

    fn from_data(data: &Data) -> Option<Self> {
        match data {
            Data::Container(container) => {
                Some(containerd::ContainerLike::Container(container.clone()))
            }
            Data::Service(service) => Some(containerd::ContainerLike::Service(service.clone())),
            _ => None,
        }
    }

    fn into_data(self) -> Data {
        match self {
            containerd::ContainerLike::Container(container) => Data::Container(container),
            containerd::ContainerLike::Service(service) => Data::Service(service),
        }
    }
}

/// Wrap a function with phantomdata to allow trait impls to work.
struct Wrap<F, P> {
    /// The function thats wrapped
    function: F,
    /// If true, `return_type` returns the type of the first argument instead of `R::KIND`.
    /// Used for nodes where the output type matches the input (e.g. Container -> Container).
    passthrough_return: bool,
    /// The argument types needs to exist as a generic on this type for rust trait resolution to be
    /// happy.
    ///
    /// Uses `fn() -> P` so `Wrap` is unconditionally `Send + Sync` regardless of `P` (it never
    /// actually owns a `P`).
    phantom: PhantomData<fn() -> P>,
    /// can this node be cached
    cache: bool,
}

impl<F, P> Wrap<F, P> {
    /// Create a new wrapped node with a fixed return type.
    fn new(func: F) -> Self {
        Self {
            function: func,
            passthrough_return: false,
            phantom: PhantomData,
            cache: true,
        }
    }

    /// Create a new wrapped node whose return type passes through from the first argument.
    fn passthrough(mut self) -> Self {
        self.passthrough_return = true;
        self
    }

    /// Mark this node as uncahacble
    fn uncached(mut self) -> Self {
        self.cache = false;
        self
    }
}

/// Implement `NodeImpl` for a closure of the given size
macro_rules! impl_node_impl {
    ($($arg: ident),*) => {
        #[expect(clippy::allow_attributes, reason="auto generated")]
        #[allow(warnings, reason="auto generated")]
        impl< F, R, Fut, $($arg),*> NodeImpl for Wrap<F, ($($arg),*)>
        where F: Fn(Arc<RuntimeContext>, $($arg),*) -> Fut + Send + Sync,
              Fut: Future<Output=miette::Result<R>> + Send,
              R: RawData,
              $($arg: RawData),*
        {
            fn should_be_cached(&self) -> bool {
                self.cache && R::KIND.is_cacheable()
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

                let mut first_type = None;
                let mut arguments = arguments.iter();
                $(
                    if let Some(argument) = arguments.next() {
                        if !$arg::accepts(**argument) {
                            return Err(CompileError::TypeMismatch {
                                expected: $arg::KIND.describe(),
                                got: argument.describe(),
                                location: argument.span(),
                                node: node_span,
                            })
                        }
                        if first_type.is_none() {
                            first_type = Some(**argument);
                        }
                    }
                )*

                if self.passthrough_return {
                    // The return type is the same as the first argument's type.
                    // This is used for nodes that operate on a ContainerLike and return the same variant.
                    first_type.ok_or_else(|| CompileError::ArgumentCountMismatch {
                        expected: 1,
                        got: 0,
                        location: node_span,
                    })
                } else {
                    Ok(R::KIND)
                }
            }

            fn execute<'scheduler>(
                &'scheduler self,
                context: &'scheduler Arc<RuntimeContext>,
                inputs: Vec<Data>,
            ) -> BoxFuture<'scheduler, miette::Result<Data>> {
                Box::pin(async move {
                    let mut inputs = inputs.into_iter();
                    let ($($arg),*,) = (
                        $({
                            #[cfg(false)]
                            {$arg;}

                            inputs.next().ok_or_else(|| internal("Missing arguments at runtime"))?
                        }),*,
                    );

                    $(
                        let $arg = $arg::from_data(&$arg).ok_or_else(|| internal("Type mismatch at runtime"))?;
                    )*

                    log::debug!("Executing {}", std::any::type_name::<F>());
                    Ok((self.function)(Arc::clone(context), $($arg),*).await?.into_data())
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
        _context: &'scheduler Arc<RuntimeContext>,
        inputs: Vec<Data>,
    ) -> BoxFuture<'scheduler, miette::Result<Data>> {
        Box::pin(async move {
            inputs
                .into_iter()
                .next()
                .ok_or_else(|| internal("Argument count mismatch at runtime"))
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
        _context: &'scheduler Arc<RuntimeContext>,
        _inputs: Vec<Data>,
    ) -> BoxFuture<'scheduler, miette::Result<Data>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}

/// The name of the noop node.
/// This is used by the compiler to insert noop nodes when a inlined node has phantom inputs.
pub const NOOP_NAME: &str = "Noop";

/// Create a container state from a remote image
async fn image(
    context: Arc<RuntimeContext>,
    image: Arc<str>,
) -> miette::Result<containerd::ContainerState> {
    context.containerd.pull_image(&image).await
}

/// Create a service state from a remote image
async fn image_service(
    context: Arc<RuntimeContext>,
    image: Arc<str>,
) -> miette::Result<containerd::ServiceState> {
    context.containerd.pull_service(&image).await
}

/// Run a command in a container
async fn exec(
    context: Arc<RuntimeContext>,
    mut container: containerd::ContainerLike,
    command: Arc<str>,
) -> miette::Result<containerd::ContainerLike> {
    *container = context
        .containerd
        .exec(&container, command.to_string())
        .await?;

    Ok(container)
}

/// Run a command in a container, getting its output
async fn exec_output(
    context: Arc<RuntimeContext>,
    container: containerd::ContainerLike,
    command: Arc<str>,
) -> miette::Result<Arc<str>> {
    context
        .containerd
        .exec_get_output(&container, command.to_string())
        .await
        .map(Into::into)
}

/// Read a file/folder into a tar from the host system.
async fn from_host(_context: Arc<RuntimeContext>, src: Arc<str>) -> miette::Result<FileSystem> {
    let src: PlatformPathBuf = UnixPath::new(src.as_bytes()).with_encoding();
    Ok(filesystem::LocalFiles(src).into())
}

/// Extract a `FileSystem` from a container at the given path
async fn export(
    context: Arc<RuntimeContext>,

    container: containerd::ContainerLike,
    path: Arc<str>,
) -> miette::Result<FileSystem> {
    context
        .containerd
        .export_path(&container, UnixPath::new(path.as_bytes()))
        .await
}

/// Write the given file to the host
async fn to_host(
    _context: Arc<RuntimeContext>,
    fs: FileSystem,
    path: Arc<str>,
) -> miette::Result<i128> {
    let mut reader = fs.get_reader().await?;

    let path: PlatformPathBuf = UnixPath::new(path.as_bytes()).with_encoding();
    serpentine_internal::read_filesystem_stream_to_disk(&path, &mut reader, false)
        .await
        .into_diagnostic()
        .with_context(|| format!("writing files to {}", path.display()))?;

    Ok(0)
}

/// Copy a `FileSystem` into a container at the given path
async fn with(
    context: Arc<RuntimeContext>,

    mut container: containerd::ContainerLike,
    fs: FileSystem,
    path: Arc<str>,
) -> miette::Result<containerd::ContainerLike> {
    *container = context
        .containerd
        .copy_fs_into_container(&container, fs, UnixPath::new(path.as_bytes()))
        .await?;

    Ok(container)
}

/// Modify the working directory of the container
async fn with_working_dir(
    _context: Arc<RuntimeContext>,
    container: containerd::ContainerLike,
    dir: Arc<str>,
) -> miette::Result<containerd::ContainerLike> {
    Ok(container.update_config(|config| config.set_working_dir(UnixPath::new(dir.as_bytes()))))
}

/// Set a environment variable.
async fn env(
    _context: Arc<RuntimeContext>,
    container: containerd::ContainerLike,
    env: Arc<str>,
    value: Arc<str>,
) -> miette::Result<containerd::ContainerLike> {
    Ok(container.update_config(|config| config.set_env_var(env, value)))
}

/// Get an environment variable.
async fn get_env(
    _context: Arc<RuntimeContext>,
    container: containerd::ContainerLike,
    env: Arc<str>,
) -> miette::Result<Arc<str>> {
    Ok(container
        .get_config()
        .get_env_var(&env)
        .map(Arc::clone)
        .unwrap_or_default())
}

/// Set container user.
async fn set_user(
    _context: Arc<RuntimeContext>,
    container: containerd::ContainerLike,
    user: Arc<str>,
) -> miette::Result<containerd::ContainerLike> {
    Ok(container.update_config(|config| config.set_user(user)))
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
                    expected: DataType::String.describe(),
                    got: argument.0.describe(),
                    location: argument.span(),
                    node: node_span,
                });
            }
        }

        Ok(DataType::String)
    }

    fn execute<'scheduler>(
        &'scheduler self,
        _context: &'scheduler Arc<RuntimeContext>,
        inputs: Vec<Data>,
    ) -> BoxFuture<'scheduler, miette::Result<Data>> {
        Box::pin(async move {
            inputs
                .into_iter()
                .map(|data| {
                    if let Data::String(data) = data {
                        Ok(data)
                    } else {
                        Err(internal("Type mismatch at runtime"))
                    }
                })
                .try_fold(String::new(), |mut result, data| {
                    result.push_str(&data?);
                    Ok(result)
                })
                .map(|result| Data::String(result.into()))
        })
    }
}

/// Convert a container layer to a service definition.
async fn to_service(
    _context: Arc<RuntimeContext>,
    container: containerd::ContainerState,
    entrypoint: Arc<str>,
) -> miette::Result<containerd::ServiceState> {
    Ok(container.into_service(entrypoint))
}

/// Attach a service to a container
async fn with_service(
    _context: Arc<RuntimeContext>,
    container: containerd::ContainerLike,
    service: containerd::ServiceState,
    hostname: Arc<str>,
) -> miette::Result<containerd::ContainerLike> {
    Ok(container.update_config(|config| config.with_service(service, hostname)))
}

/// Set the healthcheck to run for a service
async fn healthcheck(
    _context: Arc<RuntimeContext>,
    service: containerd::ServiceState,
    command: Arc<str>,
    timeout_seconds: i128,
) -> miette::Result<containerd::ServiceState> {
    let timeout = std::time::Duration::from_secs(
        timeout_seconds
            .try_into()
            .into_diagnostic()
            .context("healthcheck timeout")?,
    );
    Ok(service.update_service_config(|config| config.set_healthcheck(command, timeout)))
}

/// Return the list of prelude nodes
pub fn prelude() -> Vec<(&'static str, Box<dyn NodeImpl>)> {
    vec![
        (NOOP_NAME, Box::new(Noop) as Box<dyn NodeImpl>),
        ("Image", Box::new(Wrap::<_, Arc<str>>::new(image))),
        (
            "ImageService",
            Box::new(Wrap::<_, Arc<str>>::new(image_service)),
        ),
        (
            "Exec",
            Box::new(Wrap::<_, (containerd::ContainerLike, Arc<str>)>::new(exec).passthrough()),
        ),
        (
            "ExecOutput",
            Box::new(Wrap::<_, (containerd::ContainerLike, Arc<str>)>::new(
                exec_output,
            )),
        ),
        (
            "FromHost",
            Box::new(Wrap::<_, Arc<str>>::new(from_host).uncached()),
        ),
        (
            "Export",
            Box::new(Wrap::<_, (containerd::ContainerLike, Arc<str>)>::new(export).uncached()),
        ),
        (
            "ToHost",
            Box::new(Wrap::<_, (FileSystem, Arc<str>)>::new(to_host).uncached()),
        ),
        (
            "With",
            Box::new(
                Wrap::<_, (containerd::ContainerLike, FileSystem, Arc<str>)>::new(with)
                    .passthrough(),
            ),
        ),
        (
            "WorkingDir",
            Box::new(
                Wrap::<_, (containerd::ContainerLike, Arc<str>)>::new(with_working_dir)
                    .passthrough()
                    .uncached(),
            ),
        ),
        (
            "Env",
            Box::new(
                Wrap::<_, (containerd::ContainerLike, Arc<str>, Arc<str>)>::new(env)
                    .passthrough()
                    .uncached(),
            ),
        ),
        (
            "GetEnv",
            Box::new(Wrap::<_, (containerd::ContainerLike, Arc<str>)>::new(
                get_env,
            )),
        ),
        (
            "User",
            Box::new(
                Wrap::<_, (containerd::ContainerLike, Arc<str>)>::new(set_user)
                    .passthrough()
                    .uncached(),
            ),
        ),
        ("Join", Box::new(Join)),
        (
            "ToService",
            Box::new(Wrap::<_, (containerd::ContainerState, Arc<str>)>::new(
                to_service,
            )),
        ),
        (
            "WithService",
            Box::new(
                Wrap::<
                    _,
                    (
                        containerd::ContainerLike,
                        containerd::ServiceState,
                        Arc<str>,
                    ),
                >::new(with_service)
                .passthrough(),
            ),
        ),
        (
            "HealthCheck",
            Box::new(
                Wrap::<_, (containerd::ServiceState, Arc<str>, i128)>::new(healthcheck).uncached(),
            ),
        ),
    ]
}
