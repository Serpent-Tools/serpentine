//! This represents a snek program where all symbols have been resolved
//! and modules have been merged into a unified symbol table.

use std::fmt::Write;

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

impl Pipeline {
    /// Print out a plaintext version of the pipeline
    pub fn pretty_debug(&self) -> String {
        let mut result = String::new();

        for (id, function) in self.functions.iter().enumerate() {
            let _ = writeln!(result, "def ${id}{}", function.pretty_debug());
        }
        for node in &self.top_level.0 {
            let _ = writeln!(result, "{}", node.pretty_debug());
        }

        result
    }
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
        /// The symbol ids for default parameters, with a body that should be emitted as a prefix to
        /// the function body if the parameter isnt specified (in the order that the parameters are
        /// listed).
        ///
        /// The first symbol is the value to parameter should be ultimately set to, if no value is
        /// given then body should be inlined (which will set the second symbol), and then the
        /// second symbol should be copied into the first.
        default_parameters: Box<[(Symbol, Symbol, Body)]>,
        /// The body of the function
        body: Body,
        /// The return symbol of the function
        return_value: Symbol,
    },
}

impl Function {
    /// Pretty print this function definition (after the `def $123` part)
    fn pretty_debug(&self) -> String {
        match self {
            Self::BuiltinFunction(id) => format!("(...) {{/* builtin {} */}}", id.index()),
            Self::Custom {
                required_parameters,
                default_parameters,
                body,
                return_value,
            } => {
                let mut parameters = Vec::new();
                for param in required_parameters {
                    parameters.push(format!("%{}", param.0));
                }
                for (param, default_symbol, default_body) in default_parameters {
                    let mut body_repr = String::new();
                    for node in &default_body.0 {
                        let _ = write!(body_repr, "{} ", node.pretty_debug());
                    }
                    parameters.push(format!(
                        "%{} = %{} {{ {body_repr}}}",
                        param.0, default_symbol.0
                    ));
                }
                let parameters = parameters.join(", ");

                let mut body_repr = String::new();
                for node in &body.0 {
                    let _ = writeln!(body_repr, "\t{}", node.pretty_debug());
                }
                let _ = writeln!(body_repr, "\treturn %{};", return_value.0);

                format!("({parameters}) {{\n{body_repr}}}")
            }
        }
    }
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

impl Node {
    /// Provide a pretty string of the given node for debugging
    pub fn pretty_debug(&self) -> String {
        let arguments = self
            .arguments
            .iter()
            .map(|arg| format!("%{}", arg.0))
            .collect::<Vec<_>>()
            .join(", ");

        let phantom_inputs = self
            .phantom_inputs
            .iter()
            .map(|arg| format!("%{}", arg.0))
            .collect::<Vec<_>>()
            .join(", ");

        format!(
            "%{} = !({}) ${}({});",
            self.name.0,
            phantom_inputs,
            self.function.index(),
            arguments
        )
    }
}

/// The top level body or the body of a function.
#[derive(Debug)]
pub struct Body(pub Box<[Node]>);
