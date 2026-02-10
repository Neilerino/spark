//! Cranelift code generation backend.
//!
//! Compiles MIR to native object files using the Cranelift compiler framework.
//! This is the default backend for the Spark compiler.

pub mod context;
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
use spark_types::TypeTable;

use crate::abi;
use crate::backend::CodegenBackend;
use crate::layout::LayoutCache;
use crate::TargetSpec;

use self::context::FunctionContext;

/// The Cranelift-based code generation backend.
pub struct CraneliftBackend {
    /// The Cranelift object module.
    module: ObjectModule,
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

        let isa = cranelift_codegen::isa::lookup(target.triple.clone())
            .map_err(|e| format!("unsupported target: {e}"))?
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| format!("failed to create ISA: {e}"))?;

        let builder = ObjectBuilder::new(
            isa,
            "spark_module",
            cranelift_module::default_libcall_names(),
        )
        .map_err(|e| format!("failed to create object builder: {e}"))?;

        let module = ObjectModule::new(builder);

        Ok(Self { module })
    }

    /// Declares all MIR functions in the Cranelift module.
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

            // Build signature from parameter types and return type.
            let param_types: Vec<_> = mir_func.locals[1..=mir_func.param_count as usize]
                .iter()
                .map(|l| l.ty)
                .collect();

            let sig = types::build_signature(
                &param_types,
                mir_func.return_type,
                table,
                ptr_type,
                call_conv,
            );

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

        // Note: built-in functions like `print` are handled by type-based
        // dispatch in emit_term.rs, so they are NOT mapped to a single FuncId here.

        func_ids
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
        stubs: &runtime::RuntimeStubs,
        diagnostics: &mut Diagnostics,
    ) {
        let ptr_type = self.module.target_config().pointer_type();

        // Create the Cranelift function.
        let param_types: Vec<_> = mir_func.locals[1..=mir_func.param_count as usize]
            .iter()
            .map(|l| l.ty)
            .collect();

        let sig = types::build_signature(
            &param_types,
            mir_func.return_type,
            table,
            ptr_type,
            self.module.isa().default_call_conv(),
        );

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
        fctx.builder.append_block_params_for_function_params(entry_block);
        fctx.builder.switch_to_block(entry_block);

        // Map function parameters to locals.
        let entry_params: Vec<_> = fctx
            .builder
            .block_params(entry_block)
            .to_vec();

        let mut param_idx = 0;
        for i in 1..=mir_func.param_count {
            let local = Local(i);
            let ty_id = fctx.local_type_id(local);
            if !abi::is_zero_sized(ty_id, table)
                && param_idx < entry_params.len()
            {
                let param_val = entry_params[param_idx];

                // Aggregate params arrive as pointers — copy caller data into
                // the callee's stack slot so field accesses read correct values.
                if let context::LocalStorage::StackSlot(slot, size) =
                    fctx.local_storage(local)
                {
                    let dest_addr =
                        fctx.builder.ins().stack_addr(fctx.ptr_type, slot, 0);
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

            // Emit statements.
            for stmt in &mir_block.statements {
                emit_stmt::emit_statement(
                    &mut fctx,
                    stmt,
                    table,
                    definitions,
                    func_ids,
                    &mut self.module,
                    diagnostics,
                );
            }

            // Emit terminator.
            emit_term::emit_terminator(
                &mut fctx,
                &mir_block.terminator,
                table,
                definitions,
                func_ids,
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
        diagnostics: &mut Diagnostics,
    ) -> Vec<u8> {
        // Phase 0: Declare runtime stubs.
        let call_conv = self.module.isa().default_call_conv();
        let stubs = runtime::RuntimeStubs::declare_all(&mut self.module, call_conv);

        // Phase 1: Declare all functions.
        let func_ids = self.declare_all_functions(mir, table, definitions, diagnostics);

        // Phase 2: Compile each function.
        for (def_id, mir_func) in &mir.functions {
            if let Some(&func_id) = func_ids.get(def_id) {
                self.compile_function(
                    func_id,
                    mir_func,
                    table,
                    definitions,
                    &func_ids,
                    &stubs,
                    diagnostics,
                );
            }
        }

        // Phase 3: Emit the object file.
        let product = self.module.finish();
        match product.emit() {
            Ok(bytes) => bytes,
            Err(e) => {
                diagnostics.error(format!("failed to emit object file: {e}"));
                Vec::new()
            }
        }
    }
}

/// Returns the linker name for a function.
fn function_name(def_id: DefId, definitions: &DefinitionTable) -> String {
    definitions
        .get(def_id)
        .map_or_else(
            || format!("__spark_fn_{}", def_id.as_u32()),
            |info| info.name.as_str().to_string(),
        )
}
