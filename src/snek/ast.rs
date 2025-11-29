//! The abstract syntax tree implementation.

use super::span::{Span, Spanned};

/// A `.snek` file
pub struct File(pub Box<[Statement]>);

/// A statement
#[derive(Debug, Clone)]
pub enum Statement {
    /// A return statement
    Return {
        /// The span of the `return` keyword
        return_kw: Span,
        /// The expression being returned
        expression: Expression,
    },

    /// A import statement
    Import {
        /// A optional export keyword
        export: Option<Span>,
        /// The relative path to the file to import
        path: Spanned<Box<str>>,
        /// The name to put the module under.
        name: Ident,
    },
    /// A label statement
    Label {
        /// A optional export keyword
        export: Option<Span>,
        /// Expression of the statement
        expression: Expression,
        /// The label to store the value at.
        label: Ident,
    },

    /// Function definition
    Function {
        /// a optional export keyword
        export: Option<Span>,
        /// Name of the Function
        name: Ident,
        /// Parameters to the Function
        parameters: Box<[Ident]>,
        /// The list of the statements in this function.
        statements: Box<[Statement]>,
    },
}

/// A value
#[derive(Debug, Clone)]
pub enum Expression {
    /// A number
    Number(Spanned<i128>),
    /// A string
    String(Spanned<Box<str>>),
    /// A node label
    Label(Spanned<ItemPath>),
    /// The value is the result of this no input node
    Node(Spanned<Node>),
    /// A chain of nodes
    Chain(Spanned<Chain>),
}

/// A chain of nodes
#[derive(Debug, Clone)]
pub struct Chain {
    /// An expression to the start the chain.
    pub start: Box<Expression>,
    /// The chain of nodes to apply to the start expression
    pub nodes: Box<[Spanned<Node>]>,
}

/// A node declaration.
#[derive(Debug, Clone)]
pub struct Node {
    /// The name of this node
    pub name: Spanned<ItemPath>,
    /// Arguments for the node
    pub arguments: Box<[Expression]>,
    /// Phantom inputs the block the rest of the node from running.
    pub phantom_inputs: Box<[Spanned<ItemPath>]>,
}

/// A path for a item
#[derive(Debug, Clone)]
pub struct ItemPath {
    /// The start of the path
    pub base: Ident,
    /// The rest of the path
    pub rest: Box<[Ident]>,
}

impl ItemPath {
    /// Get the span of this item path
    pub fn span(&self) -> Span {
        if let Some(last) = self.rest.last() {
            self.base.0.span().join(last.0.span())
        } else {
            self.base.0.span()
        }
    }
}

/// A identifier
#[derive(Debug, Clone)]
pub struct Ident(pub Spanned<Box<str>>);

impl Expression {
    /// Get the span of this expression
    pub fn span(&self) -> Span {
        match self {
            Self::Number(number) => number.span(),
            Self::String(string) => string.span(),
            Self::Label(label) => label.span(),
            Self::Node(node) => node.span(),
            Self::Chain(chain) => chain.span(),
        }
    }
}
