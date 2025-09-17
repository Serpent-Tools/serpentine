//! The abstract syntax tree implementation.

use super::span::{Span, Spanned};

/// A type that might not contain a span already, but can construct one from its parts.
/// This is used to avoid calculating spans until a error has actually occured that requires one.
pub trait Spannable {
    /// Calculate the effective span of this value
    fn calc_span(&self) -> Span;
}

/// A `.snek` file
pub struct File<'src>(pub Box<[Statement<'src>]>);

/// A statement
pub enum Statement<'src> {
    /// A chain statement
    Chain(Chain<'src>),
}

impl Spannable for Statement<'_> {
    fn calc_span(&self) -> Span {
        match self {
            Self::Chain(chain) => chain.calc_span(),
        }
    }
}

/// A chain of nodes.
pub struct Chain<'src> {
    /// An expression to the start the chain.
    pub start: Expression<'src>,
    /// The chain of nodes to apply to the start expression
    pub nodes: Box<[Node<'src>]>,
}

impl Spannable for Chain<'_> {
    fn calc_span(&self) -> Span {
        self.start.calc_span()
    }
}

/// A value
pub enum Expression<'src> {
    /// The value is the result of this no input node
    Node(Node<'src>),
}

impl Spannable for Expression<'_> {
    fn calc_span(&self) -> Span {
        match self {
            Self::Node(node) => node.calc_span(),
        }
    }
}

/// A node declaration.
pub struct Node<'src> {
    /// The name of this node
    pub name: Ident<'src>,
}

impl Spannable for Node<'_> {
    fn calc_span(&self) -> Span {
        self.name.calc_span()
    }
}

/// A identifier
pub struct Ident<'src>(pub Spanned<&'src str>);

impl Spannable for Ident<'_> {
    fn calc_span(&self) -> Span {
        self.0.span()
    }
}
