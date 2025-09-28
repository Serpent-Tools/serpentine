//! Contains the implementation of all the nodes

use std::collections::HashMap;
use std::marker::PhantomData;
use std::pin::Pin;

use crate::engine::RuntimeError;
use crate::engine::data_model::{Data, DataType, NodeInstanceId};
use crate::engine::scheduler::Scheduler;
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
where F: Fn($($arg),*) -> Fut,
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

                    Ok((self.0)($($arg),*).await?.into_data())
                })
            }
        }
    };
}

#[expect(clippy::allow_attributes, reason = "auto generated")]
#[allow(warnings, reason = "auto generated")]
impl<F, R, Fut> NodeImpl for Wrap<F, ()>
where
    F: Fn() -> Fut,
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
        inputs: &'scheduler [NodeInstanceId],
    ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + 'scheduler>> {
        Box::pin(async {
            let mut inputs = inputs.iter();
            Ok((self.0)().await?.into_data())
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
            Ok(input.clone())
        })
    }
}

/// A node the just returns the given value
pub struct LiteralNode(pub Data);

impl NodeImpl for LiteralNode {
    fn return_type(
        &self,
        _arguments: &[Spanned<DataType>],
        _node_span: Span,
    ) -> Result<DataType, CompileError> {
        // This node is directly constructed by the compiler and shouldnt end up in the type check
        // flow.
        Err(CompileError::internal("return_type called on literal node"))
    }

    fn execute<'scheduler>(
        &'scheduler self,
        _scheduler: &'scheduler Scheduler,
        _inputs: &'scheduler [NodeInstanceId],
    ) -> Pin<Box<dyn Future<Output = Result<Data, RuntimeError>> + 'scheduler>> {
        Box::pin(async { Ok(self.0.clone()) })
    }
}

/// Return the list of prelude nodes
pub fn prelude() -> HashMap<&'static str, Box<dyn NodeImpl>> {
    let mut nodes: HashMap<&'static str, Box<dyn NodeImpl>> = HashMap::new();

    nodes.insert("Noop", Box::new(Noop));
    nodes.insert(
        "Add",
        Box::new(Wrap::<_, (i128, i128)>::new(async |x: i128, y: i128| {
            Ok(x.saturating_add(y))
        })),
    );
    nodes.insert(
        "Len",
        Box::new(Wrap::<_, Box<str>>::new(async |x: Box<str>| {
            Ok(x.len() as i128)
        })),
    );
    nodes.insert(
        "Sleep",
        Box::new(Wrap::<_, (i128, i128)>::new(
            async |value: i128, seconds: i128| {
                tokio::time::sleep(std::time::Duration::from_secs(
                    seconds.try_into().unwrap_or(u64::MAX),
                ))
                .await;
                Ok(value)
            },
        )),
    );
    nodes
}
