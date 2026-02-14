//! # `spark_intern`
//!
//! String interning for the Spark compiler.
//!
//! This crate provides efficient string storage where each unique string is
//! stored exactly once, and references to strings are represented as small
//! integer [`Symbol`]s.
//!
//! # Quick Start
//!
//! ```
//! use spark_intern::{sym, Symbol};
//!
//! // Intern a string
//! let hello: Symbol = sym("hello");
//!
//! // Same string gives same symbol
//! assert_eq!(hello, sym("hello"));
//!
//! // Get the string back
//! assert_eq!(hello.as_str(), "hello");
//! ```
//!
//! See the crate README for detailed documentation.

#![forbid(unsafe_code)]
#![allow(clippy::cast_possible_truncation)] // We intentionally use u32 for symbol IDs

use std::cell::RefCell;
use std::fmt;

use rustc_hash::FxHashMap;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

// ============================================================================
// Symbol
// ============================================================================

/// An interned string identifier.
///
/// A `Symbol` is a 32-bit handle that represents a string stored in an
/// [`Interner`]. Symbols are:
///
/// - **Cheap to copy**: Just 4 bytes, implements `Copy`
/// - **Cheap to compare**: Integer comparison instead of string comparison
/// - **Cheap to hash**: Just hashes the integer
///
/// # Example
///
/// ```
/// use spark_intern::sym;
///
/// let a = sym("hello");
/// let b = sym("hello");
/// let c = sym("world");
///
/// assert_eq!(a, b);     // Same string = same symbol
/// assert_ne!(a, c);     // Different string = different symbol
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Symbol(u32);

impl Symbol {
    /// Creates a new symbol from a raw index.
    ///
    /// This is primarily for internal use or deserialization.
    /// Prefer using [`sym`] to intern strings.
    #[must_use]
    pub const fn from_raw(index: u32) -> Self {
        Self(index)
    }

    /// Returns the raw index of this symbol.
    ///
    /// This can be used for serialization or as a unique identifier.
    #[must_use]
    pub const fn as_raw(self) -> u32 {
        self.0
    }

    /// Returns the string this symbol represents.
    ///
    /// This looks up the symbol in the thread-local interner.
    ///
    /// # Panics
    ///
    /// Panics if the symbol was created in a different thread's interner.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        INTERNER.with(|interner| {
            let interner = interner.borrow();
            interner.resolve(self)
        })
    }

    /// A dummy symbol for use in tests or error recovery.
    ///
    /// This symbol resolves to `"<invalid>"`.
    pub const DUMMY: Self = Self(0);
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Symbol({}: {:?})", self.0, self.as_str())
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ============================================================================
// Interner
// ============================================================================

/// A string interner that stores unique strings and returns [`Symbol`] handles.
///
/// The interner ensures that each unique string is stored exactly once. Looking
/// up or interning the same string always returns the same [`Symbol`].
///
/// # Example
///
/// ```
/// use spark_intern::Interner;
///
/// let mut interner = Interner::new();
///
/// let hello = interner.intern("hello");
/// let world = interner.intern("world");
/// let hello2 = interner.intern("hello");
///
/// assert_eq!(hello, hello2);  // Same string = same symbol
/// assert_ne!(hello, world);   // Different strings = different symbols
/// ```
#[derive(Debug)]
pub struct Interner {
    /// Map from string to symbol for O(1) lookup during interning.
    map: FxHashMap<&'static str, Symbol>,

    /// Storage for interned strings. The index is the symbol's raw value.
    /// Strings are leaked to get 'static lifetime.
    strings: Vec<&'static str>,
}

impl Default for Interner {
    fn default() -> Self {
        Self::new()
    }
}

impl Interner {
    /// Creates a new interner with pre-interned keywords.
    #[must_use]
    pub fn new() -> Self {
        let mut interner = Self {
            map: FxHashMap::default(),
            strings: Vec::with_capacity(256),
        };

        // Pre-intern the dummy/invalid symbol at index 0
        interner.intern_static("<invalid>");

        // Pre-intern all keywords
        interner.intern_keywords();

        interner
    }

    /// Creates an empty interner without pre-interned keywords.
    ///
    /// Useful for testing or when you want full control over the symbol space.
    #[must_use]
    pub fn empty() -> Self {
        let mut interner = Self {
            map: FxHashMap::default(),
            strings: Vec::new(),
        };

        // Still need the dummy symbol at index 0
        interner.intern_static("<invalid>");

        interner
    }

    /// Interns a string, returning its symbol.
    ///
    /// If the string has already been interned, returns the existing symbol.
    /// Otherwise, stores the string and returns a new symbol.
    pub fn intern(&mut self, s: &str) -> Symbol {
        if let Some(&sym) = self.map.get(s) {
            return sym;
        }

        // Leak the string to get 'static lifetime
        // This is intentional - interned strings live for the compiler's lifetime
        let s: &'static str = Box::leak(s.to_owned().into_boxed_str());
        self.intern_static(s)
    }

    /// Interns a static string without copying.
    ///
    /// This is more efficient for string literals that already have
    /// `'static` lifetime.
    pub fn intern_static(&mut self, s: &'static str) -> Symbol {
        if let Some(&sym) = self.map.get(s) {
            return sym;
        }

        let sym = Symbol(self.strings.len() as u32);
        self.strings.push(s);
        self.map.insert(s, sym);
        sym
    }

    /// Resolves a symbol to its string.
    ///
    /// # Panics
    ///
    /// Panics if the symbol is invalid (not from this interner).
    #[must_use]
    pub fn resolve(&self, sym: Symbol) -> &'static str {
        self.strings
            .get(sym.0 as usize)
            .copied()
            .expect("invalid symbol")
    }

    /// Tries to resolve a symbol to its string.
    ///
    /// Returns `None` if the symbol is invalid.
    #[must_use]
    pub fn try_resolve(&self, sym: Symbol) -> Option<&'static str> {
        self.strings.get(sym.0 as usize).copied()
    }

    /// Returns the number of interned strings.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn len(&self) -> usize {
        self.strings.len()
    }

    /// Returns true if no strings have been interned.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }

    /// Pre-interns all language keywords.
    fn intern_keywords(&mut self) {
        // Keywords (must match the order in kw module)
        let keywords = [
            "fn",
            "struct",
            "enum",
            "interface",
            "type",
            "alias",
            "const",
            "pub",
            "module",
            "using",
            "import",
            "if",
            "else",
            "match",
            "loop",
            "break",
            "continue",
            "return",
            "each",
            "via",
            "to",
            "and",
            "or",
            "not",
            "is",
            "as",
            "binds",
            "extends",
            "async",
            "await",
            "spawn",
            "block",
            "true",
            "false",
            "Null",
            "Void",
            "Never",
            // Primitive types
            "Int",
            "Int8",
            "Int16",
            "Int32",
            "Int64",
            "UInt",
            "UInt8",
            "UInt16",
            "UInt32",
            "UInt64",
            "Float",
            "Float32",
            "Float64",
            "Bool",
            "Char",
            "String",
            "Byte",
            // Collection types
            "List",
            "Map",
            "Set",
            "Tup",
            "Ref",
            "Weak",
            // Concurrency
            "Channel",
            "Mutex",
            "Handle",
            "ThreadPool",
            // Common identifiers
            "self",
            "Self",
            "_",
            // Built-in functions
            "print",
            // Built-in method names
            "len",
            "append",
            "contains",
            "remove",
            "add",
            // Pipeline operations
            "sort",
            "take",
            "reverse",
            // Concurrency operations
            "parallel",
            // Internal runtime stubs
            "__parallel_foreach",
            "__parallel_map",
        ];

        for kw in keywords {
            self.intern_static(kw);
        }
    }
}

// ============================================================================
// Thread-Local Global Interner
// ============================================================================

thread_local! {
    /// The thread-local global interner.
    static INTERNER: RefCell<Interner> = RefCell::new(Interner::new());
}

/// Interns a string using the thread-local global interner.
///
/// This is the primary API for interning strings. It's equivalent to:
/// ```ignore
/// INTERNER.with(|i| i.borrow_mut().intern(s))
/// ```
///
/// # Example
///
/// ```
/// use spark_intern::sym;
///
/// let hello = sym("hello");
/// let world = sym("world");
///
/// assert_eq!(hello, sym("hello"));
/// assert_ne!(hello, world);
/// ```
#[must_use]
pub fn sym(s: &str) -> Symbol {
    INTERNER.with(|interner| interner.borrow_mut().intern(s))
}

/// Interns a static string using the thread-local global interner.
///
/// More efficient than [`sym`] for string literals.
#[must_use]
pub fn sym_static(s: &'static str) -> Symbol {
    INTERNER.with(|interner| interner.borrow_mut().intern_static(s))
}

/// Provides access to the thread-local interner for batch operations.
///
/// # Example
///
/// ```
/// use spark_intern::with_interner;
///
/// with_interner(|interner| {
///     let a = interner.intern("a");
///     let b = interner.intern("b");
///     // ... batch operations
/// });
/// ```
pub fn with_interner<F, R>(f: F) -> R
where
    F: FnOnce(&mut Interner) -> R,
{
    INTERNER.with(|interner| f(&mut interner.borrow_mut()))
}

// ============================================================================
// Pre-interned Keywords
// ============================================================================

/// Pre-interned keyword symbols.
///
/// These symbols are created when the interner is initialized and can be
/// used for efficient comparison without interning at runtime.
///
/// # Example
///
/// ```
/// use spark_intern::{sym, kw};
///
/// let token_text = "fn";
/// let symbol = sym(token_text);
///
/// if symbol == kw::FN {
///     println!("It's a function keyword!");
/// }
/// ```
pub mod kw {
    use super::Symbol;

    // The order must match Interner::intern_keywords()
    // Index 0 is reserved for "<invalid>"

    /// `fn`
    pub const FN: Symbol = Symbol(1);
    /// `struct`
    pub const STRUCT: Symbol = Symbol(2);
    /// `enum`
    pub const ENUM: Symbol = Symbol(3);
    /// `interface`
    pub const INTERFACE: Symbol = Symbol(4);
    /// `type`
    pub const TYPE: Symbol = Symbol(5);
    /// `alias`
    pub const ALIAS: Symbol = Symbol(6);
    /// `const`
    pub const CONST: Symbol = Symbol(7);
    /// `pub`
    pub const PUB: Symbol = Symbol(8);
    /// `module`
    pub const MODULE: Symbol = Symbol(9);
    /// `using`
    pub const USING: Symbol = Symbol(10);
    /// `import`
    pub const IMPORT: Symbol = Symbol(11);
    /// `if`
    pub const IF: Symbol = Symbol(12);
    /// `else`
    pub const ELSE: Symbol = Symbol(13);
    /// `match`
    pub const MATCH: Symbol = Symbol(14);
    /// `loop`
    pub const LOOP: Symbol = Symbol(15);
    /// `break`
    pub const BREAK: Symbol = Symbol(16);
    /// `continue`
    pub const CONTINUE: Symbol = Symbol(17);
    /// `return`
    pub const RETURN: Symbol = Symbol(18);
    /// `each`
    pub const EACH: Symbol = Symbol(19);
    /// `via`
    pub const VIA: Symbol = Symbol(20);
    /// `to`
    pub const TO: Symbol = Symbol(21);
    /// `and`
    pub const AND: Symbol = Symbol(22);
    /// `or`
    pub const OR: Symbol = Symbol(23);
    /// `not`
    pub const NOT: Symbol = Symbol(24);
    /// `is`
    pub const IS: Symbol = Symbol(25);
    /// `as`
    pub const AS: Symbol = Symbol(26);
    /// `binds`
    pub const BINDS: Symbol = Symbol(27);
    /// `extends`
    pub const EXTENDS: Symbol = Symbol(28);
    /// `async`
    pub const ASYNC: Symbol = Symbol(29);
    /// `await`
    pub const AWAIT: Symbol = Symbol(30);
    /// `spawn`
    pub const SPAWN: Symbol = Symbol(31);
    /// `block`
    pub const BLOCK: Symbol = Symbol(32);
    /// `true`
    pub const TRUE: Symbol = Symbol(33);
    /// `false`
    pub const FALSE: Symbol = Symbol(34);
    /// `Null`
    pub const NULL: Symbol = Symbol(35);
    /// `Void`
    pub const VOID: Symbol = Symbol(36);
    /// `Never`
    pub const NEVER: Symbol = Symbol(37);

    // Primitive types
    /// `Int`
    pub const INT: Symbol = Symbol(38);
    /// `Int8`
    pub const INT8: Symbol = Symbol(39);
    /// `Int16`
    pub const INT16: Symbol = Symbol(40);
    /// `Int32`
    pub const INT32: Symbol = Symbol(41);
    /// `Int64`
    pub const INT64: Symbol = Symbol(42);
    /// `UInt`
    pub const UINT: Symbol = Symbol(43);
    /// `UInt8`
    pub const UINT8: Symbol = Symbol(44);
    /// `UInt16`
    pub const UINT16: Symbol = Symbol(45);
    /// `UInt32`
    pub const UINT32: Symbol = Symbol(46);
    /// `UInt64`
    pub const UINT64: Symbol = Symbol(47);
    /// `Float`
    pub const FLOAT: Symbol = Symbol(48);
    /// `Float32`
    pub const FLOAT32: Symbol = Symbol(49);
    /// `Float64`
    pub const FLOAT64: Symbol = Symbol(50);
    /// `Bool`
    pub const BOOL: Symbol = Symbol(51);
    /// `Char`
    pub const CHAR: Symbol = Symbol(52);
    /// `String`
    pub const STRING: Symbol = Symbol(53);
    /// `Byte`
    pub const BYTE: Symbol = Symbol(54);

    // Collection types
    /// `List`
    pub const LIST: Symbol = Symbol(55);
    /// `Map`
    pub const MAP: Symbol = Symbol(56);
    /// `Set`
    pub const SET: Symbol = Symbol(57);
    /// `Tup`
    pub const TUP: Symbol = Symbol(58);
    /// `Ref`
    pub const REF: Symbol = Symbol(59);
    /// `Weak`
    pub const WEAK: Symbol = Symbol(60);

    // Concurrency
    /// `Channel`
    pub const CHANNEL: Symbol = Symbol(61);
    /// `Mutex`
    pub const MUTEX: Symbol = Symbol(62);
    /// `Handle`
    pub const HANDLE: Symbol = Symbol(63);
    /// `ThreadPool`
    pub const THREAD_POOL: Symbol = Symbol(64);

    // Common identifiers
    /// `self`
    pub const SELF_LOWER: Symbol = Symbol(65);
    /// `Self`
    pub const SELF_UPPER: Symbol = Symbol(66);
    /// `_` (underscore/wildcard)
    pub const UNDERSCORE: Symbol = Symbol(67);

    // Built-in functions
    /// `print`
    pub const PRINT: Symbol = Symbol(68);

    // Built-in method names
    /// `len`
    pub const LEN: Symbol = Symbol(69);
    /// `append`
    pub const APPEND: Symbol = Symbol(70);
    /// `contains`
    pub const CONTAINS: Symbol = Symbol(71);
    /// `remove`
    pub const REMOVE: Symbol = Symbol(72);
    /// `add`
    pub const ADD: Symbol = Symbol(73);
    /// `sort`
    pub const SORT: Symbol = Symbol(74);
    /// `take`
    pub const TAKE: Symbol = Symbol(75);
    /// `reverse`
    pub const REVERSE: Symbol = Symbol(76);
    /// `parallel`
    pub const PARALLEL: Symbol = Symbol(77);
    /// `__parallel_foreach` (internal runtime stub)
    pub const PARALLEL_FOREACH: Symbol = Symbol(78);
    /// `__parallel_map` (internal runtime stub)
    pub const PARALLEL_MAP: Symbol = Symbol(79);

    /// Returns true if the given symbol is a built-in function name.
    #[must_use]
    pub const fn is_builtin_function(sym: Symbol) -> bool {
        sym.0 == 68 || (sym.0 >= 74 && sym.0 <= 76)
    }

    /// Returns true if the given symbol is a built-in method name.
    #[must_use]
    pub const fn is_builtin_method(sym: Symbol) -> bool {
        sym.0 >= 69 && sym.0 <= 73
    }

    /// Returns true if the given symbol is a keyword.
    #[must_use]
    pub const fn is_keyword(sym: Symbol) -> bool {
        // Keywords are symbols 1-37 (fn through Never) plus parallel (77)
        (sym.0 >= 1 && sym.0 <= 37) || sym.0 == 77
    }

    /// Returns true if the given symbol is a primitive type name.
    #[must_use]
    pub const fn is_primitive_type(sym: Symbol) -> bool {
        // Primitive types are symbols 38-54 (Int through Byte)
        sym.0 >= 38 && sym.0 <= 54
    }

    /// Returns true if the given symbol is a built-in type name.
    #[must_use]
    pub const fn is_builtin_type(sym: Symbol) -> bool {
        // Built-in types are symbols 35-64 (Null through ThreadPool)
        sym.0 >= 35 && sym.0 <= 64
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_basics() {
        let mut interner = Interner::empty();

        let hello = interner.intern("hello");
        let world = interner.intern("world");
        let hello2 = interner.intern("hello");

        assert_eq!(hello, hello2);
        assert_ne!(hello, world);
    }

    #[test]
    fn resolve_basics() {
        let mut interner = Interner::empty();

        let sym = interner.intern("test");
        assert_eq!(interner.resolve(sym), "test");
    }

    #[test]
    fn global_interner() {
        let a = sym("global_test");
        let b = sym("global_test");
        let c = sym("different");

        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn symbol_display() {
        let s = sym("display_test");
        assert_eq!(format!("{s}"), "display_test");
    }

    #[test]
    fn keywords_are_preinterned() {
        // Keywords should already be in the interner
        assert_eq!(kw::FN.as_str(), "fn");
        assert_eq!(kw::STRUCT.as_str(), "struct");
        assert_eq!(kw::IF.as_str(), "if");
        assert_eq!(kw::ELSE.as_str(), "else");
        assert_eq!(kw::RETURN.as_str(), "return");
    }

    #[test]
    fn keyword_comparison() {
        let fn_sym = sym("fn");
        assert_eq!(fn_sym, kw::FN);

        let not_fn = sym("notfn");
        assert_ne!(not_fn, kw::FN);
    }

    #[test]
    fn is_keyword() {
        assert!(kw::is_keyword(kw::FN));
        assert!(kw::is_keyword(kw::IF));
        assert!(kw::is_keyword(kw::RETURN));
        assert!(!kw::is_keyword(sym("hello")));
    }

    #[test]
    fn is_primitive_type() {
        assert!(kw::is_primitive_type(kw::INT));
        assert!(kw::is_primitive_type(kw::STRING));
        assert!(kw::is_primitive_type(kw::BOOL));
        assert!(!kw::is_primitive_type(kw::FN));
    }

    #[test]
    fn intern_static() {
        let mut interner = Interner::empty();

        let static_sym = interner.intern_static("static_string");
        let dynamic_sym = interner.intern("static_string");

        assert_eq!(static_sym, dynamic_sym);
    }

    #[test]
    fn with_interner_batch() {
        with_interner(|interner| {
            let a = interner.intern("batch_a");
            let b = interner.intern("batch_b");
            assert_ne!(a, b);
        });
    }

    #[test]
    fn symbol_dummy() {
        assert_eq!(Symbol::DUMMY.as_str(), "<invalid>");
    }

    #[test]
    fn interner_len() {
        let mut interner = Interner::empty();
        assert_eq!(interner.len(), 1); // Just the dummy

        interner.intern("one");
        assert_eq!(interner.len(), 2);

        interner.intern("two");
        assert_eq!(interner.len(), 3);

        interner.intern("one"); // Already interned
        assert_eq!(interner.len(), 3);
    }

    #[test]
    fn primitive_types_correct() {
        assert_eq!(kw::INT.as_str(), "Int");
        assert_eq!(kw::INT64.as_str(), "Int64");
        assert_eq!(kw::FLOAT.as_str(), "Float");
        assert_eq!(kw::STRING.as_str(), "String");
        assert_eq!(kw::BOOL.as_str(), "Bool");
    }

    #[test]
    fn collection_types_correct() {
        assert_eq!(kw::LIST.as_str(), "List");
        assert_eq!(kw::MAP.as_str(), "Map");
        assert_eq!(kw::SET.as_str(), "Set");
        assert_eq!(kw::TUP.as_str(), "Tup");
        assert_eq!(kw::REF.as_str(), "Ref");
    }
}
