//! Compile a snek ast into a graph

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use smallvec::SmallVec;

use crate::engine::data_model::{
    Data, DataType, Graph, Node, NodeInstanceId, NodeKindId, NodeStorage,
};
use crate::engine::nodes;
use crate::snek::span::{FileId, Span};
use crate::snek::{self, CompileError, ast};

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

#[derive(Debug, Clone)]
enum NodeItem {
    /// A direct node kind backed by a async rust function
    BuiltIn(NodeKindId),
    /// A custom user function in snek
    ///
    /// Currently these are implemented via inlining, as it simplifies generic functions,
    /// wiring up the nodes, etc.
    Function {
        /// The names of the parameters, when inlining these will be used to construct a temporary
        /// scope.
        params: Box<[Box<str>]>,
        /// The ast nodes in the body.
        body: Box<[ast::Statement]>,
        /// The span of the function name
        source: Span,
    },
}

/// A item in a scope
#[derive(Clone)]
enum ScopeItem {
    /// A node in the scoe
    Node(NodeItem),
    /// A value in the scope
    Value(Value),
    /// A module in the scope
    Module(FileId),
}

impl ScopeItem {
    /// Return a string describing this items kind
    fn kind_str(&self) -> &'static str {
        match self {
            ScopeItem::Node(_) => "node",
            ScopeItem::Value(_) => "value",
            ScopeItem::Module(_) => "module",
        }
    }

    /// Attempt to extract the node item
    fn node(&self, location: Span) -> Result<&NodeItem, CompileError> {
        if let ScopeItem::Node(node) = self {
            Ok(node)
        } else {
            Err(CompileError::WrongItemKind {
                expected: "node",
                got: self.kind_str(),
                location,
            })
        }
    }

    /// Attempt to extract the value item
    fn label(&self, location: Span) -> Result<&Value, CompileError> {
        if let ScopeItem::Value(value) = self {
            Ok(value)
        } else {
            Err(CompileError::WrongItemKind {
                expected: "value",
                got: self.kind_str(),
                location,
            })
        }
    }

    /// Attempt to extract the module item
    fn module(&self, location: Span) -> Result<FileId, CompileError> {
        if let ScopeItem::Module(file_id) = self {
            Ok(*file_id)
        } else {
            Err(CompileError::WrongItemKind {
                expected: "module",
                got: self.kind_str(),
                location,
            })
        }
    }
}

/// A scope
#[derive(Clone)]
struct Scope<'parent> {
    /// The node names in the local scope
    items: HashMap<Box<str>, ScopeItem>,
    /// The values in the parent scope.
    parent: Option<&'parent Scope<'parent>>,
}

impl Scope<'static> {
    /// Create a new scope
    fn new() -> Self {
        Self {
            items: HashMap::new(),
            parent: None,
        }
    }

    /// Get a builtin node.
    /// This is bound to static as the prelude scope is static.
    fn builtin(&self, name: &'static str) -> Result<NodeKindId, CompileError> {
        let Some(item) = self.items.get(name) else {
            return Err(CompileError::internal("Builtin node not found in prelude"));
        };

        let ScopeItem::Node(NodeItem::BuiltIn(node_id)) = item else {
            return Err(CompileError::internal("Builtin node not a node"));
        };

        Ok(*node_id)
    }
}

impl Scope<'_> {
    /// Create a sub scope of this one.
    fn child(&self) -> Scope<'_> {
        Scope {
            items: HashMap::new(),
            parent: Some(self),
        }
    }

    /// Lookup a value in this scope
    fn get(&self, name: &ast::Ident) -> Result<&ScopeItem, CompileError> {
        if let Some(item) = self.items.get(&**name.0) {
            Ok(item)
        } else if let Some(parent) = self.parent {
            parent.get(name)
        } else {
            Err(CompileError::ItemNotFound {
                ident: name.0.to_string(),
                location: name.0.span(),
            })
        }
    }

    /// Define a new node in this scope
    fn define(
        &mut self,
        name: Box<str>,
        node: ScopeItem,
        location: Span,
    ) -> Result<(), CompileError> {
        if self
            .get(&ast::Ident(Span::dummy().with(name.clone())))
            .is_ok()
        {
            Err(CompileError::ShadowedName {
                ident: name.to_string(),
                location,
            })
        } else {
            self.items.insert(name, node);
            Ok(())
        }
    }
}

/// Resolve the given path pair, handling special paths like `@` etc.
/// `base` should be the path to a *file*, not a directory.
fn resolve_path(base: &Path, relative: &str) -> PathBuf {
    let relative = PathBuf::from(relative);
    let result = base
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(relative);

    if result.extension().is_some() {
        result
    } else {
        result.with_extension("snek")
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

/// A module is a compiled file, and holds the exported values from it.
#[derive(Clone)]
struct Module<'prelude> {
    /// The items exported from this module.
    exports: HashSet<Box<str>>,
    /// The top level scope of the module (used when inlining functions from this module)
    scope: Scope<'prelude>,
}

/// A context for compiling statements
enum StatementContext<'context> {
    /// This statement is in the top level of a module
    TopLevel {
        /// The modules exports, should be populated by `export` statements
        exports: &'context mut HashSet<Box<str>>,
        /// The path to the file
        file_path: PathBuf,
    },
    /// This statemetn is being compiled while inlining a user defined node
    Function {
        /// The return value of the function, should be set by a return statement.
        /// Should be set exactly once.
        return_value: &'context mut Option<Value>,
    },
}

impl StatementContext<'_> {
    /// Define a export in this context
    fn define_export(&mut self, name: Box<str>, location: Span) -> Result<(), CompileError> {
        match self {
            StatementContext::TopLevel { exports, .. } => {
                exports.insert(name);
                Ok(())
            }
            StatementContext::Function { .. } => Err(CompileError::InvalidStatement {
                stmt: "export",
                context: "function body",
                maybe: Some("return"),
                location,
            }),
        }
    }
}

/// Compiler for snek files into a graph
struct Compiler<'prelude> {
    /// The graph
    graph: Graph,
    /// All the node implementations
    nodes: NodeStorage,
    /// The files we are compiling so far.
    files: snek::span::VirtualFile,
    /// The modules
    modules: HashMap<FileId, Module<'prelude>>,
    /// The prelude scope
    prelude: &'prelude Scope<'static>,
}

/// Compile the given file path into a graph
/// Bootstraps the prelude, and other needed data before passing onto the compilers compile file
/// method.
pub fn compile_graph(path: &Path) -> Result<CompileResult, crate::SerpentineError> {
    let target_value = "DEFAULT";

    let mut prelude = Scope::new();
    let mut node_storage = NodeStorage::new();

    for (name, node) in nodes::prelude() {
        let node_id = node_storage.push(node);

        let res = prelude.define(
            name.into(),
            ScopeItem::Node(NodeItem::BuiltIn(node_id)),
            Span::dummy(),
        );
        debug_assert!(res.is_ok(), "Prelude node names should not conflict");
    }

    let mut compiler = Compiler::new(node_storage, &prelude);
    let cli_id = compiler.files.push(PathBuf::from("<cli>"), target_value);
    let cli_span = Span::new(cli_id, 0, target_value.len());

    let file_id = match compiler.compile_file(path) {
        Ok(id) => id,
        Err(err) => {
            return Err(crate::SerpentineError::Compile {
                source_code: compiler.files,
                error: vec![err],
            });
        }
    };

    let start_value =
        match compiler.get_from_module(file_id, &ast::Ident(cli_span.with(target_value.into()))) {
            Ok(start_value) => start_value,
            Err(err) => {
                return Err(crate::SerpentineError::Compile {
                    source_code: compiler.files,
                    error: vec![
                        CompileError::EntryPointNotFound {
                            name: target_value.into(),
                        },
                        err,
                    ],
                });
            }
        };

    let start_value = match start_value.label(cli_span) {
        Ok(value) => value,
        Err(err) => {
            return Err(crate::SerpentineError::Compile {
                source_code: compiler.files,
                error: vec![err],
            });
        }
    };

    let start_node = start_value.node;

    Ok(CompileResult {
        graph: compiler.graph,
        nodes: compiler.nodes,
        start_node,
    })
}

impl<'prelude> Compiler<'prelude> {
    /// Create a new compiler instance.
    fn new(nodes: NodeStorage, prelude: &'prelude Scope<'static>) -> Self {
        Self {
            graph: Graph::new(),
            nodes,
            files: snek::span::VirtualFile::new(),
            modules: HashMap::new(),
            prelude,
        }
    }

    /// Compile the given file path and return the module id
    fn compile_file(&mut self, path: &Path) -> Result<FileId, CompileError> {
        log::debug!("Compiling file: {}", path.display());

        let code = std::fs::read_to_string(path).map_err(|err| CompileError::FileReading {
            file: path.to_path_buf(),
            inner: err,
        })?;
        let file_id = self.files.push(path.to_path_buf(), &code);
        if self.modules.contains_key(&file_id) {
            return Ok(file_id);
        }

        let tokens = snek::tokenizer::Tokenizer::tokenize(file_id, &code)?;
        let ast = snek::parser::Parser::parse_file(tokens)?;

        let mut scope = self.prelude.child();
        let mut exports = HashSet::new();

        self.compile_statements(
            &ast.0,
            StatementContext::TopLevel {
                exports: &mut exports,
                file_path: path.to_path_buf(),
            },
            &mut scope,
        )?;

        self.modules.insert(file_id, Module { exports, scope });
        Ok(file_id)
    }

    /// Compile a list of statements, returning the return id
    fn compile_statements(
        &mut self,
        statements: &[ast::Statement],
        mut context: StatementContext<'_>,
        scope: &mut Scope,
    ) -> Result<(), CompileError> {
        for stmt in statements {
            self.compile_statement(stmt, &mut context, scope)?;
        }

        Ok(())
    }

    /// Compile the given statement, returning if it was a return statement
    fn compile_statement(
        &mut self,
        statement: &ast::Statement,
        context: &mut StatementContext<'_>,
        scope: &mut Scope,
    ) -> Result<(), CompileError> {
        match statement {
            ast::Statement::Label {
                export,
                expression,
                label,
            } => {
                let value = self.compile_expression(expression, scope)?;
                scope.define((**label.0).into(), ScopeItem::Value(value), label.0.span())?;

                if let Some(export_span) = export {
                    context.define_export((**label.0).into(), *export_span)?;
                }
            }
            ast::Statement::Return {
                return_kw,
                expression,
            } => match context {
                StatementContext::TopLevel { .. } => {
                    return Err(CompileError::InvalidStatement {
                        stmt: "return",
                        context: "top level",
                        maybe: Some("export"),
                        location: *return_kw,
                    });
                }
                StatementContext::Function { return_value } => {
                    if return_value.is_some() {
                        return Err(CompileError::DoubleReturn {
                            location: *return_kw,
                        });
                    }
                    let value = self.compile_expression(expression, scope)?;
                    **return_value = Some(value);
                }
            },

            ast::Statement::Function {
                export,
                name,
                parameters,
                statements,
            } => {
                if let StatementContext::TopLevel { .. } = context {
                    scope.define(
                        (*name.0).clone(),
                        ScopeItem::Node(NodeItem::Function {
                            params: parameters
                                .into_iter()
                                .map(|ident| (*ident.0).clone())
                                .collect(),
                            body: statements.clone(),
                            source: name.0.span(),
                        }),
                        name.0.span(),
                    )?;

                    if let Some(export_span) = export {
                        context.define_export((*name.0).clone(), *export_span)?;
                    }
                } else {
                    return Err(CompileError::InvalidStatement {
                        stmt: "def",
                        context: "function body",
                        maybe: None,
                        location: name.0.span(),
                    });
                }
            }
            ast::Statement::Import { export, path, name } => {
                let StatementContext::TopLevel { file_path, .. } = context else {
                    return Err(CompileError::InvalidStatement {
                        stmt: "import",
                        context: "function body",
                        maybe: None,
                        location: name.0.span(),
                    });
                };

                let resolved_path = resolve_path(file_path, &path.0);

                let module_id = match self.compile_file(&resolved_path) {
                    Ok(id) => id,
                    Err(err) => {
                        return Err(CompileError::ImportError {
                            module: resolved_path.display().to_string(),
                            error: Box::new(err),
                            location: path.span(),
                        });
                    }
                };

                scope.define(
                    (*name.0).clone(),
                    ScopeItem::Module(module_id),
                    name.0.span(),
                )?;

                if let Some(export_span) = export {
                    context.define_export((*name.0).clone(), *export_span)?;
                }
            }
        }

        Ok(())
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
                let item = self.resolve_item_path(scope, &label.0)?;
                let value = item.label(label.0.span())?;
                return Ok(value.clone());
            }
            ast::Expression::Number(value) => (Data::Int(**value), DataType::Int, value.span()),
            ast::Expression::String(value) => (
                Data::String(value.as_ref().into()),
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
        let node_item = self
            .resolve_item_path(scope, &node.name)?
            .node(node.name.span())?
            .clone();

        let mut arguments = Vec::new();

        if let Some(previous) = previous {
            arguments.push(previous);
        }

        for argument in &node.arguments {
            let result = self.compile_expression(argument, scope)?;
            arguments.push(result);
        }

        let mut phantom_inputs = Vec::new();
        for input in &node.phantom_inputs {
            let value = self.resolve_item_path(scope, input)?.label(input.span())?;
            phantom_inputs.push(value.node);
        }

        match node_item {
            NodeItem::BuiltIn(kind) => {
                let node_impl = self
                    .nodes
                    .get(kind)
                    .ok_or_else(|| CompileError::internal("NodeKindID out of bounds"))?;

                let types = arguments
                    .iter()
                    .map(|arg| arg.span.with(arg.type_))
                    .collect::<Vec<_>>();
                let nodes = arguments
                    .iter()
                    .map(|arg| arg.node)
                    .collect::<SmallVec<_>>();

                let return_type = node_impl.return_type(&types, node.name.span())?;

                let graph_node = Node {
                    kind,
                    inputs: nodes,
                    phantom_inputs: phantom_inputs.into(),
                };
                let node_id = self.graph.push(graph_node);

                Ok(Value {
                    node: node_id,
                    type_: return_type,
                    span: node.name.span(),
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
                        location: node.name.span(),
                    });
                }

                let parent_scope = match self.modules.get(&source.file_id) {
                    Some(module) => &module.scope.clone(),
                    None => scope,
                };

                let mut function_scope = parent_scope.child();

                for (name, value) in params.iter().zip(arguments) {
                    function_scope.define(name.clone(), ScopeItem::Value(value), source)?;
                }

                let mut return_value = None;
                let res = self.compile_statements(
                    &body,
                    StatementContext::Function {
                        return_value: &mut return_value,
                    },
                    &mut function_scope,
                );

                if let Err(err) = res {
                    return Err(CompileError::InlineError {
                        error: Box::new(err),
                        call: node.name.span(),
                    });
                }

                let Some(return_value) = return_value else {
                    return Err(CompileError::ReturnNotFound { location: source });
                };

                let phantom_node = self.graph.push(Node {
                    kind: self.prelude.builtin(nodes::NOOP_NAME)?,
                    inputs: vec![return_value.node].into(),
                    phantom_inputs: phantom_inputs.into(),
                });

                Ok(Value {
                    node: phantom_node,
                    type_: return_value.type_,
                    span: node.name.span(),
                })
            }
        }
    }

    /// Resolve an item path in the given scope and item path
    fn resolve_item_path<'this, 'scope>(
        &'this self,
        scope: &'scope Scope,
        path: &ast::ItemPath,
    ) -> Result<&'scope ScopeItem, CompileError>
    where
        'this: 'scope,
    {
        let mut item = scope.get(&path.base)?;
        let mut item_span = path.base.0.span();

        for ident in &path.rest {
            let module_id = item.module(item_span)?;
            item = self.get_from_module(module_id, ident)?;
            item_span = ident.0.span();
        }

        Ok(item)
    }

    /// Lookup a item in a module, ensuring it is exported
    fn get_from_module(
        &self,
        module_id: FileId,
        ident: &ast::Ident,
    ) -> Result<&ScopeItem, CompileError> {
        let module = self
            .modules
            .get(&module_id)
            .ok_or_else(|| CompileError::internal("Module FileId not found"))?;

        if !module.exports.contains(&**ident.0) {
            return Err(CompileError::ItemNotFound {
                ident: ident.0.to_string(),
                location: ident.0.span(),
            });
        }

        module.scope.get(ident)
    }
}
