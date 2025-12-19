//! The abstract syntax tree implementation.

use super::span::{Span, Spanned};

/// A `.snek` file
pub struct File<'arena>(pub Box<[Statement<'arena>]>);

/// A statement
#[derive(Debug, Clone)]
pub enum Statement<'arena> {
    /// A return statement
    Return {
        /// The span of the `return` keyword
        return_kw: Span,
        /// The expression being returned
        expression: Expression<'arena>,
    },

    /// A import statement
    Import {
        /// A optional export keyword
        export: Option<Span>,
        /// The relative path to the file to import
        path: Spanned<&'arena str>,
        /// The name to put the module under.
        name: Ident<'arena>,
    },
    /// A label statement
    Label {
        /// A optional export keyword
        export: Option<Span>,
        /// Expression of the statement
        expression: Expression<'arena>,
        /// The label to store the value at.
        label: Ident<'arena>,
    },

    /// Function definition
    Function {
        /// a optional export keyword
        export: Option<Span>,
        /// Name of the Function
        name: Ident<'arena>,
        /// Parameters to the Function
        parameters: Box<[Ident<'arena>]>,
        /// The list of the statements in this function.
        statements: Box<[Statement<'arena>]>,
    },
}

/// A value
#[derive(Debug, Clone)]
pub enum Expression<'arena> {
    /// A number
    Number(Spanned<i128>),
    /// A string
    String(Spanned<&'arena str>),
    /// A node label
    Label(Spanned<ItemPath<'arena>>),
    /// The value is the result of this no input node
    Node(Spanned<Node<'arena>>),
    /// A chain of nodes
    Chain(Spanned<Chain<'arena>>),
}

/// A chain of nodes
#[derive(Debug, Clone)]
pub struct Chain<'arena> {
    /// An expression to the start the chain.
    pub start: Box<Expression<'arena>>,
    /// The chain of nodes to apply to the start expression
    pub nodes: Box<[Spanned<Node<'arena>>]>,
}

/// A node declaration.
#[derive(Debug, Clone)]
pub struct Node<'arena> {
    /// The name of this node
    pub name: Spanned<ItemPath<'arena>>,
    /// Arguments for the node
    pub arguments: Box<[Expression<'arena>]>,
    /// Phantom inputs the block the rest of the node from running.
    pub phantom_inputs: Box<[Spanned<ItemPath<'arena>>]>,
}

/// A path for a item
#[derive(Debug, Clone)]
pub struct ItemPath<'arena> {
    /// The start of the path
    pub base: Ident<'arena>,
    /// The rest of the path
    pub rest: Box<[Ident<'arena>]>,
}

impl ItemPath<'_> {
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
pub struct Ident<'arena>(pub Spanned<&'arena str>);

impl Expression<'_> {
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
