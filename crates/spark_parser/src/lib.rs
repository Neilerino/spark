//! # `spark_parser`
//!
//! Recursive descent parser for the Spark programming language.
//!
//! This crate transforms a token stream from `spark_lexer` into an Abstract
//! Syntax Tree defined in `spark_ast`.
//!
//! ## Quick Start
//!
//! ```
//! use spark_lexer::Lexer;
//! use spark_parser::parse;
//! use spark_span::FileId;
//!
//! let source = "fn main(): Void { }";
//! let file_id = FileId::new(0);
//! let mut lexer = Lexer::new(file_id, source);
//! let tokens = lexer.tokenize();
//!
//! let (ast, diagnostics) = parse(tokens, file_id);
//!
//! assert!(!diagnostics.has_errors());
//! assert_eq!(ast.items.len(), 1);
//! ```
//!
//! ## Error Recovery
//!
//! The parser implements error recovery to report multiple syntax errors per file.
//! When an unexpected token is encountered, the parser:
//! 1. Emits a diagnostic with the error location
//! 2. Returns an error node (`ExprKind::Error`, `TypeExprKind::Error`, etc.)
//! 3. Synchronizes to a known recovery point (semicolon, brace, or keyword)
//!
//! ## Design
//!
//! - **Pratt parsing** for expressions with proper operator precedence
//! - **Recursive descent** for statements, declarations, and types
//! - **Span tracking** for all AST nodes
//! - **Diagnostic collection** for error reporting

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod decl;
mod expr;
mod pattern;
mod stmt;
mod types;

use spark_ast::{Ast, Expr, Ident, Item, Path};
use spark_diagnostics::{Diagnostic, Diagnostics, Label};
use spark_lexer::{Token, TokenKind};
use spark_span::{FileId, Span};

/// Parses a token stream into an AST.
///
/// Returns the AST along with any diagnostics (errors or warnings) encountered
/// during parsing.
///
/// # Example
///
/// ```
/// use spark_lexer::Lexer;
/// use spark_parser::parse;
/// use spark_span::FileId;
///
/// let source = "fn greet(name: String): Void { }";
/// let file_id = FileId::new(0);
/// let mut lexer = Lexer::new(file_id, source);
/// let tokens = lexer.tokenize();
///
/// let (ast, diagnostics) = parse(tokens, file_id);
/// ```
#[must_use]
pub fn parse(tokens: Vec<Token>, file_id: FileId) -> (Ast, Diagnostics) {
    let mut parser = Parser::new(tokens, file_id);
    let ast = parser.parse_file();
    (ast, parser.diagnostics)
}

/// Parses a single expression from a token stream.
///
/// Useful for REPL evaluation or testing expression parsing in isolation.
#[must_use]
pub fn parse_expr(tokens: Vec<Token>, file_id: FileId) -> (Expr, Diagnostics) {
    let mut parser = Parser::new(tokens, file_id);
    let expr = parser.parse_expr();
    (expr, parser.diagnostics)
}

/// The parser state.
pub struct Parser {
    /// The token stream being parsed.
    tokens: Vec<Token>,
    /// Current position in the token stream.
    pos: usize,
    /// Accumulated diagnostics.
    diagnostics: Diagnostics,
    /// The file being parsed.
    file_id: FileId,
}

impl Parser {
    /// Creates a new parser for the given token stream.
    fn new(tokens: Vec<Token>, file_id: FileId) -> Self {
        Self {
            tokens,
            pos: 0,
            diagnostics: Diagnostics::new(),
            file_id,
        }
    }

    // ========================================================================
    // Token Navigation
    // ========================================================================

    /// Returns the current token without consuming it.
    fn current(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or_else(|| {
            self.tokens
                .last()
                .expect("token stream should have at least EOF")
        })
    }

    /// Returns the current token's kind.
    fn current_kind(&self) -> TokenKind {
        self.current().kind
    }

    /// Returns the current token's span.
    fn current_span(&self) -> Span {
        self.current().span
    }

    /// Peeks at the next token without consuming it.
    fn peek(&self) -> &Token {
        self.peek_nth(1)
    }

    /// Peeks at the nth token ahead without consuming.
    fn peek_nth(&self, n: usize) -> &Token {
        self.tokens.get(self.pos + n).unwrap_or_else(|| {
            self.tokens
                .last()
                .expect("token stream should have at least EOF")
        })
    }

    /// Returns true if the current token is of the given kind.
    fn check(&self, kind: TokenKind) -> bool {
        self.current_kind() == kind
    }

    /// Returns true if the current token is any of the given kinds.
    fn check_any(&self, kinds: &[TokenKind]) -> bool {
        kinds.contains(&self.current_kind())
    }

    /// Returns true if we're at the end of input.
    fn at_end(&self) -> bool {
        self.check(TokenKind::Eof)
    }

    /// Advances to the next token and returns the consumed token.
    fn advance(&mut self) -> Token {
        if !self.at_end() {
            self.pos += 1;
        }
        self.tokens
            .get(self.pos - 1)
            .cloned()
            .unwrap_or_else(|| self.tokens.last().cloned().unwrap())
    }

    /// Consumes the current token if it matches the given kind.
    /// Returns the token if consumed, None otherwise.
    fn eat(&mut self, kind: TokenKind) -> Option<Token> {
        if self.check(kind) {
            Some(self.advance())
        } else {
            None
        }
    }

    /// Consumes the current token if it matches the given kind.
    /// Emits an error diagnostic if it doesn't match.
    fn expect(&mut self, kind: TokenKind) -> Option<Token> {
        if self.check(kind) {
            Some(self.advance())
        } else {
            self.error_expected(kind.as_str());
            None
        }
    }

    /// Consumes an identifier token and returns its symbol.
    fn expect_ident(&mut self) -> Option<Ident> {
        if self.check(TokenKind::Identifier) {
            let token = self.advance();
            let symbol = token
                .as_ident()
                .expect("identifier token should have symbol");
            Some(Ident::new(symbol, token.span))
        } else {
            self.error_expected("identifier");
            None
        }
    }

    // ========================================================================
    // Error Handling
    // ========================================================================

    /// Emits an error diagnostic with the given message.
    fn error(&mut self, message: &str) {
        let span = self.current_span();
        self.diagnostics
            .push(Diagnostic::error(message).with_label(Label::primary(span, "here")));
    }

    /// Emits an "expected X" error diagnostic.
    fn error_expected(&mut self, expected: &str) {
        let span = self.current_span();
        let found = self.current_kind();
        self.diagnostics.push(
            Diagnostic::error(format!("expected {expected}, found {found}"))
                .with_label(Label::primary(span, format!("expected {expected}"))),
        );
    }

    /// Emits an "expected X" error at a specific span.
    #[allow(dead_code)]
    fn error_expected_at(&mut self, expected: &str, span: Span) {
        self.diagnostics.push(
            Diagnostic::error(format!("expected {expected}"))
                .with_label(Label::primary(span, format!("expected {expected}"))),
        );
    }

    /// Synchronizes the parser after an error by skipping tokens until a
    /// recovery point is found.
    fn synchronize(&mut self) {
        while !self.at_end() {
            // Stop at statement/item boundaries
            match self.current_kind() {
                // Statement terminators
                TokenKind::Semicolon => {
                    self.advance();
                    return;
                }
                // Block end
                // Item keywords
                TokenKind::RBrace
                | TokenKind::Fn
                | TokenKind::Struct
                | TokenKind::Enum
                | TokenKind::Interface
                | TokenKind::Type
                | TokenKind::Alias
                | TokenKind::Const
                | TokenKind::Pub
                | TokenKind::Module
                | TokenKind::Using
                | TokenKind::Import => return,
                _ => {
                    self.advance();
                }
            }
        }
    }

    /// Synchronizes within a block by skipping to the next statement.
    #[allow(dead_code)]
    fn synchronize_stmt(&mut self) {
        while !self.at_end() {
            match self.current_kind() {
                TokenKind::RBrace => return,
                TokenKind::Semicolon => {
                    // Don't consume - let the caller handle it
                    return;
                }
                // Control flow that starts new statements
                TokenKind::If
                | TokenKind::Match
                | TokenKind::Loop
                | TokenKind::Return
                | TokenKind::Break
                | TokenKind::Continue => return,
                _ => {
                    self.advance();
                }
            }
        }
    }

    // ========================================================================
    // Utility Helpers
    // ========================================================================

    /// Creates a span from a start position to the current position.
    fn span_from(&self, start: Span) -> Span {
        let end_span = if self.pos > 0 {
            self.tokens.get(self.pos - 1).map_or(start, |t| t.span)
        } else {
            start
        };
        start.merge(end_span)
    }

    /// Creates an error expression for recovery.
    fn error_expr(&self) -> Expr {
        Expr::error(self.current_span())
    }

    /// Parses a path (one or more dot-separated identifiers).
    fn parse_path(&mut self) -> Path {
        let start = self.current_span();
        let mut segments = Vec::new();

        // First segment (required)
        if let Some(ident) = self.expect_ident() {
            segments.push(ident);
        } else {
            // Error recovery: return empty path with error span
            return Path::new(segments, start);
        }

        // Additional segments
        while self.eat(TokenKind::Dot).is_some() {
            if let Some(ident) = self.expect_ident() {
                segments.push(ident);
            } else {
                break;
            }
        }

        let span = self.span_from(start);
        Path::new(segments, span)
    }

    /// Parses a simple identifier as a path.
    #[allow(dead_code)]
    fn parse_ident_as_path(&mut self) -> Option<Path> {
        let ident = self.expect_ident()?;
        Some(Path::from_ident(ident))
    }

    /// Returns true if the current token can start an expression.
    fn can_start_expr(&self) -> bool {
        matches!(
            self.current_kind(),
            // Literals
            TokenKind::IntLiteral
                | TokenKind::FloatLiteral
                | TokenKind::StringLiteral
                | TokenKind::CharLiteral
                | TokenKind::True
                | TokenKind::False
                | TokenKind::Null
                // Identifiers
                | TokenKind::Identifier
                // Grouping and collections
                | TokenKind::LParen
                | TokenKind::LBracket
                | TokenKind::LBrace
                // Prefix operators
                | TokenKind::Minus
                | TokenKind::Not
                | TokenKind::Tilde
                // Control flow
                | TokenKind::If
                | TokenKind::Match
                | TokenKind::Loop
                | TokenKind::Return
                | TokenKind::Break
                | TokenKind::Continue
                // Async
                | TokenKind::Await
                | TokenKind::Block
                | TokenKind::Spawn
                // Lambda
                | TokenKind::Fn
        )
    }

    /// Returns true if the current token can start an item.
    #[allow(dead_code)]
    fn can_start_item(&self) -> bool {
        matches!(
            self.current_kind(),
            TokenKind::Fn
                | TokenKind::Struct
                | TokenKind::Enum
                | TokenKind::Interface
                | TokenKind::Type
                | TokenKind::Alias
                | TokenKind::Const
                | TokenKind::Pub
                | TokenKind::Async
                | TokenKind::At // Decorator
        ) || self.is_binding_start()
    }

    /// Returns true if we're at the start of a binding declaration.
    fn is_binding_start(&self) -> bool {
        // Bindings start with a type (identifier) followed by `binds`
        if !self.check(TokenKind::Identifier) {
            return false;
        }
        // Look ahead for `binds`
        let mut lookahead = 1;
        // Skip potential path segments
        while self.peek_nth(lookahead).kind == TokenKind::Dot {
            lookahead += 2; // Skip dot and next identifier
        }
        // Skip potential generic args
        if self.peek_nth(lookahead).kind == TokenKind::LBracket {
            // Simple heuristic: look for closing bracket
            let mut depth = 1;
            lookahead += 1;
            while depth > 0 && lookahead < 100 {
                match self.peek_nth(lookahead).kind {
                    TokenKind::LBracket => depth += 1,
                    TokenKind::RBracket => depth -= 1,
                    TokenKind::Eof => return false,
                    _ => {}
                }
                lookahead += 1;
            }
        }
        self.peek_nth(lookahead).kind == TokenKind::Binds
    }

    // ========================================================================
    // Top-Level Parsing
    // ========================================================================

    /// Parses a complete file.
    fn parse_file(&mut self) -> Ast {
        let mut ast = Ast::new(self.file_id);

        // Parse module declaration (optional, must be first)
        if self.check(TokenKind::Module) {
            ast.module_decl = Some(self.parse_module_decl());
        }

        // Parse using declaration (optional, must come before imports)
        if self.check(TokenKind::Using) {
            ast.using_decl = Some(self.parse_using_decl());
        }

        // Parse imports
        while self.check(TokenKind::Import)
            || (self.check(TokenKind::Pub) && self.peek().kind == TokenKind::Import)
        {
            ast.imports.push(self.parse_import_decl());
        }

        // Parse items
        while !self.at_end() {
            let pos_before = self.pos;

            if let Some(item) = self.parse_item() {
                ast.items.push(item);
            } else {
                // Error recovery: skip to next item
                self.synchronize();
            }

            // Safety: ensure we always make progress to prevent infinite loops
            if self.pos == pos_before && !self.at_end() {
                self.advance();
            }
        }

        ast
    }

    /// Parses a top-level item.
    fn parse_item(&mut self) -> Option<Item> {
        // Handle decorators
        let decorators = self.parse_decorators();

        // Handle visibility
        let is_pub = self.eat(TokenKind::Pub).is_some();

        // Handle async
        let is_async = self.eat(TokenKind::Async).is_some();

        let item = match self.current_kind() {
            TokenKind::Fn => Some(self.parse_function(is_pub, is_async, decorators)),
            TokenKind::Struct => Some(self.parse_struct(is_pub)),
            TokenKind::Enum => Some(self.parse_enum(is_pub)),
            TokenKind::Interface => Some(self.parse_interface(is_pub)),
            TokenKind::Type => Some(self.parse_type_def(is_pub)),
            TokenKind::Alias => Some(self.parse_type_alias(is_pub)),
            TokenKind::Const => Some(self.parse_const(is_pub)),
            TokenKind::Identifier if self.is_binding_start() => Some(self.parse_binding()),
            _ => {
                self.error_expected("item (fn, struct, enum, interface, type, alias, const)");
                None
            }
        };

        // Validate: async only valid on functions
        if is_async && !matches!(&item, Some(Item::Function(_))) {
            self.error("'async' modifier is only valid on functions");
        }

        item
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_ast::ExprKind;
    use spark_lexer::Lexer;

    fn parse_source(source: &str) -> (Ast, Diagnostics) {
        let file_id = FileId::new(0);
        let mut lexer = Lexer::new(file_id, source);
        let tokens = lexer.tokenize();
        parse(tokens, file_id)
    }

    fn parse_expression(source: &str) -> (Expr, Diagnostics) {
        let file_id = FileId::new(0);
        let mut lexer = Lexer::new(file_id, source);
        let tokens = lexer.tokenize();
        parse_expr(tokens, file_id)
    }

    #[test]
    fn empty_file() {
        let (ast, diags) = parse_source("");
        assert!(!diags.has_errors());
        assert!(ast.items.is_empty());
    }

    #[test]
    fn simple_function() {
        let (ast, diags) = parse_source("fn main(): Void { }");
        assert!(!diags.has_errors(), "errors: {:?}", diags);
        assert_eq!(ast.items.len(), 1);
        assert!(ast.items[0].is_function());
    }

    #[test]
    fn pub_function() {
        let (ast, diags) = parse_source("pub fn greet(): Void { }");
        assert!(!diags.has_errors());
        assert_eq!(ast.items.len(), 1);
        if let Item::Function(f) = &ast.items[0] {
            assert!(f.visibility.is_public());
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn async_function() {
        let (ast, diags) = parse_source("async fn fetch(): Data { }");
        assert!(!diags.has_errors());
        if let Item::Function(f) = &ast.items[0] {
            assert!(f.is_async);
        } else {
            panic!("expected function");
        }
    }

    #[test]
    fn module_declaration() {
        let (ast, diags) = parse_source("module user\n\nfn create(): User { }");
        assert!(!diags.has_errors());
        assert!(ast.module_decl.is_some());
        assert_eq!(ast.items.len(), 1);
    }

    #[test]
    fn integer_literal() {
        let (expr, diags) = parse_expression("42");
        assert!(!diags.has_errors());
        assert!(matches!(expr.kind, ExprKind::Literal(_)));
    }

    #[test]
    fn binary_expression() {
        let (expr, diags) = parse_expression("1 + 2 * 3");
        assert!(!diags.has_errors());
        // Should parse as 1 + (2 * 3) due to precedence
        if let ExprKind::Binary { op, .. } = expr.kind {
            assert_eq!(op, spark_ast::BinaryOp::Add);
        } else {
            panic!("expected binary expression");
        }
    }

    #[test]
    fn error_recovery() {
        let (ast, diags) = parse_source("fn main( { }\nfn other(): Void { }");
        // Should have errors but still parse second function
        assert!(diags.has_errors());
        // May have parsed partial items
        assert!(!ast.items.is_empty());
    }
}
