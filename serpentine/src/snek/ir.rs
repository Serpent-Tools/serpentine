//! This represents a snek program where all symbols have been resolved
//! and modules have been merged into a unified symbol table.

use crate::engine::data_model::{NodeKindId, Store, StoreId};
use crate::snek::span::Span;

/// A snek pipeline
#[derive(Debug)]
pub struct Pipeline {
    /// The top level symbols
    pub top_level: Body,
    /// The functions in the pipeline
    pub functions: FunctionStore,
    /// The symbol to start execution at
    pub start_point: Symbol,
}

/// A function definition
#[derive(Debug)]
pub enum Function {
    /// A builtin function.
    BuiltinFunction(NodeKindId),
    /// A custom function
    Custom {
        /// The symbol ids set for the parameters
        required_parameters: Box<[Symbol]>,
        /// The symbol ids for default paramters, with a body that should be emitted as a prefix to
        /// the function body if the paramter isnt specified (in the order that the paramters are
        /// listed).
        ///
        /// The first symbol is the value to paramter should be ultimately set to, if no value is
        /// given then body should be inlined (which will set the second symbol), and then the
        /// second symbol should be copied into the first.
        default_parameters: Box<[(Symbol, Symbol, Body)]>,
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
#[derive(Debug)]
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
#[derive(Debug)]
pub struct Body(pub Box<[Node]>);
