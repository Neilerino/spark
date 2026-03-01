//! Package publishing support.
//!
//! Provides functions for packaging a Spark project as a `.tar.gz` tarball,
//! uploading it as a GitHub Release, and updating the registry index.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::registry::{compute_checksum, PackageMetadata, VersionEntry};

/// Packages a Spark project into a `.tar.gz` tarball.
///
/// Includes all `.spark` files and the `Spark.toml` manifest. The tarball
/// is created in the project root as `<name>-<version>.tar.gz`.
///
/// Returns `(tarball_path, sha256_checksum)`.
///
/// # Errors
///
/// Returns an error if the tarball cannot be created.
pub fn package_project(root: &Path) -> Result<(PathBuf, String), String> {
    let manifest = crate::manifest::load_manifest(root)
        .ok_or_else(|| "no Spark.toml found".to_string())??;

    let name = &manifest.project.name;
    let version = &manifest.project.version;
    let tarball_name = format!("{name}-{version}.tar.gz");
    let tarball_path = root.join(&tarball_name);

    // Collect files to include: Spark.toml + all .spark files.
    let mut files: Vec<String> = vec!["Spark.toml".to_string()];
    collect_spark_files_relative(root, root, &mut files)?;

    if files.len() <= 1 {
        return Err("no .spark source files found to package".to_string());
    }

    // Create tarball using tar. We use a wrapper directory so that extracting
    // the tarball creates `<name>-<version>/` as the top-level directory.
    let wrapper_dir = format!("{name}-{version}");
    let mut tar_args = vec![
        "czf".to_string(),
        tarball_path.display().to_string(),
        "--transform".to_string(),
        format!("s,^,{wrapper_dir}/,"),
    ];
    tar_args.extend(files);

    let output = Command::new("tar")
        .args(&tar_args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("could not run tar: {e}"))?;

    if !output.status.success() {
        // macOS tar doesn't support --transform, try with gtar or -s flag.
        let output = create_tarball_macos(root, &tarball_path, &wrapper_dir, &tar_args[4..])?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("failed to create tarball: {stderr}"));
        }
    }

    let checksum = compute_checksum(&tarball_path)?;

    Ok((tarball_path, checksum))
}

/// Fallback tarball creation for macOS (which lacks `--transform`).
fn create_tarball_macos(
    root: &Path,
    tarball_path: &Path,
    wrapper_dir: &str,
    files: &[String],
) -> Result<std::process::Output, String> {
    let mut args = vec![
        "czf".to_string(),
        tarball_path.display().to_string(),
        "-s".to_string(),
        format!(",^,{wrapper_dir}/,"),
    ];
    args.extend(files.iter().cloned());

    Command::new("tar")
        .args(&args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("could not run tar: {e}"))
}

/// Recursively collects `.spark` file paths relative to `base`.
fn collect_spark_files_relative(
    dir: &Path,
    base: &Path,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("could not read directory `{}`: {e}", dir.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("could not read directory entry: {e}"))?;
        let path = entry.path();

        // Skip hidden directories (like .spark, .git).
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') {
                continue;
            }
        }

        if path.is_dir() {
            collect_spark_files_relative(&path, base, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("spark") {
            let relative = path
                .strip_prefix(base)
                .map_err(|e| format!("could not compute relative path: {e}"))?;
            out.push(relative.display().to_string());
        }
    }

    Ok(())
}

/// Updates the registry index with a new version entry for a package.
///
/// If the package already exists in the index, appends the new version.
/// If not, creates a new package metadata file.
///
/// # Errors
///
/// Returns an error if the index cannot be read or written.
pub fn update_index(
    index_path: &Path,
    name: &str,
    description: &str,
    entry: &VersionEntry,
) -> Result<(), String> {
    let packages_dir = index_path.join("packages");
    std::fs::create_dir_all(&packages_dir)
        .map_err(|e| format!("could not create packages directory: {e}"))?;

    let pkg_file = packages_dir.join(format!("{name}.json"));

    let mut metadata = if pkg_file.exists() {
        let contents = std::fs::read_to_string(&pkg_file)
            .map_err(|e| format!("could not read {}: {e}", pkg_file.display()))?;
        serde_json::from_str::<PackageMetadata>(&contents)
            .map_err(|e| format!("malformed package metadata: {e}"))?
    } else {
        PackageMetadata {
            name: name.to_string(),
            description: description.to_string(),
            versions: Vec::new(),
        }
    };

    // Check for duplicate version.
    if metadata.versions.iter().any(|v| v.version == entry.version) {
        return Err(format!(
            "version {} of `{name}` already exists in the index",
            entry.version
        ));
    }

    metadata.versions.push(entry.clone());

    // Sort versions for deterministic output.
    metadata.versions.sort_by(|a, b| a.version.cmp(&b.version));

    let json = serde_json::to_string_pretty(&metadata)
        .map_err(|e| format!("could not serialize package metadata: {e}"))?;
    std::fs::write(&pkg_file, json)
        .map_err(|e| format!("could not write {}: {e}", pkg_file.display()))?;

    // Commit the change.
    let output = Command::new("git")
        .args(["add", &format!("packages/{name}.json")])
        .current_dir(index_path)
        .output()
        .map_err(|e| format!("could not stage index change: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("failed to stage index change: {stderr}"));
    }

    let commit_msg = format!("Publish {name} v{}", entry.version);
    let output = Command::new("git")
        .args(["commit", "--quiet", "-m", &commit_msg])
        .current_dir(index_path)
        .output()
        .map_err(|e| format!("could not commit index change: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("failed to commit index change: {stderr}"));
    }

    Ok(())
}

/// Uploads a tarball as a GitHub Release using the `gh` CLI.
///
/// Returns the download URL for the uploaded asset.
///
/// # Errors
///
/// Returns an error if `gh` is not installed or the upload fails.
pub fn upload_release(
    tarball: &Path,
    repo_url: &str,
    name: &str,
    version: &str,
    token: Option<&str>,
) -> Result<String, String> {
    // Extract owner/repo from URL.
    let repo = extract_github_repo(repo_url)?;
    let tag = format!("{name}-v{version}");
    let title = format!("{name} v{version}");

    let mut cmd = Command::new("gh");
    cmd.args([
        "release",
        "create",
        &tag,
        &tarball.display().to_string(),
        "--repo",
        &repo,
        "--title",
        &title,
    ]);

    if let Some(t) = token {
        cmd.env("GH_TOKEN", t);
    }

    let output = cmd
        .output()
        .map_err(|e| format!("could not run `gh`: {e}. Is the GitHub CLI installed?"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("failed to create GitHub release: {stderr}"));
    }

    // Construct the download URL.
    let tarball_name = tarball
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("package.tar.gz");
    let download_url = format!(
        "https://github.com/{repo}/releases/download/{tag}/{tarball_name}"
    );

    Ok(download_url)
}

/// Extracts `owner/repo` from a GitHub URL.
///
/// Supports `https://github.com/owner/repo` and `https://github.com/owner/repo.git`.
fn extract_github_repo(url: &str) -> Result<String, String> {
    let url = url.trim_end_matches(".git");

    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .ok_or_else(|| {
            format!("repository URL `{url}` does not look like a GitHub URL (expected https://github.com/owner/repo)")
        })?;

    let parts: Vec<&str> = path.splitn(3, '/').collect();
    if parts.len() < 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(format!(
            "could not extract owner/repo from URL `{url}`"
        ));
    }

    Ok(format!("{}/{}", parts[0], parts[1]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn extract_github_repo_https() {
        assert_eq!(
            extract_github_repo("https://github.com/spark-pkg/json").unwrap(),
            "spark-pkg/json"
        );
    }

    #[test]
    fn extract_github_repo_with_git_suffix() {
        assert_eq!(
            extract_github_repo("https://github.com/spark-pkg/json.git").unwrap(),
            "spark-pkg/json"
        );
    }

    #[test]
    fn extract_github_repo_invalid() {
        assert!(extract_github_repo("https://gitlab.com/owner/repo").is_err());
    }

    #[test]
    fn update_index_new_package() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("index");
        std::fs::create_dir_all(&index_path).unwrap();

        // Initialize a git repo for the index.
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&index_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&index_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&index_path)
            .output()
            .unwrap();
        // Need an initial commit for `git commit` to work.
        std::fs::write(index_path.join("config.json"), "{}").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&index_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--quiet", "-m", "init"])
            .current_dir(&index_path)
            .output()
            .unwrap();

        let entry = VersionEntry {
            version: "1.0.0".parse().unwrap(),
            download: "http://example.com/json-1.0.0.tar.gz".to_string(),
            checksum: "sha256:abc123".to_string(),
            deps: HashMap::new(),
        };

        update_index(&index_path, "json", "JSON parser", &entry).unwrap();

        let meta = crate::registry::lookup_package(&index_path, "json").unwrap();
        assert_eq!(meta.name, "json");
        assert_eq!(meta.versions.len(), 1);
        assert_eq!(meta.versions[0].version.to_string(), "1.0.0");
    }

    #[test]
    fn update_index_existing_package() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("index");
        std::fs::create_dir_all(&index_path).unwrap();

        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&index_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&index_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&index_path)
            .output()
            .unwrap();
        std::fs::write(index_path.join("config.json"), "{}").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&index_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--quiet", "-m", "init"])
            .current_dir(&index_path)
            .output()
            .unwrap();

        let entry1 = VersionEntry {
            version: "1.0.0".parse().unwrap(),
            download: "http://example.com/json-1.0.0.tar.gz".to_string(),
            checksum: "sha256:abc123".to_string(),
            deps: HashMap::new(),
        };
        update_index(&index_path, "json", "JSON parser", &entry1).unwrap();

        let entry2 = VersionEntry {
            version: "1.1.0".parse().unwrap(),
            download: "http://example.com/json-1.1.0.tar.gz".to_string(),
            checksum: "sha256:def456".to_string(),
            deps: HashMap::new(),
        };
        update_index(&index_path, "json", "JSON parser", &entry2).unwrap();

        let meta = crate::registry::lookup_package(&index_path, "json").unwrap();
        assert_eq!(meta.versions.len(), 2);
        assert_eq!(meta.versions[0].version.to_string(), "1.0.0");
        assert_eq!(meta.versions[1].version.to_string(), "1.1.0");
    }

    #[test]
    fn update_index_duplicate_version_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let index_path = tmp.path().join("index");
        std::fs::create_dir_all(&index_path).unwrap();

        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&index_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&index_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&index_path)
            .output()
            .unwrap();
        std::fs::write(index_path.join("config.json"), "{}").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&index_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "--quiet", "-m", "init"])
            .current_dir(&index_path)
            .output()
            .unwrap();

        let entry = VersionEntry {
            version: "1.0.0".parse().unwrap(),
            download: "http://example.com/json-1.0.0.tar.gz".to_string(),
            checksum: "sha256:abc123".to_string(),
            deps: HashMap::new(),
        };
        update_index(&index_path, "json", "JSON parser", &entry).unwrap();

        let result = update_index(&index_path, "json", "JSON parser", &entry);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already exists"));
    }

    #[test]
    fn package_project_creates_tarball() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        std::fs::create_dir_all(root.join("mylib")).unwrap();

        std::fs::write(
            root.join("Spark.toml"),
            "[project]\nname = \"mylib\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("mylib").join("mylib.spark"),
            "module mylib\npub fn hello(): Int { return 1 }\n",
        )
        .unwrap();

        let (tarball, checksum) = package_project(&root).unwrap();
        assert!(tarball.exists());
        assert!(checksum.starts_with("sha256:"));
        assert!(tarball.file_name().unwrap().to_str().unwrap().contains("mylib-1.0.0"));
    }
}
