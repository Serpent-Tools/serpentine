//! The abstract syntax tree implementation.

use super::span::{Span, Spanned};

/// A type that might not contain a span already, but can construct one from its parts.
/// This is used to avoid calculating spans until a error has actually occurred that requires one.
pub trait Spannable {
    /// Calculate the effective span of this value
    fn calc_span(&self) -> Span;
}

/// A `.snek` file
pub struct File<'src>(pub Box<[Statement<'src>]>);

/// A statement
pub enum Statement<'src> {
    /// A return statement
    Return(Expression<'src>),
    /// A expression statement
    Expression {
        /// Expression of the statement
        expression: Expression<'src>,
        /// A optional label to store the value at.
        label: Option<Ident<'src>>,
    },
}

impl Spannable for Statement<'_> {
    fn calc_span(&self) -> Span {
        match self {
            Self::Return(value) => value.calc_span(),
            Self::Expression { expression, label } => {
                if let Some(label) = label {
                    expression.calc_span().join(label.calc_span())
                } else {
                    expression.calc_span()
                }
            }
        }
    }
}

impl Spannable for Chain<'_> {
    fn calc_span(&self) -> Span {
        self.start.calc_span()
    }
}

/// A value
pub enum Expression<'src> {
    /// A number
    Number(Spanned<i128>),
    /// A string
    String(Spanned<&'src str>),
    /// A node label
    Label(Ident<'src>),
    /// The value is the result of this no input node
    Node(Node<'src>),
    /// A chain of nodes
    Chain(Chain<'src>),
}

impl Spannable for Expression<'_> {
    fn calc_span(&self) -> Span {
        match self {
            Self::Number(number) => number.span(),
            Self::String(string) => string.span(),
            Self::Label(label) => label.calc_span(),
            Self::Node(node) => node.calc_span(),
            Self::Chain(chain) => chain.calc_span(),
        }
    }
}

/// A chain of nodes.?
pub struct Chain<'src> {
    /// An expression to the start the chain.
    pub start: Box<Expression<'src>>,
    /// The chain of nodes to apply to the start expression
    pub nodes: Box<[Node<'src>]>,
}

/// A node declaration.
pub struct Node<'src> {
    /// The name of this node
    pub name: Ident<'src>,
    /// Arguments for the node
    pub arguments: Box<[Expression<'src>]>,
    /// Phantom inputs the block the rest of the node from running.
    pub phantom_inputs: Box<[Ident<'src>]>,
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
