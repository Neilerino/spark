//! MIR builder infrastructure.
//!
//! [`MirBuilder`] orchestrates lowering all HIR functions into MIR.
//! [`FunctionBuilder`] manages the construction of a single [`MirFunction`],
//! allocating locals, creating basic blocks, and tracking loop context.

use rustc_hash::FxHashMap;

use spark_diagnostics::Diagnostics;
use spark_intern::Symbol;
use spark_resolve::DefId;
use spark_span::Span;
use spark_types::{TypeId, TypeTable};

use crate::mir::{
    BasicBlock, BlockId, Local, LocalDecl, MirFunction, Place, Rvalue, Statement, Terminator,
};

/// Top-level MIR builder: holds the type table and diagnostics.
pub struct MirBuilder<'a> {
    /// Read-only access to the type table (reserved for future use).
    pub(crate) _type_table: &'a TypeTable,
    /// Accumulated diagnostics.
    pub(crate) diagnostics: Diagnostics,
}

impl<'a> MirBuilder<'a> {
    /// Creates a new `MirBuilder`.
    #[must_use]
    pub fn new(type_table: &'a TypeTable) -> Self {
        Self {
            _type_table: type_table,
            diagnostics: Diagnostics::new(),
        }
    }
}

/// Tracks break/continue targets for a loop.
pub struct LoopContext {
    /// Block to jump to on `continue`.
    pub continue_block: BlockId,
    /// Block to jump to on `break`.
    pub break_block: BlockId,
    /// Optional local to store the break value in.
    pub break_value: Option<Local>,
    /// Scope depth at which this loop was entered.
    pub scope_depth: usize,
}

/// Tracks locals that need RC cleanup when a scope exits.
struct ScopeContext {
    /// Locals registered for drop in this scope, in allocation order.
    /// Drops are emitted in reverse (LIFO) order.
    locals_to_drop: Vec<Local>,
}

/// Builds a single MIR function from HIR.
pub struct FunctionBuilder {
    /// The function's `DefId`.
    pub(crate) def_id: DefId,
    /// The function's return type.
    pub(crate) return_type: TypeId,
    /// All locals declared so far.
    pub(crate) locals: Vec<LocalDecl>,
    /// All basic blocks declared so far.
    pub(crate) blocks: Vec<BasicBlock>,
    /// The block we're currently appending to.
    pub(crate) current_block: BlockId,
    /// Maps HIR `DefId`s (locals, params) to MIR `Local`s.
    pub(crate) def_to_local: FxHashMap<DefId, Local>,
    /// Counter for allocating locals.
    next_local: u32,
    /// Counter for allocating blocks.
    next_block: u32,
    /// Stack of active loop contexts (innermost last).
    pub(crate) loop_stack: Vec<LoopContext>,
    /// Stack of active scopes for drop elaboration.
    scope_stack: Vec<ScopeContext>,
}

impl FunctionBuilder {
    /// Creates a new function builder with a return place and entry block.
    #[must_use]
    pub fn new(def_id: DefId, return_type: TypeId, span: Span) -> Self {
        let mut builder = Self {
            def_id,
            return_type,
            locals: Vec::new(),
            blocks: Vec::new(),
            current_block: BlockId::ENTRY,
            def_to_local: FxHashMap::default(),
            next_local: 0,
            next_block: 0,
            loop_stack: Vec::new(),
            scope_stack: vec![ScopeContext {
                locals_to_drop: Vec::new(),
            }],
        };

        // Local(0) = return place
        builder.alloc_local(return_type, None, false, false, span);
        // BlockId(0) = entry block
        builder.new_block();

        builder
    }

    /// Allocates a new local and returns its `Local` handle.
    fn alloc_local(
        &mut self,
        ty: TypeId,
        name: Option<Symbol>,
        is_user_variable: bool,
        mutable: bool,
        span: Span,
    ) -> Local {
        let local = Local(self.next_local);
        self.next_local += 1;
        self.locals.push(LocalDecl {
            local,
            ty,
            name,
            is_user_variable,
            mutable,
            span,
        });
        local
    }

    /// Allocates a named local for a user variable (binding or parameter).
    pub fn fresh_local(
        &mut self,
        ty: TypeId,
        name: Symbol,
        mutable: bool,
        span: Span,
    ) -> Local {
        self.alloc_local(ty, Some(name), true, mutable, span)
    }

    /// Allocates an unnamed compiler temporary.
    pub fn fresh_temp(&mut self, ty: TypeId, span: Span) -> Local {
        self.alloc_local(ty, None, false, false, span)
    }

    /// Creates a new basic block and returns its `BlockId`.
    pub fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.next_block);
        self.next_block += 1;
        self.blocks.push(BasicBlock {
            statements: Vec::new(),
            terminator: Terminator::Unreachable, // placeholder
            is_cleanup: false,
        });
        id
    }

    /// Appends a statement to the current block.
    pub fn push_stmt(&mut self, stmt: Statement) {
        self.blocks[self.current_block.index()]
            .statements
            .push(stmt);
    }

    /// Appends an assignment statement to the current block.
    pub fn push_assign(&mut self, place: Place, rvalue: Rvalue, span: Span) {
        self.push_stmt(Statement::Assign {
            place,
            rvalue,
            span,
        });
    }

    /// Sets the terminator of the current block.
    pub fn terminate(&mut self, term: Terminator) {
        self.blocks[self.current_block.index()].terminator = term;
    }

    /// Switches to emitting into a different block.
    pub const fn switch_to(&mut self, block: BlockId) {
        self.current_block = block;
    }

    /// Returns the current block id.
    #[must_use]
    pub const fn current_block(&self) -> BlockId {
        self.current_block
    }

    /// Returns whether the current block already has a real terminator
    /// (i.e. not the default `Unreachable` placeholder).
    #[must_use]
    pub fn is_terminated(&self) -> bool {
        !matches!(
            self.blocks[self.current_block.index()].terminator,
            Terminator::Unreachable
        )
    }

    /// Registers a `DefId` → `Local` mapping.
    pub fn register_def(&mut self, def_id: DefId, local: Local) {
        self.def_to_local.insert(def_id, local);
    }

    /// Looks up the `Local` for a given `DefId`.
    #[must_use]
    pub fn lookup_def(&self, def_id: DefId) -> Option<Local> {
        self.def_to_local.get(&def_id).copied()
    }

    /// Pushes a new loop context.
    pub fn push_loop(&mut self, ctx: LoopContext) {
        self.loop_stack.push(ctx);
    }

    /// Pops the innermost loop context.
    pub fn pop_loop(&mut self) -> Option<LoopContext> {
        self.loop_stack.pop()
    }

    /// Returns the innermost loop context, if any.
    #[must_use]
    pub fn current_loop(&self) -> Option<&LoopContext> {
        self.loop_stack.last()
    }

    // ── Scope management for drop elaboration ────────────────────

    /// Pushes a new scope onto the scope stack.
    pub fn push_scope(&mut self) {
        self.scope_stack.push(ScopeContext {
            locals_to_drop: Vec::new(),
        });
    }

    /// Registers a local for drop when its enclosing scope exits.
    pub fn register_drop(&mut self, local: Local) {
        if let Some(scope) = self.scope_stack.last_mut() {
            scope.locals_to_drop.push(local);
        }
    }

    /// Pops the current scope and emits `Drop` statements for all locals
    /// registered in that scope (in reverse / LIFO order).
    ///
    /// If `exclude` is `Some(local)`, that local is skipped (it's the value
    /// being returned or moved out of the scope).
    pub fn pop_scope_drops(&mut self, exclude: Option<Local>, span: Span) {
        if let Some(scope) = self.scope_stack.pop() {
            for &local in scope.locals_to_drop.iter().rev() {
                if exclude == Some(local) {
                    continue;
                }
                self.push_stmt(Statement::Drop {
                    place: Place::from_local(local),
                    span,
                });
            }
        }
    }

    /// Emits drops for all scopes (for `return`), excluding one local.
    pub fn drops_for_return(&mut self, exclude: Option<Local>, span: Span) {
        // Collect all locals from all scopes (innermost first).
        let mut all_locals: Vec<Local> = Vec::new();
        for scope in self.scope_stack.iter().rev() {
            for &local in scope.locals_to_drop.iter().rev() {
                if exclude != Some(local) {
                    all_locals.push(local);
                }
            }
        }
        for local in all_locals {
            self.push_stmt(Statement::Drop {
                place: Place::from_local(local),
                span,
            });
        }
    }

    /// Emits drops for scopes above the current loop (for `break`/`continue`).
    pub fn drops_to_loop(&mut self, exclude: Option<Local>, span: Span) {
        let loop_depth = self
            .current_loop()
            .map_or(0, |ctx| ctx.scope_depth);

        // Drop locals from scopes at indices >= loop_depth (innermost first).
        let mut locals_to_drop: Vec<Local> = Vec::new();
        for scope in self.scope_stack[loop_depth..].iter().rev() {
            for &local in scope.locals_to_drop.iter().rev() {
                if exclude != Some(local) {
                    locals_to_drop.push(local);
                }
            }
        }
        for local in locals_to_drop {
            self.push_stmt(Statement::Drop {
                place: Place::from_local(local),
                span,
            });
        }
    }

    /// Returns the current scope depth (used for loop context).
    #[must_use]
    pub const fn scope_depth(&self) -> usize {
        self.scope_stack.len()
    }

    /// Finishes building and returns the completed [`MirFunction`].
    #[must_use]
    pub fn finish(self, param_count: u32, span: Span) -> MirFunction {
        MirFunction {
            def_id: self.def_id,
            param_count,
            return_type: self.return_type,
            locals: self.locals,
            blocks: self.blocks,
            span,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_builder_init() {
        let fb = FunctionBuilder::new(DefId::new(0), TypeId::ERROR, Span::DUMMY);
        // Return place at Local(0)
        assert_eq!(fb.locals.len(), 1);
        assert_eq!(fb.locals[0].local, Local::RETURN_PLACE);
        // Entry block at BlockId(0)
        assert_eq!(fb.blocks.len(), 1);
        assert_eq!(fb.current_block, BlockId::ENTRY);
    }

    #[test]
    fn fresh_temp_allocates_sequentially() {
        let mut fb = FunctionBuilder::new(DefId::new(0), TypeId::ERROR, Span::DUMMY);
        let t1 = fb.fresh_temp(TypeId::ERROR, Span::DUMMY);
        let t2 = fb.fresh_temp(TypeId::ERROR, Span::DUMMY);
        assert_eq!(t1, Local(1));
        assert_eq!(t2, Local(2));
        assert_eq!(fb.locals.len(), 3); // return + 2 temps
    }

    #[test]
    fn new_block_allocates_sequentially() {
        let mut fb = FunctionBuilder::new(DefId::new(0), TypeId::ERROR, Span::DUMMY);
        let b1 = fb.new_block();
        let b2 = fb.new_block();
        assert_eq!(b1, BlockId(1));
        assert_eq!(b2, BlockId(2));
        assert_eq!(fb.blocks.len(), 3); // entry + 2
    }

    #[test]
    fn push_stmt_and_terminate() {
        let mut fb = FunctionBuilder::new(DefId::new(0), TypeId::ERROR, Span::DUMMY);
        fb.push_stmt(Statement::Nop);
        assert_eq!(fb.blocks[0].statements.len(), 1);

        assert!(!fb.is_terminated()); // default Unreachable doesn't count
        fb.terminate(Terminator::Return);
        assert!(fb.is_terminated());
    }

    #[test]
    fn register_and_lookup_def() {
        let mut fb = FunctionBuilder::new(DefId::new(0), TypeId::ERROR, Span::DUMMY);
        let local = fb.fresh_temp(TypeId::ERROR, Span::DUMMY);
        fb.register_def(DefId::new(5), local);
        assert_eq!(fb.lookup_def(DefId::new(5)), Some(local));
        assert_eq!(fb.lookup_def(DefId::new(99)), None);
    }

    #[test]
    fn loop_context_stack() {
        let mut fb = FunctionBuilder::new(DefId::new(0), TypeId::ERROR, Span::DUMMY);
        assert!(fb.current_loop().is_none());

        let brk = fb.new_block();
        let cont = fb.new_block();
        fb.push_loop(LoopContext {
            continue_block: cont,
            break_block: brk,
            break_value: None,
            scope_depth: fb.scope_depth(),
        });
        assert!(fb.current_loop().is_some());
        assert_eq!(fb.current_loop().unwrap().break_block, brk);

        fb.pop_loop();
        assert!(fb.current_loop().is_none());
    }
}
