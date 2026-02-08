//! Spark Language Server
//!
//! An LSP server for the Spark programming language, built on
//! [`tower_lsp`] and powered by the Spark compiler's core crates.
//!
//! Communicates over stdin/stdout using the Language Server Protocol.

#![forbid(unsafe_code)]
#![allow(clippy::cast_possible_truncation)]

mod backend;
mod document;
mod semantic_tokens;

use tower_lsp::{LspService, Server};

use crate::backend::Backend;

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}
