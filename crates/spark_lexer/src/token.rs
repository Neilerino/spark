//! Token definitions for the Spark lexer.

use spark_intern::Symbol;
use spark_span::Span;
use std::fmt;

/// A single token from the source code.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// The type of token.
    pub kind: TokenKind,
    /// The location in source code.
    pub span: Span,
    /// Optional value for literals and identifiers.
    pub value: Option<TokenValue>,
}

impl Token {
    /// Creates a new token.
    #[must_use]
    pub const fn new(kind: TokenKind, span: Span) -> Self {
        Self {
            kind,
            span,
            value: None,
        }
    }

    /// Creates a new token with a value.
    #[must_use]
    pub const fn with_value(kind: TokenKind, span: Span, value: TokenValue) -> Self {
        Self {
            kind,
            span,
            value: Some(value),
        }
    }

    /// Returns the symbol if this token is an identifier.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn as_ident(&self) -> Option<Symbol> {
        match &self.value {
            Some(TokenValue::Ident(sym)) => Some(*sym),
            _ => None,
        }
    }

    /// Returns true if this token is of the given kind.
    #[must_use]
    pub fn is(&self, kind: TokenKind) -> bool {
        self.kind == kind
    }

    /// Returns true if this token is a keyword.
    #[must_use]
    pub const fn is_keyword(&self) -> bool {
        self.kind.is_keyword()
    }

    /// Returns true if this token is a literal.
    #[must_use]
    pub const fn is_literal(&self) -> bool {
        self.kind.is_literal()
    }
}

/// The type of a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    // ========================================================================
    // Literals
    // ========================================================================
    /// Integer literal: `42`, `0xFF`, `0b1010`
    IntLiteral,
    /// Float literal: `3.14`, `1.5e10`
    FloatLiteral,
    /// String literal: `"hello"`
    StringLiteral,
    /// Start of an interpolated string: the opening `"`
    StringStart,
    /// End of an interpolated string: the closing `"`
    StringEnd,
    /// A text segment within an interpolated string
    StringText,
    /// Start of an interpolation: the `{` inside a string
    InterpStart,
    /// Character literal: `'a'`
    CharLiteral,

    // ========================================================================
    // Keywords
    // ========================================================================
    /// `fn`
    Fn,
    /// `struct`
    Struct,
    /// `enum`
    Enum,
    /// `interface`
    Interface,
    /// `type`
    Type,
    /// `alias`
    Alias,
    /// `const`
    Const,
    /// `pub`
    Pub,
    /// `module`
    Module,
    /// `using`
    Using,
    /// `import`
    Import,
    /// `if`
    If,
    /// `else`
    Else,
    /// `match`
    Match,
    /// `loop`
    Loop,
    /// `break`
    Break,
    /// `continue`
    Continue,
    /// `return`
    Return,
    /// `each`
    Each,
    /// `via`
    Via,
    /// `to`
    To,
    /// `and`
    And,
    /// `or`
    Or,
    /// `not`
    Not,
    /// `is`
    Is,
    /// `as`
    As,
    /// `binds`
    Binds,
    /// `extends`
    Extends,
    /// `async`
    Async,
    /// `await`
    Await,
    /// `spawn`
    Spawn,
    /// `block`
    Block,
    /// `parallel`
    Parallel,
    /// `true`
    True,
    /// `false`
    False,
    /// `Null`
    Null,

    // ========================================================================
    // Punctuation
    // ========================================================================
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `,`
    Comma,
    /// `:`
    Colon,
    /// `::`
    ColonColon,
    /// `;`
    Semicolon,
    /// `.`
    Dot,
    /// `..`
    DotDot,
    /// `..=`
    DotDotEq,
    /// `->`
    Arrow,
    /// `=>`
    FatArrow,

    // ========================================================================
    // Operators
    // ========================================================================
    /// `!`
    Bang,
    /// `?`
    Question,
    /// `??`
    QuestionQuestion,
    /// `?.`
    QuestionDot,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `+=`
    PlusEq,
    /// `-=`
    MinusEq,
    /// `*=`
    StarEq,
    /// `/=`
    SlashEq,
    /// `=`
    Eq,
    /// `==`
    EqEq,
    /// `!=`
    BangEq,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    LtEq,
    /// `>=`
    GtEq,
    /// `&`
    Ampersand,
    /// `|`
    Pipe,
    /// `^`
    Caret,
    /// `~`
    Tilde,
    /// `<<`
    LtLt,
    /// `>>`
    GtGt,
    /// `@`
    At,
    /// `...`
    Ellipsis,

    // ========================================================================
    // Special
    // ========================================================================
    /// An identifier
    Identifier,
    /// A doc comment (`///` or `/** */`)
    DocComment,
    /// End of file
    Eof,
    /// Lexer error
    Error,
}

impl TokenKind {
    /// Returns true if this is a keyword token.
    #[must_use]
    pub const fn is_keyword(self) -> bool {
        matches!(
            self,
            Self::Fn
                | Self::Struct
                | Self::Enum
                | Self::Interface
                | Self::Type
                | Self::Alias
                | Self::Const
                | Self::Pub
                | Self::Module
                | Self::Using
                | Self::Import
                | Self::If
                | Self::Else
                | Self::Match
                | Self::Loop
                | Self::Break
                | Self::Continue
                | Self::Return
                | Self::Each
                | Self::Via
                | Self::To
                | Self::And
                | Self::Or
                | Self::Not
                | Self::Is
                | Self::As
                | Self::Binds
                | Self::Extends
                | Self::Async
                | Self::Await
                | Self::Spawn
                | Self::Block
                | Self::Parallel
                | Self::True
                | Self::False
                | Self::Null
        )
    }

    /// Returns true if this is a literal token.
    #[must_use]
    pub const fn is_literal(self) -> bool {
        matches!(
            self,
            Self::IntLiteral | Self::FloatLiteral | Self::StringLiteral | Self::CharLiteral
        )
    }

    /// Returns true if this is a comparison operator.
    #[must_use]
    pub const fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::EqEq | Self::BangEq | Self::Lt | Self::Gt | Self::LtEq | Self::GtEq
        )
    }

    /// Returns true if this is an assignment operator.
    #[must_use]
    pub const fn is_assignment(self) -> bool {
        matches!(
            self,
            Self::Eq | Self::PlusEq | Self::MinusEq | Self::StarEq | Self::SlashEq
        )
    }

    /// Returns the keyword for an identifier, if it is one.
    #[must_use]
    pub fn keyword_from_str(s: &str) -> Option<Self> {
        match s {
            "fn" => Some(Self::Fn),
            "struct" => Some(Self::Struct),
            "enum" => Some(Self::Enum),
            "interface" => Some(Self::Interface),
            "type" => Some(Self::Type),
            "alias" => Some(Self::Alias),
            "const" => Some(Self::Const),
            "pub" => Some(Self::Pub),
            "module" => Some(Self::Module),
            "using" => Some(Self::Using),
            "import" => Some(Self::Import),
            "if" => Some(Self::If),
            "else" => Some(Self::Else),
            "match" => Some(Self::Match),
            "loop" => Some(Self::Loop),
            "break" => Some(Self::Break),
            "continue" => Some(Self::Continue),
            "return" => Some(Self::Return),
            "each" => Some(Self::Each),
            "via" => Some(Self::Via),
            "to" => Some(Self::To),
            "and" => Some(Self::And),
            "or" => Some(Self::Or),
            "not" => Some(Self::Not),
            "is" => Some(Self::Is),
            "as" => Some(Self::As),
            "binds" => Some(Self::Binds),
            "extends" => Some(Self::Extends),
            "async" => Some(Self::Async),
            "await" => Some(Self::Await),
            "spawn" => Some(Self::Spawn),
            "block" => Some(Self::Block),
            "parallel" => Some(Self::Parallel),
            "true" => Some(Self::True),
            "false" => Some(Self::False),
            "Null" => Some(Self::Null),
            _ => None,
        }
    }

    /// Returns the string representation of this token kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IntLiteral => "integer literal",
            Self::FloatLiteral => "float literal",
            Self::StringLiteral => "string literal",
            Self::StringStart => "string start",
            Self::StringEnd => "string end",
            Self::StringText => "string text",
            Self::InterpStart => "interpolation start",
            Self::CharLiteral => "character literal",
            Self::Fn => "fn",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Interface => "interface",
            Self::Type => "type",
            Self::Alias => "alias",
            Self::Const => "const",
            Self::Pub => "pub",
            Self::Module => "module",
            Self::Using => "using",
            Self::Import => "import",
            Self::If => "if",
            Self::Else => "else",
            Self::Match => "match",
            Self::Loop => "loop",
            Self::Break => "break",
            Self::Continue => "continue",
            Self::Return => "return",
            Self::Each => "each",
            Self::Via => "via",
            Self::To => "to",
            Self::And => "and",
            Self::Or => "or",
            Self::Not => "not",
            Self::Is => "is",
            Self::As => "as",
            Self::Binds => "binds",
            Self::Extends => "extends",
            Self::Async => "async",
            Self::Await => "await",
            Self::Spawn => "spawn",
            Self::Block => "block",
            Self::Parallel => "parallel",
            Self::True => "true",
            Self::False => "false",
            Self::Null => "Null",
            Self::LParen => "(",
            Self::RParen => ")",
            Self::LBrace => "{",
            Self::RBrace => "}",
            Self::LBracket => "[",
            Self::RBracket => "]",
            Self::Comma => ",",
            Self::Colon => ":",
            Self::ColonColon => "::",
            Self::Semicolon => ";",
            Self::Dot => ".",
            Self::DotDot => "..",
            Self::DotDotEq => "..=",
            Self::Arrow => "->",
            Self::FatArrow => "=>",
            Self::Bang => "!",
            Self::Question => "?",
            Self::QuestionQuestion => "??",
            Self::QuestionDot => "?.",
            Self::Plus => "+",
            Self::Minus => "-",
            Self::Star => "*",
            Self::Slash => "/",
            Self::Percent => "%",
            Self::PlusEq => "+=",
            Self::MinusEq => "-=",
            Self::StarEq => "*=",
            Self::SlashEq => "/=",
            Self::Eq => "=",
            Self::EqEq => "==",
            Self::BangEq => "!=",
            Self::Lt => "<",
            Self::Gt => ">",
            Self::LtEq => "<=",
            Self::GtEq => ">=",
            Self::Ampersand => "&",
            Self::Pipe => "|",
            Self::Caret => "^",
            Self::Tilde => "~",
            Self::LtLt => "<<",
            Self::GtGt => ">>",
            Self::At => "@",
            Self::Ellipsis => "...",
            Self::Identifier => "identifier",
            Self::DocComment => "doc comment",
            Self::Eof => "end of file",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// The value of a literal or identifier token.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenValue {
    /// Integer value.
    Int(i64),
    /// Float value.
    Float(f64),
    /// Interned string value.
    String(Symbol),
    /// Character value.
    Char(char),
    /// Interned identifier.
    Ident(Symbol),
}

impl TokenValue {
    /// Returns the integer value if this is an int.
    #[must_use]
    pub const fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// Returns the float value if this is a float.
    #[must_use]
    pub const fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(n) => Some(*n),
            _ => None,
        }
    }

    /// Returns the symbol if this is a string or identifier.
    #[must_use]
    pub const fn as_symbol(&self) -> Option<Symbol> {
        match self {
            Self::String(s) | Self::Ident(s) => Some(*s),
            _ => None,
        }
    }

    /// Returns the char value if this is a char.
    #[must_use]
    pub const fn as_char(&self) -> Option<char> {
        match self {
            Self::Char(c) => Some(*c),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spark_span::FileId;

    #[test]
    fn token_creation() {
        let span = Span::new(FileId::new(0), 0, 2);
        let token = Token::new(TokenKind::Fn, span);

        assert_eq!(token.kind, TokenKind::Fn);
        assert_eq!(token.span, span);
        assert!(token.value.is_none());
    }

    #[test]
    fn token_with_value() {
        let span = Span::new(FileId::new(0), 0, 5);
        let token = Token::with_value(TokenKind::IntLiteral, span, TokenValue::Int(42));

        assert_eq!(token.kind, TokenKind::IntLiteral);
        assert_eq!(token.value, Some(TokenValue::Int(42)));
    }

    #[test]
    fn keyword_from_str() {
        assert_eq!(TokenKind::keyword_from_str("fn"), Some(TokenKind::Fn));
        assert_eq!(TokenKind::keyword_from_str("if"), Some(TokenKind::If));
        assert_eq!(TokenKind::keyword_from_str("foo"), None);
    }

    #[test]
    fn token_kind_predicates() {
        assert!(TokenKind::Fn.is_keyword());
        assert!(TokenKind::If.is_keyword());
        assert!(!TokenKind::Identifier.is_keyword());

        assert!(TokenKind::IntLiteral.is_literal());
        assert!(TokenKind::StringLiteral.is_literal());
        assert!(!TokenKind::Fn.is_literal());

        assert!(TokenKind::EqEq.is_comparison());
        assert!(TokenKind::Lt.is_comparison());
        assert!(!TokenKind::Plus.is_comparison());

        assert!(TokenKind::Eq.is_assignment());
        assert!(TokenKind::PlusEq.is_assignment());
        assert!(!TokenKind::Plus.is_assignment());
    }
}
