# spark_intern

Intern names and string values as compact `Symbol` handles. The default interner is thread-local, so keep symbol identity within the interner that produced it. Dynamically allocated strings live for the process so resolution can return `&'static str`. Use the pre-interned constants for known compiler names.

See [Symbol, Interner, and the pre-interned names](src/lib.rs). The [lexer token definitions](../spark_lexer/src/token.rs) determine which spellings are language keywords.
