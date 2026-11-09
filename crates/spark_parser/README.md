# spark_parser

Parse tokens into an AST and collect syntax diagnostics. [The parser entry point](src/lib.rs) includes a Rust example and describes recovery. [Expression parsing](src/expr.rs) owns operator binding powers and uses a separate entry point where a following brace begins a body.

See the [language reference](../../docs/lang-spec.md) for supported Spark forms.
