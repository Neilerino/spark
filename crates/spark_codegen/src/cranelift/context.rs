//! Per-function compilation context.
//!
//! Wraps a Cranelift [`FunctionBuilder`] with MIR-specific state: local
//! variable mappings, block mappings, and helpers for emitting common patterns.

use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::types;
use cranelift_codegen::ir::{
    Block, InstBuilder, MemFlags, StackSlotData, StackSlotKind, Type as ClifType, Value,
};
use cranelift_frontend::{FunctionBuilder, Variable};
use cranelift_module::FuncId;
use rustc_hash::FxHashMap;
use spark_mir::{BlockId, Local, MirFunction};
use spark_resolve::{DefId, DefinitionTable};
use spark_types::{TypeId, TypeTable};

use crate::abi;
use crate::cranelift::runtime::RuntimeStubs;
use crate::cranelift::types as clif_types;
use crate::layout::LayoutCache;

/// Read-only compilation tables shared across all emission functions.
///
/// Bundles the immutable lookup tables that nearly every codegen helper needs,
/// reducing repetitive parameter passing.
pub struct EmitCtx<'a> {
    /// The type table for resolving type IDs.
    pub table: &'a TypeTable,
    /// The definition table for looking up functions, methods, etc.
    pub definitions: &'a DefinitionTable,
    /// Mapping from definition IDs to Cranelift function IDs.
    pub func_ids: &'a FxHashMap<DefId, FuncId>,
    /// Mapping from named functions to their thunk wrappers.
    pub thunk_ids: &'a FxHashMap<DefId, FuncId>,
    /// Mapping from lambda definition IDs to their env destructors.
    pub dtor_ids: &'a FxHashMap<DefId, FuncId>,
    /// Mapping from element types to their collection-element destructors.
    pub elem_dtor_ids: &'a FxHashMap<TypeId, FuncId>,
    /// Mapping from element types to their collection-element retain functions.
    pub elem_retain_ids: &'a FxHashMap<TypeId, FuncId>,
    /// Mapping from element types to their slot equality functions.
    pub elem_eq_ids: &'a FxHashMap<TypeId, FuncId>,
    /// Poll function info for cooperative async.
    pub poll_fn_map: &'a FxHashMap<DefId, spark_mir::async_transform::PollFnInfo>,
    /// Mapping from lambda `DefIds` to their async function `DefIds`.
    pub lambda_to_async: &'a FxHashMap<DefId, DefId>,
}

/// How a MIR local is stored at the Cranelift IR level.
#[derive(Debug, Clone, Copy)]
pub enum LocalStorage {
    /// Scalar value held in a Cranelift SSA variable.
    Var(Variable, ClifType),
    /// Scalar with a stable address shared by direct reads and references.
    ScalarSlot(cranelift_codegen::ir::StackSlot, ClifType),
    /// Aggregate value held in a stack slot.
    StackSlot(cranelift_codegen::ir::StackSlot, u32),
    /// Zero-sized type — no storage needed.
    ZeroSized,
}

/// Per-function compilation state.
pub struct FunctionContext<'a> {
    /// The Cranelift function builder.
    pub builder: FunctionBuilder<'a>,
    /// Mapping from MIR `Local` to Cranelift storage.
    pub locals: Vec<LocalStorage>,
    /// Mapping from MIR `BlockId` to Cranelift `Block`.
    pub blocks: Vec<Block>,
    /// The pointer type for this target.
    pub ptr_type: ClifType,
    /// Layout cache for type size computations.
    pub layouts: LayoutCache,
    /// Type table reference.
    pub table: &'a TypeTable,
    /// The MIR function being compiled.
    pub mir_func: &'a MirFunction,
    /// Runtime function stubs.
    pub stubs: &'a RuntimeStubs,
    /// For lambda functions: the `env_ptr` Value (first Cranelift block param).
    /// Used by Phase B to load captured variables from the environment.
    pub env_ptr: Option<Value>,
    /// For functions with aggregate return types: the sret pointer passed by the caller.
    /// The callee copies Local(0) to this pointer on return.
    pub sret_ptr: Option<Value>,
}

impl<'a> FunctionContext<'a> {
    /// Creates a new function context. Sets up blocks and local storage.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        mut builder: FunctionBuilder<'a>,
        mir_func: &'a MirFunction,
        table: &'a TypeTable,
        ptr_type: ClifType,
        mut layouts: LayoutCache,
        stubs: &'a RuntimeStubs,
    ) -> Self {
        // Create one Cranelift block per MIR basic block.
        let blocks: Vec<Block> = (0..mir_func.blocks.len())
            .map(|_| builder.create_block())
            .collect();

        // Declare locals.
        let mut locals = Vec::with_capacity(mir_func.locals.len());
        let mut next_var = 0usize;
        let address_taken = mir_func.address_taken_locals();

        for local_decl in &mir_func.locals {
            let ty_id = local_decl.ty;
            if abi::is_zero_sized(ty_id, table) {
                locals.push(LocalStorage::ZeroSized);
            } else if abi::is_scalar(ty_id, table) {
                let clif_ty =
                    clif_types::local_clif_type(ty_id, table, ptr_type).unwrap_or(types::I64);
                // TAG [SPARK-129]
                if address_taken.contains(&local_decl.local) {
                    let layout = layouts.layout_of(ty_id, table);
                    let slot = builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        layout.size,
                        u8::try_from(layout.align.trailing_zeros()).unwrap_or(0),
                    ));
                    locals.push(LocalStorage::ScalarSlot(slot, clif_ty));
                } else {
                    let var = Variable::new(next_var);
                    next_var += 1;
                    builder.declare_var(var, clif_ty);
                    locals.push(LocalStorage::Var(var, clif_ty));
                }
            } else {
                // Aggregate — allocate a stack slot.
                let layout = layouts.layout_of(ty_id, table);
                let slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    layout.size,
                    u8::try_from(layout.align.trailing_zeros()).unwrap_or(0),
                ));
                locals.push(LocalStorage::StackSlot(slot, layout.size));
            }
        }

        Self {
            builder,
            locals,
            blocks,
            ptr_type,
            layouts,
            table,
            mir_func,
            stubs,
            env_ptr: None,
            sret_ptr: None,
        }
    }

    /// Returns the Cranelift block for a MIR block ID.
    #[must_use]
    pub fn clif_block(&self, block_id: BlockId) -> Block {
        self.blocks[block_id.index()]
    }

    /// Reads a scalar local's current value.
    ///
    /// For stack slots, returns the address of the slot.
    /// For zero-sized types, returns an iconst 0.
    #[must_use]
    pub fn read_local(&mut self, local: Local) -> Value {
        match self.locals[local.index()] {
            LocalStorage::Var(var, _) => self.builder.use_var(var),
            LocalStorage::ScalarSlot(slot, ty) => {
                let addr = self.builder.ins().stack_addr(self.ptr_type, slot, 0);
                self.load(ty, addr, 0)
            }
            LocalStorage::StackSlot(slot, _) => {
                self.builder.ins().stack_addr(self.ptr_type, slot, 0)
            }
            LocalStorage::ZeroSized => self.builder.ins().iconst(types::I8, 0),
        }
    }

    /// Writes a scalar value to a local.
    ///
    /// For stack slots, this is a no-op (aggregates are written field-by-field).
    pub fn write_local(&mut self, local: Local, val: Value) {
        match self.locals[local.index()] {
            LocalStorage::Var(var, _) => {
                self.builder.def_var(var, val);
            }
            LocalStorage::ScalarSlot(slot, _) => {
                let addr = self.builder.ins().stack_addr(self.ptr_type, slot, 0);
                self.store(val, addr, 0);
            }
            LocalStorage::StackSlot(_, _) | LocalStorage::ZeroSized => {
                // Aggregates and ZSTs: no scalar write needed.
            }
        }
    }

    /// Returns the address of a local's stack slot (for aggregates).
    ///
    /// Returns `None` if the local is not stored in a stack slot.
    #[must_use]
    pub fn local_stack_addr(&mut self, local: Local) -> Option<Value> {
        match self.locals[local.index()] {
            LocalStorage::StackSlot(slot, _) => {
                Some(self.builder.ins().stack_addr(self.ptr_type, slot, 0))
            }
            _ => None,
        }
    }

    /// Stores a value at a memory address.
    pub fn store(&mut self, val: Value, addr: Value, offset: i32) {
        self.builder.ins().store(MemFlags::new(), val, addr, offset);
    }

    /// Loads a value from a memory address.
    #[must_use]
    pub fn load(&mut self, ty: ClifType, addr: Value, offset: i32) -> Value {
        self.builder.ins().load(ty, MemFlags::new(), addr, offset)
    }

    /// Returns the type ID of a MIR local.
    #[must_use]
    pub fn local_type_id(&self, local: Local) -> TypeId {
        self.mir_func.locals[local.index()].ty
    }

    /// Returns the storage kind for a local.
    #[must_use]
    pub fn local_storage(&self, local: Local) -> LocalStorage {
        self.locals[local.index()]
    }

    /// Returns the native scalar type for register and addressable scalar locals.
    #[must_use]
    pub fn local_scalar_type(&self, local: Local) -> Option<ClifType> {
        match self.locals[local.index()] {
            LocalStorage::Var(_, ty) | LocalStorage::ScalarSlot(_, ty) => Some(ty),
            _ => None,
        }
    }
}
