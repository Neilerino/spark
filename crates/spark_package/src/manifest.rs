//! Spark.toml manifest file parsing.
//!
//! A manifest describes a Spark project: its name, version, entry point,
//! dependencies, and build configuration (link libraries, link paths).

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

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
    /// Repository URL (required for `spark publish`).
    #[serde(default)]
    pub repository: Option<String>,
}

/// A dependency specification.
///
/// Serde `untagged` tries variants in order: `Path` (object with `path` key),
/// `Git` (object with `git` key), then `Registry` (bare string version constraint).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Dependency {
    /// A local path dependency: `{ path = "libs/json" }`.
    Path {
        /// Relative or absolute path to the dependency root.
        path: String,
    },
    /// A git repository dependency: `{ git = "https://...", tag = "v1.0" }`.
    Git {
        /// Git repository URL.
        git: String,
        /// Optional tag to check out.
        #[serde(skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
        /// Optional branch to check out.
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        /// Optional exact commit revision.
        #[serde(skip_serializing_if = "Option::is_none")]
        rev: Option<String>,
    },
    /// A registry dependency: `json = "1.2.0"` (shorthand for version constraint).
    ///
    /// The string is a semver constraint. `"1.2.0"` is shorthand for `">=1.2.0, <2.0.0"`.
    Registry(String),
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

/// Adds a dependency to a `Spark.toml` file, preserving formatting.
///
/// Reads the manifest file, inserts or updates the dependency in the
/// `[dependencies]` table, and writes back.
///
/// # Errors
///
/// Returns an error if the file cannot be read/written or is not valid TOML.
pub fn add_dependency(manifest_path: &Path, name: &str, version_str: &str) -> Result<(), String> {
    let contents = std::fs::read_to_string(manifest_path)
        .map_err(|e| format!("could not read {}: {e}", manifest_path.display()))?;

    let mut doc: toml_edit::DocumentMut = contents
        .parse()
        .map_err(|e| format!("failed to parse {}: {e}", manifest_path.display()))?;

    // Ensure [dependencies] table exists.
    if doc.get("dependencies").is_none() {
        doc["dependencies"] = toml_edit::Item::Table(toml_edit::Table::new());
    }

    doc["dependencies"][name] = toml_edit::value(version_str);

    std::fs::write(manifest_path, doc.to_string())
        .map_err(|e| format!("could not write {}: {e}", manifest_path.display()))
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
            other => panic!("expected Path dependency, got {other:?}"),
        }
    }

    #[test]
    fn parse_git_dependency_with_tag() {
        let toml = r#"
[project]
name = "test"
version = "0.1.0"

[dependencies]
json = { git = "https://github.com/spark-pkg/json", tag = "v1.0.0" }
"#;
        let manifest = parse_manifest(toml).unwrap();
        match &manifest.dependencies["json"] {
            Dependency::Git { git, tag, branch, rev } => {
                assert_eq!(git, "https://github.com/spark-pkg/json");
                assert_eq!(tag.as_deref(), Some("v1.0.0"));
                assert!(branch.is_none());
                assert!(rev.is_none());
            }
            other => panic!("expected Git dependency, got {other:?}"),
        }
    }

    #[test]
    fn parse_git_dependency_with_branch() {
        let toml = r#"
[project]
name = "test"
version = "0.1.0"

[dependencies]
json = { git = "https://github.com/spark-pkg/json", branch = "develop" }
"#;
        let manifest = parse_manifest(toml).unwrap();
        match &manifest.dependencies["json"] {
            Dependency::Git { branch, tag, rev, .. } => {
                assert_eq!(branch.as_deref(), Some("develop"));
                assert!(tag.is_none());
                assert!(rev.is_none());
            }
            other => panic!("expected Git dependency, got {other:?}"),
        }
    }

    #[test]
    fn parse_git_dependency_with_rev() {
        let toml = r#"
[project]
name = "test"
version = "0.1.0"

[dependencies]
json = { git = "https://github.com/spark-pkg/json", rev = "abc123def456" }
"#;
        let manifest = parse_manifest(toml).unwrap();
        match &manifest.dependencies["json"] {
            Dependency::Git { rev, tag, branch, .. } => {
                assert_eq!(rev.as_deref(), Some("abc123def456"));
                assert!(tag.is_none());
                assert!(branch.is_none());
            }
            other => panic!("expected Git dependency, got {other:?}"),
        }
    }

    #[test]
    fn parse_git_dependency_bare() {
        let toml = r#"
[project]
name = "test"
version = "0.1.0"

[dependencies]
json = { git = "https://github.com/spark-pkg/json" }
"#;
        let manifest = parse_manifest(toml).unwrap();
        match &manifest.dependencies["json"] {
            Dependency::Git { git, tag, branch, rev } => {
                assert_eq!(git, "https://github.com/spark-pkg/json");
                assert!(tag.is_none());
                assert!(branch.is_none());
                assert!(rev.is_none());
            }
            other => panic!("expected Git dependency, got {other:?}"),
        }
    }

    #[test]
    fn parse_mixed_path_and_git_deps() {
        let toml = r#"
[project]
name = "test"
version = "0.1.0"

[dependencies]
local = { path = "../local" }
remote = { git = "https://example.com/remote", tag = "v2.0" }
"#;
        let manifest = parse_manifest(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 2);
        assert!(matches!(&manifest.dependencies["local"], Dependency::Path { .. }));
        assert!(matches!(&manifest.dependencies["remote"], Dependency::Git { .. }));
    }

    #[test]
    fn parse_registry_dependency() {
        let toml = r#"
[project]
name = "test"
version = "0.1.0"

[dependencies]
json = "1.2.0"
"#;
        let manifest = parse_manifest(toml).unwrap();
        match &manifest.dependencies["json"] {
            Dependency::Registry(version) => assert_eq!(version, "1.2.0"),
            other => panic!("expected Registry dependency, got {other:?}"),
        }
    }

    #[test]
    fn parse_mixed_path_git_registry_deps() {
        let toml = r#"
[project]
name = "test"
version = "0.1.0"

[dependencies]
local = { path = "../local" }
remote = { git = "https://example.com/remote", tag = "v2.0" }
json = "1.0.0"
"#;
        let manifest = parse_manifest(toml).unwrap();
        assert_eq!(manifest.dependencies.len(), 3);
        assert!(matches!(&manifest.dependencies["local"], Dependency::Path { .. }));
        assert!(matches!(&manifest.dependencies["remote"], Dependency::Git { .. }));
        assert!(matches!(&manifest.dependencies["json"], Dependency::Registry(..)));
    }

    #[test]
    fn registry_dep_round_trip() {
        // Serialize within a HashMap (as it appears in Spark.toml).
        let mut deps = HashMap::new();
        deps.insert("json".to_string(), Dependency::Registry("1.2.3".to_string()));
        let serialized = toml::to_string(&deps).unwrap();
        let deserialized: HashMap<String, Dependency> = toml::from_str(&serialized).unwrap();
        match &deserialized["json"] {
            Dependency::Registry(v) => assert_eq!(v, "1.2.3"),
            other => panic!("expected Registry, got {other:?}"),
        }
    }

    #[test]
    fn parse_repository_field() {
        let toml = r#"
[project]
name = "test"
version = "0.1.0"
repository = "https://github.com/spark-pkg/test"
"#;
        let manifest = parse_manifest(toml).unwrap();
        assert_eq!(
            manifest.project.repository.as_deref(),
            Some("https://github.com/spark-pkg/test")
        );
    }

    #[test]
    fn add_dependency_to_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("Spark.toml");
        std::fs::write(
            &manifest_path,
            "[project]\nname = \"test\"\nversion = \"0.1.0\"\n\n[dependencies]\n",
        )
        .unwrap();

        add_dependency(&manifest_path, "json", "1.0.0").unwrap();

        let contents = std::fs::read_to_string(&manifest_path).unwrap();
        let manifest = parse_manifest(&contents).unwrap();
        match &manifest.dependencies["json"] {
            Dependency::Registry(v) => assert_eq!(v, "1.0.0"),
            other => panic!("expected Registry, got {other:?}"),
        }
    }

    #[test]
    fn add_dependency_creates_section() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_path = tmp.path().join("Spark.toml");
        std::fs::write(
            &manifest_path,
            "[project]\nname = \"test\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        add_dependency(&manifest_path, "utils", "0.5.0").unwrap();

        let contents = std::fs::read_to_string(&manifest_path).unwrap();
        let manifest = parse_manifest(&contents).unwrap();
        assert!(manifest.dependencies.contains_key("utils"));
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
