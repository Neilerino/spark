//! MIR node definitions.
//!
//! The Mid-Level IR is a control-flow-graph-based representation where each
//! function is a collection of basic blocks connected by explicit terminators.
//! All intermediate values are held in locals/temporaries, and control flow
//! is expressed as edges between blocks.

use std::collections::HashMap;
use std::fmt;

use spark_ast::{BinaryOp, UnaryOp};
use spark_intern::Symbol;
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::TypeId;

// ── Identifiers ──────────────────────────────────────────────────────────

/// A basic block identifier. `BlockId(0)` is always the entry block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(pub u32);

impl BlockId {
    /// The entry block for every function.
    pub const ENTRY: Self = Self(0);

    /// Returns the raw index.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bb{}", self.0)
    }
}

/// A local variable or temporary. `Local(0)` is the return place, locals
/// `1..=param_count` are function parameters, and the rest are user
/// variables or compiler temporaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Local(pub u32);

impl Local {
    /// The return place.
    pub const RETURN_PLACE: Self = Self(0);

    /// Returns the raw index.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for Local {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "_{}", self.0)
    }
}

// ── Top-level ────────────────────────────────────────────────────────────

/// The complete MIR for a compilation unit.
#[derive(Debug)]
pub struct Mir {
    /// All functions, keyed by their [`DefId`].
    pub functions: HashMap<DefId, MirFunction>,
}

/// A single function in MIR form.
#[derive(Debug, Clone)]
pub struct MirFunction {
    /// The definition ID of this function.
    pub def_id: DefId,
    /// Number of parameters (locals `1..=param_count`).
    pub param_count: u32,
    /// The function's return type.
    pub return_type: TypeId,
    /// All locals (index 0 = return place).
    pub locals: Vec<LocalDecl>,
    /// All basic blocks (index 0 = entry block).
    pub blocks: Vec<BasicBlock>,
    /// Captured variables from the enclosing scope (for closures).
    /// Each entry is `(DefId, TypeId)` — the variable's definition and type.
    /// The corresponding MIR locals are at indices
    /// `(1 + param_count)..(1 + param_count + captures.len())`.
    pub captures: Vec<(DefId, TypeId)>,
    /// Whether this function is declared `async`.
    pub is_async: bool,
    /// Source span of the function.
    pub span: Span,
}

impl MirFunction {
    /// Resolves the [`DefId`] of a callee from a function call operand.
    ///
    /// Handles two cases:
    /// 1. The operand is a direct `FunctionRef` constant.
    /// 2. The operand is a `Copy`/`Move` of a local that was assigned a
    ///    `FunctionRef` — scans all blocks' statements to find the assignment.
    pub fn resolve_callee_def_id(&self, operand: &Operand) -> Option<DefId> {
        // Direct constant case.
        if let Operand::Constant(Constant::FunctionRef(def_id)) = operand {
            return Some(*def_id);
        }

        // Copy/Move of a local — scan for the assignment.
        let local = match operand {
            Operand::Copy(place) | Operand::Move(place) if place.projections.is_empty() => {
                place.local
            }
            _ => return None,
        };

        for block in &self.blocks {
            for stmt in &block.statements {
                if let Statement::Assign { place, rvalue, .. } = stmt {
                    if place.local == local && place.projections.is_empty() {
                        if let Rvalue::Use(Operand::Constant(Constant::FunctionRef(def_id))) =
                            rvalue
                        {
                            return Some(*def_id);
                        }
                    }
                }
            }
        }

        None
    }
}

/// Declaration of a local variable or temporary.
#[derive(Debug, Clone)]
pub struct LocalDecl {
    /// The local identifier.
    pub local: Local,
    /// The type of this local.
    pub ty: TypeId,
    /// Optional user-visible name (`None` for compiler temporaries).
    pub name: Option<Symbol>,
    /// Whether this is a user-declared variable (vs. compiler temporary).
    pub is_user_variable: bool,
    /// Whether this local is mutable.
    pub mutable: bool,
    /// Source span (of declaration or expression producing the temp).
    pub span: Span,
}

// ── Basic blocks ─────────────────────────────────────────────────────────

/// A basic block: a sequence of statements followed by a terminator.
#[derive(Debug, Clone)]
pub struct BasicBlock {
    /// Statements executed sequentially.
    pub statements: Vec<Statement>,
    /// The block's terminator (controls transfer to other blocks).
    pub terminator: Terminator,
    /// Whether this block is a cleanup/unwind handler.
    pub is_cleanup: bool,
}

// ── Statements ───────────────────────────────────────────────────────────

/// A single statement within a basic block.
#[derive(Debug, Clone)]
pub enum Statement {
    /// Assign an rvalue to a place: `place = rvalue`.
    Assign {
        /// The destination place.
        place: Place,
        /// The value to assign.
        rvalue: Rvalue,
        /// Source span.
        span: Span,
    },
    /// Drop the value at a place.
    Drop {
        /// The place to drop.
        place: Place,
        /// Source span.
        span: Span,
    },
    /// No operation (placeholder).
    Nop,
}

// ── Places and projections ───────────────────────────────────────────────

/// A place in memory (l-value): a local with optional projections.
#[derive(Debug, Clone)]
pub struct Place {
    /// The root local.
    pub local: Local,
    /// Projections from the root (field access, indexing, deref).
    pub projections: Vec<Projection>,
}

impl Place {
    /// Creates a place referring to just a local with no projections.
    #[must_use]
    pub const fn from_local(local: Local) -> Self {
        Self {
            local,
            projections: Vec::new(),
        }
    }
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.local)?;
        for proj in &self.projections {
            match proj {
                Projection::Field(sym) => write!(f, ".{}", sym.as_str())?,
                Projection::Index(local) => write!(f, "[{local}]")?,
                Projection::Deref => write!(f, ".*")?,
                Projection::UnionPayload(_) => write!(f, ".union_payload")?,
                Projection::EnumField { index, .. } => write!(f, ".enum_field({index})")?,
            }
        }
        Ok(())
    }
}

/// A projection from a place.
#[derive(Debug, Clone)]
pub enum Projection {
    /// Access a named field.
    Field(Symbol),
    /// Index into a collection (the index is held in a local).
    Index(Local),
    /// Dereference a pointer/reference.
    Deref,
    /// Extract the payload from a union value, narrowed to the given type.
    UnionPayload(TypeId),
    /// Access a positional field of an enum variant payload.
    EnumField {
        /// The variant's [`DefId`] (for looking up field types).
        variant: DefId,
        /// Zero-based field index within the variant.
        index: u32,
    },
}

// ── Rvalues ──────────────────────────────────────────────────────────────

/// A right-hand-side value that can be assigned to a place.
#[derive(Debug, Clone)]
pub enum Rvalue {
    /// Use an operand directly.
    Use(Operand),
    /// Binary operation.
    BinaryOp(BinaryOp, Operand, Operand),
    /// Unary operation.
    UnaryOp(UnaryOp, Operand),
    /// Take a reference to a place.
    Ref {
        /// The place to reference.
        place: Place,
        /// Whether the reference is mutable.
        mutable: bool,
    },
    /// Construct an aggregate value (struct, enum variant, tuple, etc.).
    Aggregate(AggregateKind, Vec<Operand>),
    /// Read the discriminant of an enum.
    Discriminant(Place),
    /// Type cast.
    Cast {
        /// The source operand.
        operand: Operand,
        /// The target type.
        target_ty: TypeId,
    },
    /// String interpolation: operands are the parts (text literals and
    /// expressions). Each carries its original type; the codegen emits
    /// to-string conversions and a concat chain.
    StringInterp(Vec<Operand>),
    /// List range construction: `[start..end]` or `[start..=end:step]`.
    ///
    /// Calls `__spark_list_from_range(start, end, inclusive, step)` at runtime.
    ListRange {
        /// Range start.
        start: Operand,
        /// Range end.
        end: Operand,
        /// Whether the range is inclusive (`..=`).
        inclusive: bool,
        /// Step value.
        step: Operand,
    },
    /// Compute the length of a range without allocating a list.
    ///
    /// Result: number of elements in `[start..end]` or `[start..=end]` with
    /// the given step. Introduced by the deforestation pass to replace
    /// `list_len(ListRange(...))`.
    RangeLen {
        /// Range start.
        start: Operand,
        /// Range end.
        end: Operand,
        /// Whether the range is inclusive (`..=`).
        inclusive: bool,
        /// Step value.
        step: Operand,
    },
}

/// The kind of aggregate being constructed.
#[derive(Debug, Clone)]
pub enum AggregateKind {
    /// Struct construction.
    Struct {
        /// The struct's [`DefId`].
        def_id: DefId,
        /// Field names (in order matching operands).
        fields: Vec<Symbol>,
    },
    /// Enum variant construction.
    EnumVariant {
        /// The enum's [`DefId`].
        def_id: DefId,
        /// The variant name.
        variant: Symbol,
    },
    /// Tuple construction.
    Tuple,
    /// List construction.
    List,
    /// Map construction (operands alternate key, value).
    Map,
    /// Set construction.
    Set,
    /// Closure construction: captures are stored into a heap-allocated
    /// environment, and a fat pointer `(fn_ptr, env_ptr)` is produced.
    Closure {
        /// The DefId of the lifted closure function.
        func_def_id: DefId,
    },
    /// Channel construction: `Channel[T]()` allocates a new MPSC channel.
    /// No operands (the channel is created empty).
    Channel,
    /// Union construction: wraps a value of `member_ty` into a union of
    /// `union_ty` by writing a tag byte (the member's index in the union)
    /// followed by the payload.
    Union {
        /// The union type (`Ty::Union(members)`).
        union_ty: TypeId,
        /// The member type being stored.
        member_ty: TypeId,
    },
}

// ── Operands ─────────────────────────────────────────────────────────────

/// An operand used in rvalues and terminators.
#[derive(Debug, Clone)]
pub enum Operand {
    /// Copy the value at a place (for `Copy` types).
    Copy(Place),
    /// Move the value out of a place.
    Move(Place),
    /// An inline constant.
    Constant(Constant),
}

/// A compile-time constant.
#[derive(Debug, Clone)]
pub enum Constant {
    /// Integer constant.
    Int(i64),
    /// Float constant.
    Float(f64),
    /// Boolean constant.
    Bool(bool),
    /// String constant (interned).
    String(Symbol),
    /// Character constant.
    Char(char),
    /// Null value.
    Null,
    /// Reference to a function.
    FunctionRef(DefId),
    /// Void / unit value.
    Void,
    /// Reference to a compiler intrinsic (introduced by optimization passes).
    /// Unlike `FunctionRef`, does not require a `DefId` or symbol registration.
    IntrinsicRef(Intrinsic),
}

/// Compiler intrinsics introduced by optimization passes.
///
/// These bypass the symbol/builtin system — codegen maps them directly to
/// runtime stubs. The `Intrinsic` enum is extensible: future deforestation
/// patterns just add variants here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intrinsic {
    /// Parallel foreach over a range (no list allocation).
    ParallelRangeForEach,
    /// Parallel map over a range (no input list allocation).
    ParallelRangeMap,
    /// Fused parallel map+reduce over a range (zero allocation).
    ParallelRangeMapReduce,
}

// ── Terminators ──────────────────────────────────────────────────────────

/// A block terminator — how control leaves a basic block.
#[derive(Debug, Clone)]
pub enum Terminator {
    /// Unconditional jump.
    Goto {
        /// Target block.
        target: BlockId,
    },
    /// Return from the function (returning the value in `Local(0)`).
    Return,
    /// Conditional branch based on an integer value.
    SwitchInt {
        /// The value to switch on.
        discriminant: Operand,
        /// Matched values and their target blocks.
        targets: Vec<(i128, BlockId)>,
        /// The default/otherwise block.
        otherwise: BlockId,
    },
    /// Function call.
    Call {
        /// The function operand.
        func: Operand,
        /// Arguments.
        args: Vec<Operand>,
        /// Where to store the result.
        destination: Place,
        /// The block to continue to after the call returns.
        target: BlockId,
        /// Source span.
        span: Span,
    },
    /// Unreachable code (e.g. after a guaranteed diverging call).
    Unreachable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_id_display() {
        assert_eq!(BlockId(0).to_string(), "bb0");
        assert_eq!(BlockId(42).to_string(), "bb42");
    }

    #[test]
    fn local_display() {
        assert_eq!(Local(0).to_string(), "_0");
        assert_eq!(Local(7).to_string(), "_7");
    }

    #[test]
    fn place_from_local() {
        let place = Place::from_local(Local(3));
        assert_eq!(place.local, Local(3));
        assert!(place.projections.is_empty());
        assert_eq!(place.to_string(), "_3");
    }

    #[test]
    fn block_id_entry() {
        assert_eq!(BlockId::ENTRY, BlockId(0));
        assert_eq!(BlockId::ENTRY.index(), 0);
    }

    #[test]
    fn local_return_place() {
        assert_eq!(Local::RETURN_PLACE, Local(0));
        assert_eq!(Local::RETURN_PLACE.index(), 0);
    }

    #[test]
    fn mir_function_creation() {
        let func = MirFunction {
            def_id: DefId::new(0),
            param_count: 0,
            return_type: TypeId::ERROR,
            locals: vec![],
            blocks: vec![],
            captures: vec![],
            is_async: false,
            span: Span::DUMMY,
        };
        assert_eq!(func.param_count, 0);
        assert!(func.locals.is_empty());
        assert!(func.blocks.is_empty());
    }

    #[test]
    fn basic_block_creation() {
        let block = BasicBlock {
            statements: vec![Statement::Nop],
            terminator: Terminator::Return,
            is_cleanup: false,
        };
        assert_eq!(block.statements.len(), 1);
        assert!(!block.is_cleanup);
    }

    #[test]
    fn constant_variants() {
        let _ = Constant::Int(42);
        let _ = Constant::Float(3.14);
        let _ = Constant::Bool(true);
        let _ = Constant::Char('a');
        let _ = Constant::Null;
        let _ = Constant::Void;
    }
}
