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
    pub start_node: NodeInstanceId,
}

enum NodeItem<'src> {
    /// A direct node kind backed by a async rust function
    BuiltIn(NodeKindId),
    /// A custom user function in snek
    ///
    /// Currently these are implemented via inlining, as it simplifies generic functions,
    /// wiring up the nodes, etc.
    Function {
        /// The names of the paramaters, when inlining these will be used to construct a temporary
        /// scope.
        params: Box<[Box<str>]>,
        /// The ast nodes in the body.
        body: Box<[ast::Statement<'src>]>,
        /// The span of the function name
        source: Span,
    },
}

/// A scope
struct Scope<'parent, 'src> {
    /// The node names in the local scope
    nodes: HashMap<Box<str>, NodeItem<'src>>,
    /// Labels for node values
    labels: HashMap<Box<str>, Value>,
    /// The values in the parent scope.
    parent: Option<&'parent Scope<'parent, 'src>>,
}

impl Scope<'static, 'static> {
    /// Construct a scope of the prelude
    fn prelude(compiler: &mut Compiler) -> Self {
        let nodes = nodes::prelude()
            .into_iter()
            .map(|(name, node)| (name.into(), NodeItem::BuiltIn(compiler.nodes.push(node))))
            .collect();
        Self {
            nodes,
            labels: HashMap::new(),
            parent: None,
        }
    }
}

impl<'src> Scope<'_, 'src> {
    /// Create a sub scope of this one.
    fn child<'this>(&'this self) -> Scope<'this, 'src> {
        Scope {
            nodes: HashMap::new(),
            labels: HashMap::new(),
            parent: Some(self),
        }
    }

    /// Lookup builtin node
    fn builtin(&self, name: &str) -> Result<NodeKindId, CompileError> {
        if let Some(parent) = self.parent {
            parent.builtin(name)
        } else {
            let Some(NodeItem::BuiltIn(id)) = self.nodes.get(name) else {
                return Err(CompileError::internal(format!(
                    "Requested builtin {name} not found"
                )));
            };
            Ok(*id)
        }
    }

    /// Lookup a node in this scope
    fn node(&self, name: Spanned<&str>) -> Result<&NodeItem<'src>, CompileError> {
        if let Some(node) = self.nodes.get(&**name) {
            Ok(node)
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

    /// Define a new node in this scope
    fn define_node(&mut self, name: Box<str>, node: NodeItem<'src>) {
        self.nodes.insert(name, node);
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

pub struct Compiler {
    /// The graph
    graph: Graph,
    /// All the node implementations
    nodes: NodeStorage,
}

impl Compiler {
    /// Compile a file into its result.
    pub fn compile_file(file: &ast::File) -> Result<CompileResult, CompileError> {
        let mut compiler = Self {
            graph: Graph::new(),
            nodes: NodeStorage::new(),
        };

        let prelude = Scope::prelude(&mut compiler);
        let mut scope = prelude.child();

        let Some(return_value) = compiler.compile_statements(&file.0, &mut scope)? else {
            return Err(CompileError::ReturnNotFound {
                in_what: "file",
                location: Span::new(0, 0),
            });
        };

        Ok(CompileResult {
            graph: compiler.graph,
            nodes: compiler.nodes,
            start_node: return_value.node,
        })
    }

    /// Compile a list of statements, returning the return id
    fn compile_statements<'src>(
        &mut self,
        statements: &[ast::Statement<'src>],
        scope: &mut Scope<'_, 'src>,
    ) -> Result<Option<Value>, CompileError> {
        let mut return_value = None;
        for stmt in statements {
            if let Some(value) = self.compile_statement(stmt, scope)? {
                if return_value.is_some() {
                    return Err(CompileError::DoubleReturn {
                        location: value.span,
                    });
                }

                return_value = Some(value);
            }
        }
        Ok(return_value)
    }

    /// Compile the given statement, returning if it was a return statement
    fn compile_statement<'src>(
        &mut self,
        statement: &ast::Statement<'src>,
        scope: &mut Scope<'_, 'src>,
    ) -> Result<Option<Value>, CompileError> {
        match statement {
            ast::Statement::Expression { expression, label } => {
                let value = self.compile_expression(expression, scope)?;
                if let Some(label) = label {
                    scope.define_label((**label.0).into(), value);
                }
            }
            ast::Statement::Return(expression) => {
                let value = self.compile_expression(expression, scope)?;
                return Ok(Some(value));
            }
            ast::Statement::Function {
                name,
                paramters,
                statements,
            } => {
                scope.define_node(
                    (*name.0).into(),
                    NodeItem::Function {
                        params: paramters
                            .into_iter()
                            .map(|ident| (*ident.0).into())
                            .collect(),
                        body: statements.clone(),
                        source: name.calc_span(),
                    },
                );
            }
        }

        Ok(None)
    }

    /// Compile a expression, returning the value
    fn compile_expression(
        &mut self,
        expression: &ast::Expression,
        scope: &Scope,
    ) -> Result<Value, CompileError> {
        let (data, type_, span) = match expression {
            ast::Expression::Node(node) => return self.compile_node(node, None, scope),
            ast::Expression::Chain(chain) => return self.compile_chain(chain, scope),
            ast::Expression::Label(label) => {
                return scope
                    .label(label.calc_span().with((**label.0).into()))
                    .cloned();
            }
            ast::Expression::Number(value) => (Data::Int(**value), DataType::Int, value.span()),
            ast::Expression::String(value) => (
                Data::String((**value).into()),
                DataType::String,
                value.span(),
            ),
        };

        let kind = self.nodes.push(Box::new(nodes::LiteralNode(data)));
        let id = self.graph.push(Node {
            kind,
            inputs: SmallVec::new(),
            phantom_inputs: SmallVec::new(),
        });
        Ok(Value {
            node: id,
            type_,
            span,
        })
    }

    /// Compile a node chain
    fn compile_chain(&mut self, chain: &ast::Chain, scope: &Scope) -> Result<Value, CompileError> {
        let mut current_value = self.compile_expression(&chain.start, scope)?;
        for node in &chain.nodes {
            current_value = self.compile_node(node, Some(current_value), scope)?;
        }
        Ok(current_value)
    }

    /// Compile a node returning its value
    fn compile_node(
        &mut self,
        node: &ast::Node,
        previous: Option<Value>,
        scope: &Scope,
    ) -> Result<Value, CompileError> {
        let node_item = scope.node(node.name.0)?;
        let span = node.calc_span();

        let mut argument_nodes = Vec::new();
        let mut argument_types = Vec::new();
        let mut arguments = Vec::new();

        if let Some(previous) = previous {
            arguments.push(previous.clone());
            argument_nodes.push(previous.node);
            argument_types.push(previous.span.with(previous.type_));
        }

        for argument in &node.arguments {
            let result = self.compile_expression(argument, scope)?;
            arguments.push(result.clone());
            argument_nodes.push(result.node);
            argument_types.push(result.span.with(result.type_));
        }

        let mut phantom_inputs = Vec::new();
        for input in &node.phantom_inputs {
            phantom_inputs.push(
                scope
                    .label(input.calc_span().with((**input.0).into()))?
                    .node,
            );
        }

        match node_item {
            NodeItem::BuiltIn(kind) => {
                let node_impl = self
                    .nodes
                    .get(*kind)
                    .ok_or_else(|| CompileError::internal("NodeKindID out of bounds"))?;
                let return_type = node_impl.return_type(&argument_types, node.name.calc_span())?;

                let graph_node = Node {
                    kind: *kind,
                    inputs: argument_nodes.into(),
                    phantom_inputs: phantom_inputs.into(),
                };
                let node_id = self.graph.push(graph_node);

                Ok(Value {
                    node: node_id,
                    type_: return_type,
                    span,
                })
            }
            NodeItem::Function {
                params,
                body,
                source,
            } => {
                if arguments.len() != params.len() {
                    return Err(CompileError::ArgumentCountMismatch {
                        expected: params.len(),
                        got: arguments.len(),
                        location: node.name.calc_span(),
                    });
                }

                let mut function_scope = scope.child();
                for (name, value) in params.iter().zip(arguments) {
                    function_scope.define_label(name.clone(), value);
                }

                let return_value = match self.compile_statements(body, &mut function_scope) {
                    Ok(Some(return_value)) => return_value,
                    Ok(None) => {
                        return Err(CompileError::ReturnNotFound {
                            in_what: "function",
                            location: *source,
                        });
                    }
                    Err(err) => {
                        return Err(CompileError::InlineError {
                            error: Box::new(err),
                            call: span,
                        });
                    }
                };

                let phantom_node = self.graph.push(Node {
                    kind: scope.builtin("Noop")?,
                    inputs: vec![return_value.node].into(),
                    phantom_inputs: phantom_inputs.into(),
                });

                Ok(Value {
                    node: phantom_node,
                    type_: return_value.type_,
                    span,
                })
            }
        }
    }
}
