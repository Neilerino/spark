//! HIR node definitions.
//!
//! The High-Level IR is a desugared, typed, tree-structured representation.
//! Every expression carries its [`TypeId`] and [`Span`], and all identifiers
//! are resolved to [`DefId`]s.

use std::collections::HashMap;

use spark_ast::{BinaryOp, UnaryOp};
use spark_intern::Symbol;
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::TypeId;

/// The complete HIR for a compilation unit.
#[derive(Debug)]
pub struct Hir {
    /// All functions, keyed by their [`DefId`].
    pub functions: HashMap<DefId, HirFunction>,
    /// All module-level constants, keyed by their [`DefId`].
    pub statics: HashMap<DefId, HirStatic>,
}

/// A function in the HIR.
#[derive(Debug)]
pub struct HirFunction {
    /// The definition ID of this function.
    pub def_id: DefId,
    /// Function parameters.
    pub params: Vec<HirParam>,
    /// The function body.
    pub body: HirExpr,
    /// The return type.
    pub ret_type: TypeId,
    /// Whether this function is async.
    pub is_async: bool,
    /// Source span.
    pub span: Span,
    /// Captured variables from enclosing scope. Empty for non-closures.
    pub captures: Vec<(DefId, TypeId)>,
}

/// A module-level constant in the HIR.
#[derive(Debug)]
pub struct HirStatic {
    /// The definition ID.
    pub def_id: DefId,
    /// The type of this constant.
    pub ty: TypeId,
    /// The value expression.
    pub value: HirExpr,
    /// Source span.
    pub span: Span,
}

/// A function parameter.
#[derive(Debug, Clone)]
pub struct HirParam {
    /// The definition ID of this parameter.
    pub def_id: DefId,
    /// The parameter type.
    pub ty: TypeId,
    /// Source span.
    pub span: Span,
}

/// A typed HIR expression.
#[derive(Debug)]
pub struct HirExpr {
    /// The expression kind.
    pub kind: HirExprKind,
    /// The type of this expression.
    pub ty: TypeId,
    /// Source span.
    pub span: Span,
}

impl HirExpr {
    /// Creates a new HIR expression.
    #[must_use]
    pub const fn new(kind: HirExprKind, ty: TypeId, span: Span) -> Self {
        Self { kind, ty, span }
    }

    /// Creates an error expression.
    #[must_use]
    pub const fn error(span: Span) -> Self {
        Self::new(HirExprKind::Error, TypeId::ERROR, span)
    }
}

/// HIR expression kinds.
///
/// This is a subset of the AST's `ExprKind` with syntactic sugar removed.
/// `Paren`, `BlockAwait`, `Coalesce`, `OptionalField`, `Propagate`, and
/// `CompoundAssign` are desugared away.
#[derive(Debug)]
pub enum HirExprKind {
    // === Values ===
    /// A literal value.
    Literal(HirLiteral),
    /// A reference to a local, parameter, function, or constant.
    Local(DefId),

    // === Operators ===
    /// Binary operation.
    BinaryOp {
        /// The operator.
        op: BinaryOp,
        /// Left-hand side.
        lhs: Box<HirExpr>,
        /// Right-hand side.
        rhs: Box<HirExpr>,
    },
    /// Unary operation.
    UnaryOp {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        operand: Box<HirExpr>,
    },

    // === Access ===
    /// Field access: `base.field`.
    FieldAccess {
        /// The base expression.
        base: Box<HirExpr>,
        /// The field name.
        field: Symbol,
    },
    /// Index access: `base[index]`.
    Index {
        /// The base expression.
        base: Box<HirExpr>,
        /// The index expression.
        index: Box<HirExpr>,
    },

    // === Calls ===
    /// Function call: `callee(args...)`.
    Call {
        /// The callee expression.
        callee: Box<HirExpr>,
        /// Positional arguments (named args stripped).
        args: Vec<HirExpr>,
    },
    /// Method call: `receiver.method(args...)`.
    MethodCall {
        /// The receiver expression.
        receiver: Box<HirExpr>,
        /// The method name.
        method: Symbol,
        /// Arguments.
        args: Vec<HirExpr>,
    },

    // === Constructors ===
    /// Struct initialization: `Struct { field: value, ... }`.
    StructInit {
        /// The struct's [`DefId`].
        def_id: DefId,
        /// Field values.
        fields: Vec<(Symbol, HirExpr)>,
    },
    /// Enum variant: `Enum.Variant(fields...)`.
    EnumVariant {
        /// The enum variant's [`DefId`].
        def_id: DefId,
        /// The variant name.
        variant: Symbol,
        /// Fields (positional).
        fields: Vec<HirExpr>,
    },
    /// List literal: `[a, b, c]`.
    ListLit(Vec<HirExpr>),
    /// Map literal: `{k: v, ...}`.
    MapLit(Vec<(HirExpr, HirExpr)>),
    /// Set literal: `{a, b, c}`.
    SetLit(Vec<HirExpr>),
    /// Tuple literal: `(a, b, c)`.
    TupleLit(Vec<HirExpr>),

    // === Control flow ===
    /// Block: `{ stmts; expr }`.
    Block {
        /// Statements.
        stmts: Vec<HirStmt>,
        /// Optional trailing expression.
        expr: Option<Box<HirExpr>>,
    },
    /// If expression: `if cond { then } else { else_ }`.
    If {
        /// Condition (must be `Bool`).
        cond: Box<HirExpr>,
        /// Then branch.
        then_: Box<HirExpr>,
        /// Optional else branch.
        else_: Option<Box<HirExpr>>,
    },
    /// Match expression.
    Match {
        /// The value being matched.
        scrutinee: Box<HirExpr>,
        /// Match arms.
        arms: Vec<HirMatchArm>,
    },
    /// Infinite loop.
    Loop {
        /// Loop body.
        body: Box<HirExpr>,
    },
    /// Break out of a loop.
    Break(Option<Box<HirExpr>>),
    /// Continue to next loop iteration.
    Continue,
    /// Return from function.
    Return(Option<Box<HirExpr>>),

    // === Closures ===
    /// Lambda expression.
    Lambda {
        /// Parameters.
        params: Vec<HirParam>,
        /// Body expression.
        body: Box<HirExpr>,
    },
    /// Closure construction: a lifted function + captured variables.
    ///
    /// After lambda lifting, a capturing lambda is replaced with this node.
    /// The codegen builds a fat pointer `(fn_ptr, env_ptr)` where `env_ptr`
    /// points to a heap-allocated environment containing the capture values.
    MakeClosure {
        /// The DefId of the lifted closure function.
        func_def_id: DefId,
        /// Variables captured from the enclosing scope, with their types.
        captures: Vec<(DefId, TypeId)>,
    },

    // === Type operations ===
    /// Type test: `expr is Type`.
    Is {
        /// The expression to test.
        expr: Box<HirExpr>,
        /// The target type.
        target_ty: TypeId,
    },
    /// Type cast: `expr as Type`.
    As {
        /// The expression to cast.
        expr: Box<HirExpr>,
        /// The target type.
        target_ty: TypeId,
    },

    // === Async ===
    /// Await a future.
    Await(Box<HirExpr>),
    /// Spawn a concurrent task.
    Spawn(Box<HirExpr>),

    // === References ===
    /// Take a reference to a value.
    Borrow {
        /// The expression being borrowed.
        operand: Box<HirExpr>,
        /// Whether the borrow is mutable.
        mutable: bool,
    },
    /// Dereference a reference.
    Deref(Box<HirExpr>),

    // === String interpolation ===
    /// String interpolation desugared to a list of parts.
    /// Each part is a `HirExpr` typed as `String` — text literals become
    /// `HirLiteral::String(sym)`, and interpolation expressions are wrapped
    /// in type-conversion calls.
    StringInterp {
        /// The parts of the interpolated string, all typed as String.
        parts: Vec<HirExpr>,
    },

    // === Parallel iteration ===
    /// Parallel for-each: dispatches to `__spark_parallel_foreach` runtime stub.
    ParallelForEach {
        /// The list expression.
        list: Box<HirExpr>,
        /// The callback (fat pointer).
        callback: Box<HirExpr>,
        /// Element type of the list.
        elem_ty: TypeId,
    },
    /// Parallel map: dispatches to `__spark_parallel_map` runtime stub.
    ParallelMap {
        /// The list expression.
        list: Box<HirExpr>,
        /// The callback (fat pointer).
        callback: Box<HirExpr>,
        /// Element type of the input list.
        elem_ty: TypeId,
        /// Element type of the result list.
        result_elem_ty: TypeId,
    },

    // === Error recovery ===
    /// Error placeholder (from failed lowering).
    Error,
}

/// A literal value in the HIR.
#[derive(Debug, Clone)]
pub enum HirLiteral {
    /// Integer literal.
    Int(i64),
    /// Float literal.
    Float(f64),
    /// Boolean literal.
    Bool(bool),
    /// String literal (interned).
    String(Symbol),
    /// Character literal.
    Char(char),
    /// Null literal.
    Null,
}

/// A statement in the HIR.
#[derive(Debug)]
pub enum HirStmt {
    /// Variable binding: `name: Type = value`.
    Let {
        /// The definition ID of the local.
        def_id: DefId,
        /// Whether the variable is mutable.
        mutable: bool,
        /// The initial value.
        value: HirExpr,
    },
    /// Assignment: `target = value`.
    Assign {
        /// The assignment target.
        target: HirExpr,
        /// The value.
        value: HirExpr,
    },
    /// Expression statement (for side effects).
    Expr(HirExpr),
}

/// A match arm in the HIR.
#[derive(Debug)]
pub struct HirMatchArm {
    /// The pattern to match.
    pub pattern: HirPattern,
    /// Optional guard expression.
    pub guard: Option<HirExpr>,
    /// The arm body.
    pub body: HirExpr,
    /// Source span.
    pub span: Span,
}

/// A pattern in the HIR.
#[derive(Debug)]
pub enum HirPattern {
    /// Matches anything: `_`.
    Wildcard,
    /// Binds the matched value to a name.
    Binding {
        /// The definition ID of the binding.
        def_id: DefId,
    },
    /// Matches a literal value.
    Literal(HirLiteral),
    /// Matches an enum variant.
    Variant {
        /// The variant's [`DefId`].
        def_id: DefId,
        /// Sub-patterns for variant fields.
        fields: Vec<Self>,
    },
    /// Matches a struct.
    Struct {
        /// The struct's [`DefId`].
        def_id: DefId,
        /// Named field patterns.
        fields: Vec<(Symbol, Self)>,
        /// Whether `...` rest is present.
        rest: bool,
    },
    /// Matches a tuple.
    Tuple(Vec<Self>),
    /// Matches one of several patterns: `A | B`.
    Or(Vec<Self>),
    /// Matches a union member by type and binds the narrowed value.
    ///
    /// Used in desugared `!` (propagate) and `to` (type narrowing) to
    /// match a specific type within a union.
    TypeNarrow {
        /// The binding for the narrowed value.
        def_id: DefId,
        /// The type to match against.
        ty: TypeId,
    },
    /// Error placeholder.
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hir_expr_error() {
        let expr = HirExpr::error(Span::DUMMY);
        assert!(matches!(expr.kind, HirExprKind::Error));
        assert!(expr.ty.is_error());
    }

    #[test]
    fn hir_expr_literal() {
        let expr = HirExpr::new(
            HirExprKind::Literal(HirLiteral::Int(42)),
            TypeId::ERROR, // placeholder
            Span::DUMMY,
        );
        assert!(matches!(expr.kind, HirExprKind::Literal(HirLiteral::Int(42))));
    }

    #[test]
    fn hir_function_creation() {
        let func = HirFunction {
            def_id: DefId::new(0),
            params: vec![],
            body: HirExpr::error(Span::DUMMY),
            ret_type: TypeId::ERROR,
            is_async: false,
            span: Span::DUMMY,
            captures: vec![],
        };
        assert!(!func.is_async);
        assert!(func.params.is_empty());
    }

    #[test]
    fn hir_empty() {
        let hir = Hir {
            functions: HashMap::new(),
            statics: HashMap::new(),
        };
        assert!(hir.functions.is_empty());
        assert!(hir.statics.is_empty());
    }
}
