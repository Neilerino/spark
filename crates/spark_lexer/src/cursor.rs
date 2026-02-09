//! Character cursor for the lexer.
//!
//! Provides efficient character iteration with lookahead.

use std::str::Chars;

/// A cursor over the source code characters.
///
/// Provides efficient iteration with single-character lookahead.
pub struct Cursor<'a> {
    /// Iterator over the remaining characters.
    chars: Chars<'a>,
    /// The original source string.
    source: &'a str,
    /// Current byte position in the source.
    pos: u32,
    /// Previous character (for lookbehind).
    prev: char,
}

impl<'a> Cursor<'a> {
    /// Creates a new cursor over the given source.
    pub fn new(source: &'a str) -> Self {
        Self {
            chars: source.chars(),
            source,
            pos: 0,
            prev: '\0',
        }
    }

    /// Returns the current byte position.
    #[must_use]
    pub const fn pos(&self) -> u32 {
        self.pos
    }

    /// Returns the previous character (before the current position).
    #[must_use]
    #[allow(dead_code)]
    pub const fn prev(&self) -> char {
        self.prev
    }

    /// Peeks at the next character without consuming it.
    #[must_use]
    pub fn peek(&self) -> char {
        self.chars.clone().next().unwrap_or('\0')
    }

    /// Peeks at the second next character without consuming.
    #[must_use]
    pub fn peek_second(&self) -> char {
        let mut chars = self.chars.clone();
        chars.next();
        chars.next().unwrap_or('\0')
    }

    /// Returns true if there are no more characters.
    #[must_use]
    pub fn is_eof(&self) -> bool {
        self.chars.as_str().is_empty()
    }

    /// Consumes and returns the next character.
    pub fn bump(&mut self) -> char {
        let c = self.chars.next().unwrap_or('\0');
        self.prev = c;
        self.pos += c.len_utf8() as u32;
        c
    }

    /// Consumes characters while the predicate is true.
    ///
    /// Returns the number of characters consumed.
    pub fn eat_while(&mut self, mut predicate: impl FnMut(char) -> bool) -> u32 {
        let start = self.pos;
        while !self.is_eof() && predicate(self.peek()) {
            self.bump();
        }
        self.pos - start
    }

    /// Consumes the next character if it matches, returning true if consumed.
    pub fn eat(&mut self, c: char) -> bool {
        if self.peek() == c {
            self.bump();
            true
        } else {
            false
        }
    }

    /// Returns the remaining source from the current position.
    #[must_use]
    #[allow(dead_code)]
    pub fn rest(&self) -> &'a str {
        self.chars.as_str()
    }

    /// Returns a cloned character iterator over the remaining source.
    #[must_use]
    pub fn rest_chars(&self) -> Chars<'a> {
        self.chars.clone()
    }

    /// Returns a slice of the source from start to current position.
    #[must_use]
    pub fn slice_from(&self, start: u32) -> &'a str {
        &self.source[start as usize..self.pos as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_basics() {
        let mut cursor = Cursor::new("abc");

        assert_eq!(cursor.pos(), 0);
        assert_eq!(cursor.peek(), 'a');
        assert!(!cursor.is_eof());

        assert_eq!(cursor.bump(), 'a');
        assert_eq!(cursor.pos(), 1);
        assert_eq!(cursor.prev(), 'a');

        assert_eq!(cursor.bump(), 'b');
        assert_eq!(cursor.bump(), 'c');
        assert!(cursor.is_eof());
    }

    #[test]
    fn cursor_peek_second() {
        let cursor = Cursor::new("abc");
        assert_eq!(cursor.peek(), 'a');
        assert_eq!(cursor.peek_second(), 'b');
    }

    #[test]
    fn cursor_eat_while() {
        let mut cursor = Cursor::new("aaabbb");
        let count = cursor.eat_while(|c| c == 'a');
        assert_eq!(count, 3);
        assert_eq!(cursor.peek(), 'b');
    }

    #[test]
    fn cursor_eat() {
        let mut cursor = Cursor::new("ab");
        assert!(cursor.eat('a'));
        assert!(!cursor.eat('a'));
        assert!(cursor.eat('b'));
    }

    #[test]
    fn cursor_slice() {
        let mut cursor = Cursor::new("hello world");
        cursor.eat_while(|c| c != ' ');
        assert_eq!(cursor.slice_from(0), "hello");
    }

    #[test]
    fn cursor_unicode() {
        let mut cursor = Cursor::new("日本語");
        assert_eq!(cursor.bump(), '日');
        assert_eq!(cursor.pos(), 3); // 3 bytes for this character
        assert_eq!(cursor.bump(), '本');
        assert_eq!(cursor.bump(), '語');
        assert!(cursor.is_eof());
    }
}
