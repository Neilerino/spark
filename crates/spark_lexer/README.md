# spark_lexer

The lexer turns source text into tokens and records diagnostics. [Token definitions](src/token.rs) own the keyword spellings. The [lexer implementation](src/lexer.rs) owns scanning and recovery. The [parser](../spark_parser/README.md) consumes these tokens.
