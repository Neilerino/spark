//! Spark.toml manifest file parsing.
//!
//! A manifest describes a Spark project: its name, version, entry point,
//! dependencies, and build configuration (link libraries, link paths).

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

/// A parsed `Spark.toml` manifest.
#[derive(Debug, Deserialize)]
pub struct Manifest {
    /// The `[project]` section.
    pub project: ProjectSection,
    /// The `[dependencies]` section (optional).
    #[serde(default)]
    pub dependencies: HashMap<String, Dependency>,
    /// The `[build]` section (optional).
    #[serde(default)]
    pub build: BuildSection,
}

/// The `[project]` section of a manifest.
#[derive(Debug, Deserialize)]
pub struct ProjectSection {
    /// Project name.
    pub name: String,
    /// Project version.
    pub version: String,
    /// Entry point file (defaults to `"main.spark"`).
    #[serde(default = "default_entry")]
    pub entry: String,
}

/// A dependency specification.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Dependency {
    /// A local path dependency: `{ path = "libs/json" }`.
    Path {
        /// Relative or absolute path to the dependency root.
        path: String,
    },
}

/// The `[build]` section of a manifest.
#[derive(Debug, Default, Deserialize)]
pub struct BuildSection {
    /// C libraries to link against (e.g., `["sqlite3", "z"]`).
    #[serde(default)]
    pub links: Vec<String>,
    /// Additional search paths for the linker (e.g., `["/usr/local/lib"]`).
    #[serde(default, rename = "link-paths")]
    pub link_paths: Vec<String>,
}

/// Default entry point file.
fn default_entry() -> String {
    "main.spark".to_string()
}

/// Parses a `Spark.toml` file from the given string contents.
///
/// # Errors
///
/// Returns an error string if the TOML is malformed or missing required fields.
pub fn parse_manifest(contents: &str) -> Result<Manifest, String> {
    toml::from_str(contents).map_err(|e| format!("failed to parse Spark.toml: {e}"))
}

/// Loads and parses the `Spark.toml` from a project root directory.
///
/// Returns `None` if no `Spark.toml` exists. Returns `Some(Err(..))` if
/// the file exists but is malformed.
#[must_use]
pub fn load_manifest(root: &Path) -> Option<Result<Manifest, String>> {
    let manifest_path = root.join("Spark.toml");
    if !manifest_path.exists() {
        return None;
    }
    let contents = match std::fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(e) => return Some(Err(format!("could not read Spark.toml: {e}"))),
    };
    Some(parse_manifest(&contents))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_manifest() {
        let toml = r#"
[project]
name = "hello"
version = "0.1.0"
"#;
        let manifest = parse_manifest(toml).unwrap();
        assert_eq!(manifest.project.name, "hello");
        assert_eq!(manifest.project.version, "0.1.0");
        assert_eq!(manifest.project.entry, "main.spark");
        assert!(manifest.dependencies.is_empty());
        assert!(manifest.build.links.is_empty());
        assert!(manifest.build.link_paths.is_empty());
    }

    #[test]
    fn parse_full_manifest() {
        let toml = r#"
[project]
name = "myapp"
version = "1.0.0"
entry = "app.spark"

[dependencies]
json = { path = "libs/json" }

[build]
links = ["sqlite3", "z"]
link-paths = ["/usr/local/lib"]
"#;
        let manifest = parse_manifest(toml).unwrap();
        assert_eq!(manifest.project.name, "myapp");
        assert_eq!(manifest.project.entry, "app.spark");
        assert_eq!(manifest.dependencies.len(), 1);
        assert!(manifest.dependencies.contains_key("json"));
        assert_eq!(manifest.build.links, vec!["sqlite3", "z"]);
        assert_eq!(manifest.build.link_paths, vec!["/usr/local/lib"]);
    }

    #[test]
    fn parse_path_dependency() {
        let toml = r#"
[project]
name = "test"
version = "0.1.0"

[dependencies]
mylib = { path = "../mylib" }
"#;
        let manifest = parse_manifest(toml).unwrap();
        match &manifest.dependencies["mylib"] {
            Dependency::Path { path } => assert_eq!(path, "../mylib"),
        }
    }

    #[test]
    fn parse_missing_project_fails() {
        let toml = r#"
[build]
links = ["m"]
"#;
        assert!(parse_manifest(toml).is_err());
    }

    #[test]
    fn load_nonexistent_manifest() {
        let result = load_manifest(Path::new("/nonexistent/path"));
        assert!(result.is_none());
    }
}
