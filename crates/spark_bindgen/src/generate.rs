//! Core binding generation using libclang.
//!
//! Parses a C header file with libclang and produces a list of [`ExternFn`]
//! declarations suitable for rendering as Spark `extern fn` items.

use std::path::Path;

use clang::{Clang, EntityKind, Index};

use crate::types::{map_c_type, TypeMapping};

/// A single `extern fn` declaration extracted from a C header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternFn {
    /// The C function name (e.g. `sqlite3_open`).
    pub name: String,
    /// Parameters as `(param_name, spark_type)` pairs.
    pub params: Vec<(String, String)>,
    /// The Spark return type.
    pub return_type: String,
}

/// A warning emitted during binding generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindgenWarning {
    /// The C entity that caused the warning (e.g. function name).
    pub entity: String,
    /// Human-readable reason for the warning.
    pub reason: String,
}

/// The output of [`generate_bindings`].
#[derive(Debug, Clone)]
pub struct BindgenOutput {
    /// Generated `extern fn` declarations.
    pub functions: Vec<ExternFn>,
    /// Warnings emitted during generation.
    pub warnings: Vec<BindgenWarning>,
}

/// Parses a C header file and generates Spark binding data.
///
/// Uses libclang (loaded at runtime) to parse the header. Only functions
/// declared directly in the specified header are included — entities from
/// `#include`d system headers are excluded.
///
/// # Errors
///
/// Returns an error string if:
/// - libclang cannot be loaded (with an install hint)
/// - The header file cannot be parsed
pub fn generate_bindings(
    header_path: &str,
    clang_args: &[String],
) -> Result<BindgenOutput, String> {
    let clang = Clang::new().map_err(|_| {
        "could not load libclang. Install it with:\n  \
         macOS:  brew install llvm\n  \
         Ubuntu: sudo apt install libclang-dev\n  \
         Fedora: sudo dnf install clang-devel\n\
         Then set LIBCLANG_PATH if needed."
            .to_string()
    })?;

    let index = Index::new(&clang, false, false);

    let tu = index
        .parser(header_path)
        .arguments(clang_args)
        .skip_function_bodies(true)
        .parse()
        .map_err(|_| format!("could not parse header `{header_path}`"))?;

    let header_filename = Path::new(header_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(header_path);

    let mut functions = Vec::new();
    let mut warnings = Vec::new();

    let root = tu.get_entity();

    for entity in root.get_children() {
        // Only include entities from the main header file, not from
        // #include'd system headers.
        if !is_from_main_file(&entity, header_filename) {
            continue;
        }

        if entity.get_kind() != EntityKind::FunctionDecl {
            continue;
        }

        let Some(name) = entity.get_name() else {
            continue;
        };

        // Skip variadic functions (e.g. printf) — Spark doesn't support them.
        if entity.get_type().is_some_and(|t| t.is_variadic()) {
            warnings.push(BindgenWarning {
                entity: name,
                reason: "variadic functions are not supported".to_string(),
            });
            continue;
        }

        let Some(fn_type) = entity.get_type() else {
            continue;
        };

        // Map return type.
        let ret_mapping = fn_type
            .get_result_type()
            .map_or(TypeMapping::Mapped("Void"), |t| map_c_type(&t));

        if let TypeMapping::Fallback { original, .. } = &ret_mapping {
            warnings.push(BindgenWarning {
                entity: name.clone(),
                reason: format!("return type `{original}` mapped to CPtr"),
            });
        }

        // Map parameter types.
        let args = entity.get_arguments().unwrap_or_default();
        let mut params = Vec::new();

        for (i, arg) in args.iter().enumerate() {
            let param_name = arg
                .get_name()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("arg{i}"));

            let param_type = arg
                .get_type()
                .map_or(TypeMapping::Mapped("CPtr"), |t| map_c_type(&t));

            if let TypeMapping::Fallback { original, .. } = &param_type {
                warnings.push(BindgenWarning {
                    entity: name.clone(),
                    reason: format!("parameter `{param_name}` type `{original}` mapped to CPtr"),
                });
            }

            params.push((param_name, param_type.spark_type().to_string()));
        }

        functions.push(ExternFn {
            name,
            params,
            return_type: ret_mapping.spark_type().to_string(),
        });
    }

    Ok(BindgenOutput {
        functions,
        warnings,
    })
}

/// Returns `true` if the entity originates from the main header file.
fn is_from_main_file(entity: &clang::Entity<'_>, header_filename: &str) -> bool {
    let Some(location) = entity.get_location() else {
        return false;
    };

    let file_location = location.get_file_location();
    let Some(file) = file_location.file else {
        return false;
    };

    let path = file.get_path();
    path.file_name().and_then(|n| n.to_str()) == Some(header_filename)
}
