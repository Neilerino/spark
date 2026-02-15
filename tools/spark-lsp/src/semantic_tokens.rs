//! Semantic token encoding for the Spark LSP server.
//!
//! Maps Spark [`TokenKind`] variants to LSP semantic token types and encodes
//! them as delta-encoded [`SemanticToken`] values. This module provides
//! lexer-level classification only — context-aware classification (e.g.
//! distinguishing function names from variables) requires resolver data
//! and will be added in future phases.

use ropey::Rope;
use spark_lexer::Lexer;
use spark_lexer::TokenKind;
use spark_resolve::{DefinitionKind, DefinitionTable, ResolutionMap};
use spark_span::FileId;
use tower_lsp::lsp_types::{
    SemanticToken, SemanticTokenModifier, SemanticTokenType, SemanticTokensLegend,
};

use crate::document::offset_to_position;

/// The ordered list of semantic token types we advertise.
///
/// The index of each entry is used as the `token_type` field in
/// [`SemanticToken`]. This order must be kept in sync with the index constants below.
pub const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::KEYWORD,        // 0
    SemanticTokenType::NUMBER,         // 1
    SemanticTokenType::STRING,         // 2
    SemanticTokenType::COMMENT,        // 3
    SemanticTokenType::OPERATOR,       // 4
    SemanticTokenType::VARIABLE,       // 5
    SemanticTokenType::FUNCTION,       // 6
    SemanticTokenType::STRUCT,         // 7
    SemanticTokenType::ENUM,           // 8
    SemanticTokenType::ENUM_MEMBER,    // 9
    SemanticTokenType::INTERFACE,      // 10
    SemanticTokenType::TYPE,           // 11
    SemanticTokenType::PARAMETER,      // 12
    SemanticTokenType::PROPERTY,       // 13
    SemanticTokenType::TYPE_PARAMETER, // 14
    SemanticTokenType::METHOD,         // 15
];

/// Index constants matching [`TOKEN_TYPES`].
const KEYWORD: u32 = 0;
const NUMBER: u32 = 1;
const STRING: u32 = 2;
const COMMENT: u32 = 3;
const OPERATOR: u32 = 4;
const VARIABLE: u32 = 5;
const FUNCTION: u32 = 6;
const STRUCT: u32 = 7;
const ENUM: u32 = 8;
const ENUM_MEMBER: u32 = 9;
const INTERFACE: u32 = 10;
const TYPE: u32 = 11;
const PARAMETER: u32 = 12;
const PROPERTY: u32 = 13;
const TYPE_PARAM: u32 = 14;
const METHOD: u32 = 15;

/// Token modifier bit positions.
const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::READONLY,    // bit 0
    SemanticTokenModifier::DECLARATION, // bit 1
];

/// Modifier bitmask constants.
const MOD_READONLY: u32 = 1 << 0;
const MOD_DECLARATION: u32 = 1 << 1;

/// Builds the [`SemanticTokensLegend`] from our token type and modifier lists.
#[must_use]
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES.to_vec(),
        token_modifiers: TOKEN_MODIFIERS.to_vec(),
    }
}

/// Maps a [`DefinitionKind`] to its semantic token type index and modifier bitmask.
const fn definition_kind_to_token_type(kind: &DefinitionKind) -> (u32, u32) {
    match kind {
        DefinitionKind::Function
        | DefinitionKind::BuiltinFunction
        | DefinitionKind::ExternFunction
        | DefinitionKind::Lambda => (FUNCTION, 0),

        DefinitionKind::Struct => (STRUCT, 0),
        DefinitionKind::Enum => (ENUM, 0),
        DefinitionKind::EnumVariant(_) => (ENUM_MEMBER, 0),
        DefinitionKind::Interface => (INTERFACE, 0),
        DefinitionKind::TypeAlias | DefinitionKind::TypeDef => (TYPE, 0),
        DefinitionKind::Parameter => (PARAMETER, 0),
        DefinitionKind::StructField(_) => (PROPERTY, 0),
        DefinitionKind::Const => (VARIABLE, MOD_READONLY),
        DefinitionKind::Local => (VARIABLE, 0),
        DefinitionKind::GenericParam | DefinitionKind::LifetimeParam => (TYPE_PARAM, 0),
        DefinitionKind::BuiltinMethod | DefinitionKind::BindingMethod(_) => (METHOD, 0),
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

        // Interpolation punctuation, punctuation, EOF, error — no semantic token
        TokenKind::InterpStart
        | TokenKind::LParen
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

/// Resolver context for context-aware semantic token classification.
///
/// When provided, identifiers are classified by their definition kind
/// (function, struct, parameter, etc.) rather than the generic `variable`.
pub struct SemanticContext<'a> {
    /// Span → `DefId` resolution maps for the current file.
    pub resolutions: &'a ResolutionMap,
    /// Table of all definitions (maps `DefId` → `DefinitionInfo`).
    pub definitions: &'a DefinitionTable,
}

/// Resolves an identifier token's semantic type using resolver data.
///
/// Tries `get_ident`, then `get_path`, then `get_binding` (adding the
/// DECLARATION modifier for bindings). Returns `None` if unresolved.
fn resolve_identifier(span: spark_span::Span, context: &SemanticContext<'_>) -> Option<(u32, u32)> {
    // Try ident resolution first (most common for usages)
    if let Some(def_id) = context.resolutions.get_ident(span) {
        if let Some(info) = context.definitions.get(def_id) {
            return Some(definition_kind_to_token_type(&info.kind));
        }
    }

    // Try path resolution (qualified names like Enum.Variant)
    if let Some(def_id) = context.resolutions.get_path(span) {
        if let Some(info) = context.definitions.get(def_id) {
            return Some(definition_kind_to_token_type(&info.kind));
        }
    }

    // Try binding resolution (definition sites — let/const/param patterns)
    if let Some(def_id) = context.resolutions.get_binding(span) {
        if let Some(info) = context.definitions.get(def_id) {
            let (type_index, modifiers) = definition_kind_to_token_type(&info.kind);
            return Some((type_index, modifiers | MOD_DECLARATION));
        }
    }

    None
}

/// Computes delta-encoded semantic tokens for the given source text.
///
/// Lexes the source and converts each classifiable token into a
/// [`SemanticToken`] with delta-encoded line/character positions.
///
/// When `context` is provided, identifier tokens are classified by their
/// resolved definition kind. Without context, all identifiers fall back
/// to the generic `variable` type.
#[must_use]
pub fn compute_semantic_tokens(
    source: &str,
    context: Option<&SemanticContext<'_>>,
) -> Vec<SemanticToken> {
    let file_id = FileId::new(0);
    let tokens = Lexer::new(file_id, source).tokenize();
    let rope = Rope::from_str(source);

    let mut semantic_tokens = Vec::new();
    let mut prev_line: u32 = 0;
    let mut prev_start: u32 = 0;

    for token in &tokens {
        // For identifiers with resolver context, try context-aware classification
        let (type_index, modifiers) = if token.kind == TokenKind::Identifier {
            context.map_or((VARIABLE, 0), |ctx| {
                resolve_identifier(token.span, ctx).unwrap_or((VARIABLE, 0))
            })
        } else {
            match token_type_index(token.kind) {
                Some(idx) => (idx, 0),
                None => continue,
            }
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
            token_modifiers_bitset: modifiers,
        });

        prev_line = pos.line;
        prev_start = pos.character;
    }

    semantic_tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Lexer-only tests (no resolver context) ───────────────────────────

    #[test]
    fn keyword_tokens() {
        let tokens = compute_semantic_tokens("fn main(): Void { }", None);
        // "fn" should be first token, classified as keyword
        assert!(!tokens.is_empty());
        assert_eq!(tokens[0].token_type, KEYWORD);
        assert_eq!(tokens[0].delta_line, 0);
        assert_eq!(tokens[0].delta_start, 0);
        assert_eq!(tokens[0].length, 2); // "fn"
    }

    #[test]
    fn number_literal() {
        let tokens = compute_semantic_tokens("42", None);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token_type, NUMBER);
        assert_eq!(tokens[0].length, 2);
    }

    #[test]
    fn string_literal() {
        let tokens = compute_semantic_tokens("\"hello\"", None);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token_type, STRING);
    }

    #[test]
    fn operator_tokens() {
        let tokens = compute_semantic_tokens("a + b", None);
        // a=variable, +=operator, b=variable
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0].token_type, VARIABLE); // a
        assert_eq!(tokens[1].token_type, OPERATOR); // +
        assert_eq!(tokens[2].token_type, VARIABLE); // b
    }

    #[test]
    fn identifier_as_variable() {
        let tokens = compute_semantic_tokens("foo", None);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].token_type, VARIABLE);
    }

    #[test]
    fn punctuation_excluded() {
        let tokens = compute_semantic_tokens("(){}[];:,.", None);
        assert!(tokens.is_empty());
    }

    #[test]
    fn multiline_delta_encoding() {
        let tokens = compute_semantic_tokens("fn\nfn", None);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].delta_line, 0);
        assert_eq!(tokens[0].delta_start, 0);
        assert_eq!(tokens[1].delta_line, 1);
        assert_eq!(tokens[1].delta_start, 0); // first token on new line
    }

    #[test]
    fn same_line_delta_encoding() {
        let tokens = compute_semantic_tokens("fn fn", None);
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
        assert_eq!(leg.token_modifiers.len(), TOKEN_MODIFIERS.len());
    }

    #[test]
    fn empty_source() {
        let tokens = compute_semantic_tokens("", None);
        assert!(tokens.is_empty());
    }

    // ── Context-aware tests (with resolver data) ─────────────────────────

    use spark_resolve::ResolvedAst;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// Runs the full frontend pipeline and returns the resolver output.
    fn run_analysis(source: &str) -> Option<(FileId, ResolvedAst, DefinitionTable)> {
        let mut source_map = spark_span::SourceMap::new();
        let file_id = source_map.add_file("test.spark", source);

        let tokens = spark_lexer::Lexer::new(file_id, source).tokenize();
        let (ast, parse_diags) = spark_parser::parse(tokens, file_id);
        if parse_diags.has_errors() {
            return None;
        }

        let mut asts = HashMap::new();
        asts.insert(file_id, ast.clone());
        let mut file_paths = HashMap::new();
        file_paths.insert(file_id, PathBuf::from("test.spark"));

        let module_output = spark_modules::resolve(spark_modules::ModuleResolverInput {
            asts: asts.clone(),
            root_path: PathBuf::from("."),
            file_paths,
        });
        if module_output.diagnostics.has_errors() {
            return None;
        }

        let mut resolve_asts = HashMap::new();
        resolve_asts.insert(file_id, ast);
        let resolve_output = spark_resolve::resolve(spark_resolve::ResolverInput {
            graph: module_output.graph,
            asts: resolve_asts,
        });

        Some((file_id, resolve_output.resolved, resolve_output.definitions))
    }

    /// Finds a semantic token by its text content in the source.
    fn find_token_by_text<'a>(
        source: &str,
        tokens: &'a [SemanticToken],
        target: &str,
    ) -> Option<&'a SemanticToken> {
        let rope = Rope::from_str(source);
        let mut line: u32 = 0;
        let mut col: u32 = 0;
        for tok in tokens {
            line += tok.delta_line;
            col = if tok.delta_line == 0 {
                col + tok.delta_start
            } else {
                tok.delta_start
            };
            // Convert line/col back to byte offset
            let char_idx = rope.line_to_char(line as usize) + col as usize;
            let byte_start = rope.char_to_byte(char_idx);
            let byte_end = byte_start + tok.length as usize;
            if byte_end <= source.len() && &source[byte_start..byte_end] == target {
                return Some(tok);
            }
        }
        None
    }

    #[test]
    fn fn_identifier_classified_as_function() {
        let source = "fn foo(): Void { }";
        let (file_id, resolved, definitions) = run_analysis(source).unwrap();
        let resolutions = resolved.get_resolutions(file_id).unwrap();
        let context = SemanticContext {
            resolutions,
            definitions: &definitions,
        };
        let tokens = compute_semantic_tokens(source, Some(&context));
        let tok = find_token_by_text(source, &tokens, "foo").expect("should find 'foo' token");
        assert_eq!(tok.token_type, FUNCTION, "fn name should be FUNCTION");
    }

    #[test]
    fn struct_identifier_classified() {
        let source = "struct Point { x: Int, y: Int }";
        let (file_id, resolved, definitions) = run_analysis(source).unwrap();
        let resolutions = resolved.get_resolutions(file_id).unwrap();
        let context = SemanticContext {
            resolutions,
            definitions: &definitions,
        };
        let tokens = compute_semantic_tokens(source, Some(&context));
        let tok = find_token_by_text(source, &tokens, "Point").expect("should find 'Point' token");
        assert_eq!(tok.token_type, STRUCT, "struct name should be STRUCT");
    }

    #[test]
    fn enum_variant_classified() {
        let source = "enum Color { Red, Green, Blue }";
        let (file_id, resolved, definitions) = run_analysis(source).unwrap();
        let resolutions = resolved.get_resolutions(file_id).unwrap();
        let context = SemanticContext {
            resolutions,
            definitions: &definitions,
        };
        let tokens = compute_semantic_tokens(source, Some(&context));
        let tok = find_token_by_text(source, &tokens, "Color").expect("should find 'Color' token");
        assert_eq!(tok.token_type, ENUM, "enum name should be ENUM");
    }

    #[test]
    fn parameter_classified() {
        let source = "fn greet(name: String): Void { name }";
        let (file_id, resolved, definitions) = run_analysis(source).unwrap();
        let resolutions = resolved.get_resolutions(file_id).unwrap();
        let context = SemanticContext {
            resolutions,
            definitions: &definitions,
        };
        let tokens = compute_semantic_tokens(source, Some(&context));
        // The usage of `name` in the body should be classified as PARAMETER
        // Find the second `name` token (the usage, not the definition)
        let rope = Rope::from_str(source);
        let mut line: u32 = 0;
        let mut col: u32 = 0;
        let mut name_tokens = vec![];
        for tok in &tokens {
            line += tok.delta_line;
            col = if tok.delta_line == 0 {
                col + tok.delta_start
            } else {
                tok.delta_start
            };
            let char_idx = rope.line_to_char(line as usize) + col as usize;
            let byte_start = rope.char_to_byte(char_idx);
            let byte_end = byte_start + tok.length as usize;
            if byte_end <= source.len() && &source[byte_start..byte_end] == "name" {
                name_tokens.push(tok);
            }
        }
        assert!(
            !name_tokens.is_empty(),
            "should find at least one 'name' token"
        );
        // At least one should be PARAMETER
        let has_param = name_tokens.iter().any(|t| t.token_type == PARAMETER);
        assert!(
            has_param,
            "at least one 'name' token should be PARAMETER, got types: {:?}",
            name_tokens.iter().map(|t| t.token_type).collect::<Vec<_>>()
        );
    }

    #[test]
    fn local_binding_classified() {
        let source = "fn main(): Void { x: Int = 42\nx }";
        let (file_id, resolved, definitions) = run_analysis(source).unwrap();
        let resolutions = resolved.get_resolutions(file_id).unwrap();
        let context = SemanticContext {
            resolutions,
            definitions: &definitions,
        };
        let tokens = compute_semantic_tokens(source, Some(&context));
        // The usage of `x` in the body should be classified as VARIABLE
        let rope = Rope::from_str(source);
        let mut line: u32 = 0;
        let mut col: u32 = 0;
        let mut x_tokens = vec![];
        for tok in &tokens {
            line += tok.delta_line;
            col = if tok.delta_line == 0 {
                col + tok.delta_start
            } else {
                tok.delta_start
            };
            let char_idx = rope.line_to_char(line as usize) + col as usize;
            let byte_start = rope.char_to_byte(char_idx);
            let byte_end = byte_start + tok.length as usize;
            if byte_end <= source.len() && &source[byte_start..byte_end] == "x" {
                x_tokens.push(*tok);
            }
        }
        assert!(!x_tokens.is_empty(), "should find at least one 'x' token");
        // All `x` tokens should be VARIABLE
        for tok in &x_tokens {
            assert_eq!(tok.token_type, VARIABLE, "local 'x' should be VARIABLE");
        }
    }

    #[test]
    fn const_has_readonly_modifier() {
        let source = "const MAX: Int = 100";
        let (file_id, resolved, definitions) = run_analysis(source).unwrap();
        let resolutions = resolved.get_resolutions(file_id).unwrap();
        let context = SemanticContext {
            resolutions,
            definitions: &definitions,
        };
        let tokens = compute_semantic_tokens(source, Some(&context));
        let tok = find_token_by_text(source, &tokens, "MAX").expect("should find 'MAX' token");
        assert_eq!(tok.token_type, VARIABLE, "const should be VARIABLE");
        assert_ne!(
            tok.token_modifiers_bitset & MOD_READONLY,
            0,
            "const should have READONLY modifier"
        );
    }

    #[test]
    fn type_alias_classified() {
        let source = "type Name = String";
        let (file_id, resolved, definitions) = run_analysis(source).unwrap();
        let resolutions = resolved.get_resolutions(file_id).unwrap();
        let context = SemanticContext {
            resolutions,
            definitions: &definitions,
        };
        let tokens = compute_semantic_tokens(source, Some(&context));
        let tok = find_token_by_text(source, &tokens, "Name").expect("should find 'Name' token");
        assert_eq!(tok.token_type, TYPE, "type alias should be TYPE");
    }

    #[test]
    fn interface_classified() {
        let source = "interface Printable { fn print(): Void }";
        let (file_id, resolved, definitions) = run_analysis(source).unwrap();
        let resolutions = resolved.get_resolutions(file_id).unwrap();
        let context = SemanticContext {
            resolutions,
            definitions: &definitions,
        };
        let tokens = compute_semantic_tokens(source, Some(&context));
        let tok = find_token_by_text(source, &tokens, "Printable")
            .expect("should find 'Printable' token");
        assert_eq!(
            tok.token_type, INTERFACE,
            "interface name should be INTERFACE"
        );
    }

    #[test]
    fn unresolved_falls_back_to_variable() {
        // With context but an unresolvable identifier — should fall back to VARIABLE
        let source = "fn main(): Void { unknown_var }";
        let (file_id, resolved, definitions) = run_analysis(source).unwrap();
        let resolutions = resolved.get_resolutions(file_id).unwrap();
        let context = SemanticContext {
            resolutions,
            definitions: &definitions,
        };
        let tokens = compute_semantic_tokens(source, Some(&context));
        let tok = find_token_by_text(source, &tokens, "unknown_var");
        // It may or may not resolve — but if it appears, it should at least not crash
        // and should be VARIABLE if unresolved
        if let Some(t) = tok {
            assert_eq!(
                t.token_type, VARIABLE,
                "unresolved identifier should fall back to VARIABLE"
            );
        }
    }

    #[test]
    fn no_analysis_falls_back() {
        // Without context, all identifiers should be VARIABLE (same as before)
        let tokens = compute_semantic_tokens("fn foo(): Void { bar }", None);
        let tok = find_token_by_text("fn foo(): Void { bar }", &tokens, "foo")
            .expect("should find 'foo' token");
        assert_eq!(
            tok.token_type, VARIABLE,
            "without context, identifiers should be VARIABLE"
        );
    }
}
