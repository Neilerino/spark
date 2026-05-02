//! Centralized destructor (drop glue) generation for RC types.
//!
//! This module is the single place that answers "how do you drop a value of
//! type T?" — it eliminates the duplicated type-dispatch logic that was
//! previously in both `emit_stmt.rs::emit_drop` and
//! `mod.rs::generate_env_destructors`.
//!
//! It also provides [`DropGlueGenerator`] for generating per-type element-drop
//! functions that are stored in collection headers, enabling recursive cleanup
//! of nested collections (e.g. `List[List[String]]`).

use cranelift_codegen::ir::{InstBuilder, MemFlags, Type as ClifType, Value};
use cranelift_frontend::FunctionBuilder;
use cranelift_module::{FuncId, Module};
use rustc_hash::FxHashMap;
use spark_types::{Ty, TypeId, TypeTable};

use super::runtime::RuntimeStubs;
use super::types as clif_types;

/// Emits drop code (RC decrement) for a single value of the given type.
///
/// This is the centralized type dispatch used by both statement-level drops
/// and closure environment destructors. The caller has already determined
/// that the value needs dropping and loaded it into `ptr_val`.
///
/// # Parameters
///
/// - `builder`: the active Cranelift function builder
/// - `ptr_val`: the Cranelift IR value holding the pointer to the RC object
/// - `ty_id`: the Spark `TypeId` of the value (may be wrapped in `Mut[T]`)
/// - `table`: the type table for resolving inner types
/// - `stubs`: runtime function stubs
/// - `module`: the Cranelift module (for `declare_func_in_func`)
/// - `ptr_type`: the target pointer type (e.g. `I64` on 64-bit)
pub fn emit_typed_drop<M: Module>(
    builder: &mut FunctionBuilder<'_>,
    ptr_val: Value,
    ty_id: TypeId,
    table: &TypeTable,
    stubs: &RuntimeStubs,
    module: &mut M,
    ptr_type: ClifType,
) {
    let (_, ty) = clif_types::unwrap_mutable(ty_id, table);

    match ty {
        Ty::String => {
            let func_ref = module.declare_func_in_func(stubs.string_rc_dec, builder.func);
            builder.ins().call(func_ref, &[ptr_val]);
        }
        Ty::List(_) => {
            let func_ref = module.declare_func_in_func(stubs.list_rc_dec, builder.func);
            builder.ins().call(func_ref, &[ptr_val]);
        }
        Ty::Map(_, _) => {
            let func_ref = module.declare_func_in_func(stubs.map_rc_dec, builder.func);
            builder.ins().call(func_ref, &[ptr_val]);
        }
        Ty::Set(_) => {
            let func_ref = module.declare_func_in_func(stubs.set_rc_dec, builder.func);
            builder.ins().call(func_ref, &[ptr_val]);
        }
        Ty::Function { .. } => {
            // Function fat pointer: (fn_ptr, env_ptr).
            // Load env_ptr from the second slot and call __spark_env_rc_dec.
            let env_ptr =
                builder
                    .ins()
                    .load(ptr_type, MemFlags::new(), ptr_val, ptr_type.bytes() as i32);
            let func_ref = module.declare_func_in_func(stubs.env_rc_dec, builder.func);
            builder.ins().call(func_ref, &[env_ptr]);
        }
        Ty::Future { .. } => {
            let func_ref = module.declare_func_in_func(stubs.future_rc_dec, builder.func);
            builder.ins().call(func_ref, &[ptr_val]);
        }
        Ty::Channel { .. } => {
            let func_ref = module.declare_func_in_func(stubs.channel_rc_dec, builder.func);
            builder.ins().call(func_ref, &[ptr_val]);
        }
        _ => {
            // Non-droppable type — no-op (shouldn't reach here if the caller
            // checked `needs_drop` first).
        }
    }
}

/// Generates per-type element-drop functions and caches them by [`TypeId`].
///
/// An element-drop function has signature `fn(elem_ptr: *mut u8) -> void`.
/// It loads the pointer from `elem_ptr` (since elements in collection data
/// buffers are stored by value — for RC types that means the slot contains
/// a pointer) and calls the appropriate `_rc_dec` for that type.
///
/// These functions are stored in collection headers so the runtime can call
/// them recursively when freeing nested collections.
#[derive(Default)]
pub struct DropGlueGenerator {
    /// Cache: [`TypeId`] to generated [`FuncId`] for element destructors.
    cache: FxHashMap<TypeId, FuncId>,
}

impl DropGlueGenerator {
    /// Creates a new generator with an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the element destructor `FuncId` for a collection element type,
    /// generating it on first request. Returns `None` if the element type
    /// does not need RC cleanup (e.g. `Int`, `Bool`).
    ///
    /// For `List[String]`: generates a fn that loads ptr from slot, calls
    /// `__spark_string_rc_dec`.
    ///
    /// For `List[List[String]]`: generates a fn that loads ptr, calls
    /// `__spark_list_rc_dec` (which recursively reads the inner list's stored
    /// dtor).
    pub fn get_or_generate_elem_dtor<M: Module>(
        &mut self,
        elem_ty: TypeId,
        table: &TypeTable,
        stubs: &RuntimeStubs,
        module: &mut M,
    ) -> Option<FuncId> {
        if !spark_drop::needs_drop(elem_ty, table) {
            return None;
        }

        if let Some(&func_id) = self.cache.get(&elem_ty) {
            return Some(func_id);
        }

        let func_id = self.generate_elem_dtor(elem_ty, table, stubs, module);
        self.cache.insert(elem_ty, func_id);
        Some(func_id)
    }

    /// Generates an element destructor function for the given type.
    ///
    /// Signature: `fn(elem_ptr: *mut u8) -> void`
    ///
    /// The function loads the pointer value from `elem_ptr` and calls
    /// the appropriate `_rc_dec`.
    #[allow(clippy::unused_self)] // `&self` reserved for future per-instance state
    fn generate_elem_dtor<M: Module>(
        &self,
        elem_ty: TypeId,
        table: &TypeTable,
        stubs: &RuntimeStubs,
        module: &mut M,
    ) -> FuncId {
        let ptr_type = module.target_config().pointer_type();
        let call_conv = module.isa().default_call_conv();

        let dtor_name = format!("__spark_elem_drop_{}", elem_ty.as_u32());

        // Signature: (elem_ptr: ptr) -> void
        let mut dtor_sig = cranelift_codegen::ir::Signature::new(call_conv);
        dtor_sig
            .params
            .push(cranelift_codegen::ir::AbiParam::new(ptr_type));

        let dtor_func_id = module
            .declare_function(&dtor_name, cranelift_module::Linkage::Local, &dtor_sig)
            .unwrap_or_else(|e| panic!("ICE: failed to declare elem dtor '{dtor_name}': {e}"));

        // Build the function body.
        let mut func = cranelift_codegen::ir::Function::with_name_signature(
            cranelift_codegen::ir::UserFuncName::user(0, dtor_func_id.as_u32()),
            dtor_sig,
        );

        let mut fbc = cranelift_frontend::FunctionBuilderContext::new();
        {
            let mut builder = cranelift_frontend::FunctionBuilder::new(&mut func, &mut fbc);
            let block = builder.create_block();
            builder.append_block_params_for_function_params(block);
            builder.switch_to_block(block);

            let elem_ptr = builder.block_params(block)[0];

            let (_, unwrapped_ty) = clif_types::unwrap_mutable(elem_ty, table);

            if matches!(unwrapped_ty, Ty::Function { .. }) {
                // Function fat pointers are aggregates stored inline.
                // emit_typed_drop expects the ADDRESS of the fat pointer
                // so it can load the env_ptr from the second slot.
                emit_typed_drop(
                    &mut builder,
                    elem_ptr,
                    elem_ty,
                    table,
                    stubs,
                    module,
                    ptr_type,
                );
            } else {
                // Scalar RC types: the element slot contains a pointer to
                // the RC object. Load it and pass to emit_typed_drop.
                let inner_ptr = builder.ins().load(ptr_type, MemFlags::new(), elem_ptr, 0);
                emit_typed_drop(
                    &mut builder,
                    inner_ptr,
                    elem_ty,
                    table,
                    stubs,
                    module,
                    ptr_type,
                );
            }

            builder.ins().return_(&[]);
            builder.seal_all_blocks();
            builder.finalize();
        }

        let mut clif_ctx = cranelift_codegen::Context::for_function(func);
        #[allow(clippy::default_trait_access)]
        clif_ctx
            .compile(module.isa(), &mut Default::default())
            .unwrap_or_else(|e| panic!("ICE: codegen error in elem dtor '{dtor_name}': {e:?}"));
        module
            .define_function(dtor_func_id, &mut clif_ctx)
            .unwrap_or_else(|e| panic!("ICE: failed to define elem dtor '{dtor_name}': {e}"));

        dtor_func_id
    }
}
