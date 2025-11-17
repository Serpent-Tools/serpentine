//! This represents a snek program where all symbols have been resolved
//! and modules have been merged into a unified symbol table.

use crate::engine::data_model::{NodeKindId, Store, StoreId};
use crate::snek::span::Span;

/// A snek pipeline
pub struct Pipeline {
    /// The top level symbols
    pub top_level: Body,
    /// The functions in the pipeline
    pub functions: FunctionStore,
    /// The symbol to start execution at
    pub start_point: Symbol,
}

/// A function definition
pub enum Function {
    /// A builtin function.
    BuiltinFunction(NodeKindId),
    /// A custom function
    Custom {
        /// The symbol ids set for the parameters
        parameters: Box<[Symbol]>,
        /// The body of the function
        body: Body,
        /// The return symbol of the function
        return_value: Symbol,
    },
}

/// A store of the various functions.
pub type FunctionStore = Store<Function>;

/// A id into the function store.
pub type FunctionId = StoreId<Function>;

/// A symbol identifier
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Symbol(pub usize);

/// A node in the graph
pub struct Node {
    /// The name to reference the node by
    pub name: Symbol,
    /// The function that node is
    pub function: FunctionId,
    /// Arguments to the function
    pub arguments: Box<[Symbol]>,
    /// Phantom inputs to the node
    pub phantom_inputs: Box<[Symbol]>,
    /// The span of this value
    pub span: Span,
}

/// The top level body or the body of a function.
pub struct Body(pub Box<[Node]>);
