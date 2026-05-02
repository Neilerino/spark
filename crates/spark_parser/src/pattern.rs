//! Pattern parsing for match expressions.
//!
//! Handles parsing of patterns including:
//! - Wildcard: `_`
//! - Identifier: `x`, `x: Type`
//! - Literal: `42`, `"hello"`, `true`
//! - Variant: `Enum.Variant`, `Enum.Variant(a, b)`
//! - Struct: `Struct { field, field: pattern, .. }`
//! - Tuple: `(a, b, c)`
//! - List: `[first, second, ...rest]`
//! - Type narrowing: `name: Type`
//! - Or pattern: `A | B`
//! - Guard: `pattern if condition`

use spark_ast::{FieldPattern, Ident, Literal, LiteralKind, Path, Pattern, PatternKind};
use spark_intern::sym;
use spark_lexer::TokenKind;
use spark_span::Span;

use crate::Parser;

impl Parser {
    /// Parses a pattern.
    ///
    /// Pattern = `OrPattern`
    pub fn parse_pattern(&mut self) -> Pattern {
        self.parse_or_pattern()
    }

    /// Parses an or pattern.
    ///
    /// `OrPattern` = `GuardPattern` ("|" `GuardPattern`)*
    fn parse_or_pattern(&mut self) -> Pattern {
        let start = self.current_span();
        let first = self.parse_guard_pattern();

        if !self.check(TokenKind::Pipe) {
            return first;
        }

        let mut patterns = vec![first];
        while self.eat(TokenKind::Pipe).is_some() {
            patterns.push(self.parse_guard_pattern());
        }

        let span = self.span_from(start);
        Pattern::new(PatternKind::Or(patterns), span)
    }

    /// Parses a pattern with optional guard.
    ///
    /// `GuardPattern` = `PrimaryPattern` ("if" Expr)?
    fn parse_guard_pattern(&mut self) -> Pattern {
        let start = self.current_span();
        let pattern = self.parse_primary_pattern();

        if self.eat(TokenKind::If).is_some() {
            let guard = self.parse_expr();
            let span = self.span_from(start);
            return Pattern::new(
                PatternKind::Guard {
                    pattern: Box::new(pattern),
                    guard: Box::new(guard),
                },
                span,
            );
        }

        pattern
    }

    /// Parses a primary pattern.
    fn parse_primary_pattern(&mut self) -> Pattern {
        let start = self.current_span();

        match self.current_kind() {
            // Wildcard: _
            TokenKind::Identifier if self.is_underscore() => {
                self.advance();
                Pattern::wildcard(self.span_from(start))
            }

            // Literal patterns
            TokenKind::IntLiteral => {
                let token = self.advance();
                let value = token
                    .value
                    .as_ref()
                    .and_then(spark_lexer::TokenValue::as_int)
                    .unwrap_or(0);
                let lit = Literal::int(value, token.span);
                Pattern::new(PatternKind::Literal(lit), token.span)
            }

            TokenKind::FloatLiteral => {
                let token = self.advance();
                let value = token
                    .value
                    .as_ref()
                    .and_then(spark_lexer::TokenValue::as_float)
                    .unwrap_or(0.0);
                let lit = Literal::float(value, token.span);
                Pattern::new(PatternKind::Literal(lit), token.span)
            }

            TokenKind::StringLiteral => {
                let token = self.advance();
                let text = token
                    .value
                    .as_ref()
                    .and_then(spark_lexer::TokenValue::as_symbol)
                    .unwrap_or_else(|| sym(""));
                let lit = Literal::new(
                    LiteralKind::String(spark_ast::StringLiteral::simple(text)),
                    token.span,
                );
                Pattern::new(PatternKind::Literal(lit), token.span)
            }

            TokenKind::CharLiteral => {
                let token = self.advance();
                let value = token
                    .value
                    .as_ref()
                    .and_then(spark_lexer::TokenValue::as_char)
                    .unwrap_or('\0');
                let lit = Literal::char(value, token.span);
                Pattern::new(PatternKind::Literal(lit), token.span)
            }

            TokenKind::True => {
                let token = self.advance();
                let lit = Literal::bool(true, token.span);
                Pattern::new(PatternKind::Literal(lit), token.span)
            }

            TokenKind::False => {
                let token = self.advance();
                let lit = Literal::bool(false, token.span);
                Pattern::new(PatternKind::Literal(lit), token.span)
            }

            TokenKind::Null => {
                let token = self.advance();
                let lit = Literal::null(token.span);
                Pattern::new(PatternKind::Literal(lit), token.span)
            }

            // Tuple pattern: (a, b, c)
            TokenKind::LParen => self.parse_tuple_pattern(),

            // List pattern: [a, b, ...rest]
            TokenKind::LBracket => self.parse_list_pattern(),

            // Identifier or path pattern
            TokenKind::Identifier => self.parse_ident_or_path_pattern(),

            _ => {
                self.error_expected("pattern");
                Pattern::new(PatternKind::Error, start)
            }
        }
    }

    /// Returns true if current token is `_`.
    fn is_underscore(&self) -> bool {
        if let Some(sym) = self.current().as_ident() {
            return sym.as_str() == "_";
        }
        false
    }

    /// Parses a tuple pattern.
    ///
    /// `TuplePattern` = "(" (Pattern ("," Pattern)*)? ")"
    fn parse_tuple_pattern(&mut self) -> Pattern {
        let start = self.current_span();
        self.expect(TokenKind::LParen);

        let mut patterns = Vec::new();
        if !self.check(TokenKind::RParen) {
            patterns.push(self.parse_pattern());
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RParen) {
                    break;
                }
                patterns.push(self.parse_pattern());
            }
        }

        self.expect(TokenKind::RParen);
        let span = self.span_from(start);

        // Single element without trailing comma is just a grouped pattern
        if patterns.len() == 1 {
            return patterns
                .into_iter()
                .next()
                .expect("ICE: len() == 1 guarantees at least one element");
        }

        Pattern::new(PatternKind::Tuple(patterns), span)
    }

    /// Parses a list pattern.
    ///
    /// `ListPattern` = "[" (Pattern ("," Pattern)* ("," "..." Pattern)?)? "]"
    fn parse_list_pattern(&mut self) -> Pattern {
        let start = self.current_span();
        self.expect(TokenKind::LBracket);

        let mut elements = Vec::new();
        let mut rest = None;

        if !self.check(TokenKind::RBracket) {
            // Check for rest pattern first
            if self.eat(TokenKind::Ellipsis).is_some() {
                rest = Some(Box::new(self.parse_pattern()));
            } else {
                elements.push(self.parse_pattern());
                while self.eat(TokenKind::Comma).is_some() {
                    if self.check(TokenKind::RBracket) {
                        break;
                    }
                    if self.eat(TokenKind::Ellipsis).is_some() {
                        rest = Some(Box::new(self.parse_pattern()));
                        break;
                    }
                    elements.push(self.parse_pattern());
                }
            }
        }

        self.expect(TokenKind::RBracket);
        let span = self.span_from(start);

        Pattern::new(PatternKind::List { elements, rest }, span)
    }

    /// Parses an identifier or path pattern.
    ///
    /// This handles:
    /// - Simple identifier: `x`
    /// - Type-annotated identifier: `x: Int`
    /// - Type narrowing: `user: User`
    /// - Variant pattern: `Status.Active`
    /// - Variant with fields: `Result.Error(msg)`
    /// - Struct pattern: `User { name, age }`
    fn parse_ident_or_path_pattern(&mut self) -> Pattern {
        let start = self.current_span();
        let path = self.parse_path();

        // Check for struct destructuring: { ... }
        if self.check(TokenKind::LBrace) {
            return self.parse_struct_pattern(path, start);
        }

        // Check for variant with fields: (...)
        if self.check(TokenKind::LParen) {
            return self.parse_variant_pattern(path, start);
        }

        // Check for type annotation: identifier : Type
        if path.is_simple() && self.check(TokenKind::Colon) {
            let name = path
                .segments
                .into_iter()
                .next()
                .expect("ICE: is_simple() guarantees at least one segment");
            self.advance(); // consume ':'

            let ty = self.parse_type();
            let span = self.span_from(start);

            // This could be either:
            // 1. Type-annotated binding: `x: Int` - binds x and checks type
            // 2. Type narrowing: `user: User` - narrows union to specific type
            // We'll use TypeNarrowing for both in the AST
            return Pattern::new(PatternKind::TypeNarrowing { name, ty }, span);
        }

        // Simple path - could be variant or just identifier
        if path.is_simple() {
            let ident = path
                .segments
                .into_iter()
                .next()
                .expect("ICE: is_simple() guarantees at least one segment");
            return Pattern::ident(ident);
        }

        // Multi-segment path is a variant
        let span = self.span_from(start);
        Pattern::new(
            PatternKind::Variant {
                path,
                fields: vec![],
            },
            span,
        )
    }

    /// Parses a struct destructuring pattern.
    ///
    /// `StructPattern` = Path "{" (`FieldPattern` ("," `FieldPattern`)* ","? ".."?)? "}"
    fn parse_struct_pattern(&mut self, path: Path, start: Span) -> Pattern {
        self.expect(TokenKind::LBrace);

        let mut fields = Vec::new();
        let mut rest = false;

        if !self.check(TokenKind::RBrace) {
            // Check for just `..`
            if self.eat(TokenKind::DotDot).is_some() {
                rest = true;
            } else {
                fields.push(self.parse_field_pattern());
                while self.eat(TokenKind::Comma).is_some() {
                    if self.check(TokenKind::RBrace) {
                        break;
                    }
                    if self.eat(TokenKind::DotDot).is_some() {
                        rest = true;
                        break;
                    }
                    fields.push(self.parse_field_pattern());
                }
            }
        }

        self.expect(TokenKind::RBrace);
        let span = self.span_from(start);

        Pattern::new(PatternKind::Struct { path, fields, rest }, span)
    }

    /// Parses a field pattern in struct destructuring.
    ///
    /// `FieldPattern` = Ident (":" Pattern)?
    fn parse_field_pattern(&mut self) -> FieldPattern {
        let name = self
            .expect_ident()
            .unwrap_or_else(|| Ident::dummy(sym("_")));

        let pattern = if self.eat(TokenKind::Colon).is_some() {
            Some(self.parse_pattern())
        } else {
            None
        };

        FieldPattern::new(name.clone(), pattern, name.span)
    }

    /// Parses a variant pattern with fields.
    ///
    /// `VariantPattern` = Path "(" (Pattern ("," Pattern)*)? ")"
    fn parse_variant_pattern(&mut self, path: Path, start: Span) -> Pattern {
        self.expect(TokenKind::LParen);

        let mut fields = Vec::new();
        if !self.check(TokenKind::RParen) {
            fields.push(self.parse_pattern());
            while self.eat(TokenKind::Comma).is_some() {
                if self.check(TokenKind::RParen) {
                    break;
                }
                fields.push(self.parse_pattern());
            }
        }

        self.expect(TokenKind::RParen);
        let span = self.span_from(start);

        Pattern::new(PatternKind::Variant { path, fields }, span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_lexer::Lexer;
    use spark_span::FileId;

    fn parse_pattern_str(source: &str) -> Pattern {
        let file_id = FileId::new(0);
        let mut lexer = Lexer::new(file_id, source);
        let tokens = lexer.tokenize();
        let mut parser = Parser::new(tokens, file_id);
        parser.parse_pattern()
    }

    #[test]
    fn wildcard_pattern() {
        let pat = parse_pattern_str("_");
        assert!(matches!(pat.kind, PatternKind::Wildcard));
    }

    #[test]
    fn ident_pattern() {
        let pat = parse_pattern_str("x");
        if let PatternKind::Ident { name, ty } = pat.kind {
            let name_str = name.name.as_str();
            assert_eq!(name_str, "x");
            assert!(ty.is_none());
        } else {
            panic!("expected ident pattern");
        }
    }

    #[test]
    fn typed_ident_pattern() {
        let pat = parse_pattern_str("x: Int");
        assert!(matches!(pat.kind, PatternKind::TypeNarrowing { .. }));
    }

    #[test]
    fn int_literal_pattern() {
        let pat = parse_pattern_str("42");
        if let PatternKind::Literal(lit) = pat.kind {
            assert!(matches!(lit.kind, LiteralKind::Int(42)));
        } else {
            panic!("expected literal pattern");
        }
    }

    #[test]
    fn bool_literal_pattern() {
        let pat = parse_pattern_str("true");
        if let PatternKind::Literal(lit) = pat.kind {
            assert!(matches!(lit.kind, LiteralKind::Bool(true)));
        } else {
            panic!("expected literal pattern");
        }
    }

    #[test]
    fn tuple_pattern() {
        let pat = parse_pattern_str("(a, b, c)");
        if let PatternKind::Tuple(patterns) = pat.kind {
            assert_eq!(patterns.len(), 3);
        } else {
            panic!("expected tuple pattern");
        }
    }

    #[test]
    fn list_pattern() {
        let pat = parse_pattern_str("[first, second]");
        if let PatternKind::List { elements, rest } = pat.kind {
            assert_eq!(elements.len(), 2);
            assert!(rest.is_none());
        } else {
            panic!("expected list pattern");
        }
    }

    #[test]
    fn list_pattern_with_rest() {
        let pat = parse_pattern_str("[first, ...rest]");
        if let PatternKind::List { elements, rest } = pat.kind {
            assert_eq!(elements.len(), 1);
            assert!(rest.is_some());
        } else {
            panic!("expected list pattern");
        }
    }

    #[test]
    fn variant_pattern() {
        let pat = parse_pattern_str("Status.Active");
        if let PatternKind::Variant { path, fields } = pat.kind {
            assert!(!path.is_simple());
            assert!(fields.is_empty());
        } else {
            panic!("expected variant pattern");
        }
    }

    #[test]
    fn variant_pattern_with_fields() {
        let pat = parse_pattern_str("Result.Error(msg)");
        if let PatternKind::Variant { path, fields } = pat.kind {
            assert!(!path.is_simple());
            assert_eq!(fields.len(), 1);
        } else {
            panic!("expected variant pattern");
        }
    }

    #[test]
    fn struct_pattern() {
        let pat = parse_pattern_str("User { name, age }");
        if let PatternKind::Struct { path, fields, rest } = pat.kind {
            assert!(path.is_simple());
            assert_eq!(fields.len(), 2);
            assert!(!rest);
        } else {
            panic!("expected struct pattern");
        }
    }

    #[test]
    fn struct_pattern_with_rest() {
        let pat = parse_pattern_str("User { name, .. }");
        if let PatternKind::Struct { fields, rest, .. } = pat.kind {
            assert_eq!(fields.len(), 1);
            assert!(rest);
        } else {
            panic!("expected struct pattern");
        }
    }

    #[test]
    fn or_pattern() {
        let pat = parse_pattern_str("A | B | C");
        if let PatternKind::Or(patterns) = pat.kind {
            assert_eq!(patterns.len(), 3);
        } else {
            panic!("expected or pattern");
        }
    }

    #[test]
    fn guard_pattern() {
        let pat = parse_pattern_str("n if n > 0");
        if let PatternKind::Guard { pattern, guard } = pat.kind {
            assert!(matches!(pattern.kind, PatternKind::Ident { .. }));
            assert!(!guard.span.is_dummy());
        } else {
            panic!("expected guard pattern");
        }
    }
}
