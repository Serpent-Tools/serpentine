//! Compile the snek ir into a snek graph by inlining all function calls.

use std::collections::HashMap;

use super::resolver::ResolveResult;
use super::{CompileError, ir};
use crate::engine::data_model::{DataType, Graph, Node, NodeInstanceId, NodeKindId, NodeStorage};
use crate::snek::span::Span;

/// The result of compiling
pub struct CompileResult {
    /// The node implementations in the graph
    pub nodes: NodeStorage,
    /// The node instances
    pub graph: Graph,
    /// The id of the starting node
    pub start_node: NodeInstanceId,
}

/// Value pointed to by a symbol
#[derive(Clone)]
struct SymbolValue {
    /// The node of the symbol
    node: NodeInstanceId,
    /// The type of the value
    type_: DataType,
    /// The origin of this value
    span: Span,
}

/// Holds the immutable data for the compiler to allow holding references into it
struct ImmutableContext {
    /// Ir functions
    functions: ir::FunctionStore,
    /// Builtin nodes
    nodes: NodeStorage,
    /// The id of the noop node
    noop: NodeKindId,
}

/// Holds the common state for the compiler
struct Compiler {
    /// The graph we are building
    graph: Graph,
    /// The current symbol mapping
    symbol_mapping: HashMap<ir::Symbol, SymbolValue>,
    /// Cache of nodes to their instance ids,
    /// This lets us detect functionally identical nodes and deduplicate them.
    node_cache: HashMap<Node, NodeInstanceId>,
}

/// Compile the given resolved ir to a graph
pub fn compile(resolve_result: ResolveResult) -> Result<CompileResult, crate::SerpentineError> {
    let ResolveResult {
        ir:
            ir::Pipeline {
                top_level,
                functions,
                start_point,
            },
        nodes,
        files,
        noop,
    } = resolve_result;

    let context = ImmutableContext {
        functions,
        nodes,
        noop,
    };
    let mut compiler = Compiler {
        graph: Graph::new(),
        symbol_mapping: HashMap::new(),
        node_cache: HashMap::new(),
    };

    for node in top_level.0 {
        if let Err(err) = compiler.compile_node(&context, &node) {
            return Err(crate::SerpentineError::Compile {
                source_code: files,
                error: vec![err],
            });
        }
    }

    let start_node = compiler.get_symbol(start_point).node;
    Ok(CompileResult {
        nodes: context.nodes,
        graph: compiler.graph,
        start_node,
    })
}

impl Compiler {
    /// Compile the given node into the graph, handling both builtin and custom calls
    fn compile_node(
        &mut self,
        context: &ImmutableContext,
        node: &ir::Node,
    ) -> Result<(), CompileError> {
        let ir::Node {
            name,
            function,
            arguments,
            phantom_inputs,
            span,
        } = node;

        let arguments = arguments
            .iter()
            .map(|symbol| self.get_symbol(*symbol).clone())
            .collect::<Box<_>>();
        let phantom_inputs = phantom_inputs
            .iter()
            .map(|input| self.get_symbol(*input).node)
            .collect::<Box<_>>();

        let function = context.functions.get(*function);
        let (instance_id, return_type) = match function {
            ir::Function::BuiltinFunction(node_impl_id) => {
                self.compile_builtin(context, *node_impl_id, arguments, phantom_inputs, *span)?
            }
            ir::Function::Custom {
                parameters,
                body,
                return_value,
            } => {
                self.set_symbols_from_arguments(parameters, arguments, *span)?;
                for function_node in &body.0 {
                    self.compile_node(context, function_node)?;
                }
                let return_value = self.get_symbol(*return_value).clone();
                let node_id = self.create_phantom_noop(context, return_value.node, phantom_inputs);

                (node_id, return_value.type_)
            }
        };

        self.symbol_mapping.insert(
            *name,
            SymbolValue {
                node: instance_id,
                type_: return_type,
                span: *span,
            },
        );

        Ok(())
    }

    /// Compile a builtin node call
    fn compile_builtin(
        &mut self,
        context: &ImmutableContext,
        node_impl_id: NodeKindId,
        arguments: Box<[SymbolValue]>,
        phantom_inputs: Box<[NodeInstanceId]>,
        span: Span,
    ) -> Result<(NodeInstanceId, DataType), CompileError> {
        let node_impl = context.nodes.get(node_impl_id);

        let argument_types = arguments
            .iter()
            .map(|arg| arg.span.with(arg.type_))
            .collect::<Box<_>>();
        let return_type = node_impl.return_type(&argument_types, span)?;

        let argument_ids = arguments.into_iter().map(|arg| arg.node).collect();
        let node = Node {
            kind: node_impl_id,
            inputs: argument_ids,
            phantom_inputs: phantom_inputs.into_vec().into(),
        };

        let instance_id = if let Some(cached_value) = self.node_cache.get(&node) {
            *cached_value
        } else {
            let instance_id = self.graph.push(node.clone());
            self.node_cache.insert(node, instance_id);
            instance_id
        };

        Ok((instance_id, return_type))
    }

    /// Update the symbol map from the argument and parameter pairing.
    fn set_symbols_from_arguments(
        &mut self,
        parameters: &[ir::Symbol],
        arguments: Box<[SymbolValue]>,
        span: Span,
    ) -> Result<(), CompileError> {
        if arguments.len() != parameters.len() {
            return Err(CompileError::ArgumentCountMismatch {
                expected: parameters.len(),
                got: arguments.len(),
                location: span,
            });
        }

        for (argument, parameter) in arguments.into_iter().zip(parameters) {
            self.symbol_mapping.insert(*parameter, argument);
        }

        Ok(())
    }

    /// Create a Noop node to handle the phantom inputs for a custom function, returns its id
    fn create_phantom_noop(
        &mut self,
        context: &ImmutableContext,
        function_output: NodeInstanceId,
        phantom_inputs: Box<[NodeInstanceId]>,
    ) -> NodeInstanceId {
        self.graph.push(Node {
            kind: context.noop,
            inputs: Box::new([function_output]),
            phantom_inputs: phantom_inputs.into_vec().into(),
        })
    }

    /// Retrieve the node pointed to by a symbol at this point in time
    #[expect(
        clippy::expect_used,
        reason = "Symbols should always have been defined by the resolver before they are used."
    )]
    #[must_use]
    fn get_symbol(&self, symbol: ir::Symbol) -> &SymbolValue {
        self.symbol_mapping
            .get(&symbol)
            .expect("Symbol not defined at usage site")
    }
}
