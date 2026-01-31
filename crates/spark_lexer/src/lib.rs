//! # `spark_lexer`
//!
//! Lexical analysis for the Spark compiler.
//!
//! This crate transforms source code into a stream of tokens that can be
//! consumed by the parser.
//!
//! # Quick Start
//!
//! ```
//! use spark_lexer::{Lexer, TokenKind};
//! use spark_span::FileId;
//!
//! let source = "fn main(): Void { }";
//! let mut lexer = Lexer::new(FileId::new(0), source);
//! let tokens = lexer.tokenize();
//!
//! assert_eq!(tokens[0].kind, TokenKind::Fn);
//! assert_eq!(tokens[1].kind, TokenKind::Identifier);
//! ```
//!
//! See the crate README for detailed documentation.

#![forbid(unsafe_code)]
#![allow(clippy::cast_possible_truncation)]

mod cursor;
mod lexer;
mod token;

pub use lexer::Lexer;
pub use token::{Token, TokenKind, TokenValue};
