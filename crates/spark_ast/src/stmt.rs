//! Statement nodes.

use spark_span::Span;

use crate::{Expr, ExprId, Ident, TypeExpr};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A block of statements, optionally ending with an expression.
///
/// In Spark, blocks are expression-oriented. If a block ends with an
/// expression (no trailing statement), that expression becomes the
/// block's value.
///
/// # Examples
///
/// ```text
/// {
///     x: Int = compute()
///     x + 1              // trailing expression, block evaluates to x + 1
/// }
///
/// {
///     print("hello")     // statement (Void return)
/// }                      // block evaluates to Void
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Block {
    /// Unique node identifier for type-table lookups.
    pub id: ExprId,
    /// The statements in the block.
    pub stmts: Vec<Stmt>,
    /// Optional trailing expression (the block's value).
    pub expr: Option<Box<Expr>>,
    /// The span.
    pub span: Span,
}

impl PartialEq for Block {
    fn eq(&self, other: &Self) -> bool {
        self.stmts == other.stmts && self.expr == other.expr && self.span == other.span
    }
}

impl Block {
    /// Creates a new block.
    #[must_use]
    pub fn new(stmts: Vec<Stmt>, expr: Option<Box<Expr>>, span: Span) -> Self {
        Self { id: ExprId::next(), stmts, expr, span }
    }

    /// Creates an empty block.
    #[must_use]
    pub fn empty(span: Span) -> Self {
        Self {
            id: ExprId::next(),
            stmts: Vec::new(),
            expr: None,
            span,
        }
    }

    /// Creates a block with only a trailing expression.
    #[must_use]
    pub fn expr_only(expr: Expr, span: Span) -> Self {
        Self {
            id: ExprId::next(),
            stmts: Vec::new(),
            expr: Some(Box::new(expr)),
            span,
        }
    }
}

/// A statement.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Stmt {
    /// The kind of statement.
    pub kind: StmtKind,
    /// The source span.
    pub span: Span,
}

impl Stmt {
    /// Creates a new statement.
    #[must_use]
    pub const fn new(kind: StmtKind, span: Span) -> Self {
        Self { kind, span }
    }

    /// Creates a let statement.
    #[must_use]
    pub const fn let_stmt(
        name: Ident,
        ty: TypeExpr,
        mutable: bool,
        value: Expr,
        span: Span,
    ) -> Self {
        Self::new(
            StmtKind::Let {
                name,
                ty,
                mutable,
                value,
            },
            span,
        )
    }

    /// Creates an expression statement.
    #[must_use]
    pub const fn expr(expr: Expr) -> Self {
        let span = expr.span;
        Self::new(StmtKind::Expr(expr), span)
    }
}

/// The different kinds of statements.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum StmtKind {
    /// Variable declaration: `name: Type = value`, `name: Type! = value`
    Let {
        /// The variable name.
        name: Ident,
        /// The variable type.
        ty: TypeExpr,
        /// Whether the variable is mutable (`Type!`).
        mutable: bool,
        /// The initial value.
        value: Expr,
    },

    /// Assignment: `x = value`
    Assign {
        /// The assignment target (identifier, field, or index).
        target: Expr,
        /// The value to assign.
        value: Expr,
    },

    /// Compound assignment: `x += value`, `x -= value`, etc.
    CompoundAssign {
        /// The assignment target.
        target: Expr,
        /// The compound operator.
        op: CompoundOp,
        /// The value to apply.
        value: Expr,
    },

    /// Expression statement: `foo()`
    Expr(Expr),
}

/// Compound assignment operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum CompoundOp {
    /// `+=`
    Add,
    /// `-=`
    Sub,
    /// `*=`
    Mul,
    /// `/=`
    Div,
}

impl CompoundOp {
    /// Returns the string representation of this operator.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "+=",
            Self::Sub => "-=",
            Self::Mul => "*=",
            Self::Div => "/=",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExprKind, Path, TypeExprKind};
    use spark_intern::sym;

    fn dummy_ident(s: &str) -> Ident {
        Ident::dummy(sym(s))
    }

    fn dummy_path(s: &str) -> Path {
        Path::from_ident(dummy_ident(s))
    }

    fn int_type() -> TypeExpr {
        TypeExpr::new(
            TypeExprKind::Named {
                path: dummy_path("Int"),
                type_args: vec![],
            },
            Span::DUMMY,
        )
    }

    fn int_expr(n: i64) -> Expr {
        Expr::new(
            ExprKind::Literal(crate::Literal::int(n, Span::DUMMY)),
            Span::DUMMY,
        )
    }

    #[test]
    fn block_empty() {
        let block = Block::empty(Span::DUMMY);
        assert!(block.stmts.is_empty());
        assert!(block.expr.is_none());
    }

    #[test]
    fn block_with_stmts() {
        let stmt = Stmt::expr(int_expr(42));
        let block = Block::new(vec![stmt], None, Span::DUMMY);

        assert_eq!(block.stmts.len(), 1);
        assert!(block.expr.is_none());
    }

    #[test]
    fn block_with_trailing_expr() {
        let block = Block::expr_only(int_expr(42), Span::DUMMY);

        assert!(block.stmts.is_empty());
        assert!(block.expr.is_some());
    }

    #[test]
    fn stmt_let() {
        let stmt = Stmt::let_stmt(
            dummy_ident("x"),
            int_type(),
            false,
            int_expr(42),
            Span::DUMMY,
        );

        if let StmtKind::Let { name, mutable, .. } = stmt.kind {
            assert_eq!(name.name, sym("x"));
            assert!(!mutable);
        } else {
            panic!("Expected Let statement");
        }
    }

    #[test]
    fn stmt_let_mutable() {
        let stmt = Stmt::let_stmt(dummy_ident("y"), int_type(), true, int_expr(0), Span::DUMMY);

        if let StmtKind::Let { mutable, .. } = stmt.kind {
            assert!(mutable);
        } else {
            panic!("Expected Let statement");
        }
    }

    #[test]
    fn compound_op_str() {
        assert_eq!(CompoundOp::Add.as_str(), "+=");
        assert_eq!(CompoundOp::Sub.as_str(), "-=");
        assert_eq!(CompoundOp::Mul.as_str(), "*=");
        assert_eq!(CompoundOp::Div.as_str(), "/=");
    }
}
