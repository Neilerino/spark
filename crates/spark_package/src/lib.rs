//! # `spark_package`
//!
//! Spark project manifest (`Spark.toml`) parsing and dependency resolution.
//!
//! This crate provides:
//!
//! - [`manifest::parse_manifest`] — Parse a `Spark.toml` string into a [`Manifest`]
//! - [`manifest::load_manifest`] — Load and parse `Spark.toml` from a directory
//! - [`resolve::resolve_path_dep`] — Discover source files in a path dependency
//!
//! ## Example `Spark.toml`
//!
//! ```toml
//! [project]
//! name = "myapp"
//! version = "0.1.0"
//! entry = "main.spark"
//!
//! [dependencies]
//! json = { path = "libs/json" }
//!
//! [build]
//! links = ["sqlite3"]
//! link-paths = ["/usr/local/lib"]
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod manifest;
pub mod resolve;

pub use manifest::{load_manifest, parse_manifest, BuildSection, Dependency, Manifest, ProjectSection};
pub use resolve::resolve_path_dep;
