//! Contains the implementation of all the nodes

use std::collections::HashMap;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;

use crate::docker;
use crate::engine::data_model::{Data, DataType, NodeInstanceId};
use crate::engine::scheduler::Scheduler;
use crate::engine::{RuntimeContext, RuntimeError};
use crate::snek::CompileError;
use crate::snek::span::{Span, Spanned};

/// A node implementation
pub trait NodeImpl {
    /// Given the input types return the return type of the node.
    /// Error on invalid types
    fn return_type(
        &self,
        arguments: &[Spanned<DataType>],
        node_span: Span,
    ) -> Result<DataType, CompileError>;

    /// Execute this node, should use the scheduler to grab the value of its inputs.
    fn execute<'scheduler>(
        &'scheduler self,
        scheduler: &'scheduler Scheduler,
        inputs: &'scheduler [NodeInstanceId],
    ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + 'scheduler>>;
}

/// Trait implemented on the raw types in `Data`
/// Used for automatic node implementations to get the correct `DataType` automatically.
trait UnwrappedType: Sized {
    /// The `DataType` this would respond to
    fn data_type() -> DataType;

    /// Unwrap the `Data` into this type, returning a internal error if mismatched
    /// (compiler should have ensured types match up)
    fn from_data(data: &Data) -> Result<Self, RuntimeError>;
}

/// Convert a value back into its data variant
trait IntoData {
    /// The resulting data kind
    const RESULT: DataType;

    /// Convert this into `Data`
    fn into_data(self) -> Data;
}

impl UnwrappedType for i128 {
    fn data_type() -> DataType {
        DataType::Int
    }
    fn from_data(data: &Data) -> Result<Self, RuntimeError> {
        if let Data::Int(value) = data {
            Ok(*value)
        } else {
            Err(RuntimeError::internal("mismatched types at runtime"))
        }
    }
}

impl IntoData for i128 {
    const RESULT: DataType = DataType::Int;

    fn into_data(self) -> Data {
        Data::Int(self)
    }
}

impl UnwrappedType for Box<str> {
    fn data_type() -> DataType {
        DataType::String
    }

    fn from_data(data: &Data) -> Result<Self, RuntimeError> {
        if let Data::String(value) = data {
            Ok(value.clone())
        } else {
            Err(RuntimeError::internal("mismatched types at runtime"))
        }
    }
}

impl IntoData for Box<str> {
    const RESULT: DataType = DataType::String;

    fn into_data(self) -> Data {
        Data::String(self)
    }
}

impl UnwrappedType for docker::ContainerState {
    fn data_type() -> DataType {
        DataType::Container
    }
    fn from_data(data: &Data) -> Result<Self, RuntimeError> {
        if let Data::Container(value) = data {
            Ok(value.clone())
        } else {
            Err(RuntimeError::internal("mismatched types at runtime"))
        }
    }
}

impl IntoData for docker::ContainerState {
    const RESULT: DataType = DataType::Container;
    fn into_data(self) -> Data {
        Data::Container(self)
    }
}

/// Wrap a function with phantomdata to allow trait impls to work.
struct Wrap<F, P>(F, PhantomData<P>);

impl<F, P> Wrap<F, P> {
    /// Create a new wrapped
    fn new(func: F) -> Self {
        Self(func, PhantomData)
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
            R: IntoData,
            $($arg: UnwrappedType),*
        {
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
                        if **argument != $arg::data_type() {
                            return Err(CompileError::TypeMismatch {
                                expected: $arg::data_type().describe().to_owned(),
                                got: argument.describe().to_owned(),
                                location: argument.span(),
                                node: node_span,
                            })
                        }
                    }
                )*

                Ok(R::RESULT)
            }

            fn execute<'scheduler>(
                &'scheduler self,
                scheduler: &'scheduler Scheduler,
                inputs: &'scheduler [NodeInstanceId],
            ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + 'scheduler>> {

                Box::pin(async {
                    let mut inputs = inputs.iter();
                    let ($($arg),*,) = tokio::try_join!(
                        $({
                            #[cfg(false)]
                            {$arg;}

                            scheduler.get_output(*inputs.next().ok_or_else(|| RuntimeError::internal("Missing arguments"))?)
                        }),*
                    )?;

                    $(
                        let $arg = $arg::from_data($arg)?;
                    )*

                    log::debug!("Executing {}", std::any::type_name::<F>());
                    let context = scheduler.context();
                    context.tui.send(crate::tui::TuiMessage::RunningNode);
                    Ok((self.0)(context, $($arg),*).await?.into_data())
                })
            }
        }
    };
}

impl<F, R, Fut> NodeImpl for Wrap<F, ()>
where
    F: Fn(Rc<RuntimeContext>) -> Fut,
    Fut: Future<Output = Result<R, RuntimeError>>,
    R: IntoData,
{
    fn return_type(
        &self,
        arguments: &[Spanned<DataType>],
        node_span: Span,
    ) -> Result<DataType, CompileError> {
        let count = 0;
        if arguments.len() != count {
            return Err(CompileError::ArgumentCountMismatch {
                expected: count,
                got: arguments.len(),
                location: node_span,
            });
        }
        Ok(R::RESULT)
    }

    fn execute<'scheduler>(
        &'scheduler self,
        scheduler: &'scheduler Scheduler,
        _inputs: &'scheduler [NodeInstanceId],
    ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + 'scheduler>> {
        Box::pin(async {
            scheduler
                .context()
                .tui
                .send(crate::tui::TuiMessage::RunningNode);
            Ok((self.0)(scheduler.context()).await?.into_data())
        })
    }
}

impl_node_impl!(A);
impl_node_impl!(A, B);
impl_node_impl!(A, B, C);
impl_node_impl!(A, B, C, D);

/// A node that just returns the first input
struct Noop;

impl NodeImpl for Noop {
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
        scheduler: &'scheduler Scheduler,
        inputs: &'scheduler [NodeInstanceId],
    ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + 'scheduler>> {
        Box::pin(async {
            let input = inputs
                .first()
                .ok_or_else(|| RuntimeError::internal("Argument index out of bounds"))?;
            let input = scheduler.get_output(*input).await?;
            scheduler
                .context()
                .tui
                .send(crate::tui::TuiMessage::RunningNode);
            Ok(input.clone())
        })
    }
}

/// A node the just returns the given value
pub struct LiteralNode(pub Data);

impl NodeImpl for LiteralNode {
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
        scheduler: &'scheduler Scheduler,
        _inputs: &'scheduler [NodeInstanceId],
    ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + 'scheduler>> {
        scheduler
            .context()
            .tui
            .send(crate::tui::TuiMessage::RunningNode);
        Box::pin(async { Ok(self.0.clone()) })
    }
}

/// The name of the noop node.
/// This is used by the compiler to insert noop nodes when a inlined node has phantom inputs.
pub const NOOP_NAME: &str = "Noop";

/// Create a docker image
async fn image(
    context: Rc<RuntimeContext>,
    image: Box<str>,
) -> Result<docker::ContainerState, RuntimeError> {
    context.docker.pull_image(&image).await
}

/// Run a shell command in a container
async fn exec_sh(
    context: Rc<RuntimeContext>,
    container: docker::ContainerState,
    command: Box<str>,
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
    command: Box<str>,
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

/// Copy the given dir from host into the container at the given path
async fn with_dir(
    context: Rc<RuntimeContext>,
    container: docker::ContainerState,
    host_path: Box<str>,
    container_path: Box<str>,
) -> Result<docker::ContainerState, RuntimeError> {
    context
        .docker
        .copy_dir_into_container(&container, &host_path, &container_path)
        .await
}

/// Modify the working directory of the container
async fn with_working_dir(
    context: Rc<RuntimeContext>,
    container: docker::ContainerState,
    dir: Box<str>,
) -> Result<docker::ContainerState, RuntimeError> {
    context.docker.set_working_dir(&container, &dir).await
}

/// An Add node that adds two integers
/// Mainly used in tests (I can't be bothered to rewrite the test graphs)
async fn add(_context: Rc<RuntimeContext>, left: i128, right: i128) -> Result<i128, RuntimeError> {
    Ok(left.saturating_add(right))
}

/// Return the list of prelude nodes
pub fn prelude() -> HashMap<&'static str, Box<dyn NodeImpl>> {
    let mut nodes: HashMap<&'static str, Box<dyn NodeImpl>> = HashMap::new();

    nodes.insert(NOOP_NAME, Box::new(Noop));

    nodes.insert("Image", Box::new(Wrap::<_, Box<str>>::new(image)));
    nodes.insert(
        "ExecSh",
        Box::new(Wrap::<_, (docker::ContainerState, Box<str>)>::new(exec_sh)),
    );
    nodes.insert(
        "Exec",
        Box::new(Wrap::<_, (docker::ContainerState, Box<str>)>::new(exec)),
    );
    nodes.insert(
        "Copy",
        Box::new(Wrap::<_, (docker::ContainerState, Box<str>, Box<str>)>::new(with_dir)),
    );
    nodes.insert(
        "WorkingDir",
        Box::new(Wrap::<_, (docker::ContainerState, Box<str>)>::new(
            with_working_dir,
        )),
    );

    nodes.insert("Add", Box::new(Wrap::<_, (i128, i128)>::new(add)));

    nodes
}
