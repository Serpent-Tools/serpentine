//! The resolver parses the input file into an ast, resolves and parses all imported modules,
//! and processes everything into a simplified IR for further processing and type checking.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bumpalo::Bump;

use crate::engine::data_model::{Data, NodeKindId, NodeStorage, Store, StoreId};
use crate::snek::span::{Span, Spanned, VirtualFile};
use crate::snek::{CompileError, ast, ir};

/// A snek module
struct Module<'arena> {
    /// The items in the module
    items: Scope<'arena, 'arena>,
}

/// A store of modules
type ModuleStore<'arena> = Store<Module<'arena>>;

/// A module id
type ModuleId<'arena> = StoreId<Module<'arena>>;

/// Possible items in scope
#[derive(Clone, Copy)]
enum ScopeItem<'arena> {
    /// A node label
    Label(ir::Symbol),
    /// A function
    Function(ir::FunctionId),
    /// A module
    Module(ModuleId<'arena>),
}

impl<'arena> ScopeItem<'arena> {
    /// Return a string describing the kind of item
    #[must_use]
    fn kind(&self) -> &'static str {
        match self {
            Self::Module(_) => "module",
            Self::Label(_) => "label",
            Self::Function(_) => "function",
        }
    }

    /// Return a module or return a `CompileError`
    ///
    /// `error_span` is used in the any potential errors,
    /// and should point to the source of the item path that got this value.
    fn try_into_module(self, error_span: Span) -> Result<ModuleId<'arena>, CompileError> {
        if let Self::Module(module) = self {
            Ok(module)
        } else {
            Err(CompileError::WrongItemKind {
                expected: "module",
                got: self.kind(),
                location: error_span,
            })
        }
    }

    /// Return a label or return a `CompileError`
    ///
    /// `error_span` is used in the any potential errors,
    /// and should point to the source of the item path that got this value.
    fn try_into_label(self, error_span: Span) -> Result<ir::Symbol, CompileError> {
        if let Self::Label(symbol) = self {
            Ok(symbol)
        } else {
            Err(CompileError::WrongItemKind {
                expected: "label",
                got: self.kind(),
                location: error_span,
            })
        }
    }

    /// Return a function or return a `CompileError`
    ///
    /// `error_span` is used in the any potential errors,
    /// and should point to the source of the item path that got this value.
    fn try_into_function(self, error_span: Span) -> Result<ir::FunctionId, CompileError> {
        if let Self::Function(function) = self {
            Ok(function)
        } else {
            Err(CompileError::WrongItemKind {
                expected: "function",
                got: self.kind(),
                location: error_span,
            })
        }
    }
}

/// A lexical scope
///
/// Use `'parent = 'arena` for a root level, as its the biggest lifetime we can use.
/// (`'static` would force `'arena` to be `'static`)
struct Scope<'arena, 'parent> {
    /// The parent scope, used for lookups if not found in the current scope.
    parent: Option<&'parent Scope<'arena, 'parent>>,
    /// The items in the current scope
    items: HashMap<&'arena str, ScopeItem<'arena>>,
}

impl<'arena> Scope<'arena, 'arena> {
    /// Create a new empty root scope
    fn root() -> Self {
        Scope {
            parent: None,
            items: HashMap::new(),
        }
    }
}

impl<'arena> Scope<'arena, '_> {
    /// Create a child scope
    #[must_use]
    fn child<'this>(&'this self) -> Scope<'arena, 'this> {
        Scope {
            parent: Some(self),
            items: HashMap::new(),
        }
    }

    /// Insert an item into the scope
    fn insert(&mut self, name: &'arena str, item: ScopeItem<'arena>) {
        self.items.insert(name, item);
    }

    /// Lookup an item in the scope
    ///
    /// `error_span` is used for constructing the `ItemNotFound` error,
    /// and should point to the identifier that was the source of `name`
    fn lookup(&self, name: &str, error_span: Span) -> Result<&ScopeItem<'arena>, CompileError> {
        if let Some(item) = self.items.get(name) {
            Ok(item)
        } else if let Some(parent) = self.parent {
            parent.lookup(name, error_span)
        } else {
            Err(CompileError::ItemNotFound {
                ident: name.into(),
                location: error_span,
            })
        }
    }
}

/// Holds the immutable context of the resolver
///
/// This is split out and passed as its own argument to allow other arguments to hold borrows into
/// it (for example references to the prelude) without causing `self` to be immutable.
struct ImmutableContext<'arena> {
    /// The prelude scope
    prelude: Scope<'arena, 'arena>,
}

/// Context for the compilation of a statement.
enum StatementContext<'arena, 'caller> {
    /// A module/top level context
    Module {
        /// The map of exported items
        export_map: &'caller mut Scope<'arena, 'arena>,
    },
    /// A function context
    Function {
        /// The return value of the function
        return_value: &'caller mut Option<ir::Symbol>,
        /// The function's IR body.
        function_body: &'caller mut Vec<ir::Node>,
    },
}

impl<'arena> StatementContext<'arena, '_> {
    /// Return the in-progress ir body that should be used for emitting in this context
    ///
    /// Specifically the `top_level_body` should be used when importing stuff so that modules
    /// always emit to the `top_level`, which this will also return if we are in a module.
    /// If we are in the function return this enums contained `function_body`
    #[must_use]
    fn body_for_statements<'body>(
        &'body mut self,
        top_level_body: &'body mut Vec<ir::Node>,
    ) -> &'body mut Vec<ir::Node> {
        match self {
            Self::Module { .. } => top_level_body,
            Self::Function { function_body, .. } => function_body,
        }
    }

    /// Export an item
    ///
    /// `error_span` is used to construct an error if we are in a function,
    /// and should point to the `export` keyword.
    fn export(
        &mut self,
        name: &'arena str,
        item: ScopeItem<'arena>,
        error_span: Span,
    ) -> Result<(), CompileError> {
        if let Self::Module { export_map } = self {
            export_map.insert(name, item);
            Ok(())
        } else {
            Err(CompileError::InvalidStatement {
                stmt: "export",
                context: "function",
                maybe: "return",
                location: error_span,
            })
        }
    }

    /// Register a return value for a function
    ///
    /// `error_span` is used to construct an error if we are in the top level,
    /// and should point to the `return` keyword.
    fn set_return_value(
        &mut self,
        value: ir::Symbol,
        error_span: Span,
    ) -> Result<(), CompileError> {
        if let Self::Function { return_value, .. } = self {
            if return_value.is_none() {
                **return_value = Some(value);
                Ok(())
            } else {
                Err(CompileError::DoubleReturn {
                    location: error_span,
                })
            }
        } else {
            Err(CompileError::InvalidStatement {
                stmt: "return",
                context: "module",
                maybe: "export",
                location: error_span,
            })
        }
    }
}

/// Holds the state of the resolver
struct Resolver<'arena> {
    /// Arena used for allocating the source code
    arena: &'arena Bump,
    /// The store of modules
    modules: ModuleStore<'arena>,
    /// The module cache
    ///
    /// None indicates the module is actively being compiled.
    /// I.e if we try to retrieve it its a circular import.
    module_cache: HashMap<PathBuf, Option<ModuleId<'arena>>>,
    /// The builtin node implementations
    nodes: NodeStorage,
    /// The store of functions
    functions: ir::FunctionStore,
    /// The virtual file holding the file contents
    file: VirtualFile<'arena>,
    /// The next symbol id to use
    next_symbol_id: usize,
    /// Cache of literal values
    cached_literals: HashMap<Data, ir::Symbol>,
}

/// The result of a resolving a file
pub struct ResolveResult<'arena> {
    /// The resulting ir
    pub ir: ir::Pipeline,
    /// The builtin node implementations
    pub nodes: NodeStorage,
    /// A virtual file.
    pub files: VirtualFile<'arena>,
    /// The id of the `Noop` node
    pub noop: NodeKindId,
}

/// Compile the given file to a IR
pub fn resolve<'arena>(
    arena: &'arena Bump,
    file: &Path,
    entry_point: &'arena str,
) -> Result<ResolveResult<'arena>, crate::SerpentineError> {
    let mut resolver = Resolver {
        arena,
        modules: ModuleStore::new(),
        module_cache: HashMap::new(),
        functions: ir::FunctionStore::new(),
        nodes: NodeStorage::new(),
        file: VirtualFile::new(),
        next_symbol_id: 0,
        cached_literals: HashMap::new(),
    };
    let (prelude, noop) = resolver.create_prelude();

    let cli_file_id = resolver.file.push("<cli>".into(), entry_point);
    let cli_span = Span::new(cli_file_id, 0, entry_point.len());

    let context = ImmutableContext { prelude };
    let mut top_level_body = Vec::new();
    let start_module = match resolver.get_module(&context, &mut top_level_body, file) {
        Ok(module) => module,
        Err(err) => {
            return Err(crate::SerpentineError::Compile {
                source_code: resolver.file.into_owned(),
                error: vec![err],
            });
        }
    };

    let module = resolver.modules.get(start_module);
    let start_symbol = match module
        .items
        .lookup(entry_point, cli_span)
        .and_then(|item| item.try_into_label(cli_span))
    {
        Ok(symbol) => symbol,
        Err(err) => {
            return Err(crate::SerpentineError::Compile {
                source_code: resolver.file.into_owned(),
                error: vec![err],
            });
        }
    };

    Ok(ResolveResult {
        ir: ir::Pipeline {
            top_level: ir::Body(top_level_body.into()),
            functions: resolver.functions,
            start_point: start_symbol,
        },
        nodes: resolver.nodes,
        files: resolver.file,
        noop,
    })
}

/// A `Write`-er that writes into a Bumpalo string
struct BumpaloStringWriter<'arena>(bumpalo::collections::String<'arena>);

impl std::io::Write for BumpaloStringWriter<'_> {
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // This is a simple utf-8 check (not a clone/copy)
        let buf = str::from_utf8(buf).map_err(std::io::Error::other)?;
        self.0.push_str(buf);
        Ok(buf.len())
    }
}

impl<'arena> Resolver<'arena> {
    /// Create the prelude by creating the node storage and prelude, and populating the functions
    /// map.
    ///
    /// Also returns the id of the `Noop` node
    #[expect(
        clippy::expect_used,
        reason = "Noop is always defined, if not this function will always fail, and will be caught in tests."
    )]
    fn create_prelude(&mut self) -> (Scope<'arena, 'arena>, NodeKindId) {
        let mut prelude = Scope::root();
        let mut noop = None;

        for (name, node) in crate::engine::nodes::prelude() {
            let id = self.nodes.push(node);

            if name == crate::engine::nodes::NOOP_NAME {
                noop = Some(id);
            }

            let function_id = self.functions.push(ir::Function::BuiltinFunction(id));
            prelude.insert(name, ScopeItem::Function(function_id));
        }

        (prelude, noop.expect("Noop node not found"))
    }

    /// Get a module by path, resolving it if required.
    ///
    /// This is a wrapper around `resolve_module` that handles the cache,
    /// And should always be used by other code.
    fn get_module(
        &mut self,
        context: &ImmutableContext<'arena>,
        top_level_body: &mut Vec<ir::Node>,
        module_path: &Path,
    ) -> Result<ModuleId<'arena>, CompileError> {
        let module_path =
            module_path
                .canonicalize()
                .map_err(|io_err| CompileError::FileReading {
                    file: module_path.into(),
                    inner: io_err,
                })?;

        match self.module_cache.get(&module_path) {
            Some(Some(module)) => Ok(*module),
            Some(None) => Err(CompileError::CircularImport { file: module_path }),
            None => {
                self.module_cache.insert(module_path.clone(), None);
                let module = self.resolve_module(context, top_level_body, &module_path)?;
                let module_id = self.modules.push(module);
                self.module_cache.insert(module_path, Some(module_id));
                Ok(module_id)
            }
        }
    }

    /// Resolve the given module
    fn resolve_module(
        &mut self,
        resolve_context: &ImmutableContext<'arena>,
        top_level_body: &mut Vec<ir::Node>,
        module: &Path,
    ) -> Result<Module<'arena>, CompileError> {
        let code = std::fs::File::open(module)
            .and_then(|mut file| {
                let code = if let Ok(length) = file.metadata().map(|metadata| metadata.len()) {
                    bumpalo::collections::String::with_capacity_in(
                        length.try_into().unwrap_or(usize::MAX),
                        self.arena,
                    )
                } else {
                    bumpalo::collections::String::new_in(self.arena)
                };
                let mut code = BumpaloStringWriter(code);
                std::io::copy(&mut file, &mut code)?;
                Ok(code.0.into_bump_str())
            })
            .map_err(|io_err| CompileError::FileReading {
                file: module.into(),
                inner: io_err,
            })?;

        let file_id = self.file.push(module.to_owned(), code);
        let tokens = super::tokenizer::Tokenizer::tokenize(file_id, code)?;
        let ast = super::parser::Parser::parse_file(tokens)?;

        let mut exports = Scope::root();
        let mut statement_context = StatementContext::Module {
            export_map: &mut exports,
        };
        let mut scope = resolve_context.prelude.child();

        for statement in ast.0 {
            self.resolve_statement(
                resolve_context,
                &mut scope,
                &mut statement_context,
                module,
                top_level_body,
                statement,
            )?;
        }

        Ok(Module { items: exports })
    }

    /// Resolve a statement
    fn resolve_statement(
        &mut self,
        resolve_context: &ImmutableContext<'arena>,
        scope: &mut Scope<'arena, '_>,
        statement_context: &mut StatementContext<'arena, '_>,
        current_path: &Path,
        top_level_body: &mut Vec<ir::Node>,
        statement: ast::Statement<'arena>,
    ) -> Result<(), CompileError> {
        match statement {
            ast::Statement::Import { export, path, name } => {
                let error_span = path.span();
                let path = current_path
                    .parent()
                    .unwrap_or_else(|| Path::new("/"))
                    .join(path.take());
                let module_id = self
                    .get_module(resolve_context, top_level_body, &path)
                    .map_err(|import_error| CompileError::ImportError {
                        module: path.display().to_string(),
                        error: import_error.into(),
                        location: error_span,
                    })?;

                let item = ScopeItem::Module(module_id);
                let name = name.0.take();

                if let Some(export_span) = export {
                    statement_context.export(name, item, export_span)?;
                }
                scope.insert(name, item);
            }
            ast::Statement::Return {
                return_kw,
                expression,
            } => {
                let value =
                    self.resolve_expression(scope, top_level_body, statement_context, expression)?;
                statement_context.set_return_value(value, return_kw)?;
            }
            ast::Statement::Label {
                export,
                expression,
                label,
            } => {
                let value =
                    self.resolve_expression(scope, top_level_body, statement_context, expression)?;
                let item = ScopeItem::Label(value);

                let name = label.0.take();
                if let Some(export_span) = export {
                    statement_context.export(name, item, export_span)?;
                }
                scope.insert(name, item);
            }
            ast::Statement::Function {
                export,
                name,
                parameters,
                statements,
            } => {
                let mut function_scope = scope.child();

                let mut parameter_symbols = Vec::with_capacity(parameters.len());
                for parameter in parameters {
                    let symbol = self.new_symbol();
                    function_scope.insert(parameter.0.take(), ScopeItem::Label(symbol));
                    parameter_symbols.push(symbol);
                }

                let mut return_value = None;
                let mut function_body = Vec::new();
                let mut function_context = StatementContext::Function {
                    return_value: &mut return_value,
                    function_body: &mut function_body,
                };
                for function_statement in statements {
                    self.resolve_statement(
                        resolve_context,
                        &mut function_scope,
                        &mut function_context,
                        current_path,
                        top_level_body,
                        function_statement,
                    )?;
                }

                let Some(return_value) = return_value else {
                    return Err(CompileError::ReturnNotFound {
                        location: name.0.span(),
                    });
                };

                let function = ir::Function::Custom {
                    parameters: parameter_symbols.into(),
                    body: ir::Body(function_body.into()),
                    return_value,
                };
                let function_id = self.functions.push(function);

                let item = ScopeItem::Function(function_id);
                let name = name.0.take();
                if let Some(export_span) = export {
                    statement_context.export(name, item, export_span)?;
                }
                scope.insert(name, item);
            }
        }

        Ok(())
    }

    /// Resolve a expression, returning the final symbol
    fn resolve_expression(
        &mut self,
        scope: &Scope<'arena, '_>,
        top_level_body: &mut Vec<ir::Node>,
        statement_context: &mut StatementContext<'arena, '_>,
        expression: ast::Expression<'arena>,
    ) -> Result<ir::Symbol, CompileError> {
        match expression {
            ast::Expression::Number(number) => {
                Ok(self.resolve_literal(top_level_body, Data::Int(number.take()), number.span()))
            }
            ast::Expression::String(text) => {
                let span = text.span();
                Ok(self.resolve_literal(top_level_body, Data::String(text.take().into()), span))
            }
            ast::Expression::Label(label) => {
                let error_span = label.span();
                let item = self
                    .get_item_path(scope, label.take())?
                    .try_into_label(error_span)?;
                Ok(item)
            }
            ast::Expression::Node(node) => {
                self.resolve_node(scope, top_level_body, statement_context, node, None)
            }
            ast::Expression::Chain(chain) => {
                let chain = chain.take();
                let mut current = self.resolve_expression(
                    scope,
                    top_level_body,
                    statement_context,
                    *chain.start,
                )?;

                for node in chain.nodes {
                    current = self.resolve_node(
                        scope,
                        top_level_body,
                        statement_context,
                        node,
                        Some(current),
                    )?;
                }

                Ok(current)
            }
        }
    }

    /// Resolve a node to a symbol and emit the needed instructions to the body
    fn resolve_node(
        &mut self,
        scope: &Scope<'arena, '_>,
        top_level_body: &mut Vec<ir::Node>,
        statement_context: &mut StatementContext<'arena, '_>,
        node: Spanned<ast::Node<'arena>>,
        first_argument: Option<ir::Symbol>,
    ) -> Result<ir::Symbol, CompileError> {
        let node_span = node.span();
        let ast::Node {
            name,
            arguments,
            phantom_inputs,
        } = node.take();

        let name_error_span = name.span();
        let function = self
            .get_item_path(scope, name.take())?
            .try_into_function(name_error_span)?;

        let phantom_inputs = phantom_inputs
            .into_iter()
            .map(|input| {
                let error_span = input.span();
                let symbol = self
                    .get_item_path(scope, input.take())?
                    .try_into_label(error_span)?;
                Ok(symbol)
            })
            .collect::<Result<Box<_>, _>>()?;

        let inline_arguments = arguments.into_iter().map(|expression| {
            self.resolve_expression(scope, top_level_body, statement_context, expression)
        });
        let resolved_arguments = if let Some(first_argument) = first_argument {
            std::iter::once(Ok(first_argument))
                .chain(inline_arguments)
                .collect::<Result<Box<_>, _>>()
        } else {
            inline_arguments.collect::<Result<Box<_>, _>>()
        }?;

        let symbol = self.new_symbol();
        statement_context
            .body_for_statements(top_level_body)
            .push(ir::Node {
                name: symbol,
                function,
                arguments: resolved_arguments,
                phantom_inputs,
                span: node_span,
            });

        Ok(symbol)
    }

    /// Create a "builtin" node to represent a literal
    fn resolve_literal(
        &mut self,
        top_level_body: &mut Vec<ir::Node>,
        literal: Data,
        source_span: Span,
    ) -> ir::Symbol {
        if let Some(cached) = self.cached_literals.get(&literal) {
            return *cached;
        }

        let node_impl = crate::engine::nodes::LiteralNode(literal.clone());
        let node_id = self.nodes.push(Box::new(node_impl));
        let function_id = self.functions.push(ir::Function::BuiltinFunction(node_id));
        let node_name = self.new_symbol();

        top_level_body.push(ir::Node {
            name: node_name,
            function: function_id,
            arguments: Box::new([]),
            phantom_inputs: Box::new([]),
            span: source_span,
        });

        self.cached_literals.insert(literal, node_name);

        node_name
    }

    /// Retrieve a item by a item path by resolving modules
    fn get_item_path(
        &self,
        scope: &Scope<'arena, '_>,
        path: ast::ItemPath<'arena>,
    ) -> Result<ScopeItem<'arena>, CompileError> {
        let mut previous_span = path.base.0.span();
        let mut item = scope.lookup(&path.base.0, path.base.0.span())?;

        for segment in path.rest {
            let module_id = item.try_into_module(previous_span)?;
            let module = self.modules.get(module_id);
            previous_span = segment.0.span();
            item = module.items.lookup(&segment.0, segment.0.span())?;
        }

        Ok(*item)
    }

    /// Get a new free symbol identifier
    #[expect(
        clippy::expect_used,
        reason = "We run out of ram before we run out of usize space"
    )]
    #[must_use]
    fn new_symbol(&mut self) -> ir::Symbol {
        let symbol = ir::Symbol(self.next_symbol_id);

        self.next_symbol_id = self
            .next_symbol_id
            .checked_add(1)
            .expect("Overflowed symbol id");

        symbol
    }
}
