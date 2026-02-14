//! `spark.json` runtime implementations.
//!
//! Provides `extern "C"` functions for the `spark.json` standard library module.
//! Includes a hand-written recursive descent JSON parser, a stringifier, and a
//! thread-local node arena for storing parsed/constructed JSON values.
//!
//! ## Design
//!
//! JSON values are stored in a thread-local arena (`Vec<JsonNode>`). Each node
//! gets an integer handle (its index). Spark code sees these as opaque `Int`
//! values. This avoids complex recursive enum FFI.
//!
//! Handles are `i64` at the FFI boundary: non-negative for valid nodes, `-1`
//! for errors. The last parse error message is stored in a thread-local string.

use std::cell::RefCell;

use crate::{__spark_string_from_literal, SparkString};

// =============================================================================
// Node arena
// =============================================================================

/// A JSON value stored in the arena.
enum JsonNode {
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Array(Vec<usize>),
    /// Ordered key-value pairs.
    Object(Vec<(String, usize)>),
}

thread_local! {
    static ARENA: RefCell<Vec<JsonNode>> = RefCell::new(Vec::new());
    static LAST_ERROR: RefCell<String> = RefCell::new(String::new());
}

/// Allocates a node in the arena and returns its handle.
fn alloc_node(node: JsonNode) -> usize {
    ARENA.with(|arena| {
        let mut arena = arena.borrow_mut();
        let handle = arena.len();
        arena.push(node);
        handle
    })
}

/// Sets the last error message.
fn set_error(msg: String) {
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = msg;
    });
}

// =============================================================================
// JSON parser
// =============================================================================

/// Recursive descent JSON parser.
struct JsonParser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    fn parse(&mut self) -> Result<usize, String> {
        self.skip_whitespace();
        let handle = self.parse_value()?;
        self.skip_whitespace();
        if self.pos < self.input.len() {
            return Err(format!(
                "unexpected trailing characters at position {}",
                self.pos
            ));
        }
        Ok(handle)
    }

    fn parse_value(&mut self) -> Result<usize, String> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'"') => self.parse_string_node(),
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b't') => self.parse_true(),
            Some(b'f') => self.parse_false(),
            Some(b'n') => self.parse_null(),
            Some(c) if *c == b'-' || c.is_ascii_digit() => self.parse_number(),
            Some(c) => Err(format!(
                "unexpected character '{}' at position {}",
                *c as char, self.pos
            )),
            None => Err("unexpected end of input".to_string()),
        }
    }

    fn parse_string_node(&mut self) -> Result<usize, String> {
        let s = self.parse_string_value()?;
        Ok(alloc_node(JsonNode::Str(s)))
    }

    fn parse_string_value(&mut self) -> Result<String, String> {
        self.expect_byte(b'"')?;
        let mut result = String::new();
        loop {
            match self.next() {
                Some(b'"') => return Ok(result),
                Some(b'\\') => {
                    let escaped = self.parse_escape()?;
                    result.push(escaped);
                }
                Some(c) if c < 0x20 => {
                    return Err(format!(
                        "unescaped control character 0x{c:02x} at position {}",
                        self.pos - 1
                    ));
                }
                Some(c) => {
                    // Handle multi-byte UTF-8
                    let start = self.pos - 1;
                    let width = utf8_char_width(c);
                    if width == 1 {
                        result.push(c as char);
                    } else {
                        // Need `width - 1` more bytes
                        if self.pos + width - 1 > self.input.len() {
                            return Err(format!(
                                "incomplete UTF-8 sequence at position {start}"
                            ));
                        }
                        let bytes = &self.input[start..start + width];
                        match std::str::from_utf8(bytes) {
                            Ok(s) => {
                                result.push_str(s);
                                self.pos = start + width;
                            }
                            Err(_) => {
                                return Err(format!(
                                    "invalid UTF-8 sequence at position {start}"
                                ));
                            }
                        }
                    }
                }
                None => return Err("unterminated string".to_string()),
            }
        }
    }

    fn parse_escape(&mut self) -> Result<char, String> {
        match self.next() {
            Some(b'"') => Ok('"'),
            Some(b'\\') => Ok('\\'),
            Some(b'/') => Ok('/'),
            Some(b'b') => Ok('\u{0008}'),
            Some(b'f') => Ok('\u{000C}'),
            Some(b'n') => Ok('\n'),
            Some(b'r') => Ok('\r'),
            Some(b't') => Ok('\t'),
            Some(b'u') => self.parse_unicode_escape(),
            Some(c) => Err(format!(
                "invalid escape character '{}' at position {}",
                c as char,
                self.pos - 1
            )),
            None => Err("unterminated escape sequence".to_string()),
        }
    }

    fn parse_unicode_escape(&mut self) -> Result<char, String> {
        let hex = self.read_hex4()?;
        // Check for surrogate pair
        if (0xD800..=0xDBFF).contains(&hex) {
            // High surrogate — expect \uDC00-\uDFFF
            self.expect_byte(b'\\')?;
            self.expect_byte(b'u')?;
            let low = self.read_hex4()?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(format!(
                    "invalid low surrogate \\u{low:04X} at position {}",
                    self.pos - 4
                ));
            }
            let code_point = 0x10000 + ((hex - 0xD800) << 10) + (low - 0xDC00);
            char::from_u32(code_point).ok_or_else(|| {
                format!("invalid Unicode code point U+{code_point:04X}")
            })
        } else if (0xDC00..=0xDFFF).contains(&hex) {
            Err(format!(
                "unexpected low surrogate \\u{hex:04X} at position {}",
                self.pos - 4
            ))
        } else {
            char::from_u32(hex).ok_or_else(|| {
                format!("invalid Unicode code point U+{hex:04X}")
            })
        }
    }

    fn read_hex4(&mut self) -> Result<u32, String> {
        let mut value: u32 = 0;
        for _ in 0..4 {
            let b = self
                .next()
                .ok_or_else(|| "unterminated \\u escape".to_string())?;
            let digit = match b {
                b'0'..=b'9' => u32::from(b - b'0'),
                b'a'..=b'f' => u32::from(b - b'a') + 10,
                b'A'..=b'F' => u32::from(b - b'A') + 10,
                _ => {
                    return Err(format!(
                        "invalid hex digit '{}' at position {}",
                        b as char,
                        self.pos - 1
                    ));
                }
            };
            value = (value << 4) | digit;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<usize, String> {
        let start = self.pos;

        // Optional minus
        if self.peek() == Some(&b'-') {
            self.pos += 1;
        }

        // Integer part
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
            }
            Some(c) if c.is_ascii_digit() => {
                self.skip_digits();
            }
            _ => {
                return Err(format!(
                    "expected digit at position {}",
                    self.pos
                ));
            }
        }

        // Fractional part
        if self.peek() == Some(&b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(format!(
                    "expected digit after '.' at position {}",
                    self.pos
                ));
            }
            self.skip_digits();
        }

        // Exponent
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                return Err(format!(
                    "expected digit in exponent at position {}",
                    self.pos
                ));
            }
            self.skip_digits();
        }

        let num_str = std::str::from_utf8(&self.input[start..self.pos])
            .map_err(|_| format!("invalid number at position {start}"))?;

        let value: f64 = num_str
            .parse()
            .map_err(|_| format!("invalid number '{num_str}' at position {start}"))?;

        Ok(alloc_node(JsonNode::Number(value)))
    }

    fn parse_true(&mut self) -> Result<usize, String> {
        self.expect_literal(b"true")?;
        Ok(alloc_node(JsonNode::Bool(true)))
    }

    fn parse_false(&mut self) -> Result<usize, String> {
        self.expect_literal(b"false")?;
        Ok(alloc_node(JsonNode::Bool(false)))
    }

    fn parse_null(&mut self) -> Result<usize, String> {
        self.expect_literal(b"null")?;
        Ok(alloc_node(JsonNode::Null))
    }

    fn parse_array(&mut self) -> Result<usize, String> {
        self.expect_byte(b'[')?;
        self.skip_whitespace();

        let mut elements = Vec::new();

        if self.peek() == Some(&b']') {
            self.pos += 1;
            return Ok(alloc_node(JsonNode::Array(elements)));
        }

        loop {
            let handle = self.parse_value()?;
            elements.push(handle);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(alloc_node(JsonNode::Array(elements)));
                }
                _ => {
                    return Err(format!(
                        "expected ',' or ']' at position {}",
                        self.pos
                    ));
                }
            }
        }
    }

    fn parse_object(&mut self) -> Result<usize, String> {
        self.expect_byte(b'{')?;
        self.skip_whitespace();

        let mut entries = Vec::new();

        if self.peek() == Some(&b'}') {
            self.pos += 1;
            return Ok(alloc_node(JsonNode::Object(entries)));
        }

        loop {
            self.skip_whitespace();
            let key = self.parse_string_value()?;
            self.skip_whitespace();
            self.expect_byte(b':')?;
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(alloc_node(JsonNode::Object(entries)));
                }
                _ => {
                    return Err(format!(
                        "expected ',' or '}}' at position {}",
                        self.pos
                    ));
                }
            }
        }
    }

    // --- Utility methods ---

    fn peek(&self) -> Option<&u8> {
        self.input.get(self.pos)
    }

    fn next(&mut self) -> Option<u8> {
        if self.pos < self.input.len() {
            let b = self.input[self.pos];
            self.pos += 1;
            Some(b)
        } else {
            None
        }
    }

    fn skip_whitespace(&mut self) {
        while let Some(&b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn skip_digits(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
    }

    fn expect_byte(&mut self, expected: u8) -> Result<(), String> {
        match self.next() {
            Some(b) if b == expected => Ok(()),
            Some(b) => Err(format!(
                "expected '{}' but found '{}' at position {}",
                expected as char,
                b as char,
                self.pos - 1
            )),
            None => Err(format!(
                "expected '{}' but found end of input",
                expected as char
            )),
        }
    }

    fn expect_literal(&mut self, literal: &[u8]) -> Result<(), String> {
        let start = self.pos;
        for &expected in literal {
            match self.next() {
                Some(b) if b == expected => {}
                _ => {
                    return Err(format!(
                        "expected '{}' at position {start}",
                        std::str::from_utf8(literal).unwrap_or("?")
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Returns the expected byte width of a UTF-8 character from its leading byte.
fn utf8_char_width(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead < 0xE0 {
        2
    } else if lead < 0xF0 {
        3
    } else {
        4
    }
}

// =============================================================================
// JSON stringifier
// =============================================================================

/// Converts an arena node to a JSON string.
fn stringify_node(handle: usize, pretty: bool, indent: usize, depth: usize) -> String {
    ARENA.with(|arena| {
        let arena = arena.borrow();
        let node = match arena.get(handle) {
            Some(n) => n,
            None => return "null".to_string(),
        };
        match node {
            JsonNode::Null => "null".to_string(),
            JsonNode::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            JsonNode::Number(n) => format_number(*n),
            JsonNode::Str(s) => escape_json_string(s),
            JsonNode::Array(elements) => {
                let handles: Vec<usize> = elements.clone();
                drop(arena); // Release borrow before recursing
                stringify_array(&handles, pretty, indent, depth)
            }
            JsonNode::Object(entries) => {
                let entries: Vec<(String, usize)> = entries.clone();
                drop(arena); // Release borrow before recursing
                stringify_object(&entries, pretty, indent, depth)
            }
        }
    })
}

fn stringify_array(handles: &[usize], pretty: bool, indent: usize, depth: usize) -> String {
    if handles.is_empty() {
        return "[]".to_string();
    }
    let mut out = String::from('[');
    if pretty {
        out.push('\n');
    }
    for (i, &h) in handles.iter().enumerate() {
        if pretty {
            push_indent(&mut out, indent, depth + 1);
        }
        out.push_str(&stringify_node(h, pretty, indent, depth + 1));
        if i + 1 < handles.len() {
            out.push(',');
        }
        if pretty {
            out.push('\n');
        }
    }
    if pretty {
        push_indent(&mut out, indent, depth);
    }
    out.push(']');
    out
}

fn stringify_object(
    entries: &[(String, usize)],
    pretty: bool,
    indent: usize,
    depth: usize,
) -> String {
    if entries.is_empty() {
        return "{}".to_string();
    }
    let mut out = String::from('{');
    if pretty {
        out.push('\n');
    }
    for (i, (key, handle)) in entries.iter().enumerate() {
        if pretty {
            push_indent(&mut out, indent, depth + 1);
        }
        out.push_str(&escape_json_string(key));
        out.push(':');
        if pretty {
            out.push(' ');
        }
        out.push_str(&stringify_node(*handle, pretty, indent, depth + 1));
        if i + 1 < entries.len() {
            out.push(',');
        }
        if pretty {
            out.push('\n');
        }
    }
    if pretty {
        push_indent(&mut out, indent, depth);
    }
    out.push('}');
    out
}

fn push_indent(out: &mut String, indent: usize, depth: usize) {
    for _ in 0..indent * depth {
        out.push(' ');
    }
}

/// Formats an f64 as a JSON number.
///
/// If the number has no fractional part and fits in an i64, it is formatted as
/// an integer. Otherwise it uses Rust's default float formatting.
fn format_number(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < (i64::MAX as f64) {
        #[allow(clippy::cast_possible_truncation)]
        let i = n as i64;
        i.to_string()
    } else if n.is_nan() {
        // JSON doesn't support NaN; output null
        "null".to_string()
    } else if n.is_infinite() {
        // JSON doesn't support Infinity; output null
        "null".to_string()
    } else {
        // Use Rust's default formatting which gives enough precision
        let s = format!("{n}");
        // Ensure it has a decimal point for floating-point appearance
        if s.contains('.') || s.contains('e') || s.contains('E') {
            s
        } else {
            format!("{s}.0")
        }
    }
}

/// Escapes a string for JSON output (wraps in quotes, handles special chars).
fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if c < '\u{0020}' => {
                // Other control characters as \u00XX
                let code = c as u32;
                out.push_str(&format!("\\u{code:04x}"));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// =============================================================================
// Helper: read a SparkString pointer into a Rust &str (borrowed)
// =============================================================================

/// Reads a SparkString pointer, returning the content as a Rust String.
///
/// # Safety
///
/// `ptr` must be a valid pointer to a `SparkString`.
#[allow(clippy::cast_ptr_alignment)]
unsafe fn read_spark_str(ptr: *mut u8) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let s = unsafe { &*(ptr.cast::<SparkString>()) };
    if s.len == 0 {
        return String::new();
    }
    let slice = unsafe { std::slice::from_raw_parts(s.data, s.len) };
    std::str::from_utf8(slice)
        .unwrap_or("<invalid utf-8>")
        .to_owned()
}

/// Creates a new SparkString from a Rust str.
fn rust_str_to_spark(s: &str) -> *mut u8 {
    let bytes = s.as_bytes();
    unsafe { __spark_string_from_literal(bytes.as_ptr(), bytes.len() as i64) }
}

// =============================================================================
// Type discriminant constants
// =============================================================================

const JSON_TYPE_NULL: i64 = 0;
const JSON_TYPE_BOOL: i64 = 1;
const JSON_TYPE_NUMBER: i64 = 2;
const JSON_TYPE_STRING: i64 = 3;
const JSON_TYPE_ARRAY: i64 = 4;
const JSON_TYPE_OBJECT: i64 = 5;

// =============================================================================
// FFI functions
// =============================================================================

// --- Parsing ---

/// Parses a JSON string. Returns a handle to the root node, or -1 on error.
///
/// On error, the error message can be retrieved via `__spark_json_error`.
///
/// # Safety
///
/// `text` must be a valid `SparkString` pointer.
#[no_mangle]
#[allow(clippy::cast_possible_wrap)]
pub unsafe extern "C" fn __spark_json_parse(text: *mut u8) -> i64 {
    let input = unsafe { read_spark_str(text) };
    let mut parser = JsonParser::new(input.as_bytes());
    match parser.parse() {
        Ok(handle) => handle as i64,
        Err(msg) => {
            set_error(msg);
            -1
        }
    }
}

/// Returns the last parse error message as a new SparkString.
#[no_mangle]
pub extern "C" fn __spark_json_error() -> *mut u8 {
    LAST_ERROR.with(|e| {
        let msg = e.borrow();
        rust_str_to_spark(&msg)
    })
}

// --- Type checking ---

/// Returns the type discriminant of a node: 0=null, 1=bool, 2=number,
/// 3=string, 4=array, 5=object. Returns -1 for invalid handles.
#[no_mangle]
#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
pub extern "C" fn __spark_json_type(handle: i64) -> i64 {
    if handle < 0 {
        return -1;
    }
    ARENA.with(|arena| {
        let arena = arena.borrow();
        match arena.get(handle as usize) {
            Some(JsonNode::Null) => JSON_TYPE_NULL,
            Some(JsonNode::Bool(_)) => JSON_TYPE_BOOL,
            Some(JsonNode::Number(_)) => JSON_TYPE_NUMBER,
            Some(JsonNode::Str(_)) => JSON_TYPE_STRING,
            Some(JsonNode::Array(_)) => JSON_TYPE_ARRAY,
            Some(JsonNode::Object(_)) => JSON_TYPE_OBJECT,
            None => -1,
        }
    })
}

// --- Value access ---

/// Returns the boolean value of a node (0 = false, 1 = true).
/// Returns 0 if the node is not a boolean.
#[no_mangle]
#[allow(clippy::cast_sign_loss)]
pub extern "C" fn __spark_json_bool(handle: i64) -> i8 {
    if handle < 0 {
        return 0;
    }
    ARENA.with(|arena| {
        let arena = arena.borrow();
        match arena.get(handle as usize) {
            Some(JsonNode::Bool(b)) => i8::from(*b),
            _ => 0,
        }
    })
}

/// Returns the number value of a node. Returns 0.0 if not a number.
#[no_mangle]
#[allow(clippy::cast_sign_loss)]
pub extern "C" fn __spark_json_number(handle: i64) -> f64 {
    if handle < 0 {
        return 0.0;
    }
    ARENA.with(|arena| {
        let arena = arena.borrow();
        match arena.get(handle as usize) {
            Some(JsonNode::Number(n)) => *n,
            _ => 0.0,
        }
    })
}

/// Returns the string value of a node as a new SparkString.
/// Returns an empty string if the node is not a string.
///
/// # Safety
///
/// The returned pointer is a freshly allocated SparkString (rc = 1).
#[no_mangle]
#[allow(clippy::cast_sign_loss)]
pub extern "C" fn __spark_json_string(handle: i64) -> *mut u8 {
    if handle < 0 {
        return rust_str_to_spark("");
    }
    ARENA.with(|arena| {
        let arena = arena.borrow();
        match arena.get(handle as usize) {
            Some(JsonNode::Str(s)) => rust_str_to_spark(s),
            _ => rust_str_to_spark(""),
        }
    })
}

// --- Array access ---

/// Returns the number of elements in a JSON array. Returns -1 if not an array.
#[no_mangle]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
pub extern "C" fn __spark_json_array_len(handle: i64) -> i64 {
    if handle < 0 {
        return -1;
    }
    ARENA.with(|arena| {
        let arena = arena.borrow();
        match arena.get(handle as usize) {
            Some(JsonNode::Array(elems)) => elems.len() as i64,
            _ => -1,
        }
    })
}

/// Returns the handle of the element at `index` in a JSON array.
/// Returns -1 if not an array or index is out of bounds.
#[no_mangle]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
pub extern "C" fn __spark_json_array_get(handle: i64, index: i64) -> i64 {
    if handle < 0 || index < 0 {
        return -1;
    }
    ARENA.with(|arena| {
        let arena = arena.borrow();
        match arena.get(handle as usize) {
            Some(JsonNode::Array(elems)) => {
                let idx = index as usize;
                if idx < elems.len() {
                    elems[idx] as i64
                } else {
                    -1
                }
            }
            _ => -1,
        }
    })
}

// --- Object access ---

/// Returns the handle of the value for `key` in a JSON object.
/// Returns -1 if not an object or key not found.
///
/// # Safety
///
/// `key` must be a valid `SparkString` pointer.
#[no_mangle]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
pub unsafe extern "C" fn __spark_json_object_get(handle: i64, key: *mut u8) -> i64 {
    if handle < 0 {
        return -1;
    }
    let key_str = unsafe { read_spark_str(key) };
    ARENA.with(|arena| {
        let arena = arena.borrow();
        match arena.get(handle as usize) {
            Some(JsonNode::Object(entries)) => {
                for (k, v) in entries {
                    if k == &key_str {
                        return *v as i64;
                    }
                }
                -1
            }
            _ => -1,
        }
    })
}

/// Returns 1 if the JSON object contains `key`, 0 otherwise.
///
/// # Safety
///
/// `key` must be a valid `SparkString` pointer.
#[no_mangle]
#[allow(clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_json_object_has(handle: i64, key: *mut u8) -> i8 {
    if handle < 0 {
        return 0;
    }
    let key_str = unsafe { read_spark_str(key) };
    ARENA.with(|arena| {
        let arena = arena.borrow();
        match arena.get(handle as usize) {
            Some(JsonNode::Object(entries)) => {
                for (k, _) in entries {
                    if k == &key_str {
                        return 1;
                    }
                }
                0
            }
            _ => 0,
        }
    })
}

// --- Construction ---

/// Creates a new JSON null node.
#[no_mangle]
#[allow(clippy::cast_possible_wrap)]
pub extern "C" fn __spark_json_new_null() -> i64 {
    alloc_node(JsonNode::Null) as i64
}

/// Creates a new JSON boolean node.
#[no_mangle]
#[allow(clippy::cast_possible_wrap)]
pub extern "C" fn __spark_json_new_bool(v: i8) -> i64 {
    alloc_node(JsonNode::Bool(v != 0)) as i64
}

/// Creates a new JSON number node.
#[no_mangle]
#[allow(clippy::cast_possible_wrap)]
pub extern "C" fn __spark_json_new_number(v: f64) -> i64 {
    alloc_node(JsonNode::Number(v)) as i64
}

/// Creates a new JSON string node.
///
/// # Safety
///
/// `v` must be a valid `SparkString` pointer.
#[no_mangle]
#[allow(clippy::cast_possible_wrap)]
pub unsafe extern "C" fn __spark_json_new_string(v: *mut u8) -> i64 {
    let s = unsafe { read_spark_str(v) };
    alloc_node(JsonNode::Str(s)) as i64
}

/// Creates a new empty JSON array node.
#[no_mangle]
#[allow(clippy::cast_possible_wrap)]
pub extern "C" fn __spark_json_new_array() -> i64 {
    alloc_node(JsonNode::Array(Vec::new())) as i64
}

/// Creates a new empty JSON object node.
#[no_mangle]
#[allow(clippy::cast_possible_wrap)]
pub extern "C" fn __spark_json_new_object() -> i64 {
    alloc_node(JsonNode::Object(Vec::new())) as i64
}

// --- Mutation ---

/// Appends a child handle to a JSON array.
///
/// Does nothing if `handle` is not an array or `child` is invalid.
#[no_mangle]
#[allow(clippy::cast_sign_loss)]
pub extern "C" fn __spark_json_array_push(handle: i64, child: i64) {
    if handle < 0 || child < 0 {
        return;
    }
    ARENA.with(|arena| {
        let mut arena = arena.borrow_mut();
        if let Some(JsonNode::Array(ref mut elems)) = arena.get_mut(handle as usize) {
            elems.push(child as usize);
        }
    });
}

/// Sets a key-value pair on a JSON object. If the key already exists, its
/// value is replaced.
///
/// # Safety
///
/// `key` must be a valid `SparkString` pointer.
#[no_mangle]
#[allow(clippy::cast_sign_loss)]
pub unsafe extern "C" fn __spark_json_object_set(handle: i64, key: *mut u8, value: i64) {
    if handle < 0 || value < 0 {
        return;
    }
    let key_str = unsafe { read_spark_str(key) };
    ARENA.with(|arena| {
        let mut arena = arena.borrow_mut();
        if let Some(JsonNode::Object(ref mut entries)) = arena.get_mut(handle as usize) {
            // Check if key already exists
            for entry in entries.iter_mut() {
                if entry.0 == key_str {
                    entry.1 = value as usize;
                    return;
                }
            }
            entries.push((key_str, value as usize));
        }
    });
}

// --- Serialization ---

/// Stringifies a JSON node to compact JSON. Returns a new SparkString.
#[no_mangle]
#[allow(clippy::cast_sign_loss)]
pub extern "C" fn __spark_json_stringify(handle: i64) -> *mut u8 {
    if handle < 0 {
        return rust_str_to_spark("null");
    }
    let json = stringify_node(handle as usize, false, 0, 0);
    rust_str_to_spark(&json)
}

/// Stringifies a JSON node to pretty-printed JSON with the given indent.
/// Returns a new SparkString.
#[no_mangle]
#[allow(clippy::cast_sign_loss)]
pub extern "C" fn __spark_json_stringify_pretty(handle: i64, indent: i64) -> *mut u8 {
    if handle < 0 {
        return rust_str_to_spark("null");
    }
    #[allow(clippy::cast_possible_truncation)]
    let indent_size = indent.max(0) as usize;
    let json = stringify_node(handle as usize, true, indent_size, 0);
    rust_str_to_spark(&json)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: read a SparkString result into a Rust String.
    unsafe fn read_result_string(ptr: *mut u8) -> String {
        let s = unsafe { &*(ptr.cast::<SparkString>()) };
        if s.len == 0 {
            return String::new();
        }
        let slice = unsafe { std::slice::from_raw_parts(s.data, s.len) };
        std::str::from_utf8(slice).unwrap().to_owned()
    }

    /// Helper: create a SparkString from a &str for use as FFI input.
    fn make_spark_str(s: &str) -> *mut u8 {
        rust_str_to_spark(s)
    }

    // --- Parser tests ---

    #[test]
    fn parse_null() {
        let input = make_spark_str("null");
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        assert_eq!(__spark_json_type(handle), JSON_TYPE_NULL);
    }

    #[test]
    fn parse_true() {
        let input = make_spark_str("true");
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        assert_eq!(__spark_json_type(handle), JSON_TYPE_BOOL);
        assert_eq!(__spark_json_bool(handle), 1);
    }

    #[test]
    fn parse_false() {
        let input = make_spark_str("false");
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        assert_eq!(__spark_json_type(handle), JSON_TYPE_BOOL);
        assert_eq!(__spark_json_bool(handle), 0);
    }

    #[test]
    fn parse_integer() {
        let input = make_spark_str("42");
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        assert_eq!(__spark_json_type(handle), JSON_TYPE_NUMBER);
        assert!(((__spark_json_number(handle)) - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_negative_number() {
        let input = make_spark_str("-3.14");
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        assert!(((__spark_json_number(handle)) - (-3.14)).abs() < 1e-10);
    }

    #[test]
    fn parse_scientific_notation() {
        let input = make_spark_str("1.5e2");
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        assert!(((__spark_json_number(handle)) - 150.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_string() {
        let input = make_spark_str(r#""hello world""#);
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        assert_eq!(__spark_json_type(handle), JSON_TYPE_STRING);
        unsafe {
            assert_eq!(read_result_string(__spark_json_string(handle)), "hello world");
        }
    }

    #[test]
    fn parse_string_with_escapes() {
        let input = make_spark_str(r#""line1\nline2\ttab""#);
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        unsafe {
            assert_eq!(
                read_result_string(__spark_json_string(handle)),
                "line1\nline2\ttab"
            );
        }
    }

    #[test]
    fn parse_string_with_unicode_escape() {
        let input = make_spark_str(r#""\u0041""#);
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        unsafe {
            assert_eq!(read_result_string(__spark_json_string(handle)), "A");
        }
    }

    #[test]
    fn parse_empty_array() {
        let input = make_spark_str("[]");
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        assert_eq!(__spark_json_type(handle), JSON_TYPE_ARRAY);
        assert_eq!(__spark_json_array_len(handle), 0);
    }

    #[test]
    fn parse_array_with_values() {
        let input = make_spark_str("[1, 2, 3]");
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        assert_eq!(__spark_json_array_len(handle), 3);

        let first = __spark_json_array_get(handle, 0);
        assert!(first >= 0);
        assert!(((__spark_json_number(first)) - 1.0).abs() < f64::EPSILON);

        let third = __spark_json_array_get(handle, 2);
        assert!(third >= 0);
        assert!(((__spark_json_number(third)) - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_empty_object() {
        let input = make_spark_str("{}");
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        assert_eq!(__spark_json_type(handle), JSON_TYPE_OBJECT);
    }

    #[test]
    fn parse_object_with_values() {
        let input = make_spark_str(r#"{"name": "Alice", "age": 30}"#);
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        assert_eq!(__spark_json_type(handle), JSON_TYPE_OBJECT);

        let name_key = make_spark_str("name");
        let name_handle = unsafe { __spark_json_object_get(handle, name_key) };
        assert!(name_handle >= 0);
        unsafe {
            assert_eq!(read_result_string(__spark_json_string(name_handle)), "Alice");
        }

        let age_key = make_spark_str("age");
        let age_handle = unsafe { __spark_json_object_get(handle, age_key) };
        assert!(age_handle >= 0);
        assert!(((__spark_json_number(age_handle)) - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_nested() {
        let input = make_spark_str(r#"{"users": [{"name": "Bob"}]}"#);
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);

        let users_key = make_spark_str("users");
        let users = unsafe { __spark_json_object_get(handle, users_key) };
        assert!(users >= 0);
        assert_eq!(__spark_json_type(users), JSON_TYPE_ARRAY);
        assert_eq!(__spark_json_array_len(users), 1);

        let first_user = __spark_json_array_get(users, 0);
        assert!(first_user >= 0);
        assert_eq!(__spark_json_type(first_user), JSON_TYPE_OBJECT);

        let name_key = make_spark_str("name");
        let name = unsafe { __spark_json_object_get(first_user, name_key) };
        assert!(name >= 0);
        unsafe {
            assert_eq!(read_result_string(__spark_json_string(name)), "Bob");
        }
    }

    #[test]
    fn parse_error_invalid_json() {
        let input = make_spark_str("{invalid}");
        let handle = unsafe { __spark_json_parse(input) };
        assert_eq!(handle, -1);

        let err = __spark_json_error();
        unsafe {
            let msg = read_result_string(err);
            assert!(!msg.is_empty(), "error message should not be empty");
        }
    }

    #[test]
    fn parse_error_trailing_chars() {
        let input = make_spark_str("123 456");
        let handle = unsafe { __spark_json_parse(input) };
        assert_eq!(handle, -1);
    }

    // --- Object has ---

    #[test]
    fn object_has_existing_key() {
        let input = make_spark_str(r#"{"x": 1}"#);
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);

        let key = make_spark_str("x");
        assert_eq!(unsafe { __spark_json_object_has(handle, key) }, 1);

        let missing = make_spark_str("y");
        assert_eq!(unsafe { __spark_json_object_has(handle, missing) }, 0);
    }

    // --- Construction tests ---

    #[test]
    fn construct_null() {
        let handle = __spark_json_new_null();
        assert!(handle >= 0);
        assert_eq!(__spark_json_type(handle), JSON_TYPE_NULL);
    }

    #[test]
    fn construct_bool() {
        let t = __spark_json_new_bool(1);
        assert_eq!(__spark_json_bool(t), 1);

        let f = __spark_json_new_bool(0);
        assert_eq!(__spark_json_bool(f), 0);
    }

    #[test]
    fn construct_number() {
        let handle = __spark_json_new_number(42.5);
        assert!(((__spark_json_number(handle)) - 42.5).abs() < f64::EPSILON);
    }

    #[test]
    fn construct_string() {
        let s = make_spark_str("hello");
        let handle = unsafe { __spark_json_new_string(s) };
        unsafe {
            assert_eq!(read_result_string(__spark_json_string(handle)), "hello");
        }
    }

    #[test]
    fn construct_array_and_push() {
        let arr = __spark_json_new_array();
        let n1 = __spark_json_new_number(10.0);
        let n2 = __spark_json_new_number(20.0);
        __spark_json_array_push(arr, n1);
        __spark_json_array_push(arr, n2);

        assert_eq!(__spark_json_array_len(arr), 2);
        let first = __spark_json_array_get(arr, 0);
        assert!(((__spark_json_number(first)) - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn construct_object_and_set() {
        let obj = __spark_json_new_object();
        let key = make_spark_str("name");
        let val = unsafe { __spark_json_new_string(make_spark_str("Alice")) };
        unsafe { __spark_json_object_set(obj, key, val) };

        let retrieved = unsafe { __spark_json_object_get(obj, make_spark_str("name")) };
        assert!(retrieved >= 0);
        unsafe {
            assert_eq!(read_result_string(__spark_json_string(retrieved)), "Alice");
        }
    }

    #[test]
    fn object_set_replaces_existing() {
        let obj = __spark_json_new_object();
        let key = make_spark_str("x");
        let v1 = __spark_json_new_number(1.0);
        unsafe { __spark_json_object_set(obj, key, v1) };

        let key2 = make_spark_str("x");
        let v2 = __spark_json_new_number(2.0);
        unsafe { __spark_json_object_set(obj, key2, v2) };

        let result = unsafe { __spark_json_object_get(obj, make_spark_str("x")) };
        assert!(((__spark_json_number(result)) - 2.0).abs() < f64::EPSILON);
    }

    // --- Stringify tests ---

    #[test]
    fn stringify_null() {
        let handle = __spark_json_new_null();
        let result = __spark_json_stringify(handle);
        unsafe {
            assert_eq!(read_result_string(result), "null");
        }
    }

    #[test]
    fn stringify_bool() {
        let t = __spark_json_new_bool(1);
        unsafe {
            assert_eq!(read_result_string(__spark_json_stringify(t)), "true");
        }
    }

    #[test]
    fn stringify_integer_number() {
        let n = __spark_json_new_number(42.0);
        unsafe {
            assert_eq!(read_result_string(__spark_json_stringify(n)), "42");
        }
    }

    #[test]
    fn stringify_float_number() {
        let n = __spark_json_new_number(3.14);
        unsafe {
            let s = read_result_string(__spark_json_stringify(n));
            assert!(s.starts_with("3.14"), "expected 3.14..., got {s}");
        }
    }

    #[test]
    fn stringify_string_with_escapes() {
        let s = make_spark_str("hello \"world\"\nnewline");
        let handle = unsafe { __spark_json_new_string(s) };
        unsafe {
            let result = read_result_string(__spark_json_stringify(handle));
            assert_eq!(result, r#""hello \"world\"\nnewline""#);
        }
    }

    #[test]
    fn stringify_object() {
        let obj = __spark_json_new_object();
        let key = make_spark_str("x");
        let val = __spark_json_new_number(1.0);
        unsafe { __spark_json_object_set(obj, key, val) };

        unsafe {
            let result = read_result_string(__spark_json_stringify(obj));
            assert_eq!(result, r#"{"x":1}"#);
        }
    }

    #[test]
    fn stringify_array() {
        let arr = __spark_json_new_array();
        __spark_json_array_push(arr, __spark_json_new_number(1.0));
        __spark_json_array_push(arr, __spark_json_new_number(2.0));

        unsafe {
            let result = read_result_string(__spark_json_stringify(arr));
            assert_eq!(result, "[1,2]");
        }
    }

    #[test]
    fn stringify_pretty() {
        let obj = __spark_json_new_object();
        let key = make_spark_str("x");
        unsafe { __spark_json_object_set(obj, key, __spark_json_new_number(1.0)) };

        unsafe {
            let result = read_result_string(__spark_json_stringify_pretty(obj, 2));
            assert_eq!(result, "{\n  \"x\": 1\n}");
        }
    }

    // --- Round-trip tests ---

    #[test]
    fn round_trip_simple_object() {
        let input = make_spark_str(r#"{"name":"Alice","age":30}"#);
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);

        unsafe {
            let result = read_result_string(__spark_json_stringify(handle));
            assert_eq!(result, r#"{"name":"Alice","age":30}"#);
        }
    }

    #[test]
    fn round_trip_nested() {
        let input = make_spark_str(r#"{"a":[1,2],"b":{"c":true}}"#);
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);

        unsafe {
            let result = read_result_string(__spark_json_stringify(handle));
            assert_eq!(result, r#"{"a":[1,2],"b":{"c":true}}"#);
        }
    }

    // --- Edge cases ---

    #[test]
    fn invalid_handle_returns_defaults() {
        assert_eq!(__spark_json_type(-1), -1);
        assert_eq!(__spark_json_bool(-1), 0);
        assert!((__spark_json_number(-1)).abs() < f64::EPSILON);
        assert_eq!(__spark_json_array_len(-1), -1);
        assert_eq!(__spark_json_array_get(-1, 0), -1);
    }

    #[test]
    fn array_get_out_of_bounds() {
        let arr = __spark_json_new_array();
        assert_eq!(__spark_json_array_get(arr, 0), -1);
        assert_eq!(__spark_json_array_get(arr, -1), -1);
    }

    #[test]
    fn object_get_missing_key() {
        let obj = __spark_json_new_object();
        let key = make_spark_str("nope");
        assert_eq!(unsafe { __spark_json_object_get(obj, key) }, -1);
    }

    #[test]
    fn parse_whitespace_around_value() {
        let input = make_spark_str("  \n\t 42  \r\n ");
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        assert!(((__spark_json_number(handle)) - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_empty_string_value() {
        let input = make_spark_str(r#""""#);
        let handle = unsafe { __spark_json_parse(input) };
        assert!(handle >= 0);
        unsafe {
            assert_eq!(read_result_string(__spark_json_string(handle)), "");
        }
    }

    #[test]
    fn stringify_empty_array() {
        let arr = __spark_json_new_array();
        unsafe {
            assert_eq!(read_result_string(__spark_json_stringify(arr)), "[]");
        }
    }

    #[test]
    fn stringify_empty_object() {
        let obj = __spark_json_new_object();
        unsafe {
            assert_eq!(read_result_string(__spark_json_stringify(obj)), "{}");
        }
    }

    #[test]
    fn stringify_negative_handle() {
        unsafe {
            assert_eq!(read_result_string(__spark_json_stringify(-1)), "null");
        }
    }
}
