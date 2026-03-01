//! # `spark_package`
//!
//! Spark project manifest (`Spark.toml`) parsing, dependency resolution,
//! lock file management, and package registry support.
//!
//! This crate provides:
//!
//! - [`manifest::parse_manifest`] — Parse a `Spark.toml` string into a [`Manifest`]
//! - [`manifest::load_manifest`] — Load and parse `Spark.toml` from a directory
//! - [`manifest::add_dependency`] — Add a dependency to a `Spark.toml` file
//! - [`resolve::resolve_path_dep`] — Discover source files in a path dependency
//! - [`git::fetch_git_dep`] — Clone/fetch a git dependency
//! - [`lock::load_lock_file`] / [`lock::write_lock_file`] — Manage `Spark.lock`
//! - [`resolver::resolve_all_deps`] — Resolve all dependencies transitively
//! - [`registry::sync_index`] — Clone/update the package registry index
//! - [`registry::lookup_package`] — Look up a package in the index
//! - [`registry::resolve_version`] — Resolve a version via MVS
//! - [`registry::search_index`] — Search the index for packages
//! - [`publish::package_project`] — Package a project as a tarball
//! - [`publish::upload_release`] — Upload a release to GitHub
//!
//! ## Example `Spark.toml`
//!
//! ```toml
//! [project]
//! name = "myapp"
//! version = "0.1.0"
//! entry = "main.spark"
//! repository = "https://github.com/owner/myapp"
//!
//! [dependencies]
//! json = { path = "libs/json" }
//! http = { git = "https://github.com/spark-pkg/http", tag = "v1.0" }
//! utils = "1.0.0"
//!
//! [build]
//! links = ["sqlite3"]
//! link-paths = ["/usr/local/lib"]
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod git;
pub mod lock;
pub mod manifest;
pub mod publish;
pub mod registry;
pub mod resolve;
pub mod resolver;

pub use lock::{load_lock_file, write_lock_file, LockFile, LockedPackage};
pub use manifest::{
    load_manifest, parse_manifest, BuildSection, Dependency, LinkKind, LinkLibrary, LinkSpec,
    Manifest, ProjectSection,
};
pub use registry::{
    search_index, sync_index, FetchedRegistryDep, PackageMetadata, RegistryConfig, VersionEntry,
};
pub use resolve::resolve_path_dep;
pub use resolver::{resolve_all_deps, ResolvedDeps};
