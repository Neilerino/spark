//! Expression nodes.

use std::sync::atomic::{AtomicU32, Ordering};

use spark_span::Span;

use crate::{Block, Ident, Literal, Path, Pattern, TypeExpr};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

static EXPR_ID_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Unique identifier for expression nodes (`Expr`, `TypeExpr`, `Block`).
///
/// Each expression node is assigned a globally unique `ExprId` at construction
/// time. This is used as a collision-free key in the type table instead of
/// source spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ExprId(pub u32);

impl ExprId {
    /// A dummy `ExprId` for error recovery.
    pub const DUMMY: Self = Self(u32::MAX);

    /// Allocates the next unique `ExprId`.
    #[must_use]
    pub fn next() -> Self {
        Self(EXPR_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// An expression node.
///
/// Expressions are the primary building blocks of Spark programs.
/// Unlike statements, expressions always produce a value.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Expr {
    /// Unique node identifier for type-table lookups.
    pub id: ExprId,
    /// The kind of expression.
    pub kind: ExprKind,
    /// The source span.
    pub span: Span,
}

impl PartialEq for Expr {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.span == other.span
    }
}

impl Expr {
    /// Creates a new expression.
    #[must_use]
    pub fn new(kind: ExprKind, span: Span) -> Self {
        Self { id: ExprId::next(), kind, span }
    }

    /// Creates an error expression for recovery.
    #[must_use]
    pub fn error(span: Span) -> Self {
        Self::new(ExprKind::Error, span)
    }
}

/// The different kinds of expressions.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum ExprKind {
    // ========================================================================
    // Literals and Identifiers
    // ========================================================================
    /// A literal value.
    Literal(Literal),

    /// An identifier reference.
    Ident(Ident),

    /// A path to an item: `module.submodule.Item`
    Path(Path),

    // ========================================================================
    // Collections
    // ========================================================================
    /// List literal: `[1, 2, 3]`
    List(Vec<Expr>),

    /// List from range: `[0..10]`, `[0..=10]`, `[0..10:2]`
    ListRange {
        /// Range start.
        start: Box<Expr>,
        /// Range end.
        end: Box<Expr>,
        /// Whether the range is inclusive (`..=`).
        inclusive: bool,
        /// Optional step value.
        step: Option<Box<Expr>>,
    },

    /// Map literal: `{"key": value, ...}`
    Map(Vec<MapEntry>),

    /// Set literal: `{"a", "b", "c"}`
    Set(Vec<Expr>),

    /// Tuple literal: `(a, b, c)`
    Tuple(Vec<Expr>),

    // ========================================================================
    // Operators
    // ========================================================================
    /// Binary operation: `a + b`, `x and y`
    Binary {
        /// The operator.
        op: BinaryOp,
        /// Left-hand side.
        lhs: Box<Expr>,
        /// Right-hand side.
        rhs: Box<Expr>,
    },

    /// Unary operation: `-x`, `not y`, `~z`
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        operand: Box<Expr>,
    },

    // ========================================================================
    // Access and Calls
    // ========================================================================
    /// Field access: `user.name`
    Field {
        /// The base expression.
        base: Box<Expr>,
        /// The field name.
        field: Ident,
    },

    /// Optional field access: `user?.name`
    OptionalField {
        /// The base expression.
        base: Box<Expr>,
        /// The field name.
        field: Ident,
    },

    /// Index access: `list[0]`, `map["key"]`
    Index {
        /// The base expression.
        base: Box<Expr>,
        /// The index expression.
        index: Box<Expr>,
    },

    /// Function call: `foo(a, b, c)`
    Call {
        /// The callee expression.
        callee: Box<Expr>,
        /// The arguments.
        args: Vec<CallArg>,
    },

    /// Method call: `obj.method(args)`
    MethodCall {
        /// The receiver expression.
        receiver: Box<Expr>,
        /// The method name.
        method: Ident,
        /// The arguments.
        args: Vec<CallArg>,
    },

    /// Struct instantiation: `User { name: "Alice", age: 30 }`
    StructInit {
        /// The struct type path.
        path: Path,
        /// Explicit generic type arguments: `Pair[Int, String] { ... }`
        type_args: Vec<TypeExpr>,
        /// Field initializers.
        fields: Vec<FieldInit>,
        /// Spread from existing struct: `{ ...other, name: "Bob" }`
        spread: Option<Box<Expr>>,
    },

    /// Enum variant construction: `Status.Active`, `Result.Error(msg)`
    EnumVariant {
        /// The enum type path.
        path: Path,
        /// The variant name.
        variant: Ident,
        /// Variant fields (for variants with associated data).
        fields: Vec<Expr>,
    },

    // ========================================================================
    // Control Flow
    // ========================================================================
    /// Block expression: `{ stmt; stmt; expr }`
    Block(Block),

    /// If expression: `if cond { } else { }`
    If {
        /// The condition.
        condition: Box<Expr>,
        /// The then branch.
        then_branch: Box<Expr>,
        /// The else branch (optional).
        else_branch: Option<Box<Expr>>,
    },

    /// Match expression
    Match {
        /// The value to match on.
        scrutinee: Box<Expr>,
        /// The match arms.
        arms: Vec<MatchArm>,
    },

    /// Loop expression: `loop { }`
    Loop {
        /// The loop body.
        body: Box<Expr>,
    },

    /// Break from loop: `break`, `break value`
    Break(Option<Box<Expr>>),

    /// Continue to next loop iteration.
    Continue,

    /// Return from function: `return`, `return value`
    Return(Option<Box<Expr>>),

    // ========================================================================
    // Iteration and Pipelines
    // ========================================================================
    /// Unified pipeline expression: `source stage1 stage2 ...`
    ///
    /// Replaces the former `Each`, `EachVia`, `ParallelEach`, `ParallelEachVia`
    /// variants. Each stage is composable and carries a `parallel` flag.
    Pipeline {
        /// The source expression (e.g. a list).
        source: Box<Expr>,
        /// One or more pipeline stages.
        stages: Vec<PipelineStage>,
    },

    /// Type narrowing: `result to { NotFound -> ..., DbError(e) -> ... }`
    To {
        /// The source expression.
        expr: Box<Expr>,
        /// The match arms for type narrowing.
        arms: Vec<MatchArm>,
    },

    /// Pipeline apply operation: `data to sort`, `data via sort{count: 5}`
    PipelineApply {
        /// The source expression.
        expr: Box<Expr>,
        /// The operation name.
        operation: Ident,
        /// Optional partial application arguments.
        args: Vec<PartialArg>,
    },

    // ========================================================================
    // Optional/Error Handling
    // ========================================================================
    /// Null coalescing: `a ?? b`
    Coalesce {
        /// Left-hand side (optional value).
        lhs: Box<Expr>,
        /// Right-hand side (default value).
        rhs: Box<Expr>,
    },

    /// Propagation: `expr!` - unwrap or propagate error/null
    Propagate(Box<Expr>),

    // ========================================================================
    // Type Operations
    // ========================================================================
    /// Type check: `x is Type`
    Is {
        /// The expression to check.
        expr: Box<Expr>,
        /// The type to check against.
        ty: TypeExpr,
    },

    /// Type cast: `x as Type`
    As {
        /// The expression to cast.
        expr: Box<Expr>,
        /// The target type.
        ty: TypeExpr,
    },

    // ========================================================================
    // Functions and Closures
    // ========================================================================
    /// Lambda/inline function: `(x: Int): Int { x * 2 }`
    Lambda {
        /// Parameters.
        params: Vec<FnParam>,
        /// Return type (optional, can be inferred).
        ret_type: Option<TypeExpr>,
        /// Body expression.
        body: Box<Expr>,
    },

    /// Partial application: `greet{greeting: "Hello"}`
    Partial {
        /// The function to partially apply.
        callee: Box<Expr>,
        /// The bound arguments.
        args: Vec<PartialArg>,
    },

    // ========================================================================
    // Async/Concurrency
    // ========================================================================
    /// Await expression: `await future`
    Await(Box<Expr>),

    /// Block (sync wait): `block future`
    BlockAwait(Box<Expr>),

    /// Spawn expression: `spawn { ... }`
    Spawn(Box<Expr>),

    // ========================================================================
    // Special
    // ========================================================================
    /// Grouped expression (parenthesized): `(expr)`
    Paren(Box<Expr>),

    /// Range expression: `0..10`, `0..=10`
    Range {
        /// Range start.
        start: Box<Expr>,
        /// Range end.
        end: Box<Expr>,
        /// Whether the range is inclusive.
        inclusive: bool,
    },

    /// Error placeholder for recovery.
    Error,
}

/// A binary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum BinaryOp {
    // Arithmetic
    /// Addition: `+`
    Add,
    /// Subtraction: `-`
    Sub,
    /// Multiplication: `*`
    Mul,
    /// Division: `/`
    Div,
    /// Modulo: `%`
    Mod,

    // Comparison
    /// Equality: `==`
    Eq,
    /// Inequality: `!=`
    Ne,
    /// Less than: `<`
    Lt,
    /// Less than or equal: `<=`
    Le,
    /// Greater than: `>`
    Gt,
    /// Greater than or equal: `>=`
    Ge,

    // Logical
    /// Logical and: `and`
    And,
    /// Logical or: `or`
    Or,

    // Bitwise
    /// Bitwise and: `&`
    BitAnd,
    /// Bitwise or: `|`
    BitOr,
    /// Bitwise xor: `^`
    BitXor,
    /// Left shift: `<<`
    Shl,
    /// Right shift: `>>`
    Shr,
}

impl BinaryOp {
    /// Returns the string representation of this operator.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Mod => "%",
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::And => "and",
            Self::Or => "or",
            Self::BitAnd => "&",
            Self::BitOr => "|",
            Self::BitXor => "^",
            Self::Shl => "<<",
            Self::Shr => ">>",
        }
    }

    /// Returns true if this is an arithmetic operator.
    #[must_use]
    pub const fn is_arithmetic(self) -> bool {
        matches!(
            self,
            Self::Add | Self::Sub | Self::Mul | Self::Div | Self::Mod
        )
    }

    /// Returns true if this is a comparison operator.
    #[must_use]
    pub const fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::Eq | Self::Ne | Self::Lt | Self::Le | Self::Gt | Self::Ge
        )
    }

    /// Returns true if this is a logical operator.
    #[must_use]
    pub const fn is_logical(self) -> bool {
        matches!(self, Self::And | Self::Or)
    }

    /// Returns true if this is a bitwise operator.
    #[must_use]
    pub const fn is_bitwise(self) -> bool {
        matches!(
            self,
            Self::BitAnd | Self::BitOr | Self::BitXor | Self::Shl | Self::Shr
        )
    }
}

/// A unary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum UnaryOp {
    /// Negation: `-x`
    Neg,
    /// Logical not: `not x`
    Not,
    /// Bitwise not: `~x`
    BitNot,
}

impl UnaryOp {
    /// Returns the string representation of this operator.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Neg => "-",
            Self::Not => "not",
            Self::BitNot => "~",
        }
    }
}

/// An entry in a map literal.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct MapEntry {
    /// The key expression.
    pub key: Expr,
    /// The value expression.
    pub value: Expr,
    /// The span.
    pub span: Span,
}

impl MapEntry {
    /// Creates a new map entry.
    #[must_use]
    pub const fn new(key: Expr, value: Expr, span: Span) -> Self {
        Self { key, value, span }
    }
}

/// A call argument.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct CallArg {
    /// Optional argument name for keyword arguments.
    pub name: Option<Ident>,
    /// The argument value.
    pub value: Expr,
    /// The span.
    pub span: Span,
}

impl CallArg {
    /// Creates a positional argument.
    #[must_use]
    pub const fn positional(value: Expr) -> Self {
        let span = value.span;
        Self {
            name: None,
            value,
            span,
        }
    }

    /// Creates a named argument.
    #[must_use]
    pub const fn named(name: Ident, value: Expr, span: Span) -> Self {
        Self {
            name: Some(name),
            value,
            span,
        }
    }
}

/// A field initializer in struct instantiation.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct FieldInit {
    /// The field name.
    pub name: Ident,
    /// The field value.
    pub value: Expr,
    /// The span.
    pub span: Span,
}

impl FieldInit {
    /// Creates a new field initializer.
    #[must_use]
    pub const fn new(name: Ident, value: Expr, span: Span) -> Self {
        Self { name, value, span }
    }
}

/// A single stage in a pipeline expression.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PipelineStage {
    /// The kind of pipeline stage.
    pub kind: PipelineStageKind,
    /// Whether this stage runs in parallel.
    pub parallel: bool,
    /// The source span.
    pub span: Span,
}

/// The different kinds of pipeline stages.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PipelineStageKind {
    /// `each callback` — side-effect iteration.
    ForEach {
        /// The callback (lambda or function reference).
        callback: Box<Expr>,
    },
    /// `each via callback` — map/transform.
    Map {
        /// The callback (lambda or function reference).
        callback: Box<Expr>,
    },
    /// `reduce initial via callback` — fold/accumulate.
    Reduce {
        /// The initial accumulator value.
        initial: Box<Expr>,
        /// The callback (lambda or function reference).
        callback: Box<Expr>,
    },
}

/// A partial application argument.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PartialArg {
    /// The parameter name being bound.
    pub name: Ident,
    /// The value to bind.
    pub value: Expr,
    /// The span.
    pub span: Span,
}

impl PartialArg {
    /// Creates a new partial argument.
    #[must_use]
    pub const fn new(name: Ident, value: Expr, span: Span) -> Self {
        Self { name, value, span }
    }
}

/// A function parameter.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct FnParam {
    /// The parameter name.
    pub name: Ident,
    /// The parameter type.
    pub ty: TypeExpr,
    /// The span.
    pub span: Span,
}

impl FnParam {
    /// Creates a new function parameter.
    #[must_use]
    pub const fn new(name: Ident, ty: TypeExpr, span: Span) -> Self {
        Self { name, ty, span }
    }
}

/// A match arm.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct MatchArm {
    /// The pattern to match.
    pub pattern: Pattern,
    /// The expression if matched.
    pub body: Expr,
    /// The span.
    pub span: Span,
}

impl MatchArm {
    /// Creates a new match arm.
    #[must_use]
    pub const fn new(pattern: Pattern, body: Expr, span: Span) -> Self {
        Self {
            pattern,
            body,
            span,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_intern::sym;

    fn dummy_ident(s: &str) -> Ident {
        Ident::dummy(sym(s))
    }

    fn int_literal(n: i64) -> Expr {
        Expr::new(ExprKind::Literal(Literal::int(n, Span::DUMMY)), Span::DUMMY)
    }

    #[test]
    fn expr_literal() {
        let expr = int_literal(42);
        assert!(matches!(expr.kind, ExprKind::Literal(_)));
    }

    #[test]
    fn expr_binary() {
        let lhs = Box::new(int_literal(1));
        let rhs = Box::new(int_literal(2));
        let expr = Expr::new(
            ExprKind::Binary {
                op: BinaryOp::Add,
                lhs,
                rhs,
            },
            Span::DUMMY,
        );

        if let ExprKind::Binary { op, .. } = expr.kind {
            assert_eq!(op, BinaryOp::Add);
        } else {
            panic!("Expected Binary expression");
        }
    }

    #[test]
    fn expr_call() {
        let callee = Box::new(Expr::new(
            ExprKind::Ident(dummy_ident("print")),
            Span::DUMMY,
        ));
        let args = vec![CallArg::positional(int_literal(42))];
        let expr = Expr::new(ExprKind::Call { callee, args }, Span::DUMMY);

        if let ExprKind::Call { args, .. } = expr.kind {
            assert_eq!(args.len(), 1);
        } else {
            panic!("Expected Call expression");
        }
    }

    #[test]
    fn binary_op_properties() {
        assert!(BinaryOp::Add.is_arithmetic());
        assert!(BinaryOp::Eq.is_comparison());
        assert!(BinaryOp::And.is_logical());
        assert!(BinaryOp::BitOr.is_bitwise());

        assert!(!BinaryOp::Add.is_comparison());
        assert!(!BinaryOp::Eq.is_arithmetic());
    }

    #[test]
    fn binary_op_str() {
        assert_eq!(BinaryOp::Add.as_str(), "+");
        assert_eq!(BinaryOp::And.as_str(), "and");
        assert_eq!(BinaryOp::Shl.as_str(), "<<");
    }

    #[test]
    fn unary_op_str() {
        assert_eq!(UnaryOp::Neg.as_str(), "-");
        assert_eq!(UnaryOp::Not.as_str(), "not");
        assert_eq!(UnaryOp::BitNot.as_str(), "~");
    }

    #[test]
    fn call_arg_positional() {
        let arg = CallArg::positional(int_literal(1));
        assert!(arg.name.is_none());
    }

    #[test]
    fn call_arg_named() {
        let arg = CallArg::named(dummy_ident("x"), int_literal(1), Span::DUMMY);
        assert!(arg.name.is_some());
        assert_eq!(arg.name.unwrap().name, sym("x"));
    }
}
