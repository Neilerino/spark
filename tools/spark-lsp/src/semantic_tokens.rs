//! Semantic token encoding for the Spark LSP server.
//!
//! Maps Spark [`TokenKind`] variants to LSP semantic token types and encodes
//! them as delta-encoded [`SemanticToken`] values. This module provides
//! lexer-level classification only — context-aware classification (e.g.
//! distinguishing function names from variables) requires resolver data
//! and will be added in future phases.

use ropey::Rope;
use spark_lexer::TokenKind;
use spark_lexer::Lexer;
use spark_span::FileId;
use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenType, SemanticTokensLegend,
};

use crate::document::offset_to_position;

/// The ordered list of semantic token types we advertise.
///
/// The index of each entry is used as the `token_type` field in
/// [`SemanticToken`]. This order must be kept in sync with [`token_type_index`].
pub const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,  // 0
    SemanticTokenType::NUMBER,   // 1
    SemanticTokenType::STRING,   // 2
    SemanticTokenType::COMMENT,  // 3
    SemanticTokenType::OPERATOR, // 4
    SemanticTokenType::VARIABLE, // 5
];

/// Index constants matching [`TOKEN_TYPES`].
const KEYWORD: u32 = 0;
const NUMBER: u32 = 1;
const STRING: u32 = 2;
const COMMENT: u32 = 3;
const OPERATOR: u32 = 4;
const VARIABLE: u32 = 5;

/// Builds the [`SemanticTokensLegend`] from our token type list.
#[must_use]
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES.to_vec(),
        token_modifiers: vec![],
    }
}

/// Maps a [`TokenKind`] to its semantic token type index.
///
/// Returns `None` for tokens that should not receive semantic highlighting
/// (punctuation, EOF, error tokens).
const fn token_type_index(kind: TokenKind) -> Option<u32> {
    match kind {
        // Keywords
        TokenKind::Fn
        | TokenKind::Struct
        | TokenKind::Enum
        | TokenKind::Interface
        | TokenKind::Type
        | TokenKind::Alias
        | TokenKind::Const
        | TokenKind::Pub
        | TokenKind::Module
        | TokenKind::Using
        | TokenKind::Import
        | TokenKind::If
        | TokenKind::Else
        | TokenKind::Match
        | TokenKind::Loop
        | TokenKind::Break
        | TokenKind::Continue
        | TokenKind::Return
        | TokenKind::Each
        | TokenKind::Via
        | TokenKind::Parallel
        | TokenKind::To
        | TokenKind::And
        | TokenKind::Or
        | TokenKind::Not
        | TokenKind::Is
        | TokenKind::As
        | TokenKind::Binds
        | TokenKind::Extends
        | TokenKind::Async
        | TokenKind::Await
        | TokenKind::Spawn
        | TokenKind::Block
        | TokenKind::True
        | TokenKind::False
        | TokenKind::Null
        | TokenKind::Extern => Some(KEYWORD),

        // Numbers
        TokenKind::IntLiteral | TokenKind::FloatLiteral => Some(NUMBER),

        // Strings
        TokenKind::StringLiteral
        | TokenKind::CharLiteral
        | TokenKind::StringStart
        | TokenKind::StringEnd
        | TokenKind::StringText => Some(STRING),

        // Interpolation punctuation — no semantic token
        TokenKind::InterpStart => None,

        // Comments (only doc comments survive lexing)
        TokenKind::DocComment => Some(COMMENT),

        // Operators
        TokenKind::Plus
        | TokenKind::Minus
        | TokenKind::Star
        | TokenKind::Slash
        | TokenKind::Percent
        | TokenKind::PlusEq
        | TokenKind::MinusEq
        | TokenKind::StarEq
        | TokenKind::SlashEq
        | TokenKind::Eq
        | TokenKind::EqEq
        | TokenKind::BangEq
        | TokenKind::Lt
        | TokenKind::Gt
        | TokenKind::LtEq
        | TokenKind::GtEq
        | TokenKind::Bang
        | TokenKind::Ampersand
        | TokenKind::Pipe
        | TokenKind::Caret
        | TokenKind::Tilde
        | TokenKind::LtLt
        | TokenKind::GtGt
        | TokenKind::Arrow
        | TokenKind::FatArrow
        | TokenKind::DotDot
        | TokenKind::DotDotEq
        | TokenKind::Ellipsis
        | TokenKind::Question
        | TokenKind::QuestionQuestion
        | TokenKind::QuestionDot => Some(OPERATOR),

        // Identifiers → generic variable (no resolver data yet)
        TokenKind::Identifier => Some(VARIABLE),

        // Punctuation, EOF, error — no semantic token
        TokenKind::LParen
        | TokenKind::RParen
        | TokenKind::LBrace
        | TokenKind::RBrace
        | TokenKind::LBracket
        | TokenKind::RBracket
        | TokenKind::Comma
        | TokenKind::Colon
        | TokenKind::ColonColon
        | TokenKind::Semicolon
        | TokenKind::Dot
        | TokenKind::At
        | TokenKind::Eof
        | TokenKind::Error => None,
    }
}

/// Computes delta-encoded semantic tokens for the given source text.
///
/// Lexes the source and converts each classifiable token into a
/// [`SemanticToken`] with delta-encoded line/character positions.
#[must_use]
pub fn compute_semantic_tokens(source: &str) -> Vec<SemanticToken> {
    let file_id = FileId::new(0);
    let tokens = Lexer::new(file_id, source).tokenize();
    let rope = Rope::from_str(source);

    let mut semantic_tokens = Vec::new();
    let mut prev_line: u32 = 0;
    let mut prev_start: u32 = 0;

    for token in &tokens {
        let Some(type_index) = token_type_index(token.kind) else {
            continue;
        };

        let Some(pos) = offset_to_position(&rope, token.span.start) else {
            continue;
        };

        let length = token.span.end.saturating_sub(token.span.start);
        if length == 0 {
            continue;
        }

        let delta_line = pos.line - prev_line;
        let delta_start = if delta_line == 0 {
            pos.character - prev_start
        } else {
            pos.character
        };

        semantic_tokens.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type: type_index,
            token_modifiers_bitset: 0,
        });

        prev_line = pos.line;
        prev_start = pos.character;
    }

    semantic_tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_tokens() {
        let tokens = compute_semantic_tokens("fn main(): Void { }");
        // "fn" should be first token, classified as keyword
        assert!(!tokens.is_empty());
        assert_eq!(tokens[0].token_type, KEYWORD);
        assert_eq!(tokens[0].delta_line, 0);
        assert_eq!(tokens[0].delta_start, 0);
        assert_eq!(tokens[0].length, 2); // "fn"
    }

    #[test]
    fn number_literal() {
        let tokens = compute_semantic_tokens("42");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token_type, NUMBER);
        assert_eq!(tokens[0].length, 2);
    }

    #[test]
    fn string_literal() {
        let tokens = compute_semantic_tokens("\"hello\"");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token_type, STRING);
    }

    #[test]
    fn operator_tokens() {
        let tokens = compute_semantic_tokens("a + b");
        // a=variable, +=operator, b=variable
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].token_type, VARIABLE); // a
        assert_eq!(tokens[1].token_type, OPERATOR); // +
        assert_eq!(tokens[2].token_type, VARIABLE); // b
    }

    #[test]
    fn identifier_as_variable() {
        let tokens = compute_semantic_tokens("foo");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token_type, VARIABLE);
    }

    #[test]
    fn punctuation_excluded() {
        let tokens = compute_semantic_tokens("(){}[];:,.");
        assert!(tokens.is_empty());
    }

    #[test]
    fn multiline_delta_encoding() {
        let tokens = compute_semantic_tokens("fn\nfn");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].delta_line, 0);
        assert_eq!(tokens[0].delta_start, 0);
        assert_eq!(tokens[1].delta_line, 1);
        assert_eq!(tokens[1].delta_start, 0); // first token on new line
    }

    #[test]
    fn same_line_delta_encoding() {
        let tokens = compute_semantic_tokens("fn fn");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].delta_line, 0);
        assert_eq!(tokens[0].delta_start, 0);
        assert_eq!(tokens[1].delta_line, 0);
        assert_eq!(tokens[1].delta_start, 3); // offset from previous token start
    }

    #[test]
    fn legend_matches_token_types() {
        let leg = legend();
        assert_eq!(leg.token_types.len(), TOKEN_TYPES.len());
        assert!(leg.token_modifiers.is_empty());
    }

    #[test]
    fn empty_source() {
        let tokens = compute_semantic_tokens("");
        assert!(tokens.is_empty());
    }
}
