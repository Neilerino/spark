//! Package registry index management.
//!
//! The registry is a git-based index repository containing package metadata.
//! Each package has a `packages/<name>.json` file listing available versions,
//! download URLs, checksums, and dependencies.
//!
//! ## Index Format
//!
//! ```json
//! {
//!   "name": "json",
//!   "description": "JSON parser for Spark",
//!   "versions": [
//!     {
//!       "version": "1.0.0",
//!       "download": "https://github.com/spark-pkg/json/releases/download/json-v1.0.0/json-1.0.0.tar.gz",
//!       "checksum": "sha256:abc123...",
//!       "deps": { "utils": ">=0.5.0, <1.0.0" }
//!     }
//!   ]
//! }
//! ```
//!
//! ## Minimal Version Selection (MVS)
//!
//! Given a semver constraint and available versions, MVS picks the **minimum**
//! version that satisfies the constraint. Shorthand `"1.2.3"` is interpreted
//! as `">=1.2.3, <2.0.0"` (compatible range — same major version).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Default registry index URL.
const DEFAULT_INDEX_URL: &str = "https://github.com/spark-lang/index.git";

/// Returns the default registry index URL.
#[must_use]
pub const fn default_index_url() -> &'static str {
    DEFAULT_INDEX_URL
}

/// Configuration for the package registry.
#[derive(Debug, Clone)]
pub struct RegistryConfig {
    /// Git URL of the index repository.
    pub index_url: String,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            index_url: DEFAULT_INDEX_URL.to_string(),
        }
    }
}

/// Metadata for a single package in the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMetadata {
    /// Package name.
    pub name: String,
    /// Short description.
    pub description: String,
    /// Available versions (oldest first).
    pub versions: Vec<VersionEntry>,
}

/// A single published version of a package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionEntry {
    /// Semver version string.
    pub version: semver::Version,
    /// Direct download URL for the tarball.
    pub download: String,
    /// SHA-256 checksum in `sha256:<hex>` format.
    pub checksum: String,
    /// Dependencies: `name -> version constraint`.
    #[serde(default)]
    pub deps: HashMap<String, String>,
}

/// Result of fetching a registry package.
#[derive(Debug)]
pub struct FetchedRegistryDep {
    /// Local path to the extracted package.
    pub path: PathBuf,
    /// Resolved version.
    pub version: semver::Version,
}

/// Clones or updates the registry index to `.spark/index/`.
///
/// Uses `git clone --depth 1` if not present, `git pull` if already cloned.
///
/// # Errors
///
/// Returns an error if git operations fail.
pub fn sync_index(project_root: &Path) -> Result<PathBuf, String> {
    sync_index_from(project_root, DEFAULT_INDEX_URL)
}

/// Clones or updates the registry index from a specific URL.
///
/// # Errors
///
/// Returns an error if git operations fail.
pub fn sync_index_from(project_root: &Path, index_url: &str) -> Result<PathBuf, String> {
    let index_dir = project_root.join(".spark").join("index");

    if index_dir.join(".git").exists() {
        // Update existing index.
        let output = Command::new("git")
            .args(["pull", "--quiet"])
            .current_dir(&index_dir)
            .output()
            .map_err(|e| format!("could not run git pull on index: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("failed to update registry index: {stderr}"));
        }
    } else {
        // Clone fresh.
        std::fs::create_dir_all(index_dir.parent().unwrap_or(project_root)).map_err(|e| {
            format!("could not create .spark directory: {e}")
        })?;

        let output = Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                "--quiet",
                index_url,
                &index_dir.display().to_string(),
            ])
            .output()
            .map_err(|e| format!("could not clone registry index: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("failed to clone registry index: {stderr}"));
        }
    }

    Ok(index_dir)
}

/// Looks up a package in the local index.
///
/// Reads `packages/<name>.json` from the index directory.
///
/// # Errors
///
/// Returns an error if the package is not found or the JSON is malformed.
pub fn lookup_package(index_path: &Path, name: &str) -> Result<PackageMetadata, String> {
    let pkg_file = index_path.join("packages").join(format!("{name}.json"));
    if !pkg_file.exists() {
        return Err(format!("package `{name}` not found in registry index"));
    }

    let contents = std::fs::read_to_string(&pkg_file)
        .map_err(|e| format!("could not read package metadata for `{name}`: {e}"))?;

    serde_json::from_str(&contents)
        .map_err(|e| format!("malformed package metadata for `{name}`: {e}"))
}

/// Resolves a version using Minimal Version Selection (MVS).
///
/// Parses the constraint string and finds the minimum version that satisfies it.
/// Shorthand `"1.2.3"` is interpreted as `">=1.2.3, <2.0.0"`.
///
/// # Errors
///
/// Returns an error if no version satisfies the constraint or the constraint
/// string is malformed.
pub fn resolve_version<'a>(
    metadata: &'a PackageMetadata,
    constraint: &str,
) -> Result<&'a VersionEntry, String> {
    let req = parse_version_req(constraint)?;

    // MVS: find the minimum matching version.
    metadata
        .versions
        .iter()
        .filter(|v| req.matches(&v.version))
        .min_by(|a, b| a.version.cmp(&b.version))
        .ok_or_else(|| {
            format!(
                "no version of `{}` satisfies constraint `{constraint}`",
                metadata.name
            )
        })
}

/// Parses a version constraint string into a `semver::VersionReq`.
///
/// Bare versions like `"1.2.3"` are interpreted as `">=1.2.3, <2.0.0"`.
/// Full constraint syntax like `">=1.0.0, <2.0.0"` is passed through directly.
///
/// # Errors
///
/// Returns an error if the constraint string is malformed.
pub fn parse_version_req(constraint: &str) -> Result<semver::VersionReq, String> {
    let trimmed = constraint.trim();

    // If it looks like a bare version (no operators), convert to compatible range.
    if !trimmed.is_empty()
        && trimmed
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        && !trimmed.contains(',')
    {
        // Parse as a version to validate.
        let version: semver::Version = trimmed
            .parse()
            .map_err(|e| format!("invalid version `{trimmed}`: {e}"))?;

        let next_major = version.major + 1;
        let req_str = format!(">={trimmed}, <{next_major}.0.0");
        semver::VersionReq::parse(&req_str)
            .map_err(|e| format!("invalid version constraint `{req_str}`: {e}"))
    } else {
        semver::VersionReq::parse(trimmed)
            .map_err(|e| format!("invalid version constraint `{trimmed}`: {e}"))
    }
}

/// Downloads a registry package tarball, verifies its checksum, and extracts it.
///
/// Packages are extracted to `.spark/packages/<name>-<version>/`.
/// Idempotent: skips download if the directory already exists.
///
/// # Errors
///
/// Returns an error if the download, checksum verification, or extraction fails.
pub fn fetch_registry_dep(
    name: &str,
    entry: &VersionEntry,
    project_root: &Path,
) -> Result<FetchedRegistryDep, String> {
    let packages_dir = project_root.join(".spark").join("packages");
    let target_dir = packages_dir.join(format!("{name}-{}", entry.version));

    // Idempotent: if directory already exists, skip.
    if target_dir.exists() {
        return Ok(FetchedRegistryDep {
            path: target_dir,
            version: entry.version.clone(),
        });
    }

    std::fs::create_dir_all(&packages_dir).map_err(|e| {
        format!(
            "could not create packages directory `{}`: {e}",
            packages_dir.display()
        )
    })?;

    // Download tarball.
    let tarball_path = packages_dir.join(format!("{name}-{}.tar.gz", entry.version));
    run_curl(&entry.download, &tarball_path)?;

    // Verify checksum.
    verify_checksum(&tarball_path, &entry.checksum)?;

    // Extract tarball.
    std::fs::create_dir_all(&target_dir).map_err(|e| {
        format!("could not create directory `{}`: {e}", target_dir.display())
    })?;
    extract_tarball(&tarball_path, &target_dir)?;

    // Clean up tarball.
    let _ = std::fs::remove_file(&tarball_path);

    Ok(FetchedRegistryDep {
        path: target_dir,
        version: entry.version.clone(),
    })
}

/// Searches the index for packages matching a query string.
///
/// Performs a case-insensitive substring match on package name and description.
///
/// # Errors
///
/// Returns an error if the index directory cannot be read.
pub fn search_index(index_path: &Path, query: &str) -> Result<Vec<PackageMetadata>, String> {
    let packages_dir = index_path.join("packages");
    if !packages_dir.exists() {
        return Ok(Vec::new());
    }

    let entries = std::fs::read_dir(&packages_dir)
        .map_err(|e| format!("could not read index packages directory: {e}"))?;

    let query_lower = query.to_lowercase();
    let mut results = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|e| format!("could not read directory entry: {e}"))?;
        let path = entry.path();

        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };

        let metadata: PackageMetadata = match serde_json::from_str(&contents) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if metadata.name.to_lowercase().contains(&query_lower)
            || metadata.description.to_lowercase().contains(&query_lower)
        {
            results.push(metadata);
        }
    }

    results.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(results)
}

/// Downloads a URL to a file using `curl`.
fn run_curl(url: &str, output_path: &Path) -> Result<(), String> {
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--location",
            "--output",
            &output_path.display().to_string(),
            url,
        ])
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("failed to download `{url}`: {stderr}"))
    }
}

/// Computes the SHA-256 checksum of a file and compares it to the expected value.
///
/// Expected format: `sha256:<hex>`.
///
/// # Errors
///
/// Returns an error if the file cannot be read or the checksum does not match.
pub fn verify_checksum(path: &Path, expected: &str) -> Result<(), String> {
    let expected_hex = expected
        .strip_prefix("sha256:")
        .ok_or_else(|| format!("invalid checksum format (expected sha256:<hex>): `{expected}`"))?;

    let data = std::fs::read(path)
        .map_err(|e| format!("could not read `{}` for checksum: {e}", path.display()))?;

    let mut hasher = Sha256::new();
    hasher.update(&data);
    let result = hasher.finalize();
    let actual_hex = format!("{result:x}");

    if actual_hex != expected_hex {
        return Err(format!(
            "checksum mismatch for `{}`: expected {expected_hex}, got {actual_hex}",
            path.display()
        ));
    }

    Ok(())
}

/// Extracts a `.tar.gz` archive to a destination directory.
fn extract_tarball(tarball: &Path, dest: &Path) -> Result<(), String> {
    let output = Command::new("tar")
        .args([
            "xzf",
            &tarball.display().to_string(),
            "-C",
            &dest.display().to_string(),
            "--strip-components=1",
        ])
        .output()
        .map_err(|e| format!("could not run tar: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "failed to extract `{}`: {stderr}",
            tarball.display()
        ))
    }
}

/// Computes the SHA-256 checksum of a file and returns it in `sha256:<hex>` format.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
pub fn compute_checksum(path: &Path) -> Result<String, String> {
    let data = std::fs::read(path)
        .map_err(|e| format!("could not read `{}`: {e}", path.display()))?;

    let mut hasher = Sha256::new();
    hasher.update(&data);
    let result = hasher.finalize();
    Ok(format!("sha256:{result:x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_metadata(versions: Vec<(&str, &str)>) -> PackageMetadata {
        PackageMetadata {
            name: "test".to_string(),
            description: "A test package".to_string(),
            versions: versions
                .into_iter()
                .map(|(ver, url)| VersionEntry {
                    version: ver.parse().unwrap(),
                    download: url.to_string(),
                    checksum: "sha256:0000".to_string(),
                    deps: HashMap::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn mvs_picks_minimum_matching() {
        let meta = make_metadata(vec![
            ("1.0.0", "http://example.com/1.0.0.tar.gz"),
            ("1.2.0", "http://example.com/1.2.0.tar.gz"),
            ("1.3.0", "http://example.com/1.3.0.tar.gz"),
            ("2.0.0", "http://example.com/2.0.0.tar.gz"),
        ]);

        let entry = resolve_version(&meta, ">=1.2.0, <2.0.0").unwrap();
        assert_eq!(entry.version.to_string(), "1.2.0");
    }

    #[test]
    fn mvs_bare_version_shorthand() {
        let meta = make_metadata(vec![
            ("1.0.0", "http://example.com/1.0.0.tar.gz"),
            ("1.2.0", "http://example.com/1.2.0.tar.gz"),
            ("1.3.0", "http://example.com/1.3.0.tar.gz"),
            ("2.0.0", "http://example.com/2.0.0.tar.gz"),
        ]);

        // "1.2.0" -> ">=1.2.0, <2.0.0", MVS picks 1.2.0
        let entry = resolve_version(&meta, "1.2.0").unwrap();
        assert_eq!(entry.version.to_string(), "1.2.0");
    }

    #[test]
    fn mvs_no_match() {
        let meta = make_metadata(vec![
            ("1.0.0", "http://example.com/1.0.0.tar.gz"),
            ("1.1.0", "http://example.com/1.1.0.tar.gz"),
        ]);

        let result = resolve_version(&meta, ">=2.0.0");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no version"));
    }

    #[test]
    fn mvs_exact_constraint() {
        let meta = make_metadata(vec![
            ("1.0.0", "http://example.com/1.0.0.tar.gz"),
            ("1.2.0", "http://example.com/1.2.0.tar.gz"),
        ]);

        let entry = resolve_version(&meta, "=1.0.0").unwrap();
        assert_eq!(entry.version.to_string(), "1.0.0");
    }

    #[test]
    fn mvs_skips_lower_versions() {
        let meta = make_metadata(vec![
            ("0.5.0", "http://example.com/0.5.0.tar.gz"),
            ("0.9.0", "http://example.com/0.9.0.tar.gz"),
            ("1.0.0", "http://example.com/1.0.0.tar.gz"),
            ("1.5.0", "http://example.com/1.5.0.tar.gz"),
        ]);

        let entry = resolve_version(&meta, "1.0.0").unwrap();
        assert_eq!(entry.version.to_string(), "1.0.0");
    }

    #[test]
    fn parse_version_req_bare() {
        let req = parse_version_req("1.2.3").unwrap();
        assert!(req.matches(&"1.2.3".parse().unwrap()));
        assert!(req.matches(&"1.5.0".parse().unwrap()));
        assert!(!req.matches(&"2.0.0".parse().unwrap()));
        assert!(!req.matches(&"1.2.2".parse().unwrap()));
    }

    #[test]
    fn parse_version_req_full_constraint() {
        let req = parse_version_req(">=1.0.0, <1.5.0").unwrap();
        assert!(req.matches(&"1.0.0".parse().unwrap()));
        assert!(req.matches(&"1.4.9".parse().unwrap()));
        assert!(!req.matches(&"1.5.0".parse().unwrap()));
    }

    #[test]
    fn parse_version_req_invalid() {
        assert!(parse_version_req("not.a.version").is_err());
    }

    #[test]
    fn checksum_verification() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("test.txt");
        std::fs::write(&file_path, b"hello world").unwrap();

        let checksum = compute_checksum(&file_path).unwrap();
        assert!(checksum.starts_with("sha256:"));

        // Should succeed with correct checksum.
        verify_checksum(&file_path, &checksum).unwrap();

        // Should fail with wrong checksum.
        let result = verify_checksum(&file_path, "sha256:0000000000000000");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("checksum mismatch"));
    }

    #[test]
    fn checksum_bad_format() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("test.txt");
        std::fs::write(&file_path, b"hello").unwrap();

        let result = verify_checksum(&file_path, "md5:abcdef");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid checksum format"));
    }

    #[test]
    fn lookup_existing_package() {
        let tmp = tempfile::tempdir().unwrap();
        let packages_dir = tmp.path().join("packages");
        std::fs::create_dir_all(&packages_dir).unwrap();

        let meta = PackageMetadata {
            name: "json".to_string(),
            description: "JSON parser".to_string(),
            versions: vec![VersionEntry {
                version: "1.0.0".parse().unwrap(),
                download: "http://example.com/json-1.0.0.tar.gz".to_string(),
                checksum: "sha256:abc123".to_string(),
                deps: HashMap::new(),
            }],
        };

        let json = serde_json::to_string_pretty(&meta).unwrap();
        std::fs::write(packages_dir.join("json.json"), json).unwrap();

        let result = lookup_package(tmp.path(), "json").unwrap();
        assert_eq!(result.name, "json");
        assert_eq!(result.versions.len(), 1);
        assert_eq!(result.versions[0].version.to_string(), "1.0.0");
    }

    #[test]
    fn lookup_missing_package() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("packages")).unwrap();

        let result = lookup_package(tmp.path(), "nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn search_matches_name_and_description() {
        let tmp = tempfile::tempdir().unwrap();
        let packages_dir = tmp.path().join("packages");
        std::fs::create_dir_all(&packages_dir).unwrap();

        let write_pkg = |name: &str, desc: &str| {
            let meta = PackageMetadata {
                name: name.to_string(),
                description: desc.to_string(),
                versions: vec![],
            };
            let json = serde_json::to_string(&meta).unwrap();
            std::fs::write(packages_dir.join(format!("{name}.json")), json).unwrap();
        };

        write_pkg("json", "JSON parser for Spark");
        write_pkg("yaml", "YAML parser for Spark");
        write_pkg("http", "HTTP client library");

        // Search by name.
        let results = search_index(tmp.path(), "json").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "json");

        // Search by description.
        let results = search_index(tmp.path(), "parser").unwrap();
        assert_eq!(results.len(), 2);

        // Search with no matches.
        let results = search_index(tmp.path(), "database").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        let results = search_index(tmp.path(), "anything").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn sync_index_with_local_repo() {
        let tmp = tempfile::tempdir().unwrap();

        // Create a local git repo to serve as the index.
        let index_source = tmp.path().join("source");
        std::fs::create_dir_all(index_source.join("packages")).unwrap();
        std::fs::write(
            index_source.join("config.json"),
            r#"{"index_version": 1}"#,
        )
        .unwrap();

        let meta = PackageMetadata {
            name: "testpkg".to_string(),
            description: "A test package".to_string(),
            versions: vec![],
        };
        std::fs::write(
            index_source.join("packages").join("testpkg.json"),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();

        // Initialize as a git repo.
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&index_source)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&index_source)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&index_source)
            .output()
            .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&index_source)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--quiet", "-m", "initial"])
            .current_dir(&index_source)
            .output()
            .unwrap();

        // Sync index from local file:// URL.
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let url = format!("file://{}", index_source.display());
        let index_path = sync_index_from(&project_root, &url).unwrap();

        assert!(index_path.join("config.json").exists());
        assert!(index_path.join("packages").join("testpkg.json").exists());

        // Lookup should work on the synced index.
        let result = lookup_package(&index_path, "testpkg").unwrap();
        assert_eq!(result.name, "testpkg");
    }

    #[test]
    fn version_entry_with_deps_serialization() {
        let entry = VersionEntry {
            version: "1.2.0".parse().unwrap(),
            download: "http://example.com/pkg-1.2.0.tar.gz".to_string(),
            checksum: "sha256:abc123".to_string(),
            deps: {
                let mut m = HashMap::new();
                m.insert("utils".to_string(), ">=0.5.0, <1.0.0".to_string());
                m
            },
        };

        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: VersionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.version.to_string(), "1.2.0");
        assert_eq!(deserialized.deps.len(), 1);
        assert_eq!(deserialized.deps["utils"], ">=0.5.0, <1.0.0");
    }
}
