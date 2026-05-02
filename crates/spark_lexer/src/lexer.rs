//! The main lexer implementation.

use std::collections::VecDeque;

use crate::cursor::Cursor;
use crate::token::{Token, TokenKind, TokenValue};
use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_intern::sym;
use spark_span::{FileId, Span};

/// The mode the lexer is currently in.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LexerMode {
    /// Normal tokenization.
    Normal,
    /// Inside a string literal, scanning text segments.
    InString,
    /// Inside an interpolated expression `{expr}`, tracking brace depth.
    InInterpolation(u32),
}

/// The Spark lexer.
///
/// Converts source code into a stream of tokens.
pub struct Lexer<'a> {
    /// The file being lexed.
    file_id: FileId,
    /// Character cursor.
    cursor: Cursor<'a>,
    /// Collected diagnostics.
    diagnostics: Diagnostics,
    /// Current lexer mode for string interpolation.
    mode: LexerMode,
    /// Stack of saved modes for nested string interpolation.
    /// When a string literal appears inside an interpolation, we push the
    /// current `InInterpolation` mode here and restore it when the inner
    /// string finishes.
    mode_stack: Vec<LexerMode>,
    /// Buffered tokens from string interpolation scanning.
    pending: VecDeque<Token>,
}

impl<'a> Lexer<'a> {
    /// Creates a new lexer for the given source.
    #[must_use]
    pub fn new(file_id: FileId, source: &'a str) -> Self {
        Self {
            file_id,
            cursor: Cursor::new(source),
            diagnostics: Diagnostics::new(),
            mode: LexerMode::Normal,
            mode_stack: Vec::new(),
            pending: VecDeque::new(),
        }
    }

    /// Tokenizes the entire source, returning all tokens.
    ///
    /// The returned vector always ends with an `Eof` token.
    pub fn tokenize(&mut self) -> Vec<Token> {
        let mut tokens = Vec::new();

        loop {
            let token = self.next_token();
            let is_eof = token.kind == TokenKind::Eof;
            tokens.push(token);
            if is_eof {
                break;
            }
        }

        tokens
    }

    /// Returns the collected diagnostics.
    #[must_use]
    pub const fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    /// Consumes the lexer and returns diagnostics.
    #[must_use]
    pub fn into_diagnostics(self) -> Diagnostics {
        self.diagnostics
    }

    /// Scans the next token.
    #[allow(clippy::too_many_lines)]
    pub fn next_token(&mut self) -> Token {
        // Drain pending tokens from interpolation scanning.
        if let Some(token) = self.pending.pop_front() {
            return token;
        }

        // Handle string interpolation modes.
        match self.mode.clone() {
            LexerMode::InString => return self.scan_string_text(),
            LexerMode::InInterpolation(depth) => {
                return self.scan_interpolation_token(depth);
            }
            LexerMode::Normal => {}
        }

        // Skip whitespace and comments
        self.skip_whitespace_and_comments();

        let start = self.cursor.pos();

        if self.cursor.is_eof() {
            return Token::new(TokenKind::Eof, self.span(start));
        }

        let c = self.cursor.bump();

        match c {
            // Single character tokens
            '(' => Token::new(TokenKind::LParen, self.span(start)),
            ')' => Token::new(TokenKind::RParen, self.span(start)),
            '{' => Token::new(TokenKind::LBrace, self.span(start)),
            '}' => Token::new(TokenKind::RBrace, self.span(start)),
            '[' => Token::new(TokenKind::LBracket, self.span(start)),
            ']' => Token::new(TokenKind::RBracket, self.span(start)),
            ',' => Token::new(TokenKind::Comma, self.span(start)),
            ';' => Token::new(TokenKind::Semicolon, self.span(start)),
            '~' => Token::new(TokenKind::Tilde, self.span(start)),
            '@' => Token::new(TokenKind::At, self.span(start)),

            // Potentially multi-character tokens
            ':' => {
                if self.cursor.eat(':') {
                    Token::new(TokenKind::ColonColon, self.span(start))
                } else {
                    Token::new(TokenKind::Colon, self.span(start))
                }
            }

            '.' => {
                if self.cursor.eat('.') {
                    if self.cursor.eat('=') {
                        Token::new(TokenKind::DotDotEq, self.span(start))
                    } else if self.cursor.eat('.') {
                        Token::new(TokenKind::Ellipsis, self.span(start))
                    } else {
                        Token::new(TokenKind::DotDot, self.span(start))
                    }
                } else {
                    Token::new(TokenKind::Dot, self.span(start))
                }
            }

            '-' => {
                if self.cursor.eat('>') {
                    Token::new(TokenKind::Arrow, self.span(start))
                } else if self.cursor.eat('=') {
                    Token::new(TokenKind::MinusEq, self.span(start))
                } else {
                    Token::new(TokenKind::Minus, self.span(start))
                }
            }

            '+' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::PlusEq, self.span(start))
                } else {
                    Token::new(TokenKind::Plus, self.span(start))
                }
            }

            '*' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::StarEq, self.span(start))
                } else {
                    Token::new(TokenKind::Star, self.span(start))
                }
            }

            '/' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::SlashEq, self.span(start))
                } else {
                    Token::new(TokenKind::Slash, self.span(start))
                }
            }

            '%' => Token::new(TokenKind::Percent, self.span(start)),

            '=' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::EqEq, self.span(start))
                } else if self.cursor.eat('>') {
                    Token::new(TokenKind::FatArrow, self.span(start))
                } else {
                    Token::new(TokenKind::Eq, self.span(start))
                }
            }

            '!' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::BangEq, self.span(start))
                } else {
                    Token::new(TokenKind::Bang, self.span(start))
                }
            }

            '<' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::LtEq, self.span(start))
                } else if self.cursor.eat('<') {
                    Token::new(TokenKind::LtLt, self.span(start))
                } else {
                    Token::new(TokenKind::Lt, self.span(start))
                }
            }

            '>' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::GtEq, self.span(start))
                } else if self.cursor.eat('>') {
                    Token::new(TokenKind::GtGt, self.span(start))
                } else {
                    Token::new(TokenKind::Gt, self.span(start))
                }
            }

            '?' => {
                if self.cursor.eat('?') {
                    Token::new(TokenKind::QuestionQuestion, self.span(start))
                } else if self.cursor.eat('.') {
                    Token::new(TokenKind::QuestionDot, self.span(start))
                } else {
                    Token::new(TokenKind::Question, self.span(start))
                }
            }

            '&' => Token::new(TokenKind::Ampersand, self.span(start)),
            '|' => Token::new(TokenKind::Pipe, self.span(start)),
            '^' => Token::new(TokenKind::Caret, self.span(start)),

            // String literals
            '"' => self.scan_string(start),

            // Character literals
            '\'' => self.scan_char(start),

            // Numbers
            '0'..='9' => self.scan_number(start, c),

            // Identifiers and keywords
            c if is_ident_start(c) => self.scan_identifier(start),

            // Unknown character
            _ => {
                self.error(format!("unexpected character `{c}`"), self.span(start));
                Token::new(TokenKind::Error, self.span(start))
            }
        }
    }

    /// Skips whitespace and comments.
    fn skip_whitespace_and_comments(&mut self) {
        loop {
            // Skip whitespace
            self.cursor.eat_while(char::is_whitespace);

            // Check for comments
            if self.cursor.peek() == '/' {
                match self.cursor.peek_second() {
                    '/' => {
                        // Line comment
                        self.cursor.bump(); // /
                        self.cursor.bump(); // /
                        self.cursor.eat_while(|c| c != '\n');
                        continue;
                    }
                    '*' => {
                        // Block comment
                        self.cursor.bump(); // /
                        self.cursor.bump(); // *
                        self.skip_block_comment();
                        continue;
                    }
                    _ => break,
                }
            }

            break;
        }
    }

    /// Skips a block comment, handling nesting.
    fn skip_block_comment(&mut self) {
        let mut depth = 1;

        while depth > 0 && !self.cursor.is_eof() {
            let c = self.cursor.bump();
            match c {
                '/' if self.cursor.peek() == '*' => {
                    self.cursor.bump();
                    depth += 1;
                }
                '*' if self.cursor.peek() == '/' => {
                    self.cursor.bump();
                    depth -= 1;
                }
                _ => {}
            }
        }
    }

    /// Scans a string literal, detecting interpolation.
    ///
    /// If the string contains no unescaped `{`, emits a single `StringLiteral`.
    /// Otherwise, emits `StringStart` and switches to `InString` mode so that
    /// subsequent `next_token()` calls produce `StringText`, `InterpStart`,
    /// interpolated expression tokens, and finally `StringEnd`.
    fn scan_string(&mut self, start: u32) -> Token {
        // Peek ahead to determine if this string contains interpolation.
        // We do this by scanning a copy of the cursor.
        if self.string_has_interpolation() {
            // Interpolated string: emit StringStart and switch to InString mode.
            // If we're currently in an interpolation, push that mode so we can
            // restore it when the string ends.
            if self.mode != LexerMode::Normal {
                self.mode_stack.push(self.mode.clone());
            }
            self.mode = LexerMode::InString;
            return Token::new(TokenKind::StringStart, self.span(start));
        }

        // Simple string: scan as before into a single StringLiteral.
        let mut value = String::new();

        loop {
            if self.cursor.is_eof() {
                self.error("unterminated string literal", self.span(start));
                break;
            }

            let c = self.cursor.bump();
            match c {
                '"' => break,
                '\\' => {
                    if let Some(escaped) = self.scan_escape() {
                        value.push(escaped);
                    }
                }
                '\n' => {
                    self.error("unterminated string literal", self.span(start));
                    break;
                }
                _ => value.push(c),
            }
        }

        let symbol = sym(&value);
        Token::with_value(
            TokenKind::StringLiteral,
            self.span(start),
            TokenValue::String(symbol),
        )
    }

    /// Checks whether the current string (cursor positioned after opening `"`)
    /// contains any unescaped `{` before the closing `"`.
    fn string_has_interpolation(&self) -> bool {
        let mut chars = self.cursor.rest_chars();
        loop {
            match chars.next() {
                None | Some('\n' | '"') => return false,
                Some('\\') => {
                    // Skip the escaped character
                    chars.next();
                }
                Some('{') => return true,
                Some(_) => {}
            }
        }
    }

    /// Scans a text segment inside an interpolated string (`InString` mode).
    ///
    /// Called when `self.mode == LexerMode::InString`. Scans text until
    /// hitting `{` (start interpolation), `"` (end string), or EOF/newline (error).
    fn scan_string_text(&mut self) -> Token {
        let start = self.cursor.pos();
        let mut value = String::new();

        loop {
            if self.cursor.is_eof() {
                self.error("unterminated string literal", self.span(start));
                self.mode = self.mode_stack.pop().unwrap_or(LexerMode::Normal);
                break;
            }

            match self.cursor.peek() {
                '"' => {
                    // End of string. Emit text if any, then StringEnd.
                    let restored_mode = self.mode_stack.pop().unwrap_or(LexerMode::Normal);
                    if !value.is_empty() {
                        let symbol = sym(&value);
                        let text_token = Token::with_value(
                            TokenKind::StringText,
                            self.span(start),
                            TokenValue::String(symbol),
                        );
                        let end_start = self.cursor.pos();
                        self.cursor.bump(); // consume "
                        self.pending
                            .push_back(Token::new(TokenKind::StringEnd, self.span(end_start)));
                        self.mode = restored_mode;
                        return text_token;
                    }
                    // No text before closing quote - just emit StringEnd.
                    self.cursor.bump(); // consume "
                    self.mode = restored_mode;
                    return Token::new(TokenKind::StringEnd, self.span(start));
                }
                '{' => {
                    // Start of interpolation. Emit text if any, then InterpStart.
                    if !value.is_empty() {
                        let symbol = sym(&value);
                        let text_token = Token::with_value(
                            TokenKind::StringText,
                            self.span(start),
                            TokenValue::String(symbol),
                        );
                        let interp_start = self.cursor.pos();
                        self.cursor.bump(); // consume {
                        self.pending
                            .push_back(Token::new(TokenKind::InterpStart, self.span(interp_start)));
                        self.mode = LexerMode::InInterpolation(0);
                        return text_token;
                    }
                    // No text before { - just emit InterpStart.
                    self.cursor.bump(); // consume {
                    self.mode = LexerMode::InInterpolation(0);
                    return Token::new(TokenKind::InterpStart, self.span(start));
                }
                '\\' => {
                    self.cursor.bump(); // consume backslash
                    if let Some(escaped) = self.scan_escape() {
                        value.push(escaped);
                    }
                }
                '\n' => {
                    self.error("unterminated string literal", self.span(start));
                    self.mode = self.mode_stack.pop().unwrap_or(LexerMode::Normal);
                    break;
                }
                _ => {
                    let c = self.cursor.bump();
                    value.push(c);
                }
            }
        }

        // Error case: return what we have as text, or an error token.
        if value.is_empty() {
            Token::new(TokenKind::Error, self.span(start))
        } else {
            let symbol = sym(&value);
            Token::with_value(
                TokenKind::StringText,
                self.span(start),
                TokenValue::String(symbol),
            )
        }
    }

    /// Scans a token while inside an interpolation `{expr}`.
    ///
    /// Tracks brace depth so nested `{...}` inside the expression don't
    /// prematurely close the interpolation. When the matching `}` is found
    /// at depth 0, switches back to `InString` mode.
    fn scan_interpolation_token(&mut self, depth: u32) -> Token {
        // Skip whitespace and comments inside interpolation.
        self.skip_whitespace_and_comments();

        let start = self.cursor.pos();

        if self.cursor.is_eof() {
            self.error("unterminated string interpolation", self.span(start));
            self.mode = LexerMode::Normal;
            return Token::new(TokenKind::Eof, self.span(start));
        }

        let c = self.cursor.bump();

        match c {
            '}' if depth == 0 => {
                // Matching close brace: end interpolation, switch to InString.
                self.mode = LexerMode::InString;
                Token::new(TokenKind::RBrace, self.span(start))
            }
            '}' => {
                // Nested close brace: decrement depth.
                self.mode = LexerMode::InInterpolation(depth - 1);
                Token::new(TokenKind::RBrace, self.span(start))
            }
            '{' => {
                // Nested open brace: increment depth.
                self.mode = LexerMode::InInterpolation(depth + 1);
                Token::new(TokenKind::LBrace, self.span(start))
            }
            '"' => {
                // String literal inside interpolation.
                // scan_string() will push our current mode on the stack if
                // it detects interpolation, so nesting is handled correctly.
                self.scan_string(start)
            }
            // Everything else: delegate to the normal token logic.
            _ => self.scan_normal_token(start, c, depth),
        }
    }

    /// Scans a non-special token inside an interpolation expression.
    /// This mirrors the logic in `next_token()` for all non-brace/non-string tokens.
    #[allow(clippy::too_many_lines)]
    fn scan_normal_token(&mut self, start: u32, c: char, depth: u32) -> Token {
        // Restore mode (it was consumed by the caller).
        self.mode = LexerMode::InInterpolation(depth);

        match c {
            '(' => Token::new(TokenKind::LParen, self.span(start)),
            ')' => Token::new(TokenKind::RParen, self.span(start)),
            '[' => Token::new(TokenKind::LBracket, self.span(start)),
            ']' => Token::new(TokenKind::RBracket, self.span(start)),
            ',' => Token::new(TokenKind::Comma, self.span(start)),
            ';' => Token::new(TokenKind::Semicolon, self.span(start)),
            '~' => Token::new(TokenKind::Tilde, self.span(start)),
            '@' => Token::new(TokenKind::At, self.span(start)),
            ':' => {
                if self.cursor.eat(':') {
                    Token::new(TokenKind::ColonColon, self.span(start))
                } else {
                    Token::new(TokenKind::Colon, self.span(start))
                }
            }
            '.' => {
                if self.cursor.eat('.') {
                    if self.cursor.eat('=') {
                        Token::new(TokenKind::DotDotEq, self.span(start))
                    } else if self.cursor.eat('.') {
                        Token::new(TokenKind::Ellipsis, self.span(start))
                    } else {
                        Token::new(TokenKind::DotDot, self.span(start))
                    }
                } else {
                    Token::new(TokenKind::Dot, self.span(start))
                }
            }
            '-' => {
                if self.cursor.eat('>') {
                    Token::new(TokenKind::Arrow, self.span(start))
                } else if self.cursor.eat('=') {
                    Token::new(TokenKind::MinusEq, self.span(start))
                } else {
                    Token::new(TokenKind::Minus, self.span(start))
                }
            }
            '+' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::PlusEq, self.span(start))
                } else {
                    Token::new(TokenKind::Plus, self.span(start))
                }
            }
            '*' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::StarEq, self.span(start))
                } else {
                    Token::new(TokenKind::Star, self.span(start))
                }
            }
            '/' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::SlashEq, self.span(start))
                } else {
                    Token::new(TokenKind::Slash, self.span(start))
                }
            }
            '%' => Token::new(TokenKind::Percent, self.span(start)),
            '=' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::EqEq, self.span(start))
                } else if self.cursor.eat('>') {
                    Token::new(TokenKind::FatArrow, self.span(start))
                } else {
                    Token::new(TokenKind::Eq, self.span(start))
                }
            }
            '!' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::BangEq, self.span(start))
                } else {
                    Token::new(TokenKind::Bang, self.span(start))
                }
            }
            '<' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::LtEq, self.span(start))
                } else if self.cursor.eat('<') {
                    Token::new(TokenKind::LtLt, self.span(start))
                } else {
                    Token::new(TokenKind::Lt, self.span(start))
                }
            }
            '>' => {
                if self.cursor.eat('=') {
                    Token::new(TokenKind::GtEq, self.span(start))
                } else if self.cursor.eat('>') {
                    Token::new(TokenKind::GtGt, self.span(start))
                } else {
                    Token::new(TokenKind::Gt, self.span(start))
                }
            }
            '?' => {
                if self.cursor.eat('?') {
                    Token::new(TokenKind::QuestionQuestion, self.span(start))
                } else if self.cursor.eat('.') {
                    Token::new(TokenKind::QuestionDot, self.span(start))
                } else {
                    Token::new(TokenKind::Question, self.span(start))
                }
            }
            '&' => Token::new(TokenKind::Ampersand, self.span(start)),
            '|' => Token::new(TokenKind::Pipe, self.span(start)),
            '^' => Token::new(TokenKind::Caret, self.span(start)),
            '\'' => self.scan_char(start),
            '0'..='9' => self.scan_number(start, c),
            c if is_ident_start(c) => self.scan_identifier(start),
            _ => {
                self.error(format!("unexpected character `{c}`"), self.span(start));
                Token::new(TokenKind::Error, self.span(start))
            }
        }
    }

    /// Scans an escape sequence, returning the escaped character.
    fn scan_escape(&mut self) -> Option<char> {
        let c = self.cursor.bump();
        match c {
            'n' => Some('\n'),
            't' => Some('\t'),
            'r' => Some('\r'),
            '\\' => Some('\\'),
            '"' => Some('"'),
            '\'' => Some('\''),
            '0' => Some('\0'),
            '{' => Some('{'),
            'u' => self.scan_unicode_escape(),
            _ => {
                self.error(
                    format!("unknown escape sequence `\\{c}`"),
                    Span::new(self.file_id, self.cursor.pos() - 2, self.cursor.pos()),
                );
                None
            }
        }
    }

    /// Scans a unicode escape sequence like `\u{1F600}`.
    fn scan_unicode_escape(&mut self) -> Option<char> {
        let start = self.cursor.pos() - 2; // Include \u

        if !self.cursor.eat('{') {
            self.error("expected `{` after `\\u`", self.span(start));
            return None;
        }

        let hex_start = self.cursor.pos();
        self.cursor.eat_while(|c| c.is_ascii_hexdigit());
        let hex = self.cursor.slice_from(hex_start);

        if !self.cursor.eat('}') {
            self.error("expected `}` to close unicode escape", self.span(start));
            return None;
        }

        if hex.is_empty() || hex.len() > 6 {
            self.error("invalid unicode escape sequence", self.span(start));
            return None;
        }

        u32::from_str_radix(hex, 16)
            .ok()
            .and_then(char::from_u32)
            .or_else(|| {
                self.error("invalid unicode code point", self.span(start));
                None
            })
    }

    /// Scans a character literal.
    #[allow(clippy::option_if_let_else)]
    fn scan_char(&mut self, start: u32) -> Token {
        let value = if self.cursor.peek() == '\\' {
            self.cursor.bump();
            self.scan_escape()
        } else if self.cursor.peek() == '\'' || self.cursor.is_eof() {
            self.error("empty character literal", self.span(start));
            None
        } else {
            Some(self.cursor.bump())
        };

        if !self.cursor.eat('\'') {
            self.error("unterminated character literal", self.span(start));
        }

        match value {
            Some(c) => Token::with_value(
                TokenKind::CharLiteral,
                self.span(start),
                TokenValue::Char(c),
            ),
            None => Token::new(TokenKind::Error, self.span(start)),
        }
    }

    /// Scans a number literal (integer or float).
    fn scan_number(&mut self, start: u32, first: char) -> Token {
        // Check for hex, binary, octal
        if first == '0' {
            match self.cursor.peek() {
                'x' | 'X' => return self.scan_hex(start),
                'b' | 'B' => return self.scan_binary(start),
                'o' | 'O' => return self.scan_octal(start),
                _ => {}
            }
        }

        // Scan decimal digits
        self.cursor.eat_while(|c| c.is_ascii_digit() || c == '_');

        // Check for float
        if self.cursor.peek() == '.' && self.cursor.peek_second().is_ascii_digit() {
            self.cursor.bump(); // .
            self.cursor.eat_while(|c| c.is_ascii_digit() || c == '_');

            // Scientific notation
            if matches!(self.cursor.peek(), 'e' | 'E') {
                self.cursor.bump();
                if matches!(self.cursor.peek(), '+' | '-') {
                    self.cursor.bump();
                }
                self.cursor.eat_while(|c| c.is_ascii_digit() || c == '_');
            }

            return self.make_float(start);
        }

        // Scientific notation on integer makes it a float
        if matches!(self.cursor.peek(), 'e' | 'E') {
            self.cursor.bump();
            if matches!(self.cursor.peek(), '+' | '-') {
                self.cursor.bump();
            }
            self.cursor.eat_while(|c| c.is_ascii_digit() || c == '_');
            return self.make_float(start);
        }

        self.make_int(start, 10)
    }

    /// Scans a hexadecimal number.
    fn scan_hex(&mut self, start: u32) -> Token {
        self.cursor.bump(); // x or X
        self.cursor.eat_while(|c| c.is_ascii_hexdigit() || c == '_');
        self.make_int(start, 16)
    }

    /// Scans a binary number.
    fn scan_binary(&mut self, start: u32) -> Token {
        self.cursor.bump(); // b or B
        self.cursor.eat_while(|c| c == '0' || c == '1' || c == '_');
        self.make_int(start, 2)
    }

    /// Scans an octal number.
    fn scan_octal(&mut self, start: u32) -> Token {
        self.cursor.bump(); // o or O
        self.cursor
            .eat_while(|c| matches!(c, '0'..='7') || c == '_');
        self.make_int(start, 8)
    }

    /// Creates an integer token from the scanned text.
    fn make_int(&mut self, start: u32, radix: u32) -> Token {
        let text = self.cursor.slice_from(start);

        // Remove prefix and underscores
        let clean: String = text
            .trim_start_matches("0x")
            .trim_start_matches("0X")
            .trim_start_matches("0b")
            .trim_start_matches("0B")
            .trim_start_matches("0o")
            .trim_start_matches("0O")
            .chars()
            .filter(|&c| c != '_')
            .collect();

        if let Ok(n) = i64::from_str_radix(&clean, radix) {
            Token::with_value(TokenKind::IntLiteral, self.span(start), TokenValue::Int(n))
        } else {
            self.error(
                format!("invalid integer literal `{text}`"),
                self.span(start),
            );
            Token::new(TokenKind::Error, self.span(start))
        }
    }

    /// Creates a float token from the scanned text.
    fn make_float(&mut self, start: u32) -> Token {
        let text = self.cursor.slice_from(start);
        let clean: String = text.chars().filter(|&c| c != '_').collect();

        if let Ok(n) = clean.parse::<f64>() {
            Token::with_value(
                TokenKind::FloatLiteral,
                self.span(start),
                TokenValue::Float(n),
            )
        } else {
            self.error(format!("invalid float literal `{text}`"), self.span(start));
            Token::new(TokenKind::Error, self.span(start))
        }
    }

    /// Scans an identifier or keyword.
    fn scan_identifier(&mut self, start: u32) -> Token {
        self.cursor.eat_while(is_ident_continue);
        let text = self.cursor.slice_from(start);

        // Check if it's a keyword
        if let Some(kind) = TokenKind::keyword_from_str(text) {
            return Token::new(kind, self.span(start));
        }

        // It's an identifier
        let symbol = sym(text);
        Token::with_value(
            TokenKind::Identifier,
            self.span(start),
            TokenValue::Ident(symbol),
        )
    }

    /// Creates a span from start to current position.
    const fn span(&self, start: u32) -> Span {
        Span::new(self.file_id, start, self.cursor.pos())
    }

    /// Records an error diagnostic.
    fn error(&mut self, message: impl Into<String>, span: Span) {
        self.diagnostics
            .push(Diagnostic::error(message).with_label(Label::primary(span, "")));
    }
}

/// Returns true if the character can start an identifier.
fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

/// Returns true if the character can continue an identifier.
fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(source: &str) -> Vec<Token> {
        let mut lexer = Lexer::new(FileId::new(0), source);
        lexer.tokenize()
    }

    fn lex_kinds(source: &str) -> Vec<TokenKind> {
        lex(source).into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn empty_source() {
        let tokens = lex("");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, TokenKind::Eof);
    }

    #[test]
    fn whitespace_only() {
        let tokens = lex("   \n\t  ");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, TokenKind::Eof);
    }

    #[test]
    fn single_tokens() {
        assert_eq!(lex_kinds("("), vec![TokenKind::LParen, TokenKind::Eof]);
        assert_eq!(lex_kinds(")"), vec![TokenKind::RParen, TokenKind::Eof]);
        assert_eq!(lex_kinds("{"), vec![TokenKind::LBrace, TokenKind::Eof]);
        assert_eq!(lex_kinds("}"), vec![TokenKind::RBrace, TokenKind::Eof]);
        assert_eq!(lex_kinds("["), vec![TokenKind::LBracket, TokenKind::Eof]);
        assert_eq!(lex_kinds("]"), vec![TokenKind::RBracket, TokenKind::Eof]);
    }

    #[test]
    fn multi_char_tokens() {
        assert_eq!(lex_kinds("::"), vec![TokenKind::ColonColon, TokenKind::Eof]);
        assert_eq!(lex_kinds(".."), vec![TokenKind::DotDot, TokenKind::Eof]);
        assert_eq!(lex_kinds("..="), vec![TokenKind::DotDotEq, TokenKind::Eof]);
        assert_eq!(lex_kinds("..."), vec![TokenKind::Ellipsis, TokenKind::Eof]);
        assert_eq!(lex_kinds("->"), vec![TokenKind::Arrow, TokenKind::Eof]);
        assert_eq!(lex_kinds("=>"), vec![TokenKind::FatArrow, TokenKind::Eof]);
        assert_eq!(
            lex_kinds("??"),
            vec![TokenKind::QuestionQuestion, TokenKind::Eof]
        );
        assert_eq!(
            lex_kinds("?."),
            vec![TokenKind::QuestionDot, TokenKind::Eof]
        );
    }

    #[test]
    fn comparison_operators() {
        assert_eq!(lex_kinds("=="), vec![TokenKind::EqEq, TokenKind::Eof]);
        assert_eq!(lex_kinds("!="), vec![TokenKind::BangEq, TokenKind::Eof]);
        assert_eq!(lex_kinds("<="), vec![TokenKind::LtEq, TokenKind::Eof]);
        assert_eq!(lex_kinds(">="), vec![TokenKind::GtEq, TokenKind::Eof]);
    }

    #[test]
    fn assignment_operators() {
        assert_eq!(lex_kinds("="), vec![TokenKind::Eq, TokenKind::Eof]);
        assert_eq!(lex_kinds("+="), vec![TokenKind::PlusEq, TokenKind::Eof]);
        assert_eq!(lex_kinds("-="), vec![TokenKind::MinusEq, TokenKind::Eof]);
        assert_eq!(lex_kinds("*="), vec![TokenKind::StarEq, TokenKind::Eof]);
        assert_eq!(lex_kinds("/="), vec![TokenKind::SlashEq, TokenKind::Eof]);
    }

    #[test]
    fn keywords() {
        assert_eq!(lex_kinds("fn"), vec![TokenKind::Fn, TokenKind::Eof]);
        assert_eq!(lex_kinds("if"), vec![TokenKind::If, TokenKind::Eof]);
        assert_eq!(lex_kinds("else"), vec![TokenKind::Else, TokenKind::Eof]);
        assert_eq!(lex_kinds("return"), vec![TokenKind::Return, TokenKind::Eof]);
        assert_eq!(lex_kinds("true"), vec![TokenKind::True, TokenKind::Eof]);
        assert_eq!(lex_kinds("false"), vec![TokenKind::False, TokenKind::Eof]);
        assert_eq!(lex_kinds("Null"), vec![TokenKind::Null, TokenKind::Eof]);
    }

    #[test]
    fn identifiers() {
        let tokens = lex("foo bar _baz foo123");
        assert_eq!(tokens[0].kind, TokenKind::Identifier);
        assert_eq!(tokens[0].as_ident().unwrap().as_str(), "foo");
        assert_eq!(tokens[1].kind, TokenKind::Identifier);
        assert_eq!(tokens[2].kind, TokenKind::Identifier);
        assert_eq!(tokens[3].kind, TokenKind::Identifier);
    }

    #[test]
    fn integer_literals() {
        let tokens = lex("42 0 1_000_000");
        assert_eq!(tokens[0].value, Some(TokenValue::Int(42)));
        assert_eq!(tokens[1].value, Some(TokenValue::Int(0)));
        assert_eq!(tokens[2].value, Some(TokenValue::Int(1_000_000)));
    }

    #[test]
    fn hex_literals() {
        let tokens = lex("0xFF 0x1A");
        assert_eq!(tokens[0].value, Some(TokenValue::Int(255)));
        assert_eq!(tokens[1].value, Some(TokenValue::Int(26)));
    }

    #[test]
    fn binary_literals() {
        let tokens = lex("0b1010 0b1111");
        assert_eq!(tokens[0].value, Some(TokenValue::Int(10)));
        assert_eq!(tokens[1].value, Some(TokenValue::Int(15)));
    }

    #[test]
    fn octal_literals() {
        let tokens = lex("0o755 0o10");
        assert_eq!(tokens[0].value, Some(TokenValue::Int(493)));
        assert_eq!(tokens[1].value, Some(TokenValue::Int(8)));
    }

    #[test]
    fn float_literals() {
        let tokens = lex("3.14 1.0 1e10 1.5e-3");
        #[allow(clippy::approx_constant)]
        {
            assert!(
                matches!(tokens[0].value, Some(TokenValue::Float(f)) if (f - 3.14).abs() < 0.001)
            );
        }
        assert!(matches!(tokens[1].value, Some(TokenValue::Float(f)) if (f - 1.0).abs() < 0.001));
        assert!(matches!(tokens[2].value, Some(TokenValue::Float(f)) if (f - 1e10).abs() < 1e5));
        assert!(
            matches!(tokens[3].value, Some(TokenValue::Float(f)) if (f - 1.5e-3).abs() < 0.0001)
        );
    }

    #[test]
    fn string_literals() {
        let tokens = lex(r#""hello" "world""#);
        assert_eq!(tokens[0].kind, TokenKind::StringLiteral);
        assert_eq!(tokens[1].kind, TokenKind::StringLiteral);
    }

    #[test]
    fn string_escapes() {
        let tokens = lex(r#""\n\t\r\\\"" "#);
        let value = tokens[0].value.as_ref().unwrap().as_symbol().unwrap();
        assert_eq!(value.as_str(), "\n\t\r\\\"");
    }

    #[test]
    fn char_literals() {
        let tokens = lex("'a' '\\n' '\\t'");
        assert_eq!(tokens[0].value, Some(TokenValue::Char('a')));
        assert_eq!(tokens[1].value, Some(TokenValue::Char('\n')));
        assert_eq!(tokens[2].value, Some(TokenValue::Char('\t')));
    }

    #[test]
    fn line_comments() {
        let tokens = lex("foo // comment\nbar");
        assert_eq!(tokens[0].kind, TokenKind::Identifier);
        assert_eq!(tokens[1].kind, TokenKind::Identifier);
        assert_eq!(tokens[2].kind, TokenKind::Eof);
    }

    #[test]
    fn block_comments() {
        let tokens = lex("foo /* comment */ bar");
        assert_eq!(tokens[0].kind, TokenKind::Identifier);
        assert_eq!(tokens[1].kind, TokenKind::Identifier);
    }

    #[test]
    fn nested_block_comments() {
        let tokens = lex("foo /* outer /* inner */ outer */ bar");
        assert_eq!(tokens[0].kind, TokenKind::Identifier);
        assert_eq!(tokens[1].kind, TokenKind::Identifier);
    }

    #[test]
    fn function_declaration() {
        let kinds = lex_kinds("fn main(): Void { }");
        assert_eq!(
            kinds,
            vec![
                TokenKind::Fn,
                TokenKind::Identifier,
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::Colon,
                TokenKind::Identifier,
                TokenKind::LBrace,
                TokenKind::RBrace,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn variable_declaration() {
        let kinds = lex_kinds("x: Int = 42");
        assert_eq!(
            kinds,
            vec![
                TokenKind::Identifier,
                TokenKind::Colon,
                TokenKind::Identifier,
                TokenKind::Eq,
                TokenKind::IntLiteral,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn spans_are_correct() {
        let tokens = lex("fn main");
        assert_eq!(tokens[0].span.start, 0);
        assert_eq!(tokens[0].span.end, 2);
        assert_eq!(tokens[1].span.start, 3);
        assert_eq!(tokens[1].span.end, 7);
    }

    #[test]
    fn unknown_character_error() {
        let mut lexer = Lexer::new(FileId::new(0), "foo § bar");
        let tokens = lexer.tokenize();

        assert!(tokens.iter().any(|t| t.kind == TokenKind::Error));
        assert!(lexer.diagnostics().has_errors());
    }

    // ====================================================================
    // String Interpolation Tests
    // ====================================================================

    #[test]
    fn simple_string_unchanged() {
        // Strings without interpolation are still single StringLiteral tokens.
        let tokens = lex(r#""hello""#);
        assert_eq!(tokens[0].kind, TokenKind::StringLiteral);
        let val = tokens[0].value.as_ref().unwrap().as_symbol().unwrap();
        assert_eq!(val.as_str(), "hello");
    }

    #[test]
    fn empty_string_unchanged() {
        let tokens = lex(r#""""#);
        assert_eq!(tokens[0].kind, TokenKind::StringLiteral);
        let val = tokens[0].value.as_ref().unwrap().as_symbol().unwrap();
        assert_eq!(val.as_str(), "");
    }

    #[test]
    fn escaped_brace_no_interpolation() {
        // \{ should produce a simple string literal with a literal {
        let tokens = lex(r#""\{brace}""#);
        assert_eq!(tokens[0].kind, TokenKind::StringLiteral);
        let val = tokens[0].value.as_ref().unwrap().as_symbol().unwrap();
        assert_eq!(val.as_str(), "{brace}");
    }

    #[test]
    fn interpolated_string_simple() {
        // "hello {name}" -> StringStart, StringText("hello "), InterpStart, Ident(name), RBrace, StringEnd
        let kinds = lex_kinds(r#""hello {name}""#);
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringStart,
                TokenKind::StringText,
                TokenKind::InterpStart,
                TokenKind::Identifier,
                TokenKind::RBrace,
                TokenKind::StringEnd,
                TokenKind::Eof,
            ]
        );

        // Check the text value.
        let tokens = lex(r#""hello {name}""#);
        let text_val = tokens[1].value.as_ref().unwrap().as_symbol().unwrap();
        assert_eq!(text_val.as_str(), "hello ");
    }

    #[test]
    fn interpolated_string_multiple() {
        // "a {x} b {y}" -> StringStart, Text("a "), InterpStart, Ident(x), RBrace,
        //                   Text(" b "), InterpStart, Ident(y), RBrace, StringEnd
        let kinds = lex_kinds(r#""a {x} b {y}""#);
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringStart,
                TokenKind::StringText,
                TokenKind::InterpStart,
                TokenKind::Identifier,
                TokenKind::RBrace,
                TokenKind::StringText,
                TokenKind::InterpStart,
                TokenKind::Identifier,
                TokenKind::RBrace,
                TokenKind::StringEnd,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn interpolated_string_expr() {
        // "{x + 1}" -> StringStart, InterpStart, Ident(x), Plus, IntLiteral(1), RBrace, StringEnd
        let kinds = lex_kinds(r#""{x + 1}""#);
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringStart,
                TokenKind::InterpStart,
                TokenKind::Identifier,
                TokenKind::Plus,
                TokenKind::IntLiteral,
                TokenKind::RBrace,
                TokenKind::StringEnd,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn interpolated_string_trailing_text() {
        // "{x}!" -> StringStart, InterpStart, Ident(x), RBrace, StringText("!"), StringEnd
        let kinds = lex_kinds(r#""{x}!""#);
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringStart,
                TokenKind::InterpStart,
                TokenKind::Identifier,
                TokenKind::RBrace,
                TokenKind::StringText,
                TokenKind::StringEnd,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn interpolated_string_nested_braces() {
        // "result: {foo({x})}" -> nested call with braces should work
        // The { inside foo( is LBrace depth tracking
        // Actually foo({x}) has ( ) not { }, so let's test Map literal:
        // "{a}" where a is an ident
        // For actual nested braces, we need block expressions: "{ {1} }"
        let kinds = lex_kinds(r#""val: { {1} }""#);
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringStart,
                TokenKind::StringText,  // "val: "
                TokenKind::InterpStart, // {
                TokenKind::LBrace,      // { (inner, depth goes to 1)
                TokenKind::IntLiteral,  // 1
                TokenKind::RBrace,      // } (inner, depth back to 0)
                TokenKind::RBrace,      // } (closes interpolation)
                TokenKind::StringEnd,   // "
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn interpolated_string_adjacent_interps() {
        // "{x}{y}" -> StringStart, InterpStart, Ident(x), RBrace, InterpStart, Ident(y), RBrace, StringEnd
        let kinds = lex_kinds(r#""{x}{y}""#);
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringStart,
                TokenKind::InterpStart,
                TokenKind::Identifier,
                TokenKind::RBrace,
                TokenKind::InterpStart,
                TokenKind::Identifier,
                TokenKind::RBrace,
                TokenKind::StringEnd,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn interpolated_string_with_string_inside() {
        // Simple string inside interpolation: "hello {"world"}"
        let kinds = lex_kinds(r#""hello {"world"}""#);
        assert_eq!(
            kinds,
            vec![
                TokenKind::StringStart,
                TokenKind::StringText,    // "hello "
                TokenKind::InterpStart,   // {
                TokenKind::StringLiteral, // "world" (simple string, no interp)
                TokenKind::RBrace,        // }
                TokenKind::StringEnd,     // "
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn interpolated_string_after_normal_tokens() {
        // Tokens before and after interpolated string.
        let kinds = lex_kinds(r#"x = "val: {y}""#);
        assert_eq!(
            kinds,
            vec![
                TokenKind::Identifier,  // x
                TokenKind::Eq,          // =
                TokenKind::StringStart, // "
                TokenKind::StringText,  // "val: "
                TokenKind::InterpStart, // {
                TokenKind::Identifier,  // y
                TokenKind::RBrace,      // }
                TokenKind::StringEnd,   // "
                TokenKind::Eof,
            ]
        );
    }
}
