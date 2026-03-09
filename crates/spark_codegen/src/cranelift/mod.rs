//! Cranelift code generation backend.
//!
//! Compiles MIR to native object files using the Cranelift compiler framework.
//! This is the default backend for the Spark compiler.

pub mod context;
pub mod drop_glue;
pub mod emit_rvalue;
pub mod emit_stmt;
pub mod emit_term;
pub mod runtime;
pub mod types;

use cranelift_codegen::ir::{Function, InstBuilder, MemFlags};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context as ClifContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use rustc_hash::FxHashMap;

use spark_diagnostics::Diagnostics;
use spark_mir::{Local, Mir, MirFunction};
use spark_resolve::{DefId, DefinitionTable};
use spark_types::{Ty, TypeId, TypeTable};

use crate::abi;
use crate::backend::CodegenBackend;
use crate::layout::LayoutCache;
use crate::TargetSpec;

use self::context::FunctionContext;
use self::context::LocalStorage;
use self::types as clif_types;

/// The Cranelift-based code generation backend.
pub struct CraneliftBackend {
    /// The Cranelift object module.
    module: ObjectModule,
    /// Cache of thunk FuncIds for named function references used as values.
    /// A thunk is a thin wrapper that accepts+ignores an env pointer and
    /// forwards to the real function, enabling the fat-pointer calling convention.
    thunk_ids: FxHashMap<DefId, FuncId>,
    /// Cache of env destructor FuncIds for closures with RC captures.
    /// Each destructor decrements RC for captured values when the env is freed.
    dtor_ids: FxHashMap<DefId, FuncId>,
}

impl CraneliftBackend {
    /// Creates a new Cranelift backend for the given target.
    ///
    /// # Errors
    ///
    /// Returns an error message if the target ISA cannot be created.
    pub fn new(target: &TargetSpec) -> Result<Self, String> {
        let mut flag_builder = settings::builder();
        flag_builder
            .set("opt_level", "speed")
            .map_err(|e| format!("failed to set opt_level: {e}"))?;
        flag_builder
            .set("is_pic", "true")
            .map_err(|e| format!("failed to set is_pic: {e}"))?;

        // Enable additional Cranelift optimizations (ignore if unavailable).
        let _ = flag_builder.enable("enable_alias_analysis");

        // Use the native ISA builder when compiling for the host to
        // auto-detect CPU features (SSE4.2, AVX, AVX2, etc.).
        let is_host = target.triple == target_lexicon::Triple::host();
        let isa_builder = if is_host {
            cranelift_native::builder()
                .map_err(|e| format!("failed to create native ISA builder: {e}"))?
        } else {
            cranelift_codegen::isa::lookup(target.triple.clone())
                .map_err(|e| format!("unsupported target: {e}"))?
        };

        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| format!("failed to create ISA: {e}"))?;

        let builder = ObjectBuilder::new(
            isa,
            "spark_module",
            cranelift_module::default_libcall_names(),
        )
        .map_err(|e| format!("failed to create object builder: {e}"))?;

        let module = ObjectModule::new(builder);

        Ok(Self {
            module,
            thunk_ids: FxHashMap::default(),
            dtor_ids: FxHashMap::default(),
        })
    }

    /// Declares all MIR functions in the Cranelift module.
    ///
    /// Lambda functions get an extra hidden `__env: ptr` first parameter in their
    /// Cranelift signature (for the fat-pointer calling convention).
    /// Named (non-lambda) functions keep their normal signature.
    fn declare_all_functions(
        &mut self,
        mir: &Mir,
        table: &TypeTable,
        definitions: &DefinitionTable,
        diagnostics: &mut Diagnostics,
    ) -> FxHashMap<DefId, FuncId> {
        let mut func_ids = FxHashMap::default();
        let ptr_type = self.module.target_config().pointer_type();
        let call_conv = self.module.isa().default_call_conv();

        for (def_id, mir_func) in &mir.functions {
            let name = function_name(*def_id, definitions);
            let is_lambda = definitions.get(*def_id).map_or(false, |info| {
                matches!(info.kind, spark_resolve::DefinitionKind::Lambda)
            });

            // Build signature from parameter types and return type.
            let param_types: Vec<_> = mir_func.locals[1..=mir_func.param_count as usize]
                .iter()
                .map(|l| l.ty)
                .collect();

            let mut sig = types::build_signature(
                &param_types,
                mir_func.return_type,
                table,
                ptr_type,
                call_conv,
            );

            // Lambda functions get a hidden env_ptr parameter.
            // If sret is present (aggregate return), env_ptr goes after sret.
            if is_lambda {
                let insert_pos = if abi::needs_sret(mir_func.return_type, table) {
                    1
                } else {
                    0
                };
                sig.params
                    .insert(insert_pos, cranelift_codegen::ir::AbiParam::new(ptr_type));
            }

            let linkage = if name == "main" {
                Linkage::Export
            } else {
                Linkage::Local
            };

            match self.module.declare_function(&name, linkage, &sig) {
                Ok(func_id) => {
                    func_ids.insert(*def_id, func_id);
                }
                Err(e) => {
                    diagnostics.error(format!("failed to declare function '{name}': {e}"));
                }
            }
        }

        // Generate thunks for non-lambda functions that might be used as values.
        // A thunk wraps `foo(x)` as `__thunk_foo(env, x) { return foo(x) }`,
        // enabling the fat-pointer indirect call convention.
        self.generate_thunks(mir, table, definitions, &func_ids, diagnostics);

        func_ids
    }

    /// Generates thunk wrappers for named (non-lambda) functions.
    ///
    /// Each thunk accepts an extra `env_ptr` first parameter (which it ignores)
    /// and forwards all remaining arguments to the real function. This allows
    /// named function references to be called via the same fat-pointer indirect
    /// call convention as closures.
    #[allow(clippy::too_many_arguments)]
    fn generate_thunks(
        &mut self,
        mir: &Mir,
        table: &TypeTable,
        definitions: &DefinitionTable,
        func_ids: &FxHashMap<DefId, FuncId>,
        diagnostics: &mut Diagnostics,
    ) {
        let ptr_type = self.module.target_config().pointer_type();
        let call_conv = self.module.isa().default_call_conv();

        // Collect non-lambda functions to generate thunks for.
        let targets: Vec<_> = mir
            .functions
            .iter()
            .filter(|(def_id, _)| {
                definitions.get(**def_id).map_or(false, |info| {
                    !matches!(info.kind, spark_resolve::DefinitionKind::Lambda)
                })
            })
            .map(|(def_id, mir_func)| (*def_id, mir_func.clone()))
            .collect();

        for (def_id, mir_func) in &targets {
            let Some(&real_func_id) = func_ids.get(def_id) else {
                continue;
            };
            let name = function_name(*def_id, definitions);
            let thunk_name = format!("__thunk_{name}");

            // Build thunk signature: (sret?, env_ptr, ...original_params) -> original_return
            let param_types: Vec<_> = mir_func.locals[1..=mir_func.param_count as usize]
                .iter()
                .map(|l| l.ty)
                .collect();

            let sret = abi::needs_sret(mir_func.return_type, table);

            let mut thunk_sig = types::build_signature(
                &param_types,
                mir_func.return_type,
                table,
                ptr_type,
                call_conv,
            );
            // Insert env_ptr after sret (if present) or at position 0.
            let env_insert_pos = usize::from(sret);
            thunk_sig.params.insert(
                env_insert_pos,
                cranelift_codegen::ir::AbiParam::new(ptr_type),
            );

            let thunk_func_id =
                match self
                    .module
                    .declare_function(&thunk_name, Linkage::Local, &thunk_sig)
                {
                    Ok(fid) => fid,
                    Err(e) => {
                        diagnostics.error(format!("failed to declare thunk '{thunk_name}': {e}"));
                        continue;
                    }
                };

            // Define the thunk body: call the real function with all params except env.
            let real_sig = types::build_signature(
                &param_types,
                mir_func.return_type,
                table,
                ptr_type,
                call_conv,
            );

            let mut func = Function::with_name_signature(
                cranelift_codegen::ir::UserFuncName::user(0, thunk_func_id.as_u32()),
                thunk_sig.clone(),
            );

            let mut fbc = FunctionBuilderContext::new();
            {
                let mut builder = FunctionBuilder::new(&mut func, &mut fbc);
                let block = builder.create_block();
                builder.append_block_params_for_function_params(block);
                builder.switch_to_block(block);

                let params = builder.block_params(block).to_vec();

                // Build real_args: skip env_ptr, forward everything else (including sret).
                let real_args: Vec<_> = if sret {
                    // params = [sret, env, arg1, ...] → real_args = [sret, arg1, ...]
                    let mut args = vec![params[0]];
                    args.extend_from_slice(&params[2..]);
                    args
                } else {
                    // params = [env, arg1, ...] → real_args = [arg1, ...]
                    params[1..].to_vec()
                };

                let real_func_ref = self.module.declare_func_in_func(real_func_id, builder.func);

                // Import the real function's signature
                let sig_ref = builder.import_signature(real_sig);
                let _ = sig_ref; // use direct call instead

                let call = builder.ins().call(real_func_ref, &real_args);
                let results: Vec<_> = builder.inst_results(call).to_vec();
                builder.ins().return_(&results);

                builder.seal_all_blocks();
                builder.finalize();
            }

            let mut clif_ctx = ClifContext::for_function(func);
            #[allow(clippy::default_trait_access)]
            if let Err(e) = clif_ctx.compile(self.module.isa(), &mut Default::default()) {
                diagnostics.error(format!("codegen error in thunk '{thunk_name}': {e:?}"));
                continue;
            }
            if let Err(e) = self.module.define_function(thunk_func_id, &mut clif_ctx) {
                diagnostics.error(format!("failed to define thunk '{thunk_name}': {e}"));
                continue;
            }

            self.thunk_ids.insert(*def_id, thunk_func_id);
        }
    }

    /// Generates destructor functions for closure environments that capture
    /// RC values (String, List, Map, Set, Function, Future, Channel).
    ///
    /// Each destructor has signature `(env_ptr: ptr) -> void` and decrements
    /// the RC of every captured value that [`spark_drop::needs_drop`] returns
    /// true for. The destructor is called by `__spark_env_rc_dec` when the
    /// env's reference count reaches zero, before the allocation is freed.
    #[allow(clippy::too_many_arguments)]
    fn generate_env_destructors(
        &mut self,
        mir: &Mir,
        table: &TypeTable,
        definitions: &DefinitionTable,
        stubs: &runtime::RuntimeStubs,
        diagnostics: &mut Diagnostics,
    ) {
        let ptr_type = self.module.target_config().pointer_type();
        let call_conv = self.module.isa().default_call_conv();

        // Collect lambdas that have at least one RC/droppable capture.
        let targets: Vec<_> = mir
            .functions
            .iter()
            .filter(|(def_id, mir_func)| {
                let is_lambda = definitions
                    .get(**def_id)
                    .is_some_and(|info| matches!(info.kind, spark_resolve::DefinitionKind::Lambda));
                is_lambda
                    && !mir_func.captures.is_empty()
                    && mir_func.captures.iter().any(|(_, cap_ty)| {
                        let (_, unwrapped) = clif_types::unwrap_mutable(*cap_ty, table);
                        spark_drop::needs_drop(*cap_ty, table)
                            || matches!(unwrapped, spark_types::Ty::Function { .. })
                    })
            })
            .map(|(def_id, mir_func)| (*def_id, mir_func.clone()))
            .collect();

        for (def_id, mir_func) in &targets {
            let dtor_name = format!("__spark_env_dtor_{}", def_id.as_u32());

            // Signature: (env_ptr: ptr) -> void
            let mut dtor_sig = cranelift_codegen::ir::Signature::new(call_conv);
            dtor_sig
                .params
                .push(cranelift_codegen::ir::AbiParam::new(ptr_type));

            let dtor_func_id =
                match self
                    .module
                    .declare_function(&dtor_name, Linkage::Local, &dtor_sig)
                {
                    Ok(fid) => fid,
                    Err(e) => {
                        diagnostics.error(format!(
                            "failed to declare env destructor '{dtor_name}': {e}"
                        ));
                        continue;
                    }
                };

            // Build the destructor body.
            let mut func = Function::with_name_signature(
                cranelift_codegen::ir::UserFuncName::user(0, dtor_func_id.as_u32()),
                dtor_sig.clone(),
            );

            let mut fbc = FunctionBuilderContext::new();
            {
                let mut builder = FunctionBuilder::new(&mut func, &mut fbc);
                let block = builder.create_block();
                builder.append_block_params_for_function_params(block);
                builder.switch_to_block(block);

                let env_ptr = builder.block_params(block)[0];

                // Compute capture offsets (same logic as emit_closure_construction).
                let mut layouts = crate::layout::LayoutCache::new(ptr_type.bytes());
                let mut offset: u32 = 24; // skip header

                for (_cap_def_id, cap_ty) in &mir_func.captures {
                    let cap_layout = layouts.layout_of(*cap_ty, table);
                    offset = align_to_u32(offset, cap_layout.align);

                    let (_, unwrapped_ty) = clif_types::unwrap_mutable(*cap_ty, table);

                    if spark_drop::needs_drop(*cap_ty, table)
                        || matches!(unwrapped_ty, spark_types::Ty::Function { .. })
                    {
                        // Function fat pointers are aggregates: emit_typed_drop
                        // expects the ADDRESS of the fat pointer so it can load
                        // the env_ptr from the second slot. For all other types,
                        // we load the value (the RC pointer) and pass it directly.
                        if matches!(unwrapped_ty, spark_types::Ty::Function { .. }) {
                            let addr = builder.ins().iadd_imm(env_ptr, i64::from(offset));
                            drop_glue::emit_typed_drop(
                                &mut builder,
                                addr,
                                *cap_ty,
                                table,
                                stubs,
                                &mut self.module,
                                ptr_type,
                            );
                        } else {
                            let val = builder.ins().load(
                                ptr_type,
                                MemFlags::new(),
                                env_ptr,
                                i32::try_from(offset).unwrap_or(0),
                            );
                            drop_glue::emit_typed_drop(
                                &mut builder,
                                val,
                                *cap_ty,
                                table,
                                stubs,
                                &mut self.module,
                                ptr_type,
                            );
                        }
                    }

                    offset += cap_layout.size;
                }

                builder.ins().return_(&[]);
                builder.seal_all_blocks();
                builder.finalize();
            }

            let mut clif_ctx = ClifContext::for_function(func);
            #[allow(clippy::default_trait_access)]
            if let Err(e) = clif_ctx.compile(self.module.isa(), &mut Default::default()) {
                diagnostics.error(format!(
                    "codegen error in env destructor '{dtor_name}': {e:?}"
                ));
                continue;
            }
            if let Err(e) = self.module.define_function(dtor_func_id, &mut clif_ctx) {
                diagnostics.error(format!(
                    "failed to define env destructor '{dtor_name}': {e}"
                ));
                continue;
            }

            self.dtor_ids.insert(*def_id, dtor_func_id);
        }
    }

    /// Pre-generates element destructor functions for all collection types
    /// that appear in the MIR. Returns a map from element `TypeId` to the
    /// generated `FuncId` so codegen can store the function pointer in
    /// collection headers at construction time.
    fn generate_collection_dtors(
        &mut self,
        mir: &Mir,
        table: &TypeTable,
        stubs: &runtime::RuntimeStubs,
    ) -> FxHashMap<TypeId, FuncId> {
        let mut gen = drop_glue::DropGlueGenerator::new();
        let mut result = FxHashMap::default();

        for mir_func in mir.functions.values() {
            for local_decl in &mir_func.locals {
                self.collect_elem_dtors(local_decl.ty, table, stubs, &mut gen, &mut result);
            }
        }

        result
    }

    /// Recursively collects element destructor needs for a type.
    fn collect_elem_dtors(
        &mut self,
        ty_id: TypeId,
        table: &TypeTable,
        stubs: &runtime::RuntimeStubs,
        gen: &mut drop_glue::DropGlueGenerator,
        result: &mut FxHashMap<TypeId, FuncId>,
    ) {
        let (_, ty) = clif_types::unwrap_mutable(ty_id, table);
        match ty {
            Ty::List(elem) | Ty::Set(elem) => {
                let elem = *elem;
                if let Some(fid) =
                    gen.get_or_generate_elem_dtor(elem, table, stubs, &mut self.module)
                {
                    result.insert(elem, fid);
                }
                self.collect_elem_dtors(elem, table, stubs, gen, result);
            }
            Ty::Map(k, v) => {
                let k = *k;
                let v = *v;
                if let Some(fid) = gen.get_or_generate_elem_dtor(k, table, stubs, &mut self.module)
                {
                    result.insert(k, fid);
                }
                if let Some(fid) = gen.get_or_generate_elem_dtor(v, table, stubs, &mut self.module)
                {
                    result.insert(v, fid);
                }
                self.collect_elem_dtors(k, table, stubs, gen, result);
                self.collect_elem_dtors(v, table, stubs, gen, result);
            }
            _ => {}
        }
    }

    /// Compiles a single MIR function.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn compile_function(
        &mut self,
        func_id: FuncId,
        mir_func: &MirFunction,
        table: &TypeTable,
        definitions: &DefinitionTable,
        func_ids: &FxHashMap<DefId, FuncId>,
        thunk_ids: &FxHashMap<DefId, FuncId>,
        dtor_ids: &FxHashMap<DefId, FuncId>,
        elem_dtor_ids: &FxHashMap<TypeId, FuncId>,
        stubs: &runtime::RuntimeStubs,
        poll_fn_map: &FxHashMap<DefId, spark_mir::async_transform::PollFnInfo>,
        lambda_to_async: &FxHashMap<DefId, DefId>,
        diagnostics: &mut Diagnostics,
    ) {
        let ptr_type = self.module.target_config().pointer_type();
        let is_lambda = definitions
            .get(mir_func.def_id)
            .is_some_and(|info| matches!(info.kind, spark_resolve::DefinitionKind::Lambda));

        // Create the Cranelift function.
        let param_types: Vec<_> = mir_func.locals[1..=mir_func.param_count as usize]
            .iter()
            .map(|l| l.ty)
            .collect();

        let mut sig = types::build_signature(
            &param_types,
            mir_func.return_type,
            table,
            ptr_type,
            self.module.isa().default_call_conv(),
        );

        let sret = abi::needs_sret(mir_func.return_type, table);

        // Lambda functions have a hidden env_ptr parameter.
        // If sret is present (aggregate return), env_ptr goes after sret.
        if is_lambda {
            let insert_pos = usize::from(sret);
            sig.params
                .insert(insert_pos, cranelift_codegen::ir::AbiParam::new(ptr_type));
        }

        let mut func = Function::with_name_signature(
            cranelift_codegen::ir::UserFuncName::user(0, func_id.as_u32()),
            sig,
        );

        let mut func_builder_ctx = FunctionBuilderContext::new();
        let builder = FunctionBuilder::new(&mut func, &mut func_builder_ctx);

        let layouts = LayoutCache::new(ptr_type.bytes());

        let mut fctx = FunctionContext::new(builder, mir_func, table, ptr_type, layouts, stubs);

        // Set up the entry block with parameters.
        let entry_block = fctx.clif_block(spark_mir::BlockId::ENTRY);
        fctx.builder
            .append_block_params_for_function_params(entry_block);
        fctx.builder.switch_to_block(entry_block);

        // Map function parameters to locals.
        let entry_params: Vec<_> = fctx.builder.block_params(entry_block).to_vec();

        // Hidden params come first: sret (if aggregate return), then env_ptr (if lambda).
        // param_start tracks where user params begin.
        let mut param_start = 0;

        if sret && !entry_params.is_empty() {
            fctx.sret_ptr = Some(entry_params[0]);
            param_start += 1;
        }

        if is_lambda && param_start < entry_params.len() {
            fctx.env_ptr = Some(entry_params[param_start]);
            param_start += 1;
        }

        let mut param_idx = param_start;
        for i in 1..=mir_func.param_count {
            let local = Local(i);
            let ty_id = fctx.local_type_id(local);
            if !abi::is_zero_sized(ty_id, table) && param_idx < entry_params.len() {
                let param_val = entry_params[param_idx];

                // Aggregate params arrive as pointers — copy caller data into
                // the callee's stack slot so field accesses read correct values.
                if let LocalStorage::StackSlot(slot, size) = fctx.local_storage(local) {
                    let dest_addr = fctx.builder.ins().stack_addr(fctx.ptr_type, slot, 0);
                    let config = self.module.target_config();
                    fctx.builder.emit_small_memory_copy(
                        config,
                        dest_addr,
                        param_val,
                        u64::from(size),
                        1,
                        1,
                        true,
                        MemFlags::new(),
                    );
                } else {
                    fctx.write_local(local, param_val);
                }

                param_idx += 1;
            }
        }

        // Load captured variables from env_ptr into their locals.
        if is_lambda && !mir_func.captures.is_empty() {
            if let Some(env_ptr_val) = fctx.env_ptr {
                let mut capture_layouts = crate::layout::LayoutCache::new(ptr_type.bytes());
                let mut offset: u32 = 24; // skip header (rc: usize, alloc_size: usize, dtor: fn_ptr)
                let capture_start = 1 + mir_func.param_count;

                for (i, (_cap_def_id, cap_ty)) in mir_func.captures.iter().enumerate() {
                    let local = Local(capture_start + i as u32);
                    let cap_layout = capture_layouts.layout_of(*cap_ty, table);
                    offset = align_to_u32(offset, cap_layout.align);

                    if abi::is_scalar(*cap_ty, table) {
                        if let Some(clif_ty) = clif_types::local_clif_type(*cap_ty, table, ptr_type)
                        {
                            let val = fctx.builder.ins().load(
                                clif_ty,
                                MemFlags::new(),
                                env_ptr_val,
                                i32::try_from(offset).unwrap_or(0),
                            );
                            fctx.write_local(local, val);
                        }
                    } else if let LocalStorage::StackSlot(slot, size) = fctx.local_storage(local) {
                        // Aggregate capture — memcpy from env to local's stack slot.
                        let src = fctx.builder.ins().iadd_imm(env_ptr_val, i64::from(offset));
                        let dest = fctx.builder.ins().stack_addr(fctx.ptr_type, slot, 0);
                        let config = self.module.target_config();
                        fctx.builder.emit_small_memory_copy(
                            config,
                            dest,
                            src,
                            u64::from(size),
                            1,
                            1,
                            true,
                            MemFlags::new(),
                        );
                    }

                    offset += cap_layout.size;
                }
            }
        }

        // Initialize return place for scalars with a default value.
        let ret_ty = mir_func.return_type;
        if !abi::is_zero_sized(ret_ty, table) {
            if let Some((_, clif_ty)) = fctx.local_var(Local::RETURN_PLACE) {
                let f32_ty = cranelift_codegen::ir::types::F32;
                let f64_ty = cranelift_codegen::ir::types::F64;
                let zero = match clif_ty {
                    t if t == f32_ty => fctx.builder.ins().f32const(0.0),
                    t if t == f64_ty => fctx.builder.ins().f64const(0.0),
                    _ => fctx.builder.ins().iconst(clif_ty, 0),
                };
                fctx.write_local(Local::RETURN_PLACE, zero);
            }
        }

        // Emit all blocks.
        for (block_idx, mir_block) in mir_func.blocks.iter().enumerate() {
            if block_idx > 0 {
                let clif_block = fctx.blocks[block_idx];
                fctx.builder.switch_to_block(clif_block);
            }

            let ecx = context::EmitCtx {
                table,
                definitions,
                func_ids,
                thunk_ids,
                dtor_ids,
                elem_dtor_ids,
                poll_fn_map,
                lambda_to_async,
            };

            // Emit statements.
            for stmt in &mir_block.statements {
                emit_stmt::emit_statement(
                    &mut fctx,
                    stmt,
                    &ecx,
                    &mut self.module,
                    diagnostics,
                );
            }

            // Emit terminator.
            emit_term::emit_terminator(
                &mut fctx,
                &mir_block.terminator,
                &ecx,
                &mut self.module,
                diagnostics,
            );
        }

        // Finalize.
        fctx.builder.seal_all_blocks();
        fctx.builder.finalize();

        // Compile and define.
        let mut clif_ctx = ClifContext::for_function(func);
        #[allow(clippy::default_trait_access)]
        if let Err(e) = clif_ctx.compile(self.module.isa(), &mut Default::default()) {
            let name = function_name(mir_func.def_id, definitions);
            diagnostics.error(format!("codegen error in function '{name}': {e:?}"));
            return;
        }

        if let Err(e) = self.module.define_function(func_id, &mut clif_ctx) {
            let name = function_name(mir_func.def_id, definitions);
            diagnostics.error(format!("failed to define function '{name}': {e}"));
        }
    }
}

impl CodegenBackend for CraneliftBackend {
    fn compile(
        mut self,
        mir: &Mir,
        table: &TypeTable,
        definitions: &DefinitionTable,
        poll_fn_map: &FxHashMap<DefId, spark_mir::async_transform::PollFnInfo>,
        diagnostics: &mut Diagnostics,
    ) -> Vec<u8> {
        // Phase 0: Declare runtime stubs.
        let call_conv = self.module.isa().default_call_conv();
        let stubs = runtime::RuntimeStubs::declare_all(&mut self.module, call_conv);

        // Phase 1: Declare all functions.
        let func_ids = self.declare_all_functions(mir, table, definitions, diagnostics);

        // Phase 2a: Generate env destructors for closures with RC captures.
        self.generate_env_destructors(mir, table, definitions, &stubs, diagnostics);

        // Phase 2b: Generate element destructor functions for collections.
        let elem_dtor_ids = self.generate_collection_dtors(mir, table, &stubs);

        // Phase 2c: Precompute lambda → async function mapping for spawn_coop.
        // For each lambda that calls an async function with a poll entry,
        // record the mapping so emit_spawn_call can use the cooperative path.
        let mut lambda_to_async: FxHashMap<DefId, DefId> = FxHashMap::default();
        for mir_func in mir.functions.values() {
            for block in &mir_func.blocks {
                if let spark_mir::Terminator::Call {
                    func: spark_mir::Operand::Constant(spark_mir::Constant::FunctionRef(callee)),
                    ..
                } = &block.terminator
                {
                    if poll_fn_map.contains_key(callee) {
                        lambda_to_async.insert(mir_func.def_id, *callee);
                    }
                }
            }
        }

        // Phase 3: Compile each function.
        let thunk_ids = self.thunk_ids.clone();
        let dtor_ids = self.dtor_ids.clone();
        for (def_id, mir_func) in &mir.functions {
            if let Some(&func_id) = func_ids.get(def_id) {
                self.compile_function(
                    func_id,
                    mir_func,
                    table,
                    definitions,
                    &func_ids,
                    &thunk_ids,
                    &dtor_ids,
                    &elem_dtor_ids,
                    &stubs,
                    poll_fn_map,
                    &lambda_to_async,
                    diagnostics,
                );
            }
        }

        // Phase 4: Emit the object file.
        let is_macos = matches!(
            self.module.isa().triple().operating_system,
            target_lexicon::OperatingSystem::Darwin
                | target_lexicon::OperatingSystem::MacOSX { .. }
        );

        let mut product = self.module.finish();

        if is_macos {
            let mut build_version = object::write::MachOBuildVersion::default();
            build_version.platform = object::macho::PLATFORM_MACOS;
            build_version.minos = 0x000B_0000; // macOS 11.0.0 (Big Sur — minimum for arm64)
            build_version.sdk = 0x000B_0000; // macOS 11.0.0 SDK — matches minos
            product.object.set_macho_build_version(build_version);
        }

        match product.emit() {
            Ok(bytes) => bytes,
            Err(e) => {
                diagnostics.error(format!("failed to emit object file: {e}"));
                Vec::new()
            }
        }
    }
}

/// Rounds `offset` up to the next multiple of `align`.
const fn align_to_u32(offset: u32, align: u32) -> u32 {
    let mask = align - 1;
    (offset + mask) & !mask
}

/// Returns the linker name for a function.
///
/// Names are module-qualified to avoid collisions when different modules
/// define functions with the same name (e.g. `spark.io::lastError` vs
/// `spark.net::lastError`). The `main` function keeps its bare name
/// as the program entry point.
///
/// Binding methods are mangled as `TypeName$method_name` to avoid clashing
/// with free functions that share the same name in the same module.
fn function_name(def_id: DefId, definitions: &DefinitionTable) -> String {
    definitions.get(def_id).map_or_else(
        || format!("__spark_fn_{}", def_id.as_u32()),
        |info| {
            let name = info.name.as_str();
            if let spark_resolve::DefinitionKind::BindingMethod(type_def_id) = info.kind {
                let type_name = definitions
                    .get(type_def_id)
                    .map_or("?", |t| t.name.as_str());
                if info.module.is_root() {
                    format!("{type_name}${name}")
                } else {
                    format!("{}::{type_name}${name}", info.module)
                }
            } else if name == "main" {
                "main".to_string()
            } else if info.module.is_root() {
                name.to_string()
            } else {
                format!("{}::{name}", info.module)
            }
        },
    )
}
