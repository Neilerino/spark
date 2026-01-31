# spark_lexer

Lexical analysis (tokenization) for the Spark compiler.

## Responsibility

This crate transforms source code text into a stream of **tokens**. Tokens are the atomic units that the parser works with — keywords, identifiers, literals, operators, and punctuation.

### Core Types

| Type | Purpose |
|------|---------|
| `Token` | A single token with kind, span, and optional value |
| `TokenKind` | The type of token (keyword, operator, literal, etc.) |
| `TokenValue` | Literal values (integers, floats, strings, etc.) |
| `Lexer` | Converts source text into tokens |

### Key Operations

```rust
use spark_lexer::{Lexer, TokenKind};
use spark_span::SourceMap;

let mut source_map = SourceMap::new();
let file_id = source_map.add_file("example.spark", "fn main(): Void { }");

let mut lexer = Lexer::new(file_id, source_map.get_file(file_id).unwrap().source());
let tokens = lexer.tokenize();

assert_eq!(tokens[0].kind, TokenKind::Fn);
assert_eq!(tokens[1].kind, TokenKind::Identifier);
```

## Relationship to Other Crates

```
spark_span ─────────┐
                    │
spark_intern ───────┼──► spark_lexer (this crate)
                    │          │
spark_diagnostics ──┘          │
                               ▼
                         spark_parser
```

**Dependencies:**
- `spark_span` — For `Span`, `FileId` (token locations)
- `spark_intern` — For `Symbol` (interned identifiers/strings)
- `spark_diagnostics` — For reporting lexer errors

**Dependents:**
- `spark_parser` — Consumes tokens to build the AST

## Design Decisions

### Hand-Written Lexer

We use a hand-written lexer rather than a generator (like `logos`) because:
- Full control over error recovery and error messages
- Simpler to understand and debug
- Can handle context-sensitive tokenization if needed

### Greedy Tokenization

The lexer uses maximal munch (greedy matching). For example:
- `>>` is tokenized as `GtGt`, not two `Gt` tokens
- `...` is tokenized as `Ellipsis`, not `Dot Dot Dot`

### Keyword Recognition

Identifiers are first scanned as generic identifiers, then checked against the keyword table. This keeps the main lexer loop simple.

### String Interning

All identifiers and string literals are interned immediately during lexing. This means the parser and later phases work with `Symbol` values, not raw strings.

### Error Recovery

The lexer continues after errors, producing an `Error` token and advancing past the problematic character(s). This allows reporting multiple errors in one pass.

## Token Categories

### Keywords
```
fn, struct, enum, interface, type, alias, const, pub,
module, using, import, if, else, match, loop, break,
continue, return, each, via, to, and, or, not, is, as,
binds, extends, async, await, spawn, block, true, false, Null
```

### Literals
- Integers: `42`, `0xFF`, `0b1010`, `0o755`, `1_000_000`
- Floats: `3.14`, `1.5e10`
- Strings: `"hello"`, `"line\nbreak"`, `"interpolation {x}"`
- Characters: `'a'`, `'\n'`, `'\u{1F600}'`

### Operators & Punctuation
```
( ) { } [ ]
, : :: ; .
.. ..= -> =>
! ? ?? ?.
+ - * / %
+= -= *= /=
= == != < > <= >=
& | ^ ~ << >>
@
```

## Example

Input:
```spark
fn greet(name: String): Void {
    print("Hello, {name}!")
}
```

Output tokens:
```
Fn          "fn"         0..2
Identifier  "greet"      3..8
LParen      "("          8..9
Identifier  "name"       9..13
Colon       ":"          13..14
Identifier  "String"     15..21
RParen      ")"          21..22
Colon       ":"          22..23
Identifier  "Void"       24..28
LBrace      "{"          29..30
Identifier  "print"      35..40
LParen      "("          40..41
StringLiteral "Hello, {name}!" 41..57
RParen      ")"          57..58
RBrace      "}"          59..60
Eof         ""           60..60
```
