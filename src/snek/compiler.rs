//! Compile a snek ast into a graph

use std::collections::HashMap;

use smallvec::SmallVec;

use crate::engine::data_model::{
    Data,
    DataType,
    Graph,
    Node,
    NodeInstanceId,
    NodeKindId,
    NodeStorage,
};
use crate::engine::nodes;
use crate::snek::ast::Spannable;
use crate::snek::span::{Span, Spanned};
use crate::snek::{CompileError, ast};

/// Result of compiling a ast
#[derive(Debug)]
pub struct CompileResult {
    /// The graph
    pub graph: Graph,
    /// The nodes produces
    pub nodes: NodeStorage,
    /// The end node id
    pub end_node: Option<NodeInstanceId>,
}

/// A scope
struct Scope<'parent> {
    /// The node names in the local scope
    nodes: HashMap<Box<str>, NodeKindId>,
    /// Labels for node values
    labels: HashMap<Box<str>, Value>,
    /// The values in the parent scope.
    parent: Option<&'parent Scope<'parent>>,
}

impl Scope<'static> {
    /// Construct a scope of the prelude
    fn prelude(compile_result: &mut CompileResult) -> Self {
        let nodes = nodes::prelude()
            .into_iter()
            .map(|(name, node)| (name.into(), compile_result.nodes.push(node)))
            .collect();
        Self {
            nodes,
            labels: HashMap::new(),
            parent: None,
        }
    }
}

impl Scope<'_> {
    /// Create a sub scope of this one.
    fn child(&self) -> Scope<'_> {
        Scope {
            nodes: HashMap::new(),
            labels: HashMap::new(),
            parent: Some(self),
        }
    }

    /// Lookup a node in this scope
    fn node(&self, name: Spanned<&str>) -> Result<NodeKindId, CompileError> {
        if let Some(node) = self.nodes.get(&**name) {
            Ok(*node)
        } else if let Some(parent) = self.parent {
            parent.node(name)
        } else {
            Err(CompileError::ItemNotFound {
                kind: "Node",
                ident: name.to_string(),
                location: name.span(),
            })
        }
    }

    /// Lookup a label in this scope
    fn label(&self, name: Spanned<&str>) -> Result<&Value, CompileError> {
        if let Some(node) = self.labels.get(&**name) {
            Ok(node)
        } else if let Some(parent) = self.parent {
            parent.label(name)
        } else {
            Err(CompileError::ItemNotFound {
                kind: "Label",
                ident: name.to_string(),
                location: name.span(),
            })
        }
    }

    /// Define a new label in this scope
    fn define_label(&mut self, name: Box<str>, value: Value) {
        self.labels.insert(name, value);
    }
}

/// A value in the compiler step
#[derive(Clone)]
struct Value {
    /// The node the value is from
    node: NodeInstanceId,
    /// The type of the value.
    type_: DataType,
    /// The span of where the value is from
    span: Span,
}

/// Compile a file into its result.
pub fn compile_file(file: ast::File) -> Result<CompileResult, CompileError> {
    let mut result = CompileResult {
        graph: Graph::new(),
        nodes: NodeStorage::new(),
        end_node: None,
    };
    let prelude = Scope::prelude(&mut result);
    let mut scope = prelude.child();

    for stmt in file.0 {
        compile_statement(stmt, &mut result, &mut scope)?;
    }

    Ok(result)
}

/// Compile the given statement
fn compile_statement(
    statement: ast::Statement,
    result: &mut CompileResult,
    scope: &mut Scope,
) -> Result<(), CompileError> {
    match statement {
        ast::Statement::Expression { expression, label } => {
            let value = compile_expression(expression, result, scope)?;
            if let Some(label) = label {
                scope.define_label((**label.0).into(), value);
            }
        }
    }

    Ok(())
}

/// Compile a expression, returning the value
fn compile_expression(
    expression: ast::Expression,
    result: &mut CompileResult,
    scope: &Scope,
) -> Result<Value, CompileError> {
    let (data, type_, span) = match expression {
        ast::Expression::Node(node) => return compile_node(node, None, result, scope),
        ast::Expression::Chain(chain) => return compile_chain(chain, result, scope),
        ast::Expression::Label(label) => {
            return scope
                .label(label.calc_span().with((**label.0).into()))
                .cloned();
        }
        ast::Expression::Number(value) => (Data::Int(*value), DataType::Int, value.span()),
        ast::Expression::String(value) => (
            Data::String((**value).into()),
            DataType::String,
            value.span(),
        ),
    };

    let kind = result.nodes.push(Box::new(nodes::LiteralNode(data)));
    let id = result.graph.push(Node {
        kind,
        inputs: SmallVec::new(),
    });
    Ok(Value {
        node: id,
        type_,
        span,
    })
}

/// Compile a node chain
fn compile_chain(
    chain: ast::Chain,
    result: &mut CompileResult,
    scope: &Scope<'_>,
) -> Result<Value, CompileError> {
    let mut current_value = compile_expression(*chain.start, result, scope)?;
    for node in chain.nodes {
        current_value = compile_node(node, Some(current_value), result, scope)?;
    }
    Ok(current_value)
}

/// Compile a node returning its value
fn compile_node(
    node: ast::Node,
    previous: Option<Value>,
    result: &mut CompileResult,
    scope: &Scope,
) -> Result<Value, CompileError> {
    let kind = scope.node(node.name.0)?;
    let span = node.calc_span();

    let mut argument_nodes = Vec::new();
    let mut argument_types = Vec::new();

    if let Some(previous) = previous {
        argument_nodes.push(previous.node);
        argument_types.push(previous.span.with(previous.type_));
    }

    for argument in node.arguments {
        let result = compile_expression(argument, result, scope)?;
        argument_nodes.push(result.node);
        argument_types.push(result.span.with(result.type_));
    }

    let node_impl = result
        .nodes
        .get(kind)
        .ok_or_else(|| CompileError::internal("NodeKindID out of bounds"))?;
    let return_type = node_impl.return_type(&argument_types, node.name.calc_span())?;

    let graph_node = Node {
        kind,
        inputs: argument_nodes.into(),
    };
    let node_id = result.graph.push(graph_node);

    if *node.name.0 == "End" {
        result.end_node = Some(node_id);
    }

    Ok(Value {
        node: node_id,
        type_: return_type,
        span,
    })
}
