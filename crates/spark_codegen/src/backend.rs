//! Backend-agnostic code generation trait.
//!
//! The [`CodegenBackend`] trait defines the interface that any code generation
//! backend (Cranelift, LLVM, etc.) must implement.

use rustc_hash::FxHashMap;
use spark_diagnostics::Diagnostics;
use spark_mir::Mir;
use spark_resolve::{DefId, DefinitionTable};
use spark_types::TypeTable;

/// A code generation backend that compiles MIR to native object bytes.
///
/// Backends manage their own internal state (module creation, function ordering,
/// ISA configuration). The trait receives the full MIR program plus type and
/// definition metadata, and returns a finished object file as raw bytes.
pub trait CodegenBackend {
    /// Compiles all functions in the MIR to a native object file.
    ///
    /// Returns the raw bytes of the object file (e.g. ELF, Mach-O, COFF).
    fn compile(
        self,
        mir: &Mir,
        table: &TypeTable,
        definitions: &DefinitionTable,
        poll_fn_map: &FxHashMap<DefId, spark_mir::async_transform::PollFnInfo>,
        diagnostics: &mut Diagnostics,
    ) -> Vec<u8>;
}
