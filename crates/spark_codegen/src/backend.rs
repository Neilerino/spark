//! Backend-agnostic code generation trait.
//!
//! The [`CodegenBackend`] trait defines the interface that any code generation
//! backend (Cranelift, LLVM, etc.) must implement.

use spark_diagnostics::Diagnostics;
use spark_mir::Mir;
use spark_resolve::DefinitionTable;
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
        diagnostics: &mut Diagnostics,
    ) -> Vec<u8>;
}
