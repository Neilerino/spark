//! The main lexer implementation.

use crate::cursor::Cursor;
use crate::token::{Token, TokenKind, TokenValue};
use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_intern::sym;
use spark_span::{FileId, Span};

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
}

impl<'a> Lexer<'a> {
    /// Creates a new lexer for the given source.
    #[must_use]
    pub fn new(file_id: FileId, source: &'a str) -> Self {
        Self {
            file_id,
            cursor: Cursor::new(source),
            diagnostics: Diagnostics::new(),
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
                self.error(
                    format!("unexpected character `{c}`"),
                    self.span(start),
                );
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

    /// Scans a string literal.
    fn scan_string(&mut self, start: u32) -> Token {
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
        Token::with_value(TokenKind::StringLiteral, self.span(start), TokenValue::String(symbol))
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
            Some(c) => Token::with_value(TokenKind::CharLiteral, self.span(start), TokenValue::Char(c)),
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
        self.cursor.eat_while(|c| matches!(c, '0'..='7') || c == '_');
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
            Token::with_value(TokenKind::FloatLiteral, self.span(start), TokenValue::Float(n))
        } else {
            self.error(
                format!("invalid float literal `{text}`"),
                self.span(start),
            );
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
        Token::with_value(TokenKind::Identifier, self.span(start), TokenValue::Ident(symbol))
    }

    /// Creates a span from start to current position.
    const fn span(&self, start: u32) -> Span {
        Span::new(self.file_id, start, self.cursor.pos())
    }

    /// Records an error diagnostic.
    fn error(&mut self, message: impl Into<String>, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(message).with_label(Label::primary(span, "")),
        );
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
        assert_eq!(lex_kinds("??"), vec![TokenKind::QuestionQuestion, TokenKind::Eof]);
        assert_eq!(lex_kinds("?."), vec![TokenKind::QuestionDot, TokenKind::Eof]);
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
        assert!(matches!(tokens[0].value, Some(TokenValue::Float(f)) if (f - 3.14).abs() < 0.001));
        assert!(matches!(tokens[1].value, Some(TokenValue::Float(f)) if (f - 1.0).abs() < 0.001));
        assert!(matches!(tokens[2].value, Some(TokenValue::Float(f)) if (f - 1e10).abs() < 1e5));
        assert!(matches!(tokens[3].value, Some(TokenValue::Float(f)) if (f - 1.5e-3).abs() < 0.0001));
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
}
