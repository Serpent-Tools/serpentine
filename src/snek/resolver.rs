//! The resolver parses the input file into a ast, and parses all imported modules.
//! and processes everything into a simplified IR for further processing and type checking.

use std::collections::HashMap;

use crate::{
    engine::data_model::{NodeStorage, Store, StoreId},
    snek::ir,
};

/// A snek module
struct Module {
    /// The items in the module items
    items: Scope<'static>,
}

/// A store of modules
type ModuleStore = Store<Module>;

/// A module id
type ModuleId = StoreId<Module>;

/// Possible items in scope
enum ScopeItem {
    /// A node label
    Label(ir::Symbol),
    /// A function
    Function(ir::FunctionId),
    /// A module
    Module(ModuleId),
}

/// A lexical scope
struct Scope<'parent> {
    /// The parent scope, used for lookups if not found in the current scope.
    parent: Option<&'parent Scope<'parent>>,
    /// The items in the current scope
    items: HashMap<Box<str>, ScopeItem>,
}

impl Scope<'static> {
    /// Create a new empty root sscope
    fn root() -> Self {
        Scope {
            parent: None,
            items: HashMap::new(),
        }
    }
}

impl Scope<'_> {
    /// Create a child scope
    fn child<'this>(&'this self) -> Scope<'this> {
        Scope {
            parent: Some(self),
            items: HashMap::new(),
        }
    }

    /// Return this scope without its parent, useful for storing in modules
    fn isolated(self) -> Scope<'static> {
        Scope {
            parent: None,
            items: self.items,
        }
    }

    /// Insert an item into the scope
    fn insert(&mut self, name: Box<str>, item: ScopeItem) {
        self.items.insert(name, item);
    }

    /// Lookup an item in the scope
    fn lookup(&self, name: &str) -> Option<&ScopeItem> {
        if let Some(item) = self.items.get(name) {
            Some(item)
        } else if let Some(parent) = self.parent {
            parent.lookup(name)
        } else {
            None
        }
    }
}

/// Holds the state of the resolver
struct Resolver {
    /// The ModuleStore
    modules: ModuleStore,
    /// The store of functions
    functions: ir::FunctionStore,
}

impl Resolver {
    /// Create the prelude by creating the node storage and prelude, and populating the functions
    /// map.
    fn create_prelude(&mut self) -> (NodeStorage, Scope<'static>) {
        let mut nodes = NodeStorage::new();
        let mut prelude = Scope::root();

        for (name, node) in crate::engine::nodes::prelude() {
            let id = nodes.push(node);
            let function_id = self.functions.push(ir::Function::BuiltinFunction(id));
            prelude.insert(name.into(), ScopeItem::Function(function_id));
        }

        (nodes, prelude)
    }
}
