//! This is a version of a snek programmed where all modules are in one big virtual file.
//! And all symbols have been resolved to a global symbol table.

use smallvec::SmallVec;

use crate::engine::data_model::{NodeKindId, Store, StoreId};

/// A function definition
pub enum Function {
    /// A builtin function.
    BuiltinFunction(NodeKindId),
    /// A custom function
    Custom {
        /// The symbol ids set for the paramaters
        paramaters: SmallVec<[Symbol; 2]>,
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

/// A function reference
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Symbol(pub usize);

/// A node in the graph
pub struct Node {
    /// The name to reference the node by
    pub name: Symbol,
    /// The function that node is
    pub function: Function,
    /// Arguments to the function
    pub arguments: SmallVec<[Symbol; 2]>,
    /// Phantom inputs to the node
    pub phantom_inputs: SmallVec<[Symbol; 2]>,
}

/// The top level body or the body of a function.
pub struct Body(Box<[Node]>);
