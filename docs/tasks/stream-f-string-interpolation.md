# Stream F: Syntax & Convenience

> String interpolation

## Overview

Purely lexer/parser changes, independent of all other streams. The AST already has `StringPart::Interpolation` support — this is about wiring up the lexer and parser to produce it, and flowing it through HIR/MIR/codegen.

**Conflicts**: None. Touches only `spark_lexer`, `spark_parser`, and downstream lowering.

---

## Item: String Interpolation (P5.3)

**Status**: Not tokenized or parsed. AST structure is ready.

### Current State

**AST is already prepared** (`spark_ast/src/lit.rs:80-135`):
```rust
pub struct StringLiteral {
    pub parts: Vec<StringPart>,
    pub is_raw: bool,
    pub is_multiline: bool,
}

pub enum StringPart {
    Text(Symbol),
    Interpolation(Box<crate::Expr>),
}
```

`StringLiteral::simple(text)` creates a non-interpolated string (all current strings). The infrastructure for multi-part strings with interpolated expressions already exists.

**Lexer** (`spark_lexer/src/lexer.rs:272-303`):
- `scan_string()` handles escape sequences and unicode escapes
- Currently returns a single string symbol — no interpolation parsing

**Parser** (`spark_parser/src/expr.rs:743-752`):
- `parse_string_literal()` creates `StringLiteral::simple(text)`
- No handling of `{expr}` inside strings

**HIR** (`spark_hir/src/lower_expr.rs:325-342`):
- `lower_literal()` extracts first text part for simple strings
- Returns empty string for interpolated strings (deferred)

**Type checking** (`spark_types/src/check_expr.rs:53`):
- `LiteralKind::String(_) => builtins.string`

### Implementation Plan

#### Step 1: Lexer Changes

The lexer needs to recognize `{expr}` inside string literals. Two approaches:

**Option A: Lexer emits interpolation tokens**
```
"hello {name}, you are {age} years old"
→ StringStart, StringText("hello "), InterpStart, Ident("name"), InterpEnd,
  StringText(", you are "), InterpStart, Ident("age"), InterpEnd,
  StringText(" years old"), StringEnd
```

**Option B: Lexer emits a single token with parts**
The lexer scans the full string, splitting on `{...}`, and stores the raw text segments. The parser then re-parses the interpolated segments as expressions.

Option A is cleaner for nested expressions (e.g., `"result: {foo(x + 1)}"`). Option B is simpler but struggles with braces inside interpolations.

**Recommendation**: Option A — it integrates naturally with the parser's expression parsing.

#### Step 2: Parser Changes

When the parser sees `StringStart`:
1. Collect `StringText` parts as `StringPart::Text`
2. When hitting `InterpStart`, parse an expression until `InterpEnd`
3. Wrap the expression in `StringPart::Interpolation`
4. Continue until `StringEnd`
5. Construct `StringLiteral { parts, is_raw: false, is_multiline: false }`

#### Step 3: Type Checking

Each interpolated expression must be checkable (any type that can be converted to String). For now, require `String`, `Int`, `Float`, `Bool` — types that `print()` already supports.

#### Step 4: HIR Lowering

Desugar interpolated strings into string concatenation or a `__spark_string_format()` runtime call:
```
"hello {name}"
→ __spark_string_concat(__spark_string_from_literal("hello "), __spark_to_string(name))
```

Or use a format-style approach with a single runtime call that takes a template and args.

#### Step 5: Runtime Support

Add runtime functions:
- `__spark_to_string(value, type_tag)` — convert scalar to string
- `__spark_string_concat(a, b)` — concatenate two strings (or extend existing concat if it exists)

### Key Files

| Purpose | Path |
|---------|------|
| String AST structure | `crates/spark_ast/src/lit.rs:80-135` |
| Lexer string scanning | `crates/spark_lexer/src/lexer.rs:272-303` |
| Lexer escape handling | `crates/spark_lexer/src/lexer.rs:305-358` |
| Token definitions | `crates/spark_lexer/src/token.rs` |
| Parser string literal | `crates/spark_parser/src/expr.rs:743-752` |
| HIR literal lowering | `crates/spark_hir/src/lower_expr.rs:325-342` |
| Type checking literals | `crates/spark_types/src/check_expr.rs:53` |
| Runtime library | `crates/spark_runtime/src/lib.rs` |

## Reference Docs

- Language spec: `docs/lang-spec.md` (string interpolation section)
- Roadmap: `docs/roadmap.md` (item 5.3)
