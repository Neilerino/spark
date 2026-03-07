//! Reusable diagnostic constructors for the type checker.
//!
//! Each function takes the minimum parameters needed and returns a
//! [`Diagnostic`] (or pushes one directly for conditional helpers).
//! Centralising these keeps `check_expr.rs` focused on type logic.

use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_intern::Symbol;
use spark_span::Span;

/// Tuple index out of range.
///
/// > tuple index `N` out of range for tuple with M element(s)
pub fn tuple_oob(index: usize, len: usize, span: Span) -> Diagnostic {
    Diagnostic::error(format!(
        "tuple index `{index}` out of range for tuple with {len} element{}",
        if len == 1 { "" } else { "s" },
    ))
    .with_label(Label::primary(span, "index out of range"))
}

/// No method on a type.
///
/// > `{type_name}` has no method `{method}`
pub fn no_method(type_name: &str, method: Symbol, span: Span) -> Diagnostic {
    Diagnostic::error(format!("{type_name} has no method `{method}`"))
        .with_label(Label::primary(span, "unknown method"))
}

/// Argument type mismatch.
///
/// > argument type mismatch: expected `X`, found `Y`
pub fn arg_type_mismatch(expected: &str, found: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!(
        "argument type mismatch: expected `{expected}`, found `{found}`"
    ))
    .with_label(Label::primary(span, format!("expected `{expected}`")))
}

/// Cannot pass an immutable value where a mutable reference is required.
pub fn mutable_ref_required(span: Span) -> Diagnostic {
    Diagnostic::error("cannot pass immutable value as mutable reference")
        .with_label(Label::primary(span, "value is not mutable"))
}

/// No field on a type.
///
/// > no field `X` on type `Y`
pub fn field_not_found(field_name: Symbol, type_name: &str, span: Span) -> Diagnostic {
    Diagnostic::error(format!("no field `{field_name}` on type `{type_name}`"))
        .with_label(Label::primary(span, "unknown field"))
}

/// Propagation operator `!` used outside a function body.
pub fn propagation_outside_fn(span: Span) -> Diagnostic {
    Diagnostic::error("`!` can only be used inside a function")
        .with_label(Label::primary(span, "not inside a function"))
}

/// Reports an error if `expected != actual` argument count.
pub fn expect_args(expected: usize, actual: usize, span: Span, diagnostics: &mut Diagnostics) {
    if actual != expected {
        diagnostics.push(
            Diagnostic::error(format!(
                "expected {expected} argument{}, found {actual}",
                if expected == 1 { "" } else { "s" }
            ))
            .with_label(Label::primary(span, "wrong number of arguments")),
        );
    }
}

/// Reports an error when calling a mutating method on an immutable receiver.
pub fn require_mutable(
    type_name: &str,
    is_mutable: bool,
    recv_span: Span,
    diagnostics: &mut Diagnostics,
) {
    if !is_mutable {
        diagnostics.push(
            Diagnostic::error(format!("cannot mutate immutable {type_name}"))
                .with_label(Label::primary(recv_span, "value is not mutable")),
        );
    }
}
